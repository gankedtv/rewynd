//! H.264 Annex-B → MP4 muxing with real PTS from capture timestamps (PLAN §4.3, §6.4).
//!
//! [`EncodedChunk::pts`] carries each frame's capture-relative timestamp, which we
//! write into the container as per-sample durations so players don't guess the
//! framerate. The muxer (docs/adr/0002) is the pure-Rust `mp4` crate; we convert the
//! encoder's Annex-B output to the AVCC (length-prefixed) form MP4 stores and pull the
//! SPS/PPS out of the first IDR to build the `avcC` config.

use std::path::Path;
use std::time::Duration;

use mp4::{
    AvcConfig, FourCC, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, OpusConfig, TrackConfig,
    TrackType,
};
use rewynd_buffer::{EncodedAudioChunk, EncodedChunk};
use thiserror::Error;

/// Microsecond timescale for the movie + video track: capture PTS deltas are written
/// exactly, with no fps rounding.
const TIMESCALE: u32 = 1_000_000;
/// H.264 `nal_unit_type` for a sequence parameter set.
const NAL_SPS: u8 = 7;
/// H.264 `nal_unit_type` for a picture parameter set.
const NAL_PPS: u8 = 8;

/// Errors from muxing.
#[derive(Debug, Error)]
pub enum MuxError {
    /// The chunk sequence did not start on a keyframe, so the file would not be playable.
    #[error("clip does not start on a keyframe")]
    NotKeyframeStart,
    /// The first keyframe carried no inline SPS/PPS, so no `avcC` config can be built.
    #[error("clip has no SPS/PPS parameter sets")]
    MissingParameterSets,
    /// Underlying I/O error while writing the container.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The `mp4` muxer rejected the data.
    #[error("mp4 muxing failed: {0}")]
    Mp4(#[from] mp4::Error),
}

/// Writes encoded chunks into a container file with correct timestamps.
pub trait Muxer {
    /// Mux `chunks` — which must begin on an IDR — into an MP4 at `path`.
    ///
    /// [`EncodedChunk::pts`] is capture-relative and a flushed clip is a mid-stream
    /// slice, so the muxer rebases timestamps against the first chunk's PTS: the
    /// written clip starts at PTS zero.
    fn write_mp4(&mut self, chunks: &[EncodedChunk], path: &Path) -> Result<(), MuxError>;
}

/// The Opus audio side of an A/V clip, paired with [`Mp4Muxer::write_mp4_with_audio`].
pub struct AudioTrack<'a> {
    /// Encoded Opus packets, oldest-first, on the same capture clock as the video chunks.
    pub chunks: &'a [EncodedAudioChunk],
    /// Channel count (1 or 2).
    pub channels: u8,
    /// Sample rate (the track's media timescale); Opus capture is 48 kHz.
    pub sample_rate: u32,
    /// Encoder lookahead in samples — the `dOps` PreSkip and the trim `elst` `media_time`.
    pub pre_skip: u16,
}

/// MP4 muxer (Annex-B → AVCC) for a single H.264 video track.
#[derive(Debug, Clone, Copy)]
pub struct Mp4Muxer {
    width: u16,
    height: u16,
    framerate: u32,
}

impl Mp4Muxer {
    /// Create a muxer for an H.264 stream of the given pixel dimensions and framerate.
    /// The dimensions feed the track's visual sample entry (the decoder reads the real
    /// size from the SPS); the framerate sets the duration of the final frame, which has
    /// no successor to measure against.
    #[must_use]
    pub fn new(width: u32, height: u32, framerate: u32) -> Self {
        Self {
            width: width.min(u32::from(u16::MAX)) as u16,
            height: height.min(u32::from(u16::MAX)) as u16,
            framerate: framerate.max(1),
        }
    }
}

impl Mp4Muxer {
    /// Mux a video clip plus a synced Opus audio track into an MP4 at `path`.
    ///
    /// Both tracks rebase against the same clip base (the first video chunk's PTS), so the
    /// audio keeps its real, small offset from the video start — preserving lip-sync. The
    /// audio carries an edit list: an empty edit for that start offset, then a trim edit at
    /// the encoder pre-skip so priming samples aren't presented.
    pub fn write_mp4_with_audio(
        &mut self,
        video: &[EncodedChunk],
        audio: &AudioTrack,
        path: &Path,
    ) -> Result<(), MuxError> {
        self.write(video, Some(audio), path)
    }

