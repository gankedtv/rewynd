//! Opus audio encoding (PLAN §9, ADR 0004): interleaved `f32` PCM in, bare Opus packets
//! out, each stamped with the capture-relative PTS of its first sample.
//!
//! libopus encodes fixed-size frames, but the capture delivers variable-size buffers, so
//! [`OpusAudioEncoder`] accumulates samples and emits one [`EncodedAudioChunk`] per whole
//! Opus frame. The PTS is anchored to the first buffered sample and advanced by exactly one
//! frame each packet, which stays wall-clock-accurate for the continuous monitor capture
//! and re-anchors whenever the buffer fully drains (so a gap doesn't accumulate drift).

use std::time::Duration;

use opus::{Application, Bitrate, Channels};
use rewynd_buffer::EncodedAudioChunk;
use thiserror::Error;

/// Output buffer for one encoded packet. 4000 bytes comfortably exceeds the largest Opus
/// packet for our frame sizes (libopus recommends ~4 KiB).
const MAX_PACKET_BYTES: usize = 4000;

/// Errors from the Opus audio encoder.
#[derive(Debug, Error)]
pub enum AudioEncodeError {
    /// The encoder could not be created or configured.
    #[error("failed to initialise the Opus encoder: {0}")]
    Init(String),
    /// Encoding a frame failed.
    #[error("failed to encode audio: {0}")]
    Encode(String),
    /// The parameters are unsupported (bad channel count, sample rate, or frame size).
    #[error("unsupported audio parameters: {0}")]
    Params(String),
}

/// Opus audio encoder configuration.
///
/// Sample rate / channels / bitrate are parameters, never hard-coded (per CLAUDE.md); the
/// defaults (48 kHz stereo, 128 kbps VBR, 20 ms frames) match the capture and Opus's
/// native rate.
#[derive(Debug, Clone, Copy)]
pub struct AudioEncodeParams {
    /// Sample rate in Hz. Opus accepts 8/12/16/24/48 kHz; capture delivers 48 kHz.
    pub sample_rate: u32,
    /// Channel count (1 = mono, 2 = stereo).
    pub channels: u32,
    /// Average target bitrate in bits per second.
    pub bitrate_bps: u32,
    /// Opus frame duration in milliseconds. Must be a valid Opus size (2.5/5/10/20/40/60);
    /// 20 ms is the typical streaming choice.
    pub frame_ms: u32,
}

impl Default for AudioEncodeParams {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            bitrate_bps: 128_000,
            frame_ms: 20,
        }
    }
}

/// libopus-backed encoder that turns interleaved `f32` PCM into [`EncodedAudioChunk`]s.
pub struct OpusAudioEncoder {
    encoder: opus::Encoder,
    /// Samples per channel in one Opus frame (e.g. 960 for 20 ms at 48 kHz).
    samples_per_channel: u32,
    /// Interleaved samples in one frame (`samples_per_channel * channels`).
    frame_len: usize,
    /// One frame's wall-clock duration, added to the PTS per emitted packet.
    frame_duration: Duration,
    /// Accumulated interleaved samples awaiting a full frame.
    pending: Vec<f32>,
    /// Capture-relative PTS of `pending[0]` (the first buffered sample).
    pending_pts: Duration,
    /// Reused packet output buffer.
    packet: Vec<u8>,
    /// Encoder lookahead in 48 kHz samples — the `dOps` PreSkip / decoder priming.
    pre_skip: u16,
}

impl OpusAudioEncoder {
    /// Build an encoder for `params`. Reads the encoder lookahead once (after configuration)
    /// for the container's pre-skip.
    pub fn new(params: AudioEncodeParams) -> Result<Self, AudioEncodeError> {
        let channels = match params.channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            other => {
                return Err(AudioEncodeError::Params(format!(
                    "channels must be 1 or 2, got {other}"
                )));
            }
        };

        // samples per channel per frame; must be a whole, valid Opus frame size.
        if params.frame_ms == 0 || (params.sample_rate * params.frame_ms) % 1000 != 0 {
            return Err(AudioEncodeError::Params(format!(
                "frame_ms {} does not yield a whole frame at {} Hz",
                params.frame_ms, params.sample_rate
            )));
        }
        let samples_per_channel = params.sample_rate * params.frame_ms / 1000;

        let mut encoder = opus::Encoder::new(params.sample_rate, channels, Application::Audio)
            .map_err(|e| AudioEncodeError::Init(e.to_string()))?;
        encoder
            .set_bitrate(Bitrate::Bits(i32::try_from(params.bitrate_bps).map_err(
                |_| AudioEncodeError::Params("bitrate_bps too large".to_owned()),
            )?))
            .map_err(|e| AudioEncodeError::Init(e.to_string()))?;
        encoder
            .set_vbr(true)
            .map_err(|e| AudioEncodeError::Init(e.to_string()))?;

        // Lookahead is read after all set_* calls (bitrate/complexity affect it).
        let pre_skip = u16::try_from(
            encoder
                .get_lookahead()
                .map_err(|e| AudioEncodeError::Init(e.to_string()))?
                .max(0),
        )
        .map_err(|_| AudioEncodeError::Init("encoder lookahead out of range".to_owned()))?;

        let channels = params.channels as usize;
        let frame_len = samples_per_channel as usize * channels;
        let frame_duration = Duration::from_nanos(
            u64::from(samples_per_channel) * 1_000_000_000 / u64::from(params.sample_rate),
        );

