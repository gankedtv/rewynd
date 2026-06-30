//! Diagnostic probe for system-audio capture: connect to PipeWire, capture the default
//! sink's monitor (what you hear) as interleaved `F32LE` PCM, and log each buffer's peak
//! and RMS level plus its capture-relative PTS. Proves the capture path delivers real,
//! non-silent samples — play some audio while it runs.
//!
//! Run it (Linux, live PipeWire session) with:
//!
//! ```text
//! cargo run -p rewynd-capture --example audio_probe
//! ```
//!
//! Captures 200 buffers by default; override with `AUDIO_PROBE_BUFFERS`.
//! On non-Linux targets this compiles to a stub that just prints "Linux only".

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::cell::Cell;
    use std::ops::ControlFlow;
    use std::rc::Rc;
    use std::time::Duration;

    use rewynd_capture::linux::{AudioParams, capture_system_audio};

    /// Default number of buffers to capture when `AUDIO_PROBE_BUFFERS` is unset.
    const DEFAULT_BUFFERS: u32 = 200;
    /// Peak amplitude (of normalized `f32` PCM) above which we call the capture
    /// non-silent. Comfortably above dithered-silence noise, well below real signal.
    const SILENCE_PEAK: f32 = 1.0e-4;
    /// Give up (with a clear error) if no audio buffers arrive within this window — an idle
    /// default sink can suspend and deliver nothing, which would otherwise hang the probe.
    const IDLE_TIMEOUT: Duration = Duration::from_secs(3);

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .init();

        let max_buffers: u32 = std::env::var("AUDIO_PROBE_BUFFERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_BUFFERS);

        let params = AudioParams::default();
        tracing::info!(
            sample_rate = params.sample_rate,
            channels = params.channels,
            max_buffers,
            "starting system-audio capture probe (play audio to see non-zero levels)"
        );

        // The callback is `'static`, so it can't borrow these locals; share them through
        // `Rc<Cell<_>>` so the post-loop summary sees what the callback accumulated.
        let buffers_seen = Rc::new(Cell::new(0_u32));
        let total_frames = Rc::new(Cell::new(0_u64));
        let overall_peak = Rc::new(Cell::new(0.0_f32));
        let nonfinite = Rc::new(Cell::new(0_u64));

        capture_system_audio(params, Some(IDLE_TIMEOUT), {
            let buffers_seen = buffers_seen.clone();
            let total_frames = total_frames.clone();
            let overall_peak = overall_peak.clone();
            let nonfinite = nonfinite.clone();
            move |pcm, pts| {
                let n = buffers_seen.get() + 1;
                buffers_seen.set(n);
                let frames = pcm.len() as u64 / u64::from(params.channels.max(1));
                total_frames.set(total_frames.get() + frames);

                // One pass: peak amplitude + sum of squares (f64 accumulator avoids
                // overflow/precision loss), tracking any non-finite samples separately so a
                // NaN run can't masquerade as silence in the `.max()` fold.
                let mut peak = 0.0_f32;
                let mut sum_sq = 0.0_f64;
                let mut nan = 0_u64;
                for &s in pcm {
                    if s.is_finite() {
                        peak = peak.max(s.abs());
                        sum_sq += f64::from(s) * f64::from(s);
                    } else {
                        nan += 1;
                    }
                }
                overall_peak.set(overall_peak.get().max(peak));
                nonfinite.set(nonfinite.get() + nan);
                // Divide by the finite-sample count (sum_sq skipped the non-finite ones), so
                // a NaN/inf-laden buffer isn't reported as artificially quieter than it is.
                let finite = pcm.len() as u64 - nan;
                let rms = if finite == 0 {
                    0.0
                } else {
                    (sum_sq / finite as f64).sqrt()
                };

                tracing::info!(
                    buffer = n,
                    samples = pcm.len(),
                    frames,
                    pts_ms = pts.as_millis() as u64,
                    peak = format_args!("{peak:.5}"),
                    rms = format_args!("{rms:.5}"),
                    "audio buffer"
                );

                if n >= max_buffers {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            }
        })?;

        let overall_peak = overall_peak.get();
        let nonfinite = nonfinite.get();
        tracing::info!(
            buffers = buffers_seen.get(),
            total_frames = total_frames.get(),
            overall_peak = format_args!("{overall_peak:.5}"),
            nonfinite,
            "audio capture probe finished"
        );
        if nonfinite > 0 {
            tracing::warn!(nonfinite, "captured non-finite (NaN/inf) samples");
        }
        if overall_peak > SILENCE_PEAK {
            tracing::info!("non-silent capture confirmed ✔");
        } else {
            tracing::warn!(
                "capture was silent — was anything playing? (samples flowed, levels were ~0)"
            );
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("Linux only");
}