    fn write(
        &self,
        video: &[EncodedChunk],
        audio: Option<&AudioTrack>,
        path: &Path,
    ) -> Result<(), MuxError> {
        let first = video.first().ok_or(MuxError::NotKeyframeStart)?;
        if !first.is_keyframe {
            return Err(MuxError::NotKeyframeStart);
        }

        // gpu-video emits inline SPS/PPS before every IDR, so the first chunk carries them.
        let sps = find_nal(&first.bytes, NAL_SPS).ok_or(MuxError::MissingParameterSets)?;
        let pps = find_nal(&first.bytes, NAL_PPS).ok_or(MuxError::MissingParameterSets)?;

        // Only advertise an audio track / Opus brand when there's actually audio.
        let has_audio = audio.is_some_and(|a| !a.chunks.is_empty());

        let mut compatible_brands = vec![
            FourCC::from(*b"isom"),
            FourCC::from(*b"iso2"),
            FourCC::from(*b"avc1"),
            FourCC::from(*b"mp41"),
        ];
        if has_audio {
            compatible_brands.push(FourCC::from(*b"Opus"));
        }

        let file = std::fs::File::create(path)?;
        let mut writer = Mp4Writer::write_start(
            file,
            &Mp4Config {
                major_brand: FourCC::from(*b"isom"),
                minor_version: 512,
                compatible_brands,
                timescale: TIMESCALE,
            },
        )?;

        writer.add_track(&TrackConfig {
            track_type: TrackType::Video,
            timescale: TIMESCALE,
            language: String::from("und"),
            media_conf: MediaConfig::AvcConfig(AvcConfig {
                width: self.width,
                height: self.height,
                seq_param_set: sps.to_vec(),
                pic_param_set: pps.to_vec(),
            }),
        })?;

        let base = first.pts;
        for (i, chunk) in video.iter().enumerate() {
            let start = chunk.pts.saturating_sub(base);
            // Duration is the gap to the next frame; the last frame has no successor, so
            // reuse the previous gap, or fall back to one frame period for a single-frame
            // clip (so it stays visible rather than collapsing to ~0s).
            let duration = match video.get(i + 1) {
                Some(next) => next.pts.saturating_sub(chunk.pts),
                None if i > 0 => chunk.pts.saturating_sub(video[i - 1].pts),
                None => Duration::from_nanos(1_000_000_000 / u64::from(self.framerate)),
            };
            writer.write_sample(
                1,
                &Mp4Sample {
                    start_time: start.as_micros() as u64,
                    duration: duration.as_micros().min(u128::from(u32::MAX)) as u32,
                    rendering_offset: 0,
                    is_sync: chunk.is_keyframe,
                    bytes: annexb_to_avcc(&chunk.bytes).into(),
                },
            )?;
        }

        if has_audio {
            let audio = audio.expect("has_audio implies Some");
            write_audio_track(&mut writer, audio, base)?;
        }

        writer.write_end()?;
        Ok(())
    }
}

impl Muxer for Mp4Muxer {
    fn write_mp4(&mut self, chunks: &[EncodedChunk], path: &Path) -> Result<(), MuxError> {
        self.write(chunks, None, path)
    }
}

