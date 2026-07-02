//! Windows capture: Windows Graphics Capture (WGC) delivers frames as D3D11
//! textures; each is copied into a shareable texture whose NT handle is handed to
//! the per-frame callback (PLAN §3.5, §6.1). System audio comes from WASAPI.
//!
//! - [`wgc_capture`]: the WGC session setup, the shareable-slot pool, and the
//!   per-frame copy + NT-handle duplication.
//! - [`game_window`]: the foreground-game heuristic behind game-only capture.
//! - [`wasapi_audio`]: loopback (system mix) and microphone capture as f32 PCM.

mod game_window;
pub mod wasapi_audio;
pub mod wgc_capture;

pub use game_window::describe_foreground;
pub use wasapi_audio::capture_audio;
pub use wgc_capture::{CapturedD3d11Frame, GameCallback, capture_game_stream, capture_stream};
