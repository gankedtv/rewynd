//! The read side: open a saved MP4 and pull out what a preview needs — the clip's
//! dimensions/duration and the first keyframe converted back to Annex-B (the inverse of the
//! write side), so a CPU decoder can render a thumbnail without a full demuxer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

/// Errors from reading a clip back.
#[derive(Debug, Error)]
pub enum ReadError {
    #[error("could not read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The file is not an MP4 this reader understands (truncated, corrupt, foreign).
    #[error("not a readable MP4: {0}")]
    Mp4(#[from] mp4::Error),
    #[error("the file has no H.264 video track")]
    NoVideoTrack,
    /// No sync sample to decode (an empty or delta-only track).
    #[error("the video track has no keyframe")]
    NoKeyframe,
    /// An AVCC sample whose length prefixes overrun the sample (or an invalid prefix size).
    #[error("the keyframe sample is malformed")]
    MalformedSample,
    /// The reader panicked on inconsistent metadata (see [`catch_reader_panics`]).
    #[error("the MP4 metadata is corrupt")]
    Corrupt,
}

/// What a library card shows about a clip without decoding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipSummary {
    pub width: u32,
    pub height: u32,
    pub duration: Duration,
}

type Reader = mp4::Mp4Reader<std::io::BufReader<std::fs::File>>;

fn open(path: &Path) -> Result<Reader, ReadError> {
    let io_err = |source| ReadError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = std::fs::File::open(path).map_err(io_err)?;
    let size = file.metadata().map_err(io_err)?.len();
    Ok(mp4::Mp4Reader::read_header(
        std::io::BufReader::new(file),
        size,
    )?)
}

/// The id of the first H.264 video track.
fn video_track(reader: &Reader) -> Result<u32, ReadError> {
    reader
        .tracks()
        .iter()
        .filter(|(_, t)| {
            t.track_type().is_ok_and(|k| k == mp4::TrackType::Video)
                && t.media_type().is_ok_and(|m| m == mp4::MediaType::H264)
        })
        .map(|(id, _)| *id)
        .min()
        .ok_or(ReadError::NoVideoTrack)
}

/// Run `body`, mapping panics to [`ReadError::Corrupt`]: the vendored reader `unwrap`s
/// internally on inconsistent metadata (`read_sample` on a truncated `stts`/`stsz`) and
/// divides by the raw `mdhd` timescale (`duration()` panics when it is zero), so a corrupt
/// or hostile file must surface as an error, never a crash. `AssertUnwindSafe` is fine: the
/// reader lives and dies inside the closure, so no broken state outlives an unwind.
fn catch_reader_panics<T>(body: impl FnOnce() -> Result<T, ReadError>) -> Result<T, ReadError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).unwrap_or(Err(ReadError::Corrupt))
}

/// The dimensions and duration of the clip at `path` (from the video track's headers; no
/// sample data is read).
pub fn clip_summary(path: &Path) -> Result<ClipSummary, ReadError> {
    catch_reader_panics(|| {
        let reader = open(path)?;
        let track_id = video_track(&reader)?;
        Ok(summary_of(&reader, track_id))
    })
}

/// The first keyframe of the clip at `path` as a self-contained Annex-B buffer: the `avcC`
/// SPS/PPS, then the sync sample's NAL units, all start-code delimited — exactly what a raw
/// H.264 decoder wants for a single-frame decode.
pub fn first_keyframe_annexb(path: &Path) -> Result<Vec<u8>, ReadError> {
    catch_reader_panics(|| {
        let mut reader = open(path)?;
        let track_id = video_track(&reader)?;
        keyframe_of(&mut reader, track_id)
    })
}

/// Everything a preview needs from a single open + parse: the summary and a representative
/// keyframe, at half the file reads. The keyframe is taken from around [`PREVIEW_POSITION`] of
/// the clip, so previews skip the intro (a resume IDR's first frames are often a loading screen,
/// and pre-gating clips can open on the desktop); see [`clip_preview_at`].
pub fn clip_preview(path: &Path) -> Result<(ClipSummary, Vec<u8>), ReadError> {
    clip_preview_at(path, PREVIEW_POSITION)
}

/// Fraction of the clip duration to prefer for a thumbnail. Past the intro, before anything a
/// mid-clip resume would cut short.
const PREVIEW_POSITION: f32 = 0.4;

