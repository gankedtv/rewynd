//! Linux capture: the XDG ScreenCast portal (`ashpd`) negotiates a session and
//! PipeWire delivers frames as DMA-BUF fds (PLAN §3.5, §6.1).
//!
//! - [`portal`]: the ScreenCast handshake → a PipeWire node id + remote fd.
//! - [`pipewire_capture`]: the stream setup that negotiates DMA-BUF and reads frames.
//! - [`audio`]: system-audio (default sink monitor) capture as interleaved PCM.

use std::io::Cursor;

use pipewire as pw;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Value};

pub mod audio;
pub mod focus;
pub mod pipewire_capture;
pub mod portal;
pub mod vulkan_modifiers;

/// Serialize a pod [`Object`] to bytes (suitable for `Pod::from_bytes`). Shared by the
/// video and audio stream setups, which both build PipeWire format/buffer pods.
pub(crate) fn serialize_object(obj: Object) -> Vec<u8> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("pod serialization cannot fail for in-memory buffer")
        .0
        .into_inner()
}

pub use audio::{AudioParams, AudioSource, capture_audio};
pub use focus::{FocusError, FocusWatcher};
pub use pipewire_capture::{CapturedDmabuf, DmabufFrame, StreamPrefs, capture_stream};
// Diagnostic probe entry points, used by this crate's examples only.
#[cfg(feature = "probes")]
pub use pipewire_capture::{capture_one_dmabuf, run_capture_probe};
pub use portal::{PortalSession, open_portal, open_portal_with};
pub use vulkan_modifiers::query_drm_format_modifiers;