/// Add the Opus audio track (track 2) and write its packets, synced to the clip `base`
/// (the video's first PTS). Audio packets are contiguous in the track's 48 kHz timeline;
/// an edit list carries the real start offset (an empty edit) and the encoder pre-skip
/// trim, so the audio presents at the right moment with no priming click.
fn write_audio_track<W: std::io::Write + std::io::Seek>(
    writer: &mut Mp4Writer<W>,
    audio: &AudioTrack,
    base: Duration,
) -> Result<(), MuxError> {
    writer.add_track(&TrackConfig {
        track_type: TrackType::Audio,
        timescale: audio.sample_rate,
        language: String::from("und"),
        media_conf: MediaConfig::OpusConfig(OpusConfig {
            channels: audio.channels,
            sample_rate: audio.sample_rate,
            pre_skip: audio.pre_skip,
        }),
    })?;

    // Track 2 is the second `add_track`. Packets are written back-to-back: each sample's
    // duration is its real Opus frame length (so the audio sample clock stays exact), and the
    // cumulative durations form the timeline; `start_time` is informational (the writer uses
    // only `duration`). This assumes contiguous capture — a mid-clip gap (a sink that
    // suspends mid-session) would collapse, presenting later audio early. Continuous monitor
    // capture doesn't produce such gaps; reconstructing one (silence fill / multi-edit elst)
    // is a future refinement.
    let mut cumulative: u64 = 0;
    for chunk in audio.chunks {
        writer.write_sample(
            2,
            &Mp4Sample {
                start_time: cumulative,
                duration: chunk.frames,
                rendering_offset: 0,
                is_sync: true,
                bytes: chunk.bytes.clone().into(),
            },
        )?;
        cumulative += u64::from(chunk.frames);
    }

    // Edit list (only when it does something): an empty edit for the audio's real offset
    // from the clip base, then a trim edit at the encoder pre-skip. `segment_duration` is in
    // the movie timescale, `media_time` in the audio (48 kHz) timescale; `media_time = -1`
    // marks the empty edit. For a mid-stream clip `pre_skip` is 0 (the startup priming isn't
    // present at the clip's first packet), so the trim is usually identity and we skip the
    // whole list unless there's a real start offset to honour.
    let offset = audio.chunks[0].pts.saturating_sub(base);
    let offset_movie = offset.as_micros().min(u128::from(u64::MAX)) as u64;
    let pre_skip = u64::from(audio.pre_skip).min(cumulative);

    if offset_movie > 0 || pre_skip > 0 {
        let trim_movie =
            (cumulative - pre_skip) * u64::from(TIMESCALE) / u64::from(audio.sample_rate.max(1));
        let mut edits: Vec<(u64, i64)> = Vec::new();
        if offset_movie > 0 {
            edits.push((offset_movie, -1));
        }
        edits.push((trim_movie, pre_skip as i64));
        writer.set_track_edit_list(2, &edits)?;
    }

    Ok(())
}

/// Split an Annex-B buffer into NAL unit payloads, dropping the `00 00 01` start codes
/// (and the extra leading `00` of any four-byte `00 00 00 01` variant).
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut codes = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            codes.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }

    let mut nals = Vec::with_capacity(codes.len());
    for (k, &code) in codes.iter().enumerate() {
        let start = code + 3;
        let mut end = codes.get(k + 1).copied().unwrap_or(data.len());
        // Between NAL units the byte stream carries only zero padding — the
        // trailing/leading zero bytes around the next start code (so a four-byte
        // `00 00 00 01` is just `00 00 01` with extra leading zeros). A NAL's last RBSP
        // byte is non-zero (the rbsp stop bit), so every trailing zero here is padding.
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if start < end {
            nals.push(&data[start..end]);
        }
    }
    nals
}

/// The first NAL unit of the given `nal_unit_type`, if present.
fn find_nal(data: &[u8], nal_type: u8) -> Option<&[u8]> {
    nal_units(data)
        .into_iter()
        .find(|nal| nal.first().is_some_and(|&header| header & 0x1f == nal_type))
}