        Ok(Self {
            encoder,
            samples_per_channel,
            frame_len,
            frame_duration,
            pending: Vec::new(),
            pending_pts: Duration::ZERO,
            packet: vec![0; MAX_PACKET_BYTES],
            pre_skip,
        })
    }

    /// The encoder lookahead (`dOps` PreSkip), in 48 kHz samples.
    #[must_use]
    pub fn pre_skip(&self) -> u16 {
        self.pre_skip
    }

    /// Feed one capture buffer of interleaved `f32` PCM stamped at `pts` (the capture-
    /// relative time of its first sample), emitting an [`EncodedAudioChunk`] for each whole
    /// Opus frame now available. `on_packet` receives each packet in order.
    pub fn push(
        &mut self,
        pcm: &[f32],
        pts: Duration,
        mut on_packet: impl FnMut(EncodedAudioChunk),
    ) -> Result<(), AudioEncodeError> {
        // Re-anchor the PTS clock whenever we start from an empty buffer (first call, or
        // after a full drain), so a capture gap doesn't accumulate drift.
        if self.pending.is_empty() {
            self.pending_pts = pts;
        }
        self.pending.extend_from_slice(pcm);

        while self.pending.len() >= self.frame_len {
            let n = self
                .encoder
                .encode_float(&self.pending[..self.frame_len], &mut self.packet)
                .map_err(|e| AudioEncodeError::Encode(e.to_string()))?;
            on_packet(EncodedAudioChunk {
                bytes: self.packet[..n].to_vec(),
                frames: self.samples_per_channel,
                pts: self.pending_pts,
            });
            self.pending_pts += self.frame_duration;
            self.pending.drain(..self.frame_len);
        }
        Ok(())
    }

    /// Encode any leftover samples as a final frame, zero-padded to a whole frame. Call at
    /// shutdown so the last partial frame isn't lost; harmless if nothing is pending.
    pub fn flush(
        &mut self,
        mut on_packet: impl FnMut(EncodedAudioChunk),
    ) -> Result<(), AudioEncodeError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.pending.resize(self.frame_len, 0.0);
        let n = self
            .encoder
            .encode_float(&self.pending[..self.frame_len], &mut self.packet)
            .map_err(|e| AudioEncodeError::Encode(e.to_string()))?;
        on_packet(EncodedAudioChunk {
            bytes: self.packet[..n].to_vec(),
            frames: self.samples_per_channel,
            pts: self.pending_pts,
        });
        self.pending_pts += self.frame_duration;
        self.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second of a 440 Hz sine as interleaved stereo `f32` at 48 kHz.
    fn sine_48k_stereo(secs: f32) -> Vec<f32> {
        let n = (48_000.0 * secs) as usize;
        let mut v = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5;
            v.push(s);
            v.push(s);
        }
        v
    }

    #[test]
    fn default_params_are_48k_stereo_128k_20ms() {
        let p = AudioEncodeParams::default();
        assert_eq!(p.sample_rate, 48_000);
        assert_eq!(p.channels, 2);
        assert_eq!(p.bitrate_bps, 128_000);
        assert_eq!(p.frame_ms, 20);
    }

    #[test]
    fn rejects_bad_channel_count() {
        // `OpusAudioEncoder` isn't `Debug` (libopus's encoder isn't), so match the Result
        // rather than `unwrap_err`.
        let result = OpusAudioEncoder::new(AudioEncodeParams {
            channels: 3,
            ..Default::default()
        });
        assert!(matches!(result, Err(AudioEncodeError::Params(_))));
    }

    #[test]
    fn encodes_whole_frames_with_advancing_pts() {
        let mut enc = OpusAudioEncoder::new(AudioEncodeParams::default()).unwrap();
        // 20 ms frames at 48 kHz = 960 samples/channel. One second → 50 frames.
        assert_eq!(enc.samples_per_channel, 960);
        assert!(enc.pre_skip() > 0, "libopus reports a non-zero lookahead");

        let pcm = sine_48k_stereo(1.0);
        let mut packets = Vec::new();
        enc.push(&pcm, Duration::ZERO, |p| packets.push(p)).unwrap();

        assert_eq!(packets.len(), 50, "1 s / 20 ms = 50 packets");
        for p in &packets {
            assert_eq!(p.frames, 960);
            assert!(!p.bytes.is_empty());
        }
        // PTS advances by exactly 20 ms per packet, anchored at the first sample.
        assert_eq!(packets[0].pts, Duration::ZERO);
        assert_eq!(packets[1].pts, Duration::from_millis(20));
        assert_eq!(packets[49].pts, Duration::from_millis(980));
    }

    #[test]
    fn buffers_partial_frames_across_pushes() {
        let mut enc = OpusAudioEncoder::new(AudioEncodeParams::default()).unwrap();
        // Half a frame (480 stereo samples → 960 interleaved) → no packet yet.
        let half: Vec<f32> = vec![0.0; 960];
        let mut count = 0;
        enc.push(&half, Duration::ZERO, |_| count += 1).unwrap();
        assert_eq!(count, 0);
        // The second half completes one frame → exactly one packet, PTS at the first sample.
        let mut emitted = None;
        enc.push(&half, Duration::from_millis(10), |p| emitted = Some(p))
            .unwrap();
        let p = emitted.expect("one packet after the frame completes");
        assert_eq!(p.pts, Duration::ZERO);
        assert_eq!(p.frames, 960);
    }

    #[test]
    fn flush_emits_padded_final_frame() {
        let mut enc = OpusAudioEncoder::new(AudioEncodeParams::default()).unwrap();
        let partial: Vec<f32> = vec![0.1; 200]; // less than one frame
        enc.push(&partial, Duration::ZERO, |_| {}).unwrap();
        let mut packets = Vec::new();
        enc.flush(|p| packets.push(p)).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].frames, 960);
        // A second flush with nothing pending is a no-op.
        let mut more = 0;
        enc.flush(|_| more += 1).unwrap();
        assert_eq!(more, 0);
    }
}
