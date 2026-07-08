//! The clickable "clip saved" desktop toast. Built directly on the WinRT `ToastNotification`
//! API rather than notify-rust, because the click-to-open behavior needs the `launch` +
//! `activationType="protocol"` attributes notify-rust does not expose. Clicking the toast body
//! launches the `rewynd://` URL (see `rewynd_config::register_clip_protocol`), which the shell
//! routes to the GUI — no COM activator required.

use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::core::HSTRING;

/// Show a "clip saved" toast that opens the clip in the GUI when clicked. `app_id` is the
/// registered AUMID (so the toast carries rewynd's name/icon), `body` is the human line, and
/// `deeplink` is the `rewynd://clip/<name>` URL the body's protocol activation launches. Returns
/// an error if the toast could not be built or shown, so the caller can fall back to a plain one.
pub fn clip_saved(app_id: &str, body: &str, deeplink: &str) -> windows::core::Result<()> {
    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(format!(
        r#"<toast launch="{launch}" activationType="protocol">
            <visual>
                <binding template="ToastGeneric">
                    <text>Clip saved</text>
                    <text>{body}</text>
                </binding>
            </visual>
        </toast>"#,
        launch = xml_escape(deeplink),
        body = xml_escape(body),
    )))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(app_id))?.Show(&toast)
}

/// Escape the handful of characters that are special inside the toast's XML attributes/text.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
