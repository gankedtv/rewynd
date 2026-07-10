//! The audible half of the save feedback, shared by the Linux badge and the macOS
//! recorder: the clip-saved chime on success, a synthesised two-tone beep on failure,
//! both played through rodio (a notification server's own sound is muted under
//! fullscreen Do-Not-Disturb, which is exactly when a clip is most likely saved).

use std::num::NonZero;
use std::time::Duration;

/// The one visual/audible bit of outcome signal (mirrors the Windows overlay accent).
/// Its badge colour lives with the badge (`badge::Accent::rgb`) — the chime only
/// cares which of the two sounds to play.
#[derive(Clone, Copy)]
pub enum Accent {
    Success,
    Failure,
}

/// Play the save sound off-thread: the chime on success, a short error tone on failure.
pub fn play(accent: Accent) {
    let _ = std::thread::Builder::new()
        .name("rewynd-chime".to_owned())
        .spawn(move || play_blocking(accent));
}

fn play_blocking(accent: Accent) {
    let (samples, channels, rate) = match accent {
        Accent::Success => match decode_wav(crate::CLIP_SAVED_WAV) {
            Some(decoded) => decoded,
            None => return,
        },
        Accent::Failure => (error_tone(), 1, 44_100),
    };
    // The sink owns the device stream and must outlive playback; no sound server just
    // means the save confirms silently.
    let Ok(mut sink) = rodio::DeviceSinkBuilder::open_default_sink() else {
        return;
    };
    sink.log_on_drop(false);
    let player = rodio::Player::connect_new(sink.mixer());
    let (Some(channels_nz), Some(rate_nz)) = (NonZero::new(channels), NonZero::new(rate)) else {
        return;
    };
    let frames = samples.len() as f64 / f64::from(channels);
    player.append(rodio::buffer::SamplesBuffer::new(
        channels_nz,
        rate_nz,
        samples,
    ));
    // Let the queued samples drain before the sink drops (which would cut the tail).
    std::thread::sleep(
        Duration::from_secs_f64(frames / f64::from(rate)) + Duration::from_millis(120),
    );
}

/// Decode a 16-bit PCM WAV (the embedded chime) to `(interleaved f32 samples, channels,
/// sample rate)`. Enough for our own asset; not a general parser. `None` if the layout
/// is unexpected.
fn decode_wav(bytes: &[u8]) -> Option<(Vec<f32>, u16, u32)> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut fmt = None;
    let mut samples = None;
    let mut i = 12;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let size = u32::from_le_bytes(bytes[i + 4..i + 8].try_into().ok()?) as usize;
        let body = i + 8;
        if body + size > bytes.len() {
            break;
        }
        if id == b"fmt " && size >= 16 {
            let channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().ok()?);
            let rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().ok()?);
            fmt = Some((channels, rate));
        } else if id == b"data" {
            samples = Some(
                bytes[body..body + size]
                    .chunks_exact(2)
                    .map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32_768.0)
                    .collect::<Vec<f32>>(),
            );
        }
        // Chunks are word-aligned: an odd size carries a pad byte.
        i = body + size + (size & 1);
    }
    let ((channels, rate), samples) = (fmt?, samples?);
    (channels > 0).then_some((samples, channels, rate))
}

/// A short descending two-tone beep for the failure case (the Windows path uses the
/// system error beep here; we synthesise one so it plays without a sound theme).
fn error_tone() -> Vec<f32> {
    let rate = 44_100.0;
    let mut out = Vec::new();
    for (freq, ms) in [(620.0_f32, 110.0_f32), (440.0, 150.0)] {
        let n = (rate * ms / 1000.0) as usize;
        for k in 0..n {
            let t = k as f32 / rate;
            // A short raised-cosine envelope so the tone doesn't click on/off.
            let env = (std::f32::consts::PI * k as f32 / n as f32).sin();
            out.push(0.28 * env * (2.0 * std::f32::consts::PI * freq * t).sin());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_embedded_chime() {
        let (samples, channels, rate) =
            decode_wav(crate::CLIP_SAVED_WAV).expect("chime is a valid 16-bit WAV");
        assert!(channels >= 1, "at least one channel, got {channels}");
        assert!(rate >= 8_000, "plausible sample rate, got {rate}");
        assert!(!samples.is_empty(), "chime has samples");
        assert_eq!(
            samples.len() % channels as usize,
            0,
            "whole interleaved frames"
        );
        assert!(
            samples.iter().all(|s| (-1.0..=1.0).contains(s)),
            "samples are normalised to [-1, 1]"
        );
    }

    #[test]
    fn rejects_non_wav() {
        assert!(decode_wav(b"not a wav file at all........").is_none());
        assert!(decode_wav(&[]).is_none());
    }
}
