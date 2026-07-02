//! Focused-window watcher: is a fullscreen game active right now, and which one?
//!
//! The XDG ScreenCast portal deliberately hides the window list, so game detection
//! rides the compositor's window-management protocols on a dedicated Wayland
//! connection instead (independent of the portal/PipeWire capture path):
//!
//! - KDE: `org_kde_plasma_window_management` — active + fullscreen + app id + pid,
//!   event-driven. Recent KWin (observed on Plasma 6.7) no longer advertises it to
//!   ordinary clients, so it is tried first but expected to be rare.
//! - wlroots family (sway, Hyprland, niri, COSMIC, ...):
//!   `zwlr_foreign_toplevel_management_v1` — active + fullscreen + app id.
//! - KDE fallback: a tiny KWin script (loaded over the `org.kde.kwin.Scripting`
//!   DBus interface, the kdotool technique) that reports the active window back to
//!   us over DBus — active + fullscreen + app id + pid.
//!
//! GNOME offers none of these; [`FocusWatcher::spawn`] then fails and the caller
//! falls back to ungated capture (with a log line saying so).
//!
//! A window counts as "the game" when it is active (focused) and fullscreen and its
//! app id isn't a desktop-shell id — the same conservative heuristic as the Windows
//! detector (`windows::game_window`): capture too little rather than too much.

use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use thiserror::Error;
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window::{
    self, OrgKdePlasmaWindow,
};
use wayland_protocols_plasma::plasma_window_management::client::org_kde_plasma_window_management::{
    self, OrgKdePlasmaWindowManagement,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::{
    self, ZwlrForeignToplevelHandleV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::{
    self, ZwlrForeignToplevelManagerV1,
};

use crate::game::{GameInfo, is_shell_app_id};

/// How long the watcher thread sleeps in poll(2) between stop-flag checks.
const POLL_TIMEOUT_MS: i32 = 200;

/// Why the watcher could not start.
#[derive(Debug, Error)]
pub enum FocusError {
    #[error("could not connect to the Wayland display: {0}")]
    Connect(String),
    #[error(
        "this compositor offers no window-management path (no plasma or wlr \
         foreign-toplevel protocol, no KWin scripting); game detection is unavailable"
    )]
    NoBackend,
    #[error("KWin scripting backend failed: {0}")]
    Kwin(String),
    #[error("could not spawn the focus watcher thread: {0}")]
    Spawn(std::io::Error),
}

/// State shared between the watcher thread and its readers.
#[derive(Default)]
struct Shared {
    /// The focused fullscreen game right now, if any.
    current: Mutex<Option<GameInfo>>,
    /// The most recent game that was current — what the (paused) buffer still holds
    /// after an alt-tab or a game exit.
    last: Mutex<Option<GameInfo>>,
}

/// Watches the compositor for the focused fullscreen game on a background thread.
/// Dropping it stops the backend (and joins its thread, where it has one).
pub struct FocusWatcher {
    shared: Arc<Shared>,
    backend: &'static str,
    guts: Guts,
}

/// Backend-specific lifetime handles.
enum Guts {
    /// plasma/wlr protocol dispatch on our own thread.
    Wayland {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    },
    /// KWin script pushing updates to us over DBus; zbus serves the callback object
    /// on its own executor, so there is no thread of ours to join. The handle is
    /// held purely for its Drop (unload the script).
    KwinScript(#[allow(dead_code)] kwin_script::ScriptHandle),
}

impl FocusWatcher {
    /// Connect, pick a backend, and start watching. Fails fast (before anything
    /// spawns) when no usable path exists, so the caller can log and fall back to
    /// ungated capture.
    pub fn spawn() -> Result<Self, FocusError> {
        let shared = Arc::new(Shared::default());
        match Self::spawn_wayland(shared.clone()) {
            Err(FocusError::NoBackend) => {}
            other => return other,
        }
        let handle = kwin_script::start(shared.clone())?;
        tracing::info!(
            backend = "kwin-script",
            "focus watcher running (game detection)"
        );
        Ok(Self {
            shared,
            backend: "kwin-script",
            guts: Guts::KwinScript(handle),
        })
    }

