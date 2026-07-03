//! The clip library: every saved clip as a thumbnail card, and per clip a detail page with
//! play / show-in-folder / delete and an upload flow (title, destination, visibility) that
//! reuses the transport clients the tray uses.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use iced::widget::{
    Space, button, column, container, pick_list, row, scrollable, slider, text, text_input,
};
use iced::{Background, Border, Element, Length, Task, Theme};

use rewynd_config::upload_history::{self, ClipKey, UploadRecord};
use rewynd_config::{ClipEntry, Config};
use rewynd_upload::youtube::{
    DEFAULT_CLIENT_ID, DEFAULT_CLIENT_SECRET, YouTubeClient, user_facing_youtube_error,
};
use rewynd_upload::{GankedClient, Visibility, default_title, user_facing_upload_error};

use crate::theme::{
    self, DISPLAY_BLACK, UI_BOLD, UI_SEMIBOLD, accent_button_style, accent_chip, card, field_label,
    hint, link_button, palette, primary_button, secondary_button, setting, tinted, value_row,
};
use crate::thumbs;

/// Cards per grid row (the body column is width-capped, so a fixed count stays balanced).
const GRID_COLUMNS: usize = 3;

/// Section label for clips saved outside a per-game subfolder (desktop / no game detected).
const ROOT_GROUP: &str = "Desktop";

/// Thumbnail decodes running at once. Each holds a full decoded frame briefly, so a big
/// library must stream through a small pool instead of decoding every stale clip at once.
const MAX_DECODES: usize = 4;

/// Keyframe thumbnails shown across the trim filmstrip.
const FILMSTRIP_FRAMES: usize = 12;

/// Decoded width of one filmstrip cell (~2x its ~60px logical size for hidpi sharpness).
const FILMSTRIP_CELL_WIDTH: u32 = 120;

/// An upload destination the detail page can send a clip to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dest {
    Ganked,
    YouTube,
}

impl Dest {
    fn label(self) -> &'static str {
        match self {
            Dest::Ganked => "ganked.tv",
            Dest::YouTube => "YouTube",
        }
    }

    /// The stored history key for this destination.
    fn history_key(self) -> &'static str {
        match self {
            Dest::Ganked => upload_history::GANKED,
            Dest::YouTube => upload_history::YOUTUBE,
        }
    }
}

/// One filmstrip cell for the open clip: a decoded keyframe, still decoding, or undecodable.
enum StripCell {
    Loading,
    Ready(iced::widget::image::Handle),
    Failed,
}

/// One clip's thumbnail slot, keyed by path; `modified` is the freshness check.
enum Thumb {
    Loading {
        modified: SystemTime,
    },
    Ready {
        modified: SystemTime,
        handle: iced::widget::image::Handle,
        duration: Duration,
    },
    /// Corrupt or undecodable: the card gets a neutral placeholder, never a crash.
    Failed {
        modified: SystemTime,
    },
}

impl Thumb {
    fn modified(&self) -> SystemTime {
        match self {
            Thumb::Loading { modified }
            | Thumb::Ready { modified, .. }
            | Thumb::Failed { modified } => *modified,
        }
    }
}

/// Where the one in-flight (or just-finished) upload stands. One at a time: the whole view
/// keys off this, so a second upload can't start while one runs.
enum UploadState {
    Idle,
    Uploading {
        path: PathBuf,
        dest: Dest,
        abort: iced::task::Handle,
    },
    /// ganked.tv accepted the clip; polling its processing status until ready/failed.
    Processing {
        path: PathBuf,
        abort: iced::task::Handle,
    },
    /// Verifying whether an already-uploaded clip still exists remotely before re-uploading.
    Checking {
        path: PathBuf,
        dest: Dest,
    },
    /// The remote copy is still there, so a re-upload is blocked (with an "upload anyway" escape).
    Blocked {
        path: PathBuf,
        dest: Dest,
        link: Option<String>,
    },
    /// The remote copy could not be verified (offline, or YouTube's scope can't check); offer to
    /// upload anyway.
    Unverifiable {
        path: PathBuf,
        reason: String,
    },
    Done {
        path: PathBuf,
        dest: Dest,
        link: Option<String>,
        note: String,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

/// A finished upload: the remote id (for history + later verification), the share/watch link
/// when the server issued one, and a note.
#[derive(Debug, Clone)]
pub struct Uploaded {
    remote_id: String,
    link: Option<String>,
    note: String,
}

/// The result of polling ganked.tv processing status.
#[derive(Debug, Clone)]
pub struct StatusOutcome {
    failed: bool,
    link: Option<String>,
    message: String,
}

/// How often to poll ganked.tv processing status, and for how many reads (≈5 minutes total).
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const POLL_MAX_READS: u32 = 60;

#[derive(Debug, Clone)]
pub enum Message {
    Refresh,
    SearchEdited(String),
    GameFilterPicked(Option<String>),
    Scanned(Vec<ClipEntry>),
    ThumbDone(PathBuf, SystemTime, Result<thumbs::Loaded, String>),
    Open(PathBuf),
    Back,
    Play,
    ShowInFolder,
    DeleteRequested,
    DeleteCancelled,
    DeleteConfirmed,
    Deleted(Result<PathBuf, String>),
    TrimStartChanged(f32),
    TrimEndChanged(f32),
    TrimSave,
    TrimSaved {
        src: PathBuf,
        result: Result<PathBuf, String>,
    },
    TitleEdited(String),
    DestPicked(Dest),
    VisibilityPicked(Visibility),
    UploadPressed,
    UploadAgain,
    UploadAnyway,
    UploadCancelled,
    UploadDone(Result<Uploaded, String>),
    StatusPolled(PathBuf, Result<StatusOutcome, String>),
    Verified(PathBuf, Dest, Result<bool, String>),
    OpenLink(String),
    CopyLink(String),
    /// A frame tick while an accent fade is running (carries the frame instant).
    Tick(Instant),
    /// A filmstrip cell finished decoding: (clip path, cell index, decoded frame or error).
    StripDone(PathBuf, usize, Result<iced::widget::image::Handle, String>),
}

/// Where the open clip's trim stands.
enum TrimState {
    Idle,
    Saving,
    Saved(PathBuf),
    Failed(String),
}

pub struct Library {
    entries: Vec<ClipEntry>,
    thumbs: HashMap<PathBuf, Thumb>,
    /// Thumbnail decodes not yet started; drained into `decoding` as slots free up.
    pending_thumbs: VecDeque<(PathBuf, SystemTime)>,
    /// The decodes currently on blocking tasks, keyed by path (the value is the mtime that
    /// decode was started for). Its size is the in-flight count, capped at [`MAX_DECODES`].
    decoding: HashMap<PathBuf, SystemTime>,
    scanning: bool,
    /// Fuzzy filter over game name / date / file name; empty shows everything.
    search: String,
    /// The game section the chip row has narrowed to, if any (`None` shows all).
    game_filter: Option<String>,
    /// The clip whose detail page is open, if any.
    open: Option<PathBuf>,
    /// The clip the filmstrip cells belong to (guards a late `StripDone` for a since-closed clip).
    strip_path: Option<PathBuf>,
    /// One slot per [`FILMSTRIP_FRAMES`] position for the open clip; empty when no clip is open.
    strip: Vec<StripCell>,
    /// Cells not yet decoding, drained into flight as slots free up: (cell index, position).
    strip_pending: VecDeque<(usize, f32)>,
    /// How many strip decodes are in flight (capped at [`MAX_DECODES`]).
    strip_decoding: usize,
    confirm_delete: bool,
    /// Trim in/out points for the open clip, in seconds; reset to the full span on open.
    trim_start: f32,
    trim_end: f32,
    /// The open clip's duration in seconds (read from its header on open), the trim range's ceiling.
    open_dur: f32,
    trim: TrimState,
    /// Play / show-in-folder / delete failure for the open clip.
    action_error: Option<String>,
    title: String,
    /// The suggested title, snapshotted when the detail page opens (recomputing it per
    /// `view()` would make the placeholder's minute stamp tick while the page sits open).
    title_hint: String,
    dest: Dest,
    /// An in-flight destination-accent fade, or `None` when the panel sits on a fixed brand.
    accent_fade: Option<AccentFade>,
    visibility: Visibility,
    upload: UploadState,
    /// Remembered successful uploads (per clip, per destination), for badges + the duplicate
    /// guard. Reloaded on each scan and after a record/forget.
    history: Vec<UploadRecord>,
}

impl Library {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            thumbs: HashMap::new(),
            pending_thumbs: VecDeque::new(),
            decoding: HashMap::new(),
            scanning: false,
            search: String::new(),
            game_filter: None,
            open: None,
            strip_path: None,
            strip: Vec::new(),
            strip_pending: VecDeque::new(),
            strip_decoding: 0,
            confirm_delete: false,
            trim_start: 0.0,
            trim_end: 0.0,
            open_dur: 0.0,
            trim: TrimState::Idle,
            action_error: None,
            title: String::new(),
            title_hint: String::new(),
            dest: Dest::Ganked,
            accent_fade: None,
            visibility: Visibility::default(),
            upload: UploadState::Idle,
            history: upload_history::load(),
        }
    }