/// Like [`clip_preview`] but with an explicit position hint (`0.0..=1.0`) into the clip. The
/// keyframe returned is the sync sample nearest `position` of the duration, falling back to the
/// first keyframe when the track declares no sync-sample table.
pub fn clip_preview_at(path: &Path, position: f32) -> Result<(ClipSummary, Vec<u8>), ReadError> {
    catch_reader_panics(|| {
        let mut reader = open(path)?;
        let track_id = video_track(&reader)?;
        let summary = summary_of(&reader, track_id);
        let sample_id = keyframe_sample_near(&reader, track_id, position);
        let keyframe = keyframe_at(&mut reader, track_id, sample_id)?;
        Ok((summary, keyframe))
    })
}

fn summary_of(reader: &Reader, track_id: u32) -> ClipSummary {
    let track = &reader.tracks()[&track_id];
    ClipSummary {
        width: u32::from(track.width()),
        height: u32::from(track.height()),
        duration: track.duration(),
    }
}

fn keyframe_of(reader: &mut Reader, track_id: u32) -> Result<Vec<u8>, ReadError> {
    let track = &reader.tracks()[&track_id];
    let (sps, pps) = (
        track.sequence_parameter_set()?.to_vec(),
        track.picture_parameter_set()?.to_vec(),
    );
    let prefix_size = nal_length_size(track)?;
    let sample_count = track.sample_count();

    let mut out = Vec::new();
    append_nal(&mut out, &sps);
    append_nal(&mut out, &pps);
    for sample_id in 1..=sample_count {
        let Some(sample) = reader.read_sample(track_id, sample_id)? else {
            break;
        };
        if sample.is_sync {
            avcc_to_annexb(&sample.bytes, prefix_size, &mut out)?;
            return Ok(out);
        }
    }
    Err(ReadError::NoKeyframe)
}

/// The sync-sample id whose decode time sits nearest `position` (`0.0..=1.0`) of the clip's
/// duration. Metadata only, no sample bytes read. Falls back to sample 1 when the track has no
/// sync table (then every sample is nominally a keyframe, but only the first is self-contained).
fn keyframe_sample_near(reader: &Reader, track_id: u32, position: f32) -> u32 {
    let track = &reader.tracks()[&track_id];
    let Some(syncs) = track.sync_sample_ids() else {
        return 1;
    };
    let Some(&first) = syncs.first() else {
        return 1;
    };
    let position = f64::from(position.clamp(0.0, 1.0));
    let timescale = f64::from(track.timescale().max(1));
    let target = track.duration().as_secs_f64() * position;
    let mut best = first;
    let mut best_dist = f64::INFINITY;
    for &id in syncs {
        let Ok((start, _)) = track.sample_time(id) else {
            continue;
        };
        let dist = (start as f64 / timescale - target).abs();
        if dist < best_dist {
            best = id;
            best_dist = dist;
        }
    }
    best
}

/// The keyframe at `sample_id` as a self-contained Annex-B buffer (`avcC` SPS/PPS, then the
/// sample's NALs). The caller picks a sync sample; a non-sync id would not decode alone, so it
/// is reported as [`ReadError::NoKeyframe`].
fn keyframe_at(reader: &mut Reader, track_id: u32, sample_id: u32) -> Result<Vec<u8>, ReadError> {
    let track = &reader.tracks()[&track_id];
    let (sps, pps) = (
        track.sequence_parameter_set()?.to_vec(),
        track.picture_parameter_set()?.to_vec(),
    );
    let prefix_size = nal_length_size(track)?;
    let mut out = Vec::new();
    append_nal(&mut out, &sps);
    append_nal(&mut out, &pps);
    match reader.read_sample(track_id, sample_id)? {
        Some(sample) if sample.is_sync => {
            avcc_to_annexb(&sample.bytes, prefix_size, &mut out)?;
            Ok(out)
        }
        _ => Err(ReadError::NoKeyframe),
    }
}

/// The sample NAL length-prefix size the track's `avcC` declares: 1, 2, or 4 bytes (our write
/// side always uses 4; 3 is invalid per ISO 14496-15 and rejected).
fn nal_length_size(track: &mp4::Mp4Track) -> Result<usize, ReadError> {
    let avc1 = track
        .trak
        .mdia
        .minf
        .stbl
        .stsd
        .avc1
        .as_ref()
        .ok_or(ReadError::NoVideoTrack)?;
    // Only the low two bits are meaningful (the write side pads the reserved bits with 1s).
    match usize::from(avc1.avcc.length_size_minus_one & 0x3) + 1 {
        3 => Err(ReadError::MalformedSample),
        size => Ok(size),
    }
}

