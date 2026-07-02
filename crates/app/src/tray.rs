//! System tray for the recorder: a StatusNotifierItem (no GTK, over the D-Bus stack we already
//! use for the portals) plus a "clip saved" desktop toast. Linux/KDE.

use std::path::Path;
use std::sync::LazyLock;

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Handle, Icon, ToolTip, Tray, TrayMethods};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// What a tray menu click asks the recorder to do.
pub enum TrayCmd {
    SaveClip,
    UploadClip,
    UploadYouTube,
    OpenSettings,
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
            StandardItem {
                label: "Save clip now".to_owned(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayCmd::SaveClip);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Upload last clip".to_owned(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayCmd::UploadClip);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Upload last clip to YouTube".to_owned(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayCmd::UploadYouTube);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open settings".to_owned(),
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
pub async fn spawn() -> anyhow::Result<(Handle<RewyndTray>, UnboundedReceiver<TrayCmd>)> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let tray = RewyndTray {
        tx,
        status: "rewynd is recording".to_owned(),
    };
    let handle = tray.spawn().await?;
    Ok((handle, rx))
}

/// Best-effort desktop notification. Async: the zbus backend's blocking `show()` would panic if
/// called inside our tokio runtime, so it is sent via `show_async`.
pub async fn toast(summary: &str, body: &str) {
    // Notification bodies are markup on many servers (KDE renders a HTML subset); escape so
    // server-provided text (error details, share codes) can't inject tags.
    let body = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    if let Err(e) = notify_rust::Notification::new()
        .summary(summary)
        .body(&body)
        .icon(rewynd_config::APP_ID)
        .appname("rewynd")
        .show_async()
        .await
    {
        tracing::warn!(error = %e, summary, "could not show notification");
    }
}

/// "Clip saved" notification for a freshly written clip.
pub async fn clip_saved_toast(path: &Path) {
    toast("Clip saved", &path.display().to_string()).await;
}