    /// Leave any open clip's detail and return to the grid (the nav "Library" tab / brand logo).
    pub fn show_grid(&mut self) {
        self.open = None;
        self.confirm_delete = false;
        self.action_error = None;
        self.clear_strip();
    }

    /// The upload record for `entry` at `dest`, if the clip was uploaded there.
    fn record_for(&self, entry: &ClipEntry, dest: Dest) -> Option<&UploadRecord> {
        let key = clip_key(entry)?;
        self.history.iter().find(|r| {
            key.file_name == r.file_name
                && key.size_bytes == r.size_bytes
                && key.modified_millis == r.modified_millis
                && r.destination == dest.history_key()
        })
    }

    /// The destinations `entry` has already been uploaded to (for the card badges).
    fn uploaded_dests(&self, entry: &ClipEntry) -> Vec<Dest> {
        [Dest::Ganked, Dest::YouTube]
            .into_iter()
            .filter(|d| self.record_for(entry, *d).is_some())
            .collect()
    }

    /// Rescan the clip directory (the same resolution the recorder saves through) on a
    /// blocking task. Called on view-enter and by the Refresh button.
    pub fn refresh(&mut self, config: &Config) -> Task<Message> {
        self.scanning = true;
        let dir = rewynd_config::clips_dir(config.output_dir().as_deref());
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || rewynd_config::list_clips(&dir))
                    .await
                    .unwrap_or_default()
            },
            Message::Scanned,
        )
    }

    pub fn update(&mut self, message: Message, config: &Config) -> Task<Message> {
        match message {
            Message::Refresh => return self.refresh(config),
            Message::SearchEdited(q) => self.search = q,
            Message::GameFilterPicked(game) => self.game_filter = game,
            Message::Scanned(entries) => return self.scanned(entries),
            Message::ThumbDone(path, modified, result) => {
                // Free the decode slot, unless a newer decode for the same path superseded it.
                if self.decoding.get(&path) == Some(&modified) {
                    self.decoding.remove(&path);
                }
                // Drop a stale result: the file changed since this decode started.
                if self.thumbs.get(&path).map(Thumb::modified) == Some(modified) {
                    let thumb = match result {
                        Ok(loaded) => Thumb::Ready {
                            modified,
                            handle: loaded.handle,
                            duration: loaded.duration,
                        },
                        Err(e) => {
                            tracing::warn!(path = %path.display(), error = %e, "no thumbnail");
                            Thumb::Failed { modified }
                        }
                    };
                    self.thumbs.insert(path, thumb);
                }
                return self.start_pending_decodes();
            }
            Message::Open(path) => {
                // Reset the trim to the clip's full span (its duration comes from the file header).
                self.open_dur =
                    rewynd_mux::read::clip_summary(&path).map_or(0.0, |s| s.duration.as_secs_f32());
                self.trim_start = 0.0;
                self.trim_end = self.open_dur;
                self.trim = TrimState::Idle;
                self.open = Some(path.clone());
                self.confirm_delete = false;
                self.action_error = None;
                self.upload = UploadState::Idle;
                self.accent_fade = None;
                self.title_hint = default_title();
                self.title = self.title_hint.clone();
                let (ganked, youtube) = dest_statuses(config);
                self.dest = if !ganked.ready() && youtube.ready() {
                    Dest::YouTube
                } else {
                    Dest::Ganked
                };
                self.visibility = self.default_visibility(config);
                return self.build_strip(path);
            }
            Message::Back => {
                self.open = None;
                self.confirm_delete = false;
                self.action_error = None;
                self.clear_strip();
            }
            Message::Play => {
                if let Some(path) = &self.open
                    && let Err(e) = open::that_detached(path)
                {
                    self.action_error = Some(format!("Could not open a video player: {e}"));
                }
            }
            Message::ShowInFolder => {
                if let Some(dir) = self.open.as_ref().and_then(|p| p.parent())
                    && let Err(e) = open::that_detached(dir)
                {
                    self.action_error = Some(format!("Could not open the folder: {e}"));
                }
            }
            Message::DeleteRequested => self.confirm_delete = true,
            Message::DeleteCancelled => self.confirm_delete = false,
            Message::DeleteConfirmed => {
                self.confirm_delete = false;
                let Some(path) = self.open.clone() else {
                    return Task::none();
                };
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            std::fs::remove_file(&path)
                                .map(|()| path)
                                .map_err(|e| e.to_string())
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    Message::Deleted,
                );
            }
            Message::Deleted(Ok(path)) => {
                // The clip is gone; its cached preview (a frame of it) must not outlive it.
                if let Some(entry) = self.entries.iter().find(|e| e.path == path) {
                    thumbs::remove_cached(&path, entry.modified);
                }
                self.entries.retain(|e| e.path != path);
                self.thumbs.remove(&path);
                self.pending_thumbs.retain(|(p, _)| p != &path);
                if self.open == Some(path) {
                    self.open = None;
                    self.clear_strip();
                }
                self.action_error = None;
            }
            Message::Deleted(Err(e)) => {
                self.action_error = Some(format!("Could not delete the clip: {e}"));
            }
            Message::TrimStartChanged(v) => {
                // Keep at least a small gap below the end.
                self.trim_start = v.clamp(0.0, (self.trim_end - 0.2).max(0.0));
                self.trim = TrimState::Idle;
            }
            Message::TrimEndChanged(v) => {
                let hi = self.open_duration();
                self.trim_end = v.clamp(self.trim_start + 0.2, hi.max(0.2));
                self.trim = TrimState::Idle;
            }
            Message::TrimSave => {
                let Some(src) = self.open.clone() else {
                    return Task::none();
                };
                let start = Duration::from_secs_f32(self.trim_start);
                let end = Duration::from_secs_f32(self.trim_end);
                self.trim = TrimState::Saving;
                let work_src = src.clone();
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            let dst = unique_trim_path(&work_src);
                            rewynd_mux::read::trim_clip(&work_src, &dst, start, end)
                                .map(|_| dst)
                                .map_err(|e| e.to_string())
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    move |result| Message::TrimSaved {
                        src: src.clone(),
                        result,
                    },
                );
            }
            Message::TrimSaved { src, result } => {
                // The trim may finish after the user opened another clip; only stamp the result on
                // its own detail page. The rescan still runs so the new clip shows up regardless.
                let current = self.open.as_deref() == Some(src.as_path());
                match result {
                    Ok(path) => {
                        if current {
                            self.trim = TrimState::Saved(path);
                        }
                        return self.refresh(config);
                    }
                    Err(e) if current => {
                        self.trim =
                            TrimState::Failed(format!("Could not save the trimmed clip: {e}"));
                    }
                    Err(_) => {}
                }
            }
            Message::TitleEdited(s) => self.title = s,
            Message::DestPicked(dest) => {
                if self.dest != dest {
                    let from = self.current_accent();
                    self.dest = dest;
                    self.visibility = self.default_visibility(config);
                    self.accent_fade = Some(AccentFade {
                        from,
                        to: dest_accent(dest),
                        start: None,
                        progress: 0.0,
                    });
                }
            }
            Message::VisibilityPicked(v) => self.visibility = v,
            Message::UploadPressed | Message::UploadAnyway => return self.start_upload(config),
            Message::UploadAgain => return self.recheck_before_reupload(config),
            Message::UploadCancelled => {
                match std::mem::replace(&mut self.upload, UploadState::Idle) {
                    UploadState::Uploading { abort, .. }
                    | UploadState::Processing { abort, .. } => {
                        abort.abort();
                    }
                    _ => {}
                }
            }
            Message::UploadDone(result) => return self.upload_done(result, config),
            Message::StatusPolled(path, result) => return self.status_polled(&path, result),
            Message::Verified(path, dest, result) => {
                return self.verified(path, dest, result, config);
            }
            Message::Tick(now) => self.advance_fade(now),
            Message::StripDone(path, index, result) => {
                if self.strip_path.as_deref() == Some(path.as_path()) {
                    self.strip_decoding = self.strip_decoding.saturating_sub(1);
                    if let Some(cell) = self.strip.get_mut(index) {
                        *cell = match result {
                            Ok(handle) => StripCell::Ready(handle),
                            Err(e) => {
                                tracing::warn!(path = %path.display(), index, error = %e, "no filmstrip frame");
                                StripCell::Failed
                            }
                        };
                    }
                    return self.start_strip_decodes();
                }
            }
            Message::OpenLink(url) => {
                if let Err(e) = open::that_detached(&url) {
                    tracing::warn!(error = %e, url, "could not open the link");
                }
            }
            Message::CopyLink(url) => return iced::clipboard::write(url),
        }
        Task::none()
    }

    /// Store a fresh scan and queue thumbnail decodes for entries whose (path, mtime) slot is
    /// missing or stale. The queue is rebuilt from this scan; decodes already in flight keep
    /// their slot and are not restarted.
    fn scanned(&mut self, entries: Vec<ClipEntry>) -> Task<Message> {
        self.scanning = false;
        self.thumbs
            .retain(|path, _| entries.iter().any(|e| &e.path == path));
        self.pending_thumbs.clear();
        for entry in &entries {
            let decoded = self.thumbs.get(&entry.path).is_some_and(|t| {
                t.modified() == entry.modified && !matches!(t, Thumb::Loading { .. })
            });
            let in_flight = self.decoding.get(&entry.path) == Some(&entry.modified);
            if decoded || in_flight {
                continue;
            }
            let modified = entry.modified;
            self.thumbs
                .insert(entry.path.clone(), Thumb::Loading { modified });
            self.pending_thumbs
                .push_back((entry.path.clone(), modified));
        }
        self.entries = entries;
        // Refresh the upload memory so badges + the duplicate guard reflect uploads made from the
        // tray (or a prior run) without a restart.
        self.history = upload_history::load();
        // Drop a game filter whose section vanished (its last clip was deleted or moved).
        let stale = self
            .game_filter
            .as_ref()
            .is_some_and(|game| !self.entries.iter().any(|e| group_label(e) == game));
        if stale {
            self.game_filter = None;
        }
        self.start_pending_decodes()
    }

    /// Start queued decodes until [`MAX_DECODES`] are in flight, each on its own blocking
    /// task; [`Message::ThumbDone`] frees a slot and comes back here for the next one.
    fn start_pending_decodes(&mut self) -> Task<Message> {
        let mut tasks = Vec::new();
        while self.decoding.len() < MAX_DECODES {
            let Some((path, modified)) = self.pending_thumbs.pop_front() else {
                break;
            };
            self.decoding.insert(path.clone(), modified);
            tasks.push(Task::perform(
                async move {
                    let result = tokio::task::spawn_blocking({
                        let path = path.clone();
                        move || thumbs::load(&path, modified)
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                    (path, modified, result)
                },
                |(path, modified, result)| Message::ThumbDone(path, modified, result),
            ));
        }
        Task::batch(tasks)
    }

    /// Drop the filmstrip for whatever clip was open (free the decoded frames).
    fn clear_strip(&mut self) {
        self.strip_path = None;
        self.strip.clear();
        self.strip_pending.clear();
        self.strip_decoding = 0;
    }

    /// Queue all filmstrip cells for `path` and kick off the first decodes.
    fn build_strip(&mut self, path: PathBuf) -> Task<Message> {
        self.strip_path = Some(path);
        self.strip = (0..FILMSTRIP_FRAMES).map(|_| StripCell::Loading).collect();
        self.strip_pending = filmstrip_positions(FILMSTRIP_FRAMES)
            .into_iter()
            .enumerate()
            .collect();
        self.strip_decoding = 0;
        self.start_strip_decodes()
    }

    /// Start queued strip decodes until [`MAX_DECODES`] are in flight; each is one blocking
    /// `thumbs::load_at`. [`Message::StripDone`] frees a slot and returns here for the next.
    fn start_strip_decodes(&mut self) -> Task<Message> {
        let Some(path) = self.strip_path.clone() else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        while self.strip_decoding < MAX_DECODES {
            let Some((index, position)) = self.strip_pending.pop_front() else {
                break;
            };
            self.strip_decoding += 1;
            let path = path.clone();
            tasks.push(Task::perform(
                async move {
                    let result = tokio::task::spawn_blocking({
                        let path = path.clone();
                        move || thumbs::load_at(&path, position, FILMSTRIP_CELL_WIDTH)
                    })
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                    (path, index, result)
                },
                |(path, index, result)| Message::StripDone(path, index, result),
            ));
        }
        Task::batch(tasks)
    }

    /// The config's default visibility for the chosen destination.
    fn default_visibility(&self, config: &Config) -> Visibility {
        match self.dest {
            Dest::Ganked => Visibility::parse(&config.upload().visibility),
            Dest::YouTube => Visibility::parse(&config.youtube().visibility),
        }
    }

    /// Kick off the upload for the open clip: the client is built from the config at click
    /// time, exactly as the tray does. Only the destination-specific upload future differs;
    /// the readiness check and task scaffolding are shared.
    fn start_upload(&mut self, config: &Config) -> Task<Message> {
        if matches!(self.upload, UploadState::Uploading { .. }) {
            return Task::none();
        }
        let Some(path) = self.open.clone() else {
            return Task::none();
        };
        let dest = self.dest;
        let (ganked, youtube) = dest_statuses(config);
        let status = match dest {
            Dest::Ganked => ganked,
            Dest::YouTube => youtube,
        };
        if let Some(reason) = status.blocker(dest) {
            self.upload = UploadState::Failed {
                path,
                error: reason,
            };
            return Task::none();
        }
        let title = {
            let t = self.title.trim();
            if t.is_empty() {
                self.title_hint.clone()
            } else {
                t.to_owned()
            }
        };
        let visibility = self.visibility;
        let upload: std::pin::Pin<Box<dyn Future<Output = Result<Uploaded, String>> + Send>> = {
            let clip = path.clone();
            match dest {
                Dest::Ganked => Box::pin(upload_ganked(config.upload(), clip, title, visibility)),
                Dest::YouTube => {
                    Box::pin(upload_youtube(config.youtube(), clip, title, visibility))
                }
            }
        };
        let (task, abort) = Task::perform(upload, Message::UploadDone).abortable();
        self.upload = UploadState::Uploading { path, dest, abort };
        task
    }

    /// Handle a finished upload: remember it (so badges + the guard persist), copy the link to the
    /// clipboard, then for ganked.tv start polling its processing status; YouTube has no further
    /// server step to watch.
    fn upload_done(&mut self, result: Result<Uploaded, String>, config: &Config) -> Task<Message> {
        let UploadState::Uploading { path, dest, .. } =
            std::mem::replace(&mut self.upload, UploadState::Idle)
        else {
            return Task::none();
        };
        let done = match result {
            Ok(done) => done,
            Err(error) => {
                self.upload = UploadState::Failed { path, error };
                return Task::none();
            }
        };

        self.remember_upload(&path, dest, &done);
        let mut tasks = Vec::new();
        if let Some(url) = &done.link {
            tasks.push(iced::clipboard::write(url.clone()));
        }

        match dest {
            Dest::Ganked => {
                let up = config.upload();
                let clip_id = done.remote_id.clone();
                let for_path = path.clone();
                let (task, abort) = Task::perform(
                    async move { poll_ganked(up, for_path, clip_id).await },
                    |(path, result)| Message::StatusPolled(path, result),
                )
                .abortable();
                self.upload = UploadState::Processing { path, abort };
                tasks.push(task);
            }
            Dest::YouTube => {
                self.upload = UploadState::Done {
                    path,
                    dest,
                    link: done.link,
                    note: done.note,
                };
            }
        }
        Task::batch(tasks)
    }

    /// Fold a ganked.tv status poll into a terminal state; ignore a poll for a clip the user has
    /// since navigated away from. A share code usually only appears once processing finishes, so
    /// this is where the link is copied to the clipboard for ganked.tv (it was still `processing`
    /// at upload time).
    fn status_polled(
        &mut self,
        path: &Path,
        result: Result<StatusOutcome, String>,
    ) -> Task<Message> {
        if !matches!(&self.upload, UploadState::Processing { path: p, .. } if p == path) {
            return Task::none();
        }
        let path = path.to_path_buf();
        let mut task = Task::none();
        self.upload = match result {
            Ok(outcome) if outcome.failed => UploadState::Failed {
                path,
                error: outcome.message,
            },
            Ok(outcome) => {
                if let Some(url) = &outcome.link {
                    // Keep the badge's link fresh and put the now-available link on the clipboard.
                    self.update_history_link(&path, Dest::Ganked, url);
                    task = iced::clipboard::write(url.clone());
                }
                UploadState::Done {
                    path,
                    dest: Dest::Ganked,
                    link: outcome.link,
                    note: outcome.message,
                }
            }
            Err(error) => UploadState::Failed { path, error },
        };
        task
    }

    /// "Upload again" on an already-uploaded clip: verify the remote copy still exists before
    /// allowing a second upload. ganked.tv can be checked (a 404 means it was deleted); YouTube's
    /// upload-only scope can't, so it goes straight to the "upload anyway" escape.
    fn recheck_before_reupload(&mut self, config: &Config) -> Task<Message> {
        let Some(path) = self.open.clone() else {
            return Task::none();
        };
        let dest = self.dest;
        let Some(record) = self
            .open_entry()
            .and_then(|e| self.record_for(e, dest))
            .cloned()
        else {
            return self.start_upload(config);
        };
        match dest {
            Dest::Ganked => {
                self.upload = UploadState::Checking {
                    path: path.clone(),
                    dest,
                };
                let up = config.upload();
                let remote_id = record.remote_id;
                Task::perform(
                    async move { verify_ganked(up, path.clone(), remote_id).await },
                    move |(path, result)| Message::Verified(path, dest, result),
                )
            }
            Dest::YouTube => {
                self.upload = UploadState::Unverifiable {
                    path,
                    reason: "YouTube can't confirm whether the video is still up.".to_owned(),
                };
                Task::none()
            }
        }
    }

    /// Resolve a remote-existence check: gone → forget the record and upload; still there → block;
    /// couldn't check → offer to upload anyway.
    fn verified(
        &mut self,
        path: PathBuf,
        dest: Dest,
        result: Result<bool, String>,
        config: &Config,
    ) -> Task<Message> {
        if !matches!(&self.upload, UploadState::Checking { path: p, .. } if *p == path) {
            return Task::none();
        }
        match result {
            Ok(false) => {
                // The remote copy is gone; drop the stale record and upload afresh.
                if let Some(key) = self.open_entry().and_then(clip_key) {
                    let _ = upload_history::forget(&key, dest.history_key());
                    self.history = upload_history::load();
                }
                self.start_upload(config)
            }
            Ok(true) => {
                let link = self
                    .open_entry()
                    .and_then(|e| self.record_for(e, dest))
                    .and_then(|r| r.url.clone());
                self.upload = UploadState::Blocked { path, dest, link };
                Task::none()
            }
            Err(reason) => {
                self.upload = UploadState::Unverifiable { path, reason };
                Task::none()
            }
        }
    }

    /// Persist a successful upload and refresh the in-memory history so the badge + guard update
    /// without a rescan.
    fn remember_upload(&mut self, path: &Path, dest: Dest, done: &Uploaded) {
        let Some(entry) = self.entries.iter().find(|e| e.path == path) else {
            return;
        };
        let Some(key) = clip_key(entry) else {
            return;
        };
        let record = UploadRecord {
            file_name: key.file_name,
            size_bytes: key.size_bytes,
            modified_millis: key.modified_millis,
            destination: dest.history_key().to_owned(),
            remote_id: done.remote_id.clone(),
            url: done.link.clone(),
            uploaded_millis: ClipKey::now_millis(),
        };
        if let Err(e) = upload_history::record(record) {
            tracing::warn!(error = %e, "could not record the upload");
        }
        self.history = upload_history::load();
    }

    /// Update a stored record's link (e.g. once ganked.tv issues the share code after processing).
    fn update_history_link(&mut self, path: &Path, dest: Dest, url: &str) {
        let Some(entry) = self.entries.iter().find(|e| e.path == path) else {
            return;
        };
        let Some(record) = self.record_for(entry, dest).cloned() else {
            return;
        };
        if record.url.as_deref() == Some(url) {
            return;
        }
        let updated = UploadRecord {
            url: Some(url.to_owned()),
            ..record
        };
        if upload_history::record(updated).is_ok() {
            self.history = upload_history::load();
        }
    }

    /// The `ClipEntry` for the open clip, if any.
    fn open_entry(&self) -> Option<&ClipEntry> {
        let path = self.open.as_ref()?;
        self.entries.iter().find(|e| &e.path == path)
    }

    /// The (fill, ink) accent the upload panel paints with right now: the destination brand,
    /// or the interpolated value while a switch is fading.
    fn current_accent(&self) -> (iced::Color, iced::Color) {
        self.accent_fade
            .as_ref()
            .map_or_else(|| dest_accent(self.dest), AccentFade::accent)
    }

    /// Whether an accent fade is running (drives the frame subscription in `main`).
    pub fn animating(&self) -> bool {
        self.accent_fade.is_some()
    }

    /// Advance any running accent fade to frame time `now`, dropping it once complete so the
    /// frame subscription in `main` stops (no idle redraw).
    fn advance_fade(&mut self, now: Instant) {
        if let Some(fade) = &mut self.accent_fade
            && fade.advance(now)
        {
            self.accent_fade = None;
        }
    }

    pub fn view(&self, config: &Config) -> Element<'_, Message> {
        let body: Element<Message> = match self.open.as_ref().and_then(|p| self.entry(p)) {
            Some(entry) => self.detail(entry, dest_statuses(config)),
            None => self.grid(),
        };
        let content = container(
            column![body]
                .spacing(20)
                .padding(28)
                .max_width(880)
                .width(Length::Fill),
        )
        .center_x(Length::Fill);
        container(scrollable(content))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn entry(&self, path: &Path) -> Option<&ClipEntry> {
        self.entries.iter().find(|e| e.path == path)
    }

    /// The default page: header + the clip cards, grouped by game, newest first.
    fn grid(&self) -> Element<'_, Message> {
        let title = column![
            text("LIBRARY").size(32).font(DISPLAY_BLACK),
            text(self.library_stats())
                .size(12)
                .font(UI_SEMIBOLD)
                .style(tinted(palette::TEXT_SECONDARY)),
        ]
        .spacing(4);
        let header = row![
            title,
            Space::new().width(Length::Fill),
            if self.scanning {
                hint("Refreshing...")
            } else {
                Space::new().into()
            },
            button(text("Refresh").size(12).font(UI_SEMIBOLD))
                .on_press(Message::Refresh)
                .style(secondary_button)
                .padding([8, 16]),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center);

        if self.entries.is_empty() {
            let empty = column![
                text("NO CLIPS YET").size(22).font(DISPLAY_BLACK),
                hint(if self.scanning {
                    "Looking for saved clips..."
                } else {
                    "Clips you save appear here. Press your hotkey while playing to save one."
                }),
            ]
            .spacing(8)
            .align_x(iced::Alignment::Center);
            return column![
                header,
                container(empty)
                    .center_x(Length::Fill)
                    .padding([80, 0])
                    .width(Length::Fill),
            ]
            .spacing(20)
            .into();
        }

        let groups = self.grouped();
        let mut sections = column![].spacing(24);
        if groups.is_empty() {
            // The empty result can come from the search box, the game chips, or both; word it
            // for whichever the user actually touched.
            let reason = if self.search.trim().is_empty() {
                "No clips in this section."
            } else {
                "No clips match your search."
            };
            sections = sections.push(
                container(hint(reason))
                    .center_x(Length::Fill)
                    .padding([48, 0])
                    .width(Length::Fill),
            );
        }
        for (label, clips) in &groups {
            sections = sections.push(self.section(label, clips));
        }
        column![header, self.controls(), sections]
            .spacing(20)
            .into()
    }

    /// The "N clips · X.X GB" line under the title, over the whole library (unfiltered).
    fn library_stats(&self) -> String {
        let count = self.entries.len();
        let bytes: u64 = self.entries.iter().map(|e| e.size_bytes).sum();
        let clips = if count == 1 { "clip" } else { "clips" };
        format!("{count} {clips} · {} on disk", disk_label(bytes))
    }

    /// Search field plus a chip per game section (and an "All" chip), shown only when there is
    /// more than one section to move between.
    fn controls(&self) -> Element<'_, Message> {
        let search = text_input("Search clips", &self.search)
            .on_input(Message::SearchEdited)
            .style(theme::arena_input)
            .width(Length::Fixed(260.0));

        let labels = self.game_labels();
        let mut chips = row![chip(
            "All",
            self.game_filter.is_none(),
            Message::GameFilterPicked(None)
        )]
        .spacing(8)
        .align_y(iced::Alignment::Center);
        if labels.len() > 1 {
            for label in labels {
                let active = self.game_filter.as_deref() == Some(label.as_str());
                chips = chips.push(chip(
                    &label,
                    active,
                    Message::GameFilterPicked(Some(label.clone())),
                ));
            }
        }

        row![search, Space::new().width(Length::Fill), chips]
            .spacing(12)
            .align_y(iced::Alignment::Center)
            .into()
    }

    /// One game section: a header (label + clip count) over that group's grid. `label`'s
    /// lifetime is independent of the returned element (its text is owned), so the caller's
    /// grouping scratch does not have to outlive the view.
    fn section<'a>(&'a self, label: &str, clips: &[&'a ClipEntry]) -> Element<'a, Message> {
        let head = row![
            text(label.to_uppercase()).size(15).font(UI_BOLD),
            text(format!("{}", clips.len()))
                .size(11)
                .font(UI_SEMIBOLD)
                .style(tinted(palette::MUTED)),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center);

        let mut rows = column![].spacing(14);
        for chunk in clips.chunks(GRID_COLUMNS) {
            let mut r = row![].spacing(14);
            for entry in chunk {
                r = r.push(self.clip_card(entry));
            }
            for _ in chunk.len()..GRID_COLUMNS {
                r = r.push(Space::new().width(Length::Fill));
            }
            rows = rows.push(r);
        }
        column![head, rows].spacing(12).into()
    }

    /// The filtered entries grouped by game, each group newest-first, groups ordered by most
    /// recent activity (entries arrive newest-first, so first sighting fixes group order).
    fn grouped(&self) -> Vec<(String, Vec<&ClipEntry>)> {
        let mut groups: Vec<(String, Vec<&ClipEntry>)> = Vec::new();
        for entry in self.entries.iter().filter(|e| self.matches(e)) {
            let label = group_label(entry);
            if let Some(group) = groups.iter_mut().find(|(l, _)| l == label) {
                group.1.push(entry);
            } else {
                groups.push((label.to_owned(), vec![entry]));
            }
        }
        groups
    }

    /// Distinct game section labels present, ordered by most recent activity (for the chip row).
    fn game_labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = Vec::new();
        for entry in &self.entries {
            let label = group_label(entry);
            if !labels.iter().any(|l| l == label) {
                labels.push(label.to_owned());
            }
        }
        labels
    }

    /// Whether `entry` passes the active game filter and the search query.
    fn matches(&self, entry: &ClipEntry) -> bool {
        if let Some(filter) = &self.game_filter
            && group_label(entry) != filter
        {
            return false;
        }
        let query = self.search.trim();
        if query.is_empty() {
            return true;
        }
        let name = entry
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        fuzzy_match(query, group_label(entry))
            || fuzzy_match(query, &saved_at_label(entry.saved_at))
            || fuzzy_match(query, name)
    }

    fn clip_card<'a>(&'a self, entry: &'a ClipEntry) -> Element<'a, Message> {
        let info = column![
            text(saved_at_label(entry.saved_at))
                .size(12)
                .font(UI_SEMIBOLD)
                .style(tinted(palette::TEXT)),
            self.meta_row(entry, 10.0, palette::MUTED),
        ]
        .spacing(7);

        button(
            column![
                self.thumbnail(entry, 148.0),
                container(info).padding([11, 12]),
            ]
            .spacing(0),
        )
        .on_press(Message::Open(entry.path.clone()))
        .style(clip_card_style)
        .padding(0)
        .width(Length::Fill)
        .into()
    }

    /// The "duration · size" line with the per-game chip, shared by the cards and the
    /// detail page (which differ only in type size and tint).
    fn meta_row<'a>(
        &'a self,
        entry: &'a ClipEntry,
        size: f32,
        color: iced::Color,
    ) -> Element<'a, Message> {
        let mut meta = size_label(entry.size_bytes);
        if let Some(Thumb::Ready { duration, .. }) = self.thumbs.get(&entry.path) {
            meta = format!("{} · {meta}", duration_label(*duration));
        }
        let mut line = row![].spacing(8).align_y(iced::Alignment::Center);
        if let Some(game) = &entry.game {
            line = line.push(accent_chip(game.clone()));
        }
        for dest in self.uploaded_dests(entry) {
            line = line.push(accent_chip(format!("On {}", dest.label())));
        }
        line.push(text(meta).size(size).style(tinted(color))).into()
    }

    /// The clip's thumbnail at the given height, or a neutral placeholder while it loads
    /// (or when the clip can't be decoded).
    fn thumbnail<'a>(&'a self, entry: &'a ClipEntry, height: f32) -> Element<'a, Message> {
        match self.thumbs.get(&entry.path) {
            Some(Thumb::Ready { handle, .. }) => iced::widget::image(handle.clone())
                .width(Length::Fill)
                .height(height)
                .content_fit(iced::ContentFit::Cover)
                .into(),
            Some(Thumb::Failed { .. }) => placeholder("No preview", height),
            _ => placeholder("Loading...", height),
        }
    }

    /// The detail page for one clip: preview, facts, actions, and the upload panel.
    fn detail<'a>(
        &'a self,
        entry: &'a ClipEntry,
        (ganked, youtube): (DestStatus, DestStatus),
    ) -> Element<'a, Message> {
        let back = button(text("Back to library").size(12).font(UI_SEMIBOLD))
            .on_press(Message::Back)
            .style(link_button)
            .padding(0);

        let mut facts = column![
            text(saved_at_label(entry.saved_at).to_uppercase())
                .size(26)
                .font(DISPLAY_BLACK),
            self.meta_row(entry, 11.0, palette::TEXT_SECONDARY),
            hint(entry.path.display().to_string()),
        ]
        .spacing(10);
        facts = facts.push(self.actions());
        if let Some(e) = &self.action_error {
            facts = facts.push(text(e.clone()).size(12).style(tinted(palette::DANGER)));
        }

        let preview = container(self.thumbnail(entry, 240.0)).style(theme::card_style);
        let top = row![
            container(preview).width(Length::FillPortion(5)),
            container(facts).width(Length::FillPortion(4)),
        ]
        .spacing(20);

        column![
            back,
            top,
            self.trim_panel(),
            self.upload_panel(entry, ganked, youtube),
        ]
        .spacing(20)
        .into()
    }

    /// The open clip's duration in seconds (read from its header on open), the trim ceiling.
    fn open_duration(&self) -> f32 {
        self.open_dur
    }

    /// A row of keyframe thumbnails across the whole clip, the kept `[start, end]` band bright
    /// with a mint edge and the rest scrimmed. Driven by the trim sliders; purely visual.
    fn filmstrip(&self) -> Element<'_, Message> {
        if self.strip.is_empty() {
            return Space::new().into();
        }
        let mut cells = row![].width(Length::Fill);
        for cell in &self.strip {
            let content: Element<Message> = match cell {
                StripCell::Ready(handle) => iced::widget::image(handle.clone())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(iced::ContentFit::Cover)
                    .into(),
                _ => container(Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(|_: &Theme| container::Style {
                        background: Some(Background::Color(palette::HIGH)),
                        ..container::Style::default()
                    })
                    .into(),
            };
            cells = cells.push(container(content).width(Length::FillPortion(1)));
        }

        let (left, mid, right) = range_portions(self.trim_start, self.trim_end, self.open_dur);
        let scrim = |portion: u16| -> Element<Message> {
            container(Space::new())
                .width(Length::FillPortion(portion))
                .height(Length::Fill)
                .style(|_: &Theme| container::Style {
                    background: Some(Background::Color(iced::Color {
                        a: 0.62,
                        ..palette::BACKGROUND
                    })),
                    ..container::Style::default()
                })
                .into()
        };
        let kept = container(Space::new())
            .width(Length::FillPortion(mid))
            .height(Length::Fill)
            .style(|_: &Theme| container::Style {
                border: Border {
                    color: palette::ACCENT,
                    width: 2.0,
                    radius: 4.0.into(),
                },
                ..container::Style::default()
            });
        let overlay = row![scrim(left), kept, scrim(right)].width(Length::Fill);

        container(iced::widget::stack![cells, overlay])
            .width(Length::Fill)
            .height(Length::Fixed(64.0))
            .style(theme::card_style)
            .into()
    }

    /// The trim panel: a start/end range over the clip, and a lossless "save a trimmed copy" action.
    fn trim_panel(&self) -> Element<'_, Message> {
        let dur = self.open_dur.max(0.2);
        let start = self.trim_start.clamp(0.0, dur);
        let end = self.trim_end.clamp(0.0, dur);
        let length = (end - start).max(0.0);

        let panel = column![
            self.filmstrip(),
            setting(
                "Start",
                secs_label(start),
                slider(0.0..=dur, start, Message::TrimStartChanged)
                    .step(0.1f32)
                    .style(theme::arena_slider),
            ),
            setting(
                "End",
                secs_label(end),
                slider(0.0..=dur, end, Message::TrimEndChanged)
                    .step(0.1f32)
                    .style(theme::arena_slider),
            ),
            value_row("Trimmed length", secs_label(length)),
            hint(
                "The start snaps back to the nearest keyframe (about a second), so the cut is \
                 instant and lossless. The trimmed clip is saved as a new file.",
            ),
        ]
        .spacing(14);

        let busy = matches!(self.trim, TrimState::Saving);
        let mut save = button(text("Save trimmed clip").size(13).font(UI_BOLD))
            .style(primary_button)
            .padding([11, 24]);
        if !busy && length >= 0.2 {
            save = save.on_press(Message::TrimSave);
        }
        let panel = panel.push(row![save].spacing(10));

        let panel = match &self.trim {
            TrimState::Idle => panel,
            TrimState::Saving => panel.push(hint("Saving the trimmed clip...")),
            TrimState::Saved(path) => panel.push(
                column![
                    text("Saved a trimmed clip.")
                        .size(12)
                        .style(tinted(palette::ACCENT)),
                    hint(path.display().to_string()),
                ]
                .spacing(6),
            ),
            TrimState::Failed(e) => {
                panel.push(text(e.clone()).size(12).style(tinted(palette::DANGER)))
            }
        };
        card("TRIM", panel)
    }

    /// Play / show in folder / delete, with the delete confirm inline (no OS dialog).
    fn actions(&self) -> Element<'_, Message> {
        if self.confirm_delete {
            return row![
                text("Delete this clip from disk?")
                    .size(12)
                    .style(tinted(palette::TEXT)),
                button(text("Yes, delete").size(11).font(UI_SEMIBOLD))
                    .on_press(Message::DeleteConfirmed)
                    .style(secondary_button)
                    .padding([7, 14]),
                button(text("Keep it").size(11).font(UI_SEMIBOLD))
                    .on_press(Message::DeleteCancelled)
                    .style(link_button)
                    .padding([7, 4]),
            ]
            .spacing(10)
            .align_y(iced::Alignment::Center)
            .into();
        }
        row![
            button(text("Play").size(12).font(UI_BOLD))
                .on_press(Message::Play)
                .style(primary_button)
                .padding([9, 22]),
            button(text("Show in folder").size(11).font(UI_SEMIBOLD))
                .on_press(Message::ShowInFolder)
                .style(secondary_button)
                .padding([9, 14]),
            button(text("Delete").size(11).font(UI_SEMIBOLD))
                .on_press(Message::DeleteRequested)
                .style(secondary_button)
                .padding([9, 14]),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center)
        .into()
    }

    /// The upload panel: title, destination, visibility, and the upload state line.
    fn upload_panel<'a>(
        &'a self,
        entry: &'a ClipEntry,
        ganked: DestStatus,
        youtube: DestStatus,
    ) -> Element<'a, Message> {
        let busy = matches!(
            self.upload,
            UploadState::Uploading { .. }
                | UploadState::Processing { .. }
                | UploadState::Checking { .. }
        );

        let title = column![
            field_label("Title"),
            text_input(&self.title_hint, &self.title)
                .on_input(Message::TitleEdited)
                .style(theme::arena_input),
        ]
        .spacing(6);

        let seg = |label: &'static str, dest: Dest, ready: bool| {
            let active = self.dest == dest;
            let (accent, ink) = if active {
                self.current_accent()
            } else {
                dest_accent(dest)
            };
            let b = button(
                container(text(label).size(11).font(UI_SEMIBOLD))
                    .center_x(Length::Fill)
                    .padding([2, 0]),
            )
            .style(move |theme: &Theme, status| segment_style(theme, status, active, accent, ink))
            .width(Length::Fill)
            .padding([5, 15]);
            if ready && !busy {
                b.on_press(Message::DestPicked(dest))
            } else {
                b
            }
        };
        let dest_control = container(
            row![
                seg("ganked.tv", Dest::Ganked, ganked.ready()),
                seg("YouTube", Dest::YouTube, youtube.ready()),
            ]
            .spacing(2),
        )
        .padding(3)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(palette::HIGH)),
            border: Border {
                color: palette::BORDER,
                width: 1.0,
                radius: 8.0.into(),
            },
            ..container::Style::default()
        });
        let mut destination = column![field_label("Destination"), dest_control].spacing(6);
        if let Some(reason) = ganked.blocker(Dest::Ganked) {
            destination = destination.push(hint(reason));
        }
        if let Some(reason) = youtube.blocker(Dest::YouTube) {
            destination = destination.push(hint(reason));
        }

        let mut visibility = column![
            field_label("Visibility"),
            pick_list(
                &Visibility::ALL[..],
                Some(self.visibility),
                Message::VisibilityPicked,
            )
            .style(theme::arena_pick)
            .width(Length::Fill),
        ]
        .spacing(6);
        if self.dest == Dest::Ganked && self.visibility == Visibility::Private {
            visibility = visibility.push(hint(
                "Private needs a ganked.tv server that supports it; \
                 the clip may come back unlisted.",
            ));
        }

        let dest_ready = match self.dest {
            Dest::Ganked => ganked.ready(),
            Dest::YouTube => youtube.ready(),
        };
        // Once a clip is on a destination, the primary action becomes "Upload again", which
        // verifies the remote copy still exists before it lets a duplicate through.
        let already_uploaded = self.record_for(entry, self.dest).is_some();
        // The action button takes the destination's brand accent: mint for ganked.tv, red for
        // YouTube, so the whole panel reads as "this clip is headed to YouTube".
        let (accent, ink) = self.current_accent();
        // Hover tracks the destination target (a hover mid-fade is rare and cosmetic).
        let accent_hover = match self.dest {
            Dest::Ganked => palette::ACCENT_HOVER,
            Dest::YouTube => palette::YOUTUBE_HOVER,
        };
        let mut send = button(
            text(if already_uploaded {
                "Upload again".to_owned()
            } else {
                format!("Upload to {}", self.dest.label())
            })
            .size(13)
            .font(UI_BOLD),
        )
        .style(move |_theme: &Theme, status| accent_button_style(status, accent, accent_hover, ink))
        .padding([11, 24]);
        if dest_ready && !busy {
            send = send.on_press(if already_uploaded {
                Message::UploadAgain
            } else {
                Message::UploadPressed
            });
        }

        let mut panel = column![title, destination, visibility].spacing(16);
        panel = panel.push(row![send].spacing(10).align_y(iced::Alignment::Center));
        panel = panel.push(self.upload_status(entry, accent));
        theme::card_accent("UPLOAD", accent, panel)
    }

    /// The status line under the upload button, for whatever upload concerns this clip. `accent`
    /// is the destination brand colour (mint/red, or the fading value), so success lines and
    /// share links match the rest of the panel.
    fn upload_status(&self, entry: &ClipEntry, accent: iced::Color) -> Element<'_, Message> {
        match &self.upload {
            UploadState::Uploading { path, dest, .. } if *path == entry.path => {
                cancellable(format!("Uploading to {}...", dest.label()))
            }
            UploadState::Processing { path, .. } if *path == entry.path => {
                cancellable("Processing on ganked.tv...".to_owned())
            }
            UploadState::Checking { path, dest, .. } if *path == entry.path => hint(format!(
                "Checking whether the clip is still on {}...",
                dest.label()
            )),
            UploadState::Blocked { path, dest, link } if *path == entry.path => {
                let mut col = column![
                    text(format!("It's still on {}.", dest.label()))
                        .size(12)
                        .style(tinted(palette::TEXT_SECONDARY))
                ]
                .spacing(8);
                if let Some(url) = link {
                    col = col.push(link_actions(url, accent));
                }
                col.push(anyway_button("Upload another copy")).into()
            }
            UploadState::Unverifiable { path, reason, .. } if *path == entry.path => column![
                text(reason.clone())
                    .size(12)
                    .style(tinted(palette::TEXT_SECONDARY)),
                anyway_button("Upload anyway"),
            ]
            .spacing(8)
            .into(),
            UploadState::Done {
                path,
                dest,
                link,
                note,
            } if *path == entry.path => {
                let mut line = row![
                    text(format!("Uploaded to {}.", dest.label()))
                        .size(12)
                        .style(tinted(accent))
                ]
                .spacing(12)
                .align_y(iced::Alignment::Center);
                line = match link {
                    Some(url) => line.push(link_actions(url, accent)),
                    None => line.push(
                        text(note.clone())
                            .size(12)
                            .style(tinted(palette::TEXT_SECONDARY)),
                    ),
                };
                line.into()
            }
            UploadState::Failed { path, error } if *path == entry.path => text(error.clone())
                .size(12)
                .style(tinted(palette::DANGER))
                .into(),
            UploadState::Uploading { .. }
            | UploadState::Processing { .. }
            | UploadState::Checking { .. } => {
                hint("Another clip is uploading; one upload runs at a time.")
            }
            // Idle for this clip: if it is already on the chosen destination, show where it landed.
            _ => match self.record_for(entry, self.dest) {
                Some(record) => {
                    let mut line = row![
                        text(format!("Already on {}.", self.dest.label()))
                            .size(12)
                            .style(tinted(accent))
                    ]
                    .spacing(12)
                    .align_y(iced::Alignment::Center);
                    if let Some(url) = &record.url {
                        line = line.push(link_actions(url, accent));
                    }
                    line.into()
                }
                None => Space::new().into(),
            },
        }
    }
}

