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
    /// An AVCC sample whose length prefixes overrun the sample.
    #[error("the keyframe sample is malformed")]
    MalformedSample,
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

/// The dimensions and duration of the clip at `path` (from the video track's headers; no
/// sample data is read).
pub fn clip_summary(path: &Path) -> Result<ClipSummary, ReadError> {
    let reader = open(path)?;
    let track_id = video_track(&reader)?;
    let track = &reader.tracks()[&track_id];
    Ok(ClipSummary {
        width: u32::from(track.width()),
        height: u32::from(track.height()),
        duration: track.duration(),
    })
}

/// The first keyframe of the clip at `path` as a self-contained Annex-B buffer: the `avcC`
/// SPS/PPS, then the sync sample's NAL units, all start-code delimited — exactly what a raw
/// H.264 decoder wants for a single-frame decode.
pub fn first_keyframe_annexb(path: &Path) -> Result<Vec<u8>, ReadError> {
    let mut reader = open(path)?;
    let track_id = video_track(&reader)?;
    let track = &reader.tracks()[&track_id];
    let (sps, pps) = (
        track.sequence_parameter_set()?.to_vec(),
        track.picture_parameter_set()?.to_vec(),
    );
    let sample_count = track.sample_count();

    let mut out = Vec::new();
    append_nal(&mut out, &sps);
    append_nal(&mut out, &pps);
    for sample_id in 1..=sample_count {
        let Some(sample) = reader.read_sample(track_id, sample_id)? else {
            break;
        };
        if sample.is_sync {
            avcc_to_annexb(&sample.bytes, &mut out)?;
            return Ok(out);
        }
    }
    Err(ReadError::NoKeyframe)
}

fn append_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
}

/// Convert one AVCC sample (four-byte big-endian length prefixes, as the write side stores)
/// into Annex-B, appending to `out`.
fn avcc_to_annexb(sample: &[u8], out: &mut Vec<u8>) -> Result<(), ReadError> {
    let mut rest = sample;
    while !rest.is_empty() {
        let (prefix, tail) = rest.split_at_checked(4).ok_or(ReadError::MalformedSample)?;
        let len = u32::from_be_bytes(prefix.try_into().expect("split_at gave 4 bytes")) as usize;
        let (nal, tail) = tail.split_at_checked(len).ok_or(ReadError::MalformedSample)?;
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
        assert!(matches!(clip_summary(&out.0).unwrap_err(), ReadError::Mp4(_)));
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
            avcc_to_annexb(&[0, 0, 0, 9, 0x65], &mut out),
            Err(ReadError::MalformedSample)
        ));
        // Truncated length prefix.
        assert!(matches!(
            avcc_to_annexb(&[0, 0, 1], &mut out),
            Err(ReadError::MalformedSample)
        ));
        // A zero-length NAL unit.
        assert!(matches!(
            avcc_to_annexb(&[0, 0, 0, 0], &mut out),
            Err(ReadError::MalformedSample)
        ));
        // Well-formed input converts and appends.
        let mut ok = Vec::new();
        avcc_to_annexb(&[0, 0, 0, 2, 0x65, 0x11, 0, 0, 0, 1, 0x41], &mut ok).expect("converts");
        assert_eq!(ok, vec![0, 0, 0, 1, 0x65, 0x11, 0, 0, 0, 1, 0x41]);
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
        let io = ReadError::Io {
            path: PathBuf::from("/x"),
            source: std::io::Error::other("boom"),
        };
        assert!(io.to_string().contains("/x"));
    }
}