    /// The protocol-based backends (plasma / wlr foreign-toplevel).
    fn spawn_wayland(shared: Arc<Shared>) -> Result<Self, FocusError> {
        let conn = Connection::connect_to_env().map_err(|e| FocusError::Connect(e.to_string()))?;
        let (globals, mut queue) = registry_queue_init::<Tracker>(&conn)
            .map_err(|e| FocusError::Connect(e.to_string()))?;
        let qh = queue.handle();

        let mut tracker = Tracker {
            windows: HashMap::new(),
            plasma_ids: HashSet::new(),
            plasma_mgmt: None,
            shared: shared.clone(),
        };

        // Plasma first (gives pid too); wlroots second. Binding also subscribes us to
        // the initial burst of window announcements.
        let backend = if let Ok(mgmt) =
            globals.bind::<OrgKdePlasmaWindowManagement, _, _>(&qh, 17..=18, ())
        {
            tracker.plasma_mgmt = Some(mgmt);
            "plasma-window-management"
        } else if globals
            .bind::<ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, ())
            .is_ok()
        {
            "wlr-foreign-toplevel-management"
        } else {
            return Err(FocusError::NoBackend);
        };

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("rewynd-focus".to_owned())
            .spawn(move || {
                if let Err(e) = watch_loop(&conn, &mut queue, &mut tracker, &thread_stop) {
                    tracing::warn!(error = %e, "focus watcher stopped; game detection is stale");
                }
            })
            .map_err(FocusError::Spawn)?;

        tracing::info!(backend, "focus watcher running (game detection)");
        Ok(Self {
            shared,
            backend,
            guts: Guts::Wayland {
                stop,
                thread: Some(thread),
            },
        })
    }

    /// The focused fullscreen game right now, if any.
    #[must_use]
    pub fn current_game(&self) -> Option<GameInfo> {
        self.shared
            .current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The most recent game that was focused fullscreen — the one the paused buffer
    /// still holds right after an alt-tab or a game exit.
    #[must_use]
    pub fn last_game(&self) -> Option<GameInfo> {
        self.shared
            .last
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Which protocol backs this watcher (for logs/diagnostics).
    #[must_use]
    pub fn backend(&self) -> &'static str {
        self.backend
    }
}

impl Drop for FocusWatcher {
    fn drop(&mut self) {
        match &mut self.guts {
            Guts::Wayland { stop, thread } => {
                stop.store(true, Ordering::Relaxed);
                if let Some(thread) = thread.take() {
                    let _ = thread.join();
                }
            }
            // ScriptHandle's own Drop unloads the KWin script.
            Guts::KwinScript(_) => {}
        }
    }
}

/// Dispatch Wayland events until `stop`: poll the connection fd with a timeout so the
/// stop flag is honored within [`POLL_TIMEOUT_MS`] even when the compositor is silent.
fn watch_loop(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<Tracker>,
    tracker: &mut Tracker,
    stop: &AtomicBool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    while !stop.load(Ordering::Relaxed) {
        queue.dispatch_pending(tracker)?;
        tracker.publish();
        queue.flush()?;

        // `None` means events are already queued locally — loop straight to dispatch.
        let Some(guard) = conn.prepare_read() else {
            continue;
        };
        let mut pfd = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd, in scope for the call.
        let ready = unsafe { libc::poll(&raw mut pfd, 1, POLL_TIMEOUT_MS) };
        if ready > 0 {
            // A read error here is a lost connection; surface it and end the watcher.
            guard.read()?;
        } else {
            drop(guard);
        }
    }
    Ok(())
}

/// One tracked toplevel. Plasma applies state as events arrive; wlr stages into
/// `pending_*` and applies on `done` (the protocol's atomicity marker).
#[derive(Default)]
struct Win {
    app_id: String,
    title: String,
    pid: Option<u32>,
    active: bool,
    fullscreen: bool,
    pending_active: bool,
    pending_fullscreen: bool,
}

struct Tracker {
    windows: HashMap<ObjectId, Win>,
    /// Plasma announces windows by numeric id, once via `window` and once via
    /// `window_with_uuid`; remember which we already materialized.
    plasma_ids: HashSet<u32>,
    plasma_mgmt: Option<OrgKdePlasmaWindowManagement>,
    shared: Arc<Shared>,
}

impl Tracker {
    /// Recompute the focused fullscreen game and publish it to readers.
    fn publish(&self) {
        let game = self
            .windows
            .values()
            .find(|w| w.active && w.fullscreen && !is_shell_app_id(&w.app_id))
            .map(|w| GameInfo {
                app_id: w.app_id.clone(),
                title: w.title.clone(),
                pid: w.pid,
            });
        let mut current = self
            .shared
            .current
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if *current != game {
            if let Some(game) = &game {
                // Never log the title (documents/URLs/chat); the app id is generic.
                tracing::info!(app_id = %game.app_id, "fullscreen game focused; recording");
                *self.shared.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(game.clone());
            } else {
                tracing::info!("no fullscreen game focused; replay buffer paused");
            }
            *current = game;
        }
    }