/// A share/watch link with Open + Copy-link buttons, in the destination's brand accent.
fn link_actions(url: &str, accent: iced::Color) -> Element<'static, Message> {
    row![
        text(url.to_owned())
            .size(12)
            .font(UI_SEMIBOLD)
            .style(tinted(accent)),
        button(text("Open").size(11).font(UI_SEMIBOLD))
            .on_press(Message::OpenLink(url.to_owned()))
            .style(secondary_button)
            .padding([6, 12]),
        button(text("Copy link").size(11).font(UI_SEMIBOLD))
            .on_press(Message::CopyLink(url.to_owned()))
            .style(secondary_button)
            .padding([6, 12]),
    ]
    .spacing(12)
    .align_y(iced::Alignment::Center)
    .into()
}

/// A status line with a Cancel button, for the in-flight upload/processing states.
fn cancellable(message: String) -> Element<'static, Message> {
    row![
        text(message)
            .size(12)
            .style(tinted(palette::TEXT_SECONDARY)),
        button(text("Cancel").size(11).font(UI_SEMIBOLD))
            .on_press(Message::UploadCancelled)
            .style(secondary_button)
            .padding([6, 14]),
    ]
    .spacing(12)
    .align_y(iced::Alignment::Center)
    .into()
}

/// The "upload anyway / another copy" escape for the duplicate guard.
fn anyway_button(label: &'static str) -> Element<'static, Message> {
    button(text(label).size(11).font(UI_SEMIBOLD))
        .on_press(Message::UploadAnyway)
        .style(secondary_button)
        .padding([6, 14])
        .into()
}

/// Whether one upload destination can actually receive a clip: logged in (key / refresh
/// token present) AND switched on in the config — the same bar the tray applies before it
/// builds an uploader.
#[derive(Debug, Clone, Copy)]
struct DestStatus {
    logged_in: bool,
    enabled: bool,
}

impl DestStatus {
    fn ready(self) -> bool {
        self.logged_in && self.enabled
    }

    /// Why `dest` can't be uploaded to right now, in words the user can act on.
    fn blocker(self, dest: Dest) -> Option<String> {
        if !self.logged_in {
            Some(match dest {
                Dest::Ganked => "Log in to ganked.tv under Settings first.".to_owned(),
                Dest::YouTube => "Log in with YouTube under Settings first.".to_owned(),
            })
        } else if !self.enabled {
            Some(format!(
                "{} uploads are switched off in Settings; enable them there first.",
                dest.label()
            ))
        } else {
            None
        }
    }
}

/// Both destinations' status, from the same validated config the tray uploads read.
fn dest_statuses(config: &Config) -> (DestStatus, DestStatus) {
    let up = config.upload();
    let yt = config.youtube();
    (
        DestStatus {
            logged_in: !up.api_key.is_empty(),
            enabled: up.enabled,
        },
        DestStatus {
            logged_in: !yt.refresh_token.is_empty(),
            enabled: yt.enabled,
        },
    )
}

/// Upload to ganked.tv with the same flow (and outcome wording) as the tray.
async fn upload_ganked(
    up: rewynd_config::UploadSettings,
    path: PathBuf,
    title: String,
    visibility: Visibility,
) -> Result<Uploaded, String> {
    let client = GankedClient::new(&up.api_url, &up.api_key).map_err(|e| e.to_string())?;
    let clip = client
        .upload(&path, &title, visibility)
        .await
        .map_err(|e| {
            // The user-facing copy is shared with the tray; the full error goes to the log.
            tracing::error!(error = %e, "upload failed");
            user_facing_upload_error(&e)
        })?;
    if clip.failed() {
        return Err(
            "ganked.tv could not process the clip (check its length and format).".to_owned(),
        );
    }
    let link = clip.share_url(&up.share_url);
    Ok(Uploaded {
        remote_id: clip.id,
        link,
        note: "Processing on ganked.tv.".to_owned(),
    })
}

/// Poll ganked.tv processing status until it is ready or failed (or the budget runs out), so the
/// user learns of a server-side failure that only surfaces after "processing".
async fn poll_ganked(
    up: rewynd_config::UploadSettings,
    path: PathBuf,
    clip_id: String,
) -> (PathBuf, Result<StatusOutcome, String>) {
    let outcome = async {
        let client = GankedClient::new(&up.api_url, &up.api_key).map_err(|e| e.to_string())?;
        let report = client
            .poll_status(&clip_id, POLL_INTERVAL, POLL_MAX_READS)
            .await
            .map_err(|e| user_facing_upload_error(&e))?;
        let message = if report.failed() {
            report.failure_message()
        } else if report.is_ready() {
            "Live on ganked.tv.".to_owned()
        } else {
            "Still processing on ganked.tv.".to_owned()
        };
        Ok(StatusOutcome {
            failed: report.failed(),
            link: report.share_url(&up.share_url),
            message,
        })
    }
    .await;
    (path, outcome)
}

/// Whether a ganked.tv clip still exists (for the duplicate guard). A 404 → gone (re-upload
/// allowed); any other error is a verification failure the caller surfaces as "upload anyway".
async fn verify_ganked(
    up: rewynd_config::UploadSettings,
    path: PathBuf,
    clip_id: String,
) -> (PathBuf, Result<bool, String>) {
    let exists = async {
        let client = GankedClient::new(&up.api_url, &up.api_key).map_err(|e| e.to_string())?;
        client
            .clip_exists(&clip_id)
            .await
            .map_err(|e| user_facing_upload_error(&e))
    }
    .await;
    (path, exists)
}

/// Upload to YouTube with the same flow (and outcome wording) as the tray.
async fn upload_youtube(
    yt: rewynd_config::YouTubeSettings,
    path: PathBuf,
    title: String,
    visibility: Visibility,
) -> Result<Uploaded, String> {
    let client = YouTubeClient::new(
        rewynd_config::non_empty_or(&yt.client_id, DEFAULT_CLIENT_ID),
        rewynd_config::non_empty_or(&yt.client_secret, DEFAULT_CLIENT_SECRET),
        &yt.refresh_token,
    )
    .map_err(|e| e.to_string())?;
    let video = client
        .upload(&path, &title, visibility)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "YouTube upload failed");
            user_facing_youtube_error(&e)
        })?;
    Ok(Uploaded {
        remote_id: video.id.clone(),
        link: video.watch_url(),
        note: "Uploaded to YouTube.".to_owned(),
    })
}