/// Convert Annex-B (start-code-delimited) NAL units into AVCC form: each NAL prefixed
/// by its four-byte big-endian length, which is how MP4 stores H.264 samples.
fn annexb_to_avcc(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for nal in nal_units(data) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an Annex-B buffer from NAL payloads, prefixing each with a four-byte start code.
    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        out
    }

    fn chunk(bytes: Vec<u8>, is_keyframe: bool, pts_us: u64) -> EncodedChunk {
        EncodedChunk {
            bytes,
            is_keyframe,
            pts: Duration::from_micros(pts_us),
        }
    }

    /// A unique temp-file path that removes itself on drop, so parallel runs don't
    /// collide and a failed assertion doesn't leave a stale file behind.
    struct TempMp4(std::path::PathBuf);

    impl TempMp4 {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!("rewynd-mux-{}-{n}.mp4", std::process::id())))
        }
    }

    impl Drop for TempMp4 {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        // 4-byte start code, a NAL, then a 3-byte start code and another NAL.
        let mut data = vec![0, 0, 0, 1, 0xAA, 0xBB];
        data.extend_from_slice(&[0, 0, 1, 0xCC]);
        let nals = nal_units(&data);
        assert_eq!(nals, vec![&[0xAA, 0xBB][..], &[0xCC][..]]);
    }

    #[test]
    fn trims_extra_leading_zeros_and_trailing_padding() {
        // NAL [67 88], then a start code with two extra leading zeros, then NAL [68].
        let mut data = vec![0, 0, 0, 1, 0x67, 0x88];
        data.extend_from_slice(&[0, 0, 0, 0, 1, 0x68]);
        assert_eq!(nal_units(&data), vec![&[0x67, 0x88][..], &[0x68][..]]);

        // Trailing zero padding at end of stream is not part of the NAL.
        let mut padded = annexb(&[&[0x67, 0x88]]);
        padded.extend_from_slice(&[0, 0]);
        assert_eq!(nal_units(&padded), vec![&[0x67, 0x88][..]]);
    }

    #[test]
    fn annexb_to_avcc_length_prefixes_each_nal() {
        let data = annexb(&[&[0x65, 0x11], &[0x41]]);
        assert_eq!(
            annexb_to_avcc(&data),
            vec![0, 0, 0, 2, 0x65, 0x11, 0, 0, 0, 1, 0x41]
        );
    }

    #[test]
    fn find_nal_locates_sps_and_pps() {
        // NALs end on a non-zero rbsp stop bit, so no trailing-zero trimming applies here.
        let sps = [0x67, 0x42, 0x1f];
        let pps = [0x68, 0xCE];
        let idr = [0x65, 0x88];
        let data = annexb(&[&sps, &pps, &idr]);
        assert_eq!(find_nal(&data, NAL_SPS), Some(&sps[..]));
        assert_eq!(find_nal(&data, NAL_PPS), Some(&pps[..]));
        assert_eq!(find_nal(&data, 1), None);
    }

    #[test]
    fn rejects_non_keyframe_start_and_empty() {
        let mut muxer = Mp4Muxer::new(1920, 1080, 60);
        let chunks = [chunk(annexb(&[&[0x41]]), false, 0)];
        assert!(matches!(
            muxer
                .write_mp4(&chunks, Path::new("/dev/null"))
                .unwrap_err(),
            MuxError::NotKeyframeStart
        ));
        assert!(matches!(
            muxer.write_mp4(&[], Path::new("/dev/null")).unwrap_err(),
            MuxError::NotKeyframeStart
        ));
    }

    #[test]
    fn keyframe_without_parameter_sets_errors() {
        let mut muxer = Mp4Muxer::new(1920, 1080, 60);
        let chunks = [chunk(annexb(&[&[0x65, 0x88]]), true, 0)];
        assert!(matches!(
            muxer
                .write_mp4(&chunks, Path::new("/dev/null"))
                .unwrap_err(),
            MuxError::MissingParameterSets
        ));
    }

    #[test]
    fn writes_a_readable_mp4_with_sps_pps_and_sync_sample() {
        let sps = [0x67, 0x42, 0x00, 0x1f];
        let pps = [0x68, 0xCE, 0x3c, 0x80];
        let key = annexb(&[&sps, &pps, &[0x65, 0x88, 0x84]]);
        let inter = annexb(&[&[0x41, 0x9a, 0x00]]);
        let chunks = [
            chunk(key, true, 1000),
            chunk(inter.clone(), false, 17_667),
            chunk(inter, false, 34_334),
        ];

        let out = TempMp4::new();
        Mp4Muxer::new(1920, 1080, 60)
            .write_mp4(&chunks, &out.0)
            .expect("write_mp4");

        let file = std::fs::File::open(&out.0).unwrap();
        let size = file.metadata().unwrap().len();
        let reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size).unwrap();

        assert_eq!(reader.tracks().len(), 1);
        let track = reader.tracks().get(&1).expect("track 1");
        assert_eq!(track.track_type().unwrap(), TrackType::Video);
        assert_eq!(track.width(), 1920);
        assert_eq!(track.height(), 1080);
        assert_eq!(track.sequence_parameter_set().unwrap(), &sps[..]);
        assert_eq!(track.picture_parameter_set().unwrap(), &pps[..]);
        assert_eq!(track.sample_count(), 3);
    }

    #[test]
    fn single_frame_clip_uses_one_frame_duration() {
        let sps = [0x67, 0x42, 0x00, 0x1f];
        let pps = [0x68, 0xCE, 0x3c, 0x80];
        let key = annexb(&[&sps, &pps, &[0x65, 0x88, 0x84]]);
        let out = TempMp4::new();
        Mp4Muxer::new(1920, 1080, 60)
            .write_mp4(&[chunk(key, true, 0)], &out.0)
            .expect("write_mp4");

        let file = std::fs::File::open(&out.0).unwrap();
        let size = file.metadata().unwrap().len();
        let mut reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size).unwrap();
        let sample = reader.read_sample(1, 1).unwrap().expect("one sample");
        // One frame at 60fps on a microsecond timescale ≈ 16_666 ticks, not ~0.
        assert_eq!(sample.duration, 1_000_000 / 60);
    }

    fn audio_chunk(pts_us: u64) -> EncodedAudioChunk {
        EncodedAudioChunk {
            // The container does not decode Opus, so arbitrary bytes round-trip fine.
            bytes: vec![0xFC, 0xFF, 0xFE],
            frames: 960,
            pts: Duration::from_micros(pts_us),
        }
    }

    #[test]
    fn writes_a_two_track_av_mp4_with_opus() {
        let sps = [0x67, 0x42, 0x00, 0x1f];
        let pps = [0x68, 0xCE, 0x3c, 0x80];
        let key = annexb(&[&sps, &pps, &[0x65, 0x88, 0x84]]);
        let inter = annexb(&[&[0x41, 0x9a, 0x00]]);
        let video = [
            chunk(key, true, 1_000),
            chunk(inter.clone(), false, 17_667),
            chunk(inter, false, 34_334),
        ];
        // Audio starts ~9 ms after the clip base (1000 µs) — a real, preserved offset.
        let audio_chunks = [
            audio_chunk(10_000),
            audio_chunk(30_000),
            audio_chunk(50_000),
        ];
        let audio = AudioTrack {
            chunks: &audio_chunks,
            channels: 2,
            sample_rate: 48_000,
            pre_skip: 312,
        };

        let out = TempMp4::new();
        Mp4Muxer::new(1920, 1080, 60)
            .write_mp4_with_audio(&video, &audio, &out.0)
            .expect("write_mp4_with_audio");

        let file = std::fs::File::open(&out.0).unwrap();
        let size = file.metadata().unwrap().len();
        let reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size).unwrap();

        assert_eq!(reader.tracks().len(), 2);
        let video_track = reader.tracks().get(&1).expect("track 1");
        assert_eq!(video_track.track_type().unwrap(), TrackType::Video);
        assert_eq!(video_track.sample_count(), 3);

        let audio_track = reader.tracks().get(&2).expect("track 2");
        assert_eq!(audio_track.track_type().unwrap(), TrackType::Audio);
        assert_eq!(audio_track.media_type().unwrap(), mp4::MediaType::OPUS);
        assert_eq!(audio_track.sample_count(), 3);

        // Byte-exact dOps guard, run in CI (the vendored crate's own tests aren't part of
        // `cargo test --workspace`). 48 kHz / stereo / pre_skip=312, per RFC 7845 §5.1.
        let bytes = std::fs::read(&out.0).unwrap();
        let pos = bytes
            .windows(4)
            .position(|w| w == b"dOps")
            .expect("dOps box present in the written file");
        let dops = &bytes[pos - 4..pos - 4 + 19];
        assert_eq!(
            dops,
            &[
                0x00, 0x00, 0x00, 0x13, // box size = 19
                b'd', b'O', b'p', b's', // 'dOps'
                0x00, // version
                0x02, // output channel count
                0x01, 0x38, // pre_skip = 312
                0x00, 0x00, 0xbb, 0x80, // input sample rate = 48000
                0x00, 0x00, // output gain
                0x00, // channel mapping family
            ]
        );
    }

    #[test]
    fn av_writer_with_empty_audio_is_video_only() {
        let sps = [0x67, 0x42, 0x00, 0x1f];
        let pps = [0x68, 0xCE, 0x3c, 0x80];
        let key = annexb(&[&sps, &pps, &[0x65, 0x88, 0x84]]);
        let video = [chunk(key, true, 0)];
        let audio = AudioTrack {
            chunks: &[],
            channels: 2,
            sample_rate: 48_000,
            pre_skip: 312,
        };

        let out = TempMp4::new();
        Mp4Muxer::new(1920, 1080, 60)
            .write_mp4_with_audio(&video, &audio, &out.0)
            .expect("write_mp4_with_audio");

        let file = std::fs::File::open(&out.0).unwrap();
        let size = file.metadata().unwrap().len();
        let reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size).unwrap();
        assert_eq!(
            reader.tracks().len(),
            1,
            "no audio track when chunks are empty"
        );
    }

    #[test]
    fn error_variants_display() {
        assert_eq!(
            MuxError::NotKeyframeStart.to_string(),
            "clip does not start on a keyframe"
        );
        assert_eq!(
            MuxError::MissingParameterSets.to_string(),
            "clip has no SPS/PPS parameter sets"
        );
        let io = MuxError::from(std::io::Error::other("x"));
        assert!(matches!(io, MuxError::Io(_)));
        assert_eq!(io.to_string(), "x");
    }
}
