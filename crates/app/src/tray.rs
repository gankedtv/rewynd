//! System tray for the recorder: a StatusNotifierItem (no GTK, over the D-Bus stack we already
//! use for the portals) plus a "clip saved" desktop toast. Linux/KDE.

use std::path::Path;
use std::sync::LazyLock;

use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
use ksni::{Handle, Icon, ToolTip, Tray, TrayMethods};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// What a tray menu click asks the recorder to do.
pub enum TrayCmd {
    SaveClip,
    OpenSettings,
    /// Flip the microphone on/off and restart the recorder to apply it.
    ToggleMic,
    Quit,
}

// The brand mark in every shipped size (the host picks the closest), decoded once to ksni's
// ARGB32 byte order.
static ICON: LazyLock<Vec<Icon>> = LazyLock::new(|| {
    rewynd_config::BRAND_ICONS
        .iter()
        .filter_map(|(_, bytes)| {
            let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?;
            let (width, height) = (img.width() as i32, img.height() as i32);
            let mut data = img.into_rgba8().into_vec();
            for px in data.chunks_exact_mut(4) {
                px.rotate_right(1); // RGBA -> ARGB32
            }
            Some(Icon {
                width,
                height,
                data,
            })
        })
        .collect()
});

pub struct RewyndTray {
    tx: UnboundedSender<TrayCmd>,
    /// One-line pipeline status shown as the tooltip title; the recorder updates it on failures.
    pub status: String,
    /// Whether the microphone is recording, for the menu checkmark (read at spawn; the toggle
    /// restarts the recorder, so a fresh tray reflects the new value).
    mic_enabled: bool,
}

impl Tray for RewyndTray {
    fn id(&self) -> String {
        rewynd_config::APP_ID.to_owned()
    }

    fn title(&self) -> String {
        "rewynd".to_owned()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        ICON.clone()
    }

    // Fall back to the installed themed icon only if the embedded mark failed to decode (else
    // the pixmap wins).
    fn icon_name(&self) -> String {
        if ICON.is_empty() {
            rewynd_config::APP_ID.to_owned()
        } else {
            String::new()
        }
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.status.clone(),
            description: "Buffering recent gameplay. Press your hotkey to save a clip.".to_owned(),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            // A read-only header so a tester can report which build they are on.
            StandardItem {
                label: concat!("rewynd v", env!("CARGO_PKG_VERSION")).to_owned(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Save clip now".to_owned(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayCmd::SaveClip);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Record microphone".to_owned(),
                checked: self.mic_enabled,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayCmd::ToggleMic);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open rewynd".to_owned(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayCmd::OpenSettings);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit rewynd".to_owned(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayCmd::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Register the tray on the current tokio runtime, returning the command receiver. The returned
/// `Handle` keeps the icon alive — hold it for as long as the tray should show.
pub async fn spawn(
    mic_enabled: bool,
) -> anyhow::Result<(Handle<RewyndTray>, UnboundedReceiver<TrayCmd>)> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let tray = RewyndTray {
        tx,
        status: "rewynd is recording".to_owned(),
        mic_enabled,
    };
    let handle = tray.spawn().await?;
    Ok((handle, rx))
}

/// How long a save confirmation stays up before we close it ourselves; ~4s matches the Windows
/// in-game badge.
const DISMISS_AFTER: std::time::Duration = std::time::Duration::from_secs(4);

/// Best-effort desktop notification. Async: the zbus backend's blocking `show()` would panic if
/// called inside our tokio runtime, so it is sent via `show_async`.
///
/// `urgency` [`Critical`](notify_rust::Urgency::Critical) is what makes a save confirmation show
/// over a fullscreen game: KWin/Plasma auto-enables Do-Not-Disturb while a window is fullscreen,
/// which swallows normal-urgency notifications. Critical bypasses that, but KDE then holds the
/// notification until dismissed (it ignores the expire timeout for critical urgency), so we close
/// it ourselves after [`DISMISS_AFTER`] to keep repeated saves from stacking on screen. The sound
/// is deliberately left off — the in-game badge plays the chime itself (see `crate::badge`), which
/// is the only route that survives the same fullscreen Do-Not-Disturb.
async fn notify(summary: &str, body: &str, urgency: notify_rust::Urgency) {
    // Notification bodies are markup on many servers (KDE renders a HTML subset); escape so
    // server-provided text (error details, share codes) can't inject tags.
    let body = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let mut note = notify_rust::Notification::new();
    note.summary(summary)
        .body(&body)
        .icon(rewynd_config::APP_ID)
        .appname("rewynd")
        .urgency(urgency);
    let handle = match note.show_async().await {
        Ok(handle) => handle,
        Err(e) => {
            tracing::warn!(error = %e, summary, "could not show notification");
            return;
        }
    };
    if urgency == notify_rust::Urgency::Critical {
        tokio::spawn(async move {
            tokio::time::sleep(DISMISS_AFTER).await;
            handle.close_async().await;
        });
    }
}

/// Best-effort desktop notification at normal urgency (mic toggle, config errors).
pub async fn toast(summary: &str, body: &str) {
    notify(summary, body, notify_rust::Urgency::Normal).await;
}

/// "Clip saved" notification for a freshly written clip. Only used as the fallback when the in-game
/// badge can't be shown (a compositor without layer-shell); critical so it still surfaces in-game.
pub async fn clip_saved_toast(path: &Path) {
    notify(
        "Clip saved",
        &path.display().to_string(),
        notify_rust::Urgency::Critical,
    )
    .await;
}

/// A failed/empty save notification (the badge fallback). Critical so it surfaces over a fullscreen
/// game.
pub async fn save_failed_toast(summary: &str, body: &str) {
    notify(summary, body, notify_rust::Urgency::Critical).await;
}
