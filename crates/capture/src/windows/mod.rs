//! Windows capture: Windows Graphics Capture (WGC) delivers frames as D3D11
//! textures; each is copied into a shareable texture whose NT handle is handed to
//! the per-frame callback (PLAN §3.5, §6.1).
//!
//! - [`wgc_capture`]: the WGC session setup, the shareable-slot pool, and the
//!   per-frame copy + NT-handle duplication.

pub mod wgc_capture;

pub use wgc_capture::{CapturedD3d11Frame, capture_stream};
