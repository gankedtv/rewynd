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
    OpenSettings,
    Quit,
}

// The embedded mark, decoded once to ksni's ARGB32 byte order.
static ICON: LazyLock<Vec<Icon>> = LazyLock::new(|| {
    let Ok(img) = image::load_from_memory_with_format(
        include_bytes!("../assets/tray.png"),
        image::ImageFormat::Png,
    ) else {
        return Vec::new();
    };
    let (width, height) = (img.width() as i32, img.height() as i32);
    let mut data = img.into_rgba8().into_vec();
    for px in data.chunks_exact_mut(4) {
        px.rotate_right(1); // RGBA -> ARGB32
    }
    vec![Icon {
        width,
        height,
        data,
    }]
});

pub struct RewyndTray {
    tx: UnboundedSender<TrayCmd>,
}

impl Tray for RewyndTray {
    fn id(&self) -> String {
        "tv.ganked.rewynd".to_owned()
    }

    fn title(&self) -> String {
        "rewynd".to_owned()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        ICON.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "rewynd is recording".to_owned(),
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
    let handle = RewyndTray { tx }.spawn().await?;
    Ok((handle, rx))
}

/// Best-effort "clip saved" desktop notification.
pub fn clip_saved_toast(path: &Path) {
    if let Err(e) = notify_rust::Notification::new()
        .summary("Clip saved")
        .body(&path.display().to_string())
        .icon("tv.ganked.rewynd")
        .appname("rewynd")
        .show()
    {
        tracing::warn!(error = %e, "could not show clip-saved notification");
    }
}