/// The saved-at instant in local time, for card titles.
fn saved_at_label(t: SystemTime) -> String {
    jiff::Timestamp::try_from(t).map_or_else(
        |_| "Unknown time".to_owned(),
        |ts| {
            ts.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

fn size_label(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// A whole-library size in whichever of MB / GB reads cleanest.
fn disk_label(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

fn duration_label(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// The section label for a clip: its game subfolder, or [`ROOT_GROUP`] for a root clip.
fn group_label(entry: &ClipEntry) -> &str {
    entry.game.as_deref().unwrap_or(ROOT_GROUP)
}

/// The upload-history identity key for a clip entry.
fn clip_key(entry: &ClipEntry) -> Option<ClipKey> {
    ClipKey::new(&entry.path, entry.size_bytes, entry.modified)
}

/// Case-insensitive subsequence match: every character of `needle` appears in `haystack` in
/// order (so "eldn" matches "Elden Ring"). An empty needle matches everything.
fn fuzzy_match(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars().flat_map(char::to_lowercase);
    'needle: for nc in needle.chars().flat_map(char::to_lowercase) {
        for hc in hay.by_ref() {
            if hc == nc {
                continue 'needle;
            }
        }
        return false;
    }
    true
}

/// A filter chip: mint fill when active, a quiet outline otherwise. The label text is owned, so
/// the chip does not borrow the caller's string.
fn chip(label: &str, active: bool, msg: Message) -> Element<'static, Message> {
    button(text(label.to_uppercase()).size(10).font(UI_BOLD))
        .on_press(msg)
        .style(move |_: &Theme, status| chip_style(status, active))
        .padding([5, 11])
        .into()
}

fn chip_style(status: iced::widget::button::Status, active: bool) -> iced::widget::button::Style {
    use iced::widget::button::{Status, Style};
    let (background, text_color, border_color) = if active {
        (
            Some(Background::Color(palette::ACCENT)),
            palette::INK_ON_ACCENT,
            palette::ACCENT,
        )
    } else {
        match status {
            Status::Hovered | Status::Pressed => (None, palette::ACCENT, palette::ACCENT_BORDER),
            _ => (None, palette::TEXT_SECONDARY, palette::BORDER),
        }
    };
    Style {
        background,
        text_color,
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 12.0.into(),
        },
        ..Style::default()
    }
}

/// The clip card shell: a raised panel that is also a button (hover lifts the border to the
/// accent tint, the design's one sanctioned hover cue).
fn clip_card_style(
    _theme: &Theme,
    status: iced::widget::button::Status,
) -> iced::widget::button::Style {
    use iced::widget::button::{Status, Style};
    let border_color = match status {
        Status::Hovered | Status::Pressed => palette::ACCENT_BORDER,
        _ => palette::BORDER,
    };
    Style {
        background: Some(Background::Color(palette::PANEL)),
        text_color: palette::TEXT,
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Style::default()
    }
}

/// One segment of the destination control: mint fill + ink when active, quiet otherwise.
/// A `m:ss` label for a number of seconds.
fn secs_label(seconds: f32) -> String {
    let s = seconds.max(0.0).round() as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

/// A fresh sibling path for a trimmed copy of `src` (`<stem>-trim.<ext>`, bumping a counter when
/// taken), so re-trimming never overwrites an earlier trim and the copy lands beside the original
/// (same per-game folder).
fn unique_trim_path(src: &Path) -> PathBuf {
    let parent = src.parent().unwrap_or_else(|| Path::new("."));
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("clip");
    let ext = src.extension().and_then(|s| s.to_str()).unwrap_or("mp4");
    let mut candidate = parent.join(format!("{stem}-trim.{ext}"));
    let mut n = 2;
    while candidate.exists() {
        candidate = parent.join(format!("{stem}-trim-{n}.{ext}"));
        n += 1;
    }
    candidate
}

/// `n` evenly spaced, centred sample positions in `0.0..1.0` (cell i samples its own midpoint),
/// so the strip skips the very first and last frames.
fn filmstrip_positions(n: usize) -> Vec<f32> {
    (0..n).map(|i| (i as f32 + 0.5) / n as f32).collect()
}

/// FillPortion weights (left scrim, kept band, right scrim) for the `[start, end]` window over
/// `dur`, on a fixed 1000 scale. A zero weight renders as zero width (iced treats FillPortion(0)
/// as non-fluid). A non-positive `dur` degenerates to "all kept".
fn range_portions(start: f32, end: f32, dur: f32) -> (u16, u16, u16) {
    if dur <= 0.0 {
        return (0, 1000, 0);
    }
    let clamp = |v: f32| (v / dur).clamp(0.0, 1.0);
    let left = (clamp(start) * 1000.0).round() as u16;
    let right = ((1.0 - clamp(end)) * 1000.0).round() as u16;
    let mid = 1000u16.saturating_sub(left).saturating_sub(right);
    (left, mid, right)
}

/// How long the upload-panel accent takes to fade when the destination switches.
const ACCENT_FADE: Duration = Duration::from_millis(180);

/// An in-flight accent fade between two (fill, ink) brand pairs. `start` is anchored on the
/// first tick, so all time comes from the frame subscription rather than `Instant::now()` in
/// update.
struct AccentFade {
    from: (iced::Color, iced::Color),
    to: (iced::Color, iced::Color),
    start: Option<Instant>,
    progress: f32,
}

impl AccentFade {
    /// The interpolated (fill, ink) at the current progress.
    fn accent(&self) -> (iced::Color, iced::Color) {
        (
            lerp_color(self.from.0, self.to.0, self.progress),
            lerp_color(self.from.1, self.to.1, self.progress),
        )
    }

    /// Advance to frame time `now`, anchoring the clock on the first call. Returns `true` once
    /// the fade has reached its end (the caller then drops it).
    fn advance(&mut self, now: Instant) -> bool {
        let start = *self.start.get_or_insert(now);
        let linear = now.duration_since(start).as_secs_f32() / ACCENT_FADE.as_secs_f32();
        self.progress = ease(linear);
        linear >= 1.0
    }
}

/// Smoothstep easing, clamped to `0.0..=1.0`.
fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Per-channel linear interpolation between two colours.
fn lerp_color(a: iced::Color, b: iced::Color, t: f32) -> iced::Color {
    iced::Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// The brand accent and its ink for an upload destination: mint for ganked.tv, red for YouTube.
fn dest_accent(dest: Dest) -> (iced::Color, iced::Color) {
    match dest {
        Dest::Ganked => (palette::ACCENT, palette::INK_ON_ACCENT),
        Dest::YouTube => (palette::YOUTUBE, palette::INK_ON_YOUTUBE),
    }
}

fn segment_style(
    _theme: &Theme,
    status: iced::widget::button::Status,
    active: bool,
    accent: iced::Color,
    ink: iced::Color,
) -> iced::widget::button::Style {
    use iced::widget::button::{Status, Style};
    let (background, text_color) = if active {
        (Some(Background::Color(accent)), ink)
    } else {
        match status {
            Status::Hovered | Status::Pressed => (None, accent),
            Status::Disabled => (None, palette::MUTED),
            _ => (None, palette::TEXT_SECONDARY),
        }
    };
    Style {
        background,
        text_color,
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..Style::default()
    }
}

/// A neutral thumbnail placeholder: a surface-high well with a muted caption.
fn placeholder<'a>(label: &'a str, height: f32) -> Element<'a, Message> {
    container(text(label).size(11).style(tinted(palette::MUTED)))
        .width(Length::Fill)
        .height(height)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_: &Theme| container::Style {
            background: Some(Background::Color(palette::HIGH)),
            ..container::Style::default()
        })
        .into()
}

#[cfg(test)]
mod filmstrip_tests {
    use super::*;

    #[test]
    fn positions_are_centred_and_ordered() {
        let p = filmstrip_positions(4);
        assert_eq!(p.len(), 4);
        assert!((p[0] - 0.125).abs() < 1e-6);
        assert!((p[3] - 0.875).abs() < 1e-6);
        assert!(p.windows(2).all(|w| w[0] < w[1]), "strictly increasing");
        assert!(p.iter().all(|&x| x > 0.0 && x < 1.0), "inside (0,1)");
        assert!(filmstrip_positions(0).is_empty());
    }

    #[test]
    fn portions_split_the_track_by_time() {
        // Whole clip kept: no scrim on either side.
        assert_eq!(range_portions(0.0, 10.0, 10.0), (0, 1000, 0));
        // A middle window.
        assert_eq!(range_portions(2.0, 9.0, 10.0), (200, 700, 100));
        // Empty band (start == end): both scrims, no kept middle.
        assert_eq!(range_portions(5.0, 5.0, 10.0), (500, 0, 500));
        // Degenerate duration stays drawable as "all kept".
        assert_eq!(range_portions(0.0, 0.0, 0.0), (0, 1000, 0));
    }
}

#[cfg(test)]
mod accent_tests {
    use super::*;
    use iced::Color;
    use std::time::{Duration, Instant};

    fn approx(a: Color, b: Color) {
        for (x, y) in [(a.r, b.r), (a.g, b.g), (a.b, b.b), (a.a, b.a)] {
            assert!((x - y).abs() < 1e-4, "{x} vs {y}");
        }
    }

    #[test]
    fn lerp_color_hits_both_ends() {
        let a = Color::from_rgb(0.0, 0.0, 0.0);
        let b = Color::from_rgb(1.0, 0.5, 0.25);
        approx(lerp_color(a, b, 0.0), a);
        approx(lerp_color(a, b, 1.0), b);
        approx(lerp_color(a, b, 0.5), Color::from_rgb(0.5, 0.25, 0.125));
    }

    #[test]
    fn ease_is_clamped_and_smooth() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert_eq!(ease(-1.0), 0.0);
        assert_eq!(ease(2.0), 1.0);
        assert!((ease(0.5) - 0.5).abs() < 1e-6, "symmetric midpoint");
    }

    #[test]
    fn fade_runs_from_source_to_target_then_ends() {
        let from = (palette::ACCENT, palette::INK_ON_ACCENT);
        let to = (palette::YOUTUBE, palette::INK_ON_YOUTUBE);
        let mut fade = AccentFade {
            from,
            to,
            start: None,
            progress: 0.0,
        };
        let t0 = Instant::now();

        // First tick anchors the clock; still fully at `from`.
        assert!(!fade.advance(t0));
        approx(fade.accent().0, from.0);

        // Partway through: strictly between the endpoints.
        assert!(!fade.advance(t0 + Duration::from_millis(90)));
        let mid = fade.accent().0;
        assert!(mid.r > from.0.r && mid.r < to.0.r + 1e-3);

        // At/after the duration: reports done and sits on the target.
        assert!(fade.advance(t0 + ACCENT_FADE));
        approx(fade.accent().0, to.0);
    }

    #[test]
    fn advance_fade_clears_when_complete() {
        let mut fade = AccentFade {
            from: (palette::ACCENT, palette::INK_ON_ACCENT),
            to: (palette::YOUTUBE, palette::INK_ON_YOUTUBE),
            start: None,
            progress: 0.0,
        };
        let t0 = Instant::now();
        assert!(!fade.advance(t0));
        assert!(fade.advance(t0 + ACCENT_FADE + Duration::from_millis(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_and_duration_labels() {
        assert_eq!(size_label(123_456_789), "123.5 MB");
        assert_eq!(size_label(900_000), "0.9 MB");
        assert_eq!(duration_label(Duration::from_secs(30)), "0:30");
        assert_eq!(duration_label(Duration::from_secs(65)), "1:05");
        assert_eq!(duration_label(Duration::from_secs(600)), "10:00");
    }

    #[test]
    fn dest_status_gates_on_login_and_enabled() {
        let ready = DestStatus {
            logged_in: true,
            enabled: true,
        };
        assert!(ready.ready());
        assert_eq!(ready.blocker(Dest::Ganked), None);
        let logged_out = DestStatus {
            logged_in: false,
            enabled: false,
        };
        assert!(!logged_out.ready());
        let blocker = logged_out.blocker(Dest::YouTube).expect("blocked");
        assert!(blocker.contains("Log in"), "{blocker}");
        let disabled = DestStatus {
            logged_in: true,
            enabled: false,
        };
        assert!(!disabled.ready());
        let blocker = disabled.blocker(Dest::Ganked).expect("blocked");
        assert!(blocker.contains("switched off"), "{blocker}");
    }

    #[test]
    fn fuzzy_match_is_a_case_insensitive_subsequence() {
        assert!(fuzzy_match("eldn", "Elden Ring"));
        assert!(fuzzy_match("OW", "Overwatch"));
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("desktop", "Desktop"));
        assert!(!fuzzy_match("zzz", "Overwatch"));
        assert!(
            !fuzzy_match("ringx", "Elden Ring"),
            "extra char past the end"
        );
    }

    #[test]
    fn disk_label_scales_mb_to_gb() {
        assert_eq!(disk_label(500_000_000), "500 MB");
        assert_eq!(disk_label(0), "0 MB");
        assert_eq!(disk_label(1_500_000_000), "1.5 GB");
        assert_eq!(disk_label(12_300_000_000), "12.3 GB");
    }

    #[test]
    fn group_label_falls_back_to_the_root_section() {
        let with_game = ClipEntry {
            path: PathBuf::from("/c/Elden Ring/rewynd-1-0.mp4"),
            game: Some("Elden Ring".to_owned()),
            saved_at: SystemTime::UNIX_EPOCH,
            modified: SystemTime::UNIX_EPOCH,
            size_bytes: 1,
        };
        let rootless = ClipEntry {
            game: None,
            ..with_game.clone()
        };
        assert_eq!(group_label(&with_game), "Elden Ring");
        assert_eq!(group_label(&rootless), ROOT_GROUP);
    }

    #[test]
    fn saved_at_label_formats_local_time() {
        let label = saved_at_label(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(label.len(), 16, "{label}");
        assert!(label.starts_with("2023-11-1"), "{label}");
    }
}
