//! rewynd — instant-replay clip recorder.
//!
//! This binary wires the pipeline together: capture → RGBA→NV12 → encode → keyframe
//! -aware ring buffer → hotkey → MP4 mux → disk (→ optional ganked.tv upload). The
//! scaffold only sets up logging and reports the default target; each stage is
//! assembled in its own issue (capture #4–#7, encode #8/#9, buffer #10, hotkey #11,
//! mux #12).

use std::time::Duration;

use anyhow::Result;
use rewynd_buffer::RingBuffer;
use rewynd_encode::EncodeParams;

/// Buffer retention window for the MVP (PLAN §2). Configurable in #16.
const BUFFER_WINDOW: Duration = Duration::from_secs(60);

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // 1080p60 target (PLAN §2); every field is a parameter, never a hard limit (PLAN §9).
    let params = EncodeParams::default();
    let buffer = RingBuffer::new(BUFFER_WINDOW);

    tracing::info!(
        width = params.width,
        height = params.height,
        fps = params.framerate,
        bitrate_bps = params.bitrate_bps,
        idr_period = params.idr_period,
        buffer_window_s = buffer.window().as_secs(),
        "rewynd scaffold initialised — pipeline stages are wired in their respective issues"
    );

    Ok(())
}