fn append_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

/// Convert one AVCC sample (big-endian length prefixes of `prefix_size` bytes, per the
/// track's `avcC`) into Annex-B, appending to `out`.
fn avcc_to_annexb(sample: &[u8], prefix_size: usize, out: &mut Vec<u8>) -> Result<(), ReadError> {
    let mut rest = sample;
    while !rest.is_empty() {
        let (prefix, tail) = rest
            .split_at_checked(prefix_size)
            .ok_or(ReadError::MalformedSample)?;
        let len = prefix
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        let (nal, tail) = tail
            .split_at_checked(len)
            .ok_or(ReadError::MalformedSample)?;
        if nal.is_empty() {
            return Err(ReadError::MalformedSample);
        }
        append_nal(out, nal);
        rest = tail;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mp4Muxer;
    use rewynd_buffer::EncodedChunk;

    const SPS: [u8; 4] = [0x67, 0x42, 0x00, 0x1f];
    const PPS: [u8; 4] = [0x68, 0xCE, 0x3c, 0x80];
    const IDR: [u8; 3] = [0x65, 0x88, 0x84];
    const INTER: [u8; 3] = [0x41, 0x9a, 0x00];

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
            bytes: bytes.into(),
            is_keyframe,
            pts: Duration::from_micros(pts_us),
        }
    }

    /// A unique temp-file path that removes itself on drop.
    struct TempMp4(PathBuf);

    impl TempMp4 {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!("rewynd-read-{}-{n}.mp4", std::process::id())))
        }
    }

    impl Drop for TempMp4 {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Whether `haystack` contains the byte sequence `needle`.
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// A clip with keyframes at samples 1, 5, 9 (0%, 40%, 80% of a ten-frame span), each IDR
    /// tagged with its frame index so the selected keyframe is identifiable.
    fn multi_keyframe_clip() -> TempMp4 {
        let chunks: Vec<_> = (0..10u64)
            .map(|i| {
                let tag = i as u8;
                if i % 4 == 0 {
                    chunk(annexb(&[&SPS, &PPS, &[0x65, 0x88, tag]]), true, i * 16_667)
                } else {
                    chunk(annexb(&[&[0x41, 0x9a, tag]]), false, i * 16_667)
                }
            })
            .collect();
        let out = TempMp4::new();
        Mp4Muxer::new(1920, 1080, 60)
            .write_mp4(&chunks, &out.0)
            .expect("write_mp4");
        out
    }

    #[test]
    fn keyframe_sample_near_picks_the_nearest_sync_sample() {
        let clip = multi_keyframe_clip();
        let reader = open(&clip.0).expect("open");
        let track_id = video_track(&reader).expect("track");
        assert_eq!(keyframe_sample_near(&reader, track_id, 0.0), 1);
        assert_eq!(keyframe_sample_near(&reader, track_id, 0.4), 5);
        assert_eq!(keyframe_sample_near(&reader, track_id, 1.0), 9);
        // Out-of-range positions clamp rather than misbehave.
        assert_eq!(keyframe_sample_near(&reader, track_id, -1.0), 1);
        assert_eq!(keyframe_sample_near(&reader, track_id, 5.0), 9);
    }

    #[test]
    fn preview_at_returns_a_mid_clip_keyframe() {
        let clip = multi_keyframe_clip();
        let (_summary, frame) = clip_preview_at(&clip.0, 0.4).expect("preview");
        assert!(contains(&frame, &[0x65, 0x88, 4]), "the 40% keyframe");
        assert!(
            !contains(&frame, &[0x65, 0x88, 0]),
            "not the first keyframe"
        );
        // The default preview position lands on the mid keyframe, not the very first frame.
        let (_s, dflt) = clip_preview(&clip.0).expect("preview");
        assert!(contains(&dflt, &[0x65, 0x88, 4]));
    }

    #[test]
    fn keyframe_at_rejects_a_non_sync_sample() {
        let clip = multi_keyframe_clip();
        let mut reader = open(&clip.0).expect("open");
        let track_id = video_track(&reader).expect("track");
        // Sample 2 is a delta frame; it cannot stand alone as a keyframe.
        assert!(matches!(
            keyframe_at(&mut reader, track_id, 2),
            Err(ReadError::NoKeyframe)
        ));
    }

    #[test]
    fn preview_falls_back_to_first_frame_without_a_sync_table() {
        // A track with no samples declares no stss, so selection falls back to sample 1.
        let out = raw_video_mp4(&[]);
        let reader = open(&out.0).expect("open");
        let track_id = video_track(&reader).expect("track");
        assert!(reader.tracks()[&track_id].sync_sample_ids().is_none());
        assert_eq!(keyframe_sample_near(&reader, track_id, 0.4), 1);
    }

    /// Mux a tiny keyframe + delta clip and return its path holder.
    fn tiny_clip() -> TempMp4 {
        let key = annexb(&[&SPS, &PPS, &IDR]);
        let inter = annexb(&[&INTER]);
        let chunks = [
            chunk(key, true, 0),
            chunk(inter.clone(), false, 16_667),
            chunk(inter, false, 33_334),
        ];
        let out = TempMp4::new();
        Mp4Muxer::new(1920, 1080, 60)
            .write_mp4(&chunks, &out.0)
            .expect("write_mp4");
        out
    }

    #[test]
    fn summary_round_trips_dimensions_and_duration() {
        let clip = tiny_clip();
        let summary = clip_summary(&clip.0).expect("summary");
        assert_eq!(summary.width, 1920);
        assert_eq!(summary.height, 1080);
        // Three frames of 16_667 µs each (the last reuses the previous gap).
        assert_eq!(summary.duration, Duration::from_micros(50_001));
    }

    #[test]
    fn first_keyframe_round_trips_to_annexb() {
        let clip = tiny_clip();
        let frame = first_keyframe_annexb(&clip.0).expect("keyframe");
        // avcC SPS/PPS first, then the sample's own NALs (which carry them inline too:
        // the write side stores gpu-video's in-band parameter sets verbatim).
        let mut expected = annexb(&[&SPS, &PPS]);
        expected.extend_from_slice(&annexb(&[&SPS, &PPS, &IDR]));
        assert_eq!(frame, expected);
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let path = Path::new("/nonexistent/clip.mp4");
        assert!(matches!(
            clip_summary(path).unwrap_err(),
            ReadError::Io { .. }
        ));
        assert!(matches!(
            first_keyframe_annexb(path).unwrap_err(),
            ReadError::Io { .. }
        ));
    }

    #[test]
    fn garbage_file_is_an_mp4_error() {
        let out = TempMp4::new();
        std::fs::write(&out.0, b"this is not an mp4 at all").expect("write");
        assert!(matches!(
            clip_summary(&out.0).unwrap_err(),
            ReadError::Mp4(_)
        ));
        assert!(matches!(
            first_keyframe_annexb(&out.0).unwrap_err(),
            ReadError::Mp4(_)
        ));
    }

    /// An MP4 with only an Opus track has no video to preview.
    #[test]
    fn audio_only_file_has_no_video_track() {
        let out = TempMp4::new();
        let file = std::fs::File::create(&out.0).unwrap();
        let mut writer = mp4::Mp4Writer::write_start(
            file,
            &mp4::Mp4Config {
                major_brand: mp4::FourCC::from(*b"isom"),
                minor_version: 512,
                compatible_brands: vec![mp4::FourCC::from(*b"isom")],
                timescale: 1_000_000,
            },
        )
        .unwrap();
        writer
            .add_track(&mp4::TrackConfig {
                track_type: mp4::TrackType::Audio,
                timescale: 48_000,
                language: String::from("und"),
                media_conf: mp4::MediaConfig::OpusConfig(mp4::OpusConfig {
                    channels: 2,
                    sample_rate: 48_000,
                    pre_skip: 0,
                }),
            })
            .unwrap();
        writer
            .write_sample(
                1,
                &mp4::Mp4Sample {
                    start_time: 0,
                    duration: 960,
                    rendering_offset: 0,
                    is_sync: true,
                    bytes: vec![0xFC, 0xFF, 0xFE].into(),
                },
            )
            .unwrap();
        writer.write_end().unwrap();

        assert!(matches!(
            clip_summary(&out.0).unwrap_err(),
            ReadError::NoVideoTrack
        ));
        assert!(matches!(
            first_keyframe_annexb(&out.0).unwrap_err(),
            ReadError::NoVideoTrack
        ));
    }

    /// Write a raw video track (bypassing our muxer, which refuses non-keyframe starts) with
    /// the given samples as `(is_sync, avcc bytes)`.
    fn raw_video_mp4(samples: &[(bool, Vec<u8>)]) -> TempMp4 {
        let out = TempMp4::new();
        let file = std::fs::File::create(&out.0).unwrap();
        let mut writer = mp4::Mp4Writer::write_start(
            file,
            &mp4::Mp4Config {
                major_brand: mp4::FourCC::from(*b"isom"),
                minor_version: 512,
                compatible_brands: vec![mp4::FourCC::from(*b"isom")],
                timescale: 1_000_000,
            },
        )
        .unwrap();
        writer
            .add_track(&mp4::TrackConfig {
                track_type: mp4::TrackType::Video,
                timescale: 1_000_000,
                language: String::from("und"),
                media_conf: mp4::MediaConfig::AvcConfig(mp4::AvcConfig {
                    width: 640,
                    height: 360,
                    seq_param_set: SPS.to_vec(),
                    pic_param_set: PPS.to_vec(),
                }),
            })
            .unwrap();
        for (i, (is_sync, bytes)) in samples.iter().enumerate() {
            writer
                .write_sample(
                    1,
                    &mp4::Mp4Sample {
                        start_time: i as u64 * 16_667,
                        duration: 16_667,
                        rendering_offset: 0,
                        is_sync: *is_sync,
                        bytes: bytes.clone().into(),
                    },
                )
                .unwrap();
        }
        writer.write_end().unwrap();
        out
    }

    /// A clip that starts on a delta: the scan must skip to the first sync sample.
    #[test]
    fn keyframe_scan_skips_leading_delta_samples() {
        let delta = vec![0, 0, 0, 1, 0x41];
        let key = vec![0, 0, 0, 3, 0x65, 0x88, 0x84];
        let out = raw_video_mp4(&[(false, delta), (true, key)]);

        let frame = first_keyframe_annexb(&out.0).expect("keyframe");
        let mut expected = annexb(&[&SPS, &PPS]);
        expected.extend_from_slice(&annexb(&[&[0x65, 0x88, 0x84]]));
        assert_eq!(frame, expected);
    }

    /// A video track with no samples at all has no keyframe to hand out; the summary side
    /// still works (headers don't need one).
    #[test]
    fn empty_video_track_has_no_keyframe() {
        let out = raw_video_mp4(&[]);
        assert!(matches!(
            first_keyframe_annexb(&out.0).unwrap_err(),
            ReadError::NoKeyframe
        ));
        let summary = clip_summary(&out.0).expect("summary");
        assert_eq!((summary.width, summary.height), (640, 360));
    }

    #[test]
    fn malformed_avcc_lengths_are_rejected() {
        let mut out = Vec::new();
        // Length prefix runs past the sample.
        assert!(matches!(
            avcc_to_annexb(&[0, 0, 0, 9, 0x65], 4, &mut out),
            Err(ReadError::MalformedSample)
        ));
        // Truncated length prefix.
        assert!(matches!(
            avcc_to_annexb(&[0, 0, 1], 4, &mut out),
            Err(ReadError::MalformedSample)
        ));
        // A zero-length NAL unit.
        assert!(matches!(
            avcc_to_annexb(&[0, 0, 0, 0], 4, &mut out),
            Err(ReadError::MalformedSample)
        ));
        // Well-formed input converts and appends.
        let mut ok = Vec::new();
        avcc_to_annexb(&[0, 0, 0, 2, 0x65, 0x11, 0, 0, 0, 1, 0x41], 4, &mut ok).expect("converts");
        assert_eq!(ok, vec![0, 0, 0, 1, 0x65, 0x11, 0, 0, 0, 1, 0x41]);
    }

    /// The avcC's declared prefix size drives the parse: the same NALs under 1- and 2-byte
    /// prefixes convert identically (our muxer only ever writes 4, so this is unit-level).
    #[test]
    fn short_length_prefixes_convert_too() {
        let expected = vec![0, 0, 0, 1, 0x65, 0x11, 0, 0, 0, 1, 0x41];
        let mut one = Vec::new();
        avcc_to_annexb(&[2, 0x65, 0x11, 1, 0x41], 1, &mut one).expect("1-byte prefixes");
        assert_eq!(one, expected);
        let mut two = Vec::new();
        avcc_to_annexb(&[0, 2, 0x65, 0x11, 0, 1, 0x41], 2, &mut two).expect("2-byte prefixes");
        assert_eq!(two, expected);
        // A 4-byte parse of 2-byte-prefixed data must fail, not misread.
        let mut wrong = Vec::new();
        assert!(matches!(
            avcc_to_annexb(&[0, 2, 0x65, 0x11], 4, &mut wrong),
            Err(ReadError::MalformedSample)
        ));
    }

    /// The files our own muxer writes declare 4-byte prefixes; an invalid size of 3 is refused.
    #[test]
    fn nal_length_size_reads_the_avcc() {
        let clip = tiny_clip();
        let reader = open(&clip.0).expect("open");
        let track_id = video_track(&reader).expect("track");
        let track = &reader.tracks()[&track_id];
        assert_eq!(nal_length_size(track).expect("size"), 4);

        let mut bad_trak = track.trak.clone();
        bad_trak
            .mdia
            .minf
            .stbl
            .stsd
            .avc1
            .as_mut()
            .expect("avc1")
            .avcc
            .length_size_minus_one = 2; // length size 3: invalid per 14496-15
        let bad = mp4::Mp4Track {
            trak: bad_trak,
            trafs: Vec::new(),
            default_sample_duration: 0,
        };
        assert!(matches!(
            nal_length_size(&bad),
            Err(ReadError::MalformedSample)
        ));
    }

    /// One open returns both halves, matching the two single-purpose reads.
    #[test]
    fn preview_matches_summary_plus_keyframe() {
        let clip = tiny_clip();
        let (summary, keyframe) = clip_preview(&clip.0).expect("preview");
        assert_eq!(summary, clip_summary(&clip.0).expect("summary"));
        assert_eq!(keyframe, first_keyframe_annexb(&clip.0).expect("keyframe"));
        assert!(matches!(
            clip_preview(Path::new("/nonexistent/clip.mp4")).unwrap_err(),
            ReadError::Io { .. }
        ));
    }

    /// A zero `mdhd` timescale makes the vendored reader divide by zero inside `duration()`;
    /// that panic must come back as `Corrupt`, not abort the caller.
    #[test]
    fn zero_timescale_is_corrupt_not_a_panic() {
        let out = TempMp4::new();
        let file = std::fs::File::create(&out.0).unwrap();
        let mut writer = mp4::Mp4Writer::write_start(
            file,
            &mp4::Mp4Config {
                major_brand: mp4::FourCC::from(*b"isom"),
                minor_version: 512,
                compatible_brands: vec![mp4::FourCC::from(*b"isom")],
                timescale: 1_000_000,
            },
        )
        .unwrap();
        writer
            .add_track(&mp4::TrackConfig {
                track_type: mp4::TrackType::Video,
                timescale: 0,
                language: String::from("und"),
                media_conf: mp4::MediaConfig::AvcConfig(mp4::AvcConfig {
                    width: 640,
                    height: 360,
                    seq_param_set: SPS.to_vec(),
                    pic_param_set: PPS.to_vec(),
                }),
            })
            .unwrap();
        writer.write_end().unwrap();

        assert!(matches!(
            clip_summary(&out.0).unwrap_err(),
            ReadError::Corrupt
        ));
        assert!(matches!(
            clip_preview(&out.0).unwrap_err(),
            ReadError::Corrupt
        ));
    }

    #[test]
    fn error_variants_display() {
        assert_eq!(
            ReadError::NoVideoTrack.to_string(),
            "the file has no H.264 video track"
        );
        assert_eq!(
            ReadError::NoKeyframe.to_string(),
            "the video track has no keyframe"
        );
        assert_eq!(
            ReadError::MalformedSample.to_string(),
            "the keyframe sample is malformed"
        );
        assert_eq!(
            ReadError::Corrupt.to_string(),
            "the MP4 metadata is corrupt"
        );
        let io = ReadError::Io {
            path: PathBuf::from("/x"),
            source: std::io::Error::other("boom"),
        };
        assert!(io.to_string().contains("/x"));
    }
}