    fn plasma_window(&mut self, qh: &QueueHandle<Self>, id: u32) {
        if !self.plasma_ids.insert(id) {
            return;
        }
        if let Some(mgmt) = &self.plasma_mgmt {
            let window = mgmt.get_window(id, qh, ());
            self.windows.insert(window.id(), Win::default());
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for Tracker {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<OrgKdePlasmaWindowManagement, ()> for Tracker {
    fn event(
        state: &mut Self,
        _: &OrgKdePlasmaWindowManagement,
        event: org_kde_plasma_window_management::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use org_kde_plasma_window_management::Event;
        match event {
            Event::Window { id } => state.plasma_window(qh, id),
            Event::WindowWithUuid { id, .. } => state.plasma_window(qh, id),
            _ => {}
        }
    }
}

impl Dispatch<OrgKdePlasmaWindow, ()> for Tracker {
    fn event(
        state: &mut Self,
        window: &OrgKdePlasmaWindow,
        event: org_kde_plasma_window::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use org_kde_plasma_window::Event;
        let key = window.id();
        match event {
            Event::AppIdChanged { app_id } => {
                if let Some(win) = state.windows.get_mut(&key) {
                    win.app_id = app_id;
                }
            }
            Event::TitleChanged { title } => {
                if let Some(win) = state.windows.get_mut(&key) {
                    win.title = title;
                }
            }
            Event::PidChanged { pid } => {
                if let Some(win) = state.windows.get_mut(&key) {
                    win.pid = (pid > 0).then_some(pid);
                }
            }
            Event::StateChanged { flags } => {
                if let Some(win) = state.windows.get_mut(&key) {
                    // A bitfield of `org_kde_plasma_window_management::State` values.
                    win.active =
                        flags & org_kde_plasma_window_management::State::Active as u32 != 0;
                    win.fullscreen =
                        flags & org_kde_plasma_window_management::State::Fullscreen as u32 != 0;
                }
            }
            Event::Unmapped => {
                state.windows.remove(&key);
                window.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Tracker {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_manager_v1::Event;
        if let Event::Toplevel { toplevel } = event {
            state.windows.insert(toplevel.id(), Win::default());
        }
    }

    event_created_child!(Tracker, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

/// The KWin fallback: load a small script into the compositor (over
/// `org.kde.kwin.Scripting`) that reports the active window's app id / title / pid /
/// fullscreen state back to us over DBus whenever focus or fullscreen changes.
/// This is how kdotool and GPU Screen Recorder read KWin's window state.
mod kwin_script {
    use std::sync::Arc;

    use super::{FocusError, Shared};
    use crate::game::{GameInfo, is_shell_app_id};

    /// Our callback bus name + interface; the script calls back into this.
    const BUS_NAME: &str = "tv.ganked.rewynd.Focus";
    const OBJECT_PATH: &str = "/tv/ganked/rewynd/Focus";
    /// The plugin name the script loads/unloads under (stable, so a crashed previous
    /// instance's script is replaced, not duplicated).
    const PLUGIN_NAME: &str = "rewynd-focus";

    /// All values ride as strings: KWin's `callDBus` marshals JS numbers as doubles,
    /// which would not match an integer parameter, so the script stringifies.
    const SCRIPT: &str = r#"
function rewyndPush(w) {
    var appId = "", title = "", pid = "0", full = "0";
    if (w) {
        appId = String(w.resourceClass);
        title = String(w.caption);
        pid = String(w.pid);
        full = w.fullScreen === true ? "1" : "0";
    }
    callDBus("tv.ganked.rewynd.Focus", "/tv/ganked/rewynd/Focus",
             "tv.ganked.rewynd.Focus", "Update", appId, title, pid, full);
}
function rewyndHook(w) {
    if (!w || w.rewyndHooked === true) { return; }
    w.rewyndHooked = true;
    w.fullScreenChanged.connect(function () {
        if (workspace.activeWindow === w) { rewyndPush(w); }
    });
}
workspace.windowActivated.connect(function (w) { rewyndHook(w); rewyndPush(w); });
rewyndHook(workspace.activeWindow);
rewyndPush(workspace.activeWindow);
"#;

    /// Keeps the DBus connection (serving the callback object) and the loaded script
    /// alive; Drop unloads the script and removes the temp file. The runtime hosts
    /// zbus's background tasks (zbus is built on tokio in this tree, via ashpd) and
    /// must outlive the connection.
    pub(super) struct ScriptHandle {
        conn: zbus::blocking::Connection,
        script_file: std::path::PathBuf,
        _runtime: tokio::runtime::Runtime,
    }

    impl Drop for ScriptHandle {
        fn drop(&mut self) {
            let _guard = self._runtime.enter();
            if let Ok(scripting) = scripting_proxy(&self.conn) {
                let _ = scripting.call_method("unloadScript", &(PLUGIN_NAME,));
            }
            let _ = std::fs::remove_file(&self.script_file);
        }
    }

    /// The DBus object the KWin script calls back into.
    struct FocusService {
        shared: Arc<Shared>,
    }

    #[zbus::interface(name = "tv.ganked.rewynd.Focus")]
    impl FocusService {
        fn update(&self, app_id: String, title: String, pid: String, fullscreen: String) {
            let game = (fullscreen == "1" && !is_shell_app_id(&app_id)).then(|| GameInfo {
                pid: pid.parse().ok().filter(|&p: &u32| p > 0),
                app_id,
                title,
            });
            let mut current = self
                .shared
                .current
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if *current != game {
                if let Some(game) = &game {
                    tracing::info!(app_id = %game.app_id, "fullscreen game focused; recording");
                    *self.shared.last.lock().unwrap_or_else(|p| p.into_inner()) =
                        Some(game.clone());
                } else {
                    tracing::info!("no fullscreen game focused; replay buffer paused");
                }
                *current = game;
            }
        }
    }

    fn scripting_proxy(
        conn: &zbus::blocking::Connection,
    ) -> zbus::Result<zbus::blocking::Proxy<'static>> {
        zbus::blocking::Proxy::new(conn, "org.kde.KWin", "/Scripting", "org.kde.kwin.Scripting")
    }

    /// Serve the callback object, write the script, and load + run it in KWin.
    /// "KWin isn't on the bus" maps to [`FocusError::NoBackend`] (a non-KDE desktop);
    /// anything else is a real [`FocusError::Kwin`] failure.
    pub(super) fn start(shared: Arc<Shared>) -> Result<ScriptHandle, FocusError> {
        let kwin_err = |e: zbus::Error| match &e {
            zbus::Error::MethodError(name, _, _) if name.as_str().ends_with("ServiceUnknown") => {
                FocusError::NoBackend
            }
            _ => FocusError::Kwin(e.to_string()),
        };

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("rewynd-focus-dbus")
            .enable_all()
            .build()
            .map_err(|e| FocusError::Kwin(e.to_string()))?;
        let guard = runtime.enter();

        let conn = zbus::blocking::Connection::session()
            .map_err(|e| FocusError::Connect(e.to_string()))?;
        conn.object_server()
            .at(OBJECT_PATH, FocusService { shared })
            .map_err(|e| FocusError::Kwin(e.to_string()))?;
        conn.request_name(BUS_NAME)
            .map_err(|e| FocusError::Kwin(e.to_string()))?;

        let script_file = write_script().map_err(|e| FocusError::Kwin(e.to_string()))?;
        let scripting = scripting_proxy(&conn).map_err(kwin_err)?;
        // Replace any script a crashed previous instance left behind.
        let _ = scripting.call_method("unloadScript", &(PLUGIN_NAME,));
        let script_path = script_file.to_string_lossy().into_owned();
        let id: i32 = scripting
            .call("loadScript", &(script_path, PLUGIN_NAME))
            .map_err(kwin_err)?;
        let script: zbus::blocking::Proxy<'_> = zbus::blocking::Proxy::new(
            &conn,
            "org.kde.KWin",
            format!("/Scripting/Script{id}"),
            "org.kde.kwin.Script",
        )
        .map_err(kwin_err)?;
        script.call::<_, _, ()>("run", &()).map_err(kwin_err)?;

        drop(guard);
        Ok(ScriptHandle {
            conn,
            script_file,
            _runtime: runtime,
        })
    }

    /// Write the script where only we can touch it: the user-private XDG runtime dir
    /// when available, else a 0600 file in the temp dir (replaced, never reused, so a
    /// squatter can't feed KWin someone else's code).
    fn write_script() -> std::io::Result<std::path::PathBuf> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        // SAFETY: geteuid is infallible and takes no arguments.
        let path = dir.join(format!("rewynd-focus-{}.js", unsafe { libc::geteuid() }));
        let _ = std::fs::remove_file(&path);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(SCRIPT.as_bytes())?;
        Ok(path)
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for Tracker {
    fn event(
        state: &mut Self,
        toplevel: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event;
        let key = toplevel.id();
        match event {
            Event::AppId { app_id } => {
                if let Some(win) = state.windows.get_mut(&key) {
                    win.app_id = app_id;
                }
            }
            Event::Title { title } => {
                if let Some(win) = state.windows.get_mut(&key) {
                    win.title = title;
                }
            }
            Event::State { state: flags } => {
                if let Some(win) = state.windows.get_mut(&key) {
                    // Array of native-endian u32 state values.
                    let states: Vec<u32> = flags
                        .chunks_exact(4)
                        .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                        .collect();
                    win.pending_active = states
                        .contains(&(zwlr_foreign_toplevel_handle_v1::State::Activated as u32));
                    win.pending_fullscreen = states
                        .contains(&(zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32));
                }
            }
            Event::Done => {
                if let Some(win) = state.windows.get_mut(&key) {
                    win.active = win.pending_active;
                    win.fullscreen = win.pending_fullscreen;
                }
            }
            Event::Closed => {
                state.windows.remove(&key);
                toplevel.destroy();
            }
            _ => {}
        }
    }
}
