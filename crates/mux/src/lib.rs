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
    AvcConfig, FourCC, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType,
};
use rewynd_buffer::EncodedChunk;
use thiserror::Error;

/// Microsecond timescale: capture PTS deltas are written exactly, with no fps rounding.
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

/// MP4 muxer (Annex-B → AVCC) for a single H.264 video track.
#[derive(Debug, Clone, Copy)]
pub struct Mp4Muxer {
    width: u16,
    height: u16,
}

impl Mp4Muxer {
    /// Create a muxer for an H.264 stream of the given pixel dimensions (used for the
    /// track's visual sample entry; the decoder reads the real size from the SPS).
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width.min(u32::from(u16::MAX)) as u16,
            height: height.min(u32::from(u16::MAX)) as u16,
        }
    }
}

impl Muxer for Mp4Muxer {
    fn write_mp4(&mut self, chunks: &[EncodedChunk], path: &Path) -> Result<(), MuxError> {
        let first = chunks.first().ok_or(MuxError::NotKeyframeStart)?;
        if !first.is_keyframe {
            return Err(MuxError::NotKeyframeStart);
        }

        // gpu-video emits inline SPS/PPS before every IDR, so the first chunk carries them.
        let sps = find_nal(&first.bytes, NAL_SPS).ok_or(MuxError::MissingParameterSets)?;
        let pps = find_nal(&first.bytes, NAL_PPS).ok_or(MuxError::MissingParameterSets)?;

        let file = std::fs::File::create(path)?;
        let mut writer = Mp4Writer::write_start(
            file,
            &Mp4Config {
                major_brand: FourCC::from(*b"isom"),
                minor_version: 512,
                compatible_brands: vec![
                    FourCC::from(*b"isom"),
                    FourCC::from(*b"iso2"),
                    FourCC::from(*b"avc1"),
                    FourCC::from(*b"mp41"),
                ],
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
        for (i, chunk) in chunks.iter().enumerate() {
            let start = chunk.pts.saturating_sub(base);
            // Duration is the gap to the next frame; the last frame reuses the previous
            // gap (or a 1µs floor for a single-frame clip) so the track has a real length.
            let duration = match chunks.get(i + 1) {
                Some(next) => next.pts.saturating_sub(chunk.pts),
                None if i > 0 => chunk.pts.saturating_sub(chunks[i - 1].pts),
                None => Duration::from_micros(1),
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

        writer.write_end()?;
        Ok(())
    }
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
        let mut muxer = Mp4Muxer::new(1920, 1080);
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
        let mut muxer = Mp4Muxer::new(1920, 1080);
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

        let path = std::env::temp_dir().join("rewynd-mux-roundtrip.mp4");
        Mp4Muxer::new(1920, 1080)
            .write_mp4(&chunks, &path)
            .expect("write_mp4");

        let file = std::fs::File::open(&path).unwrap();
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

        let _ = std::fs::remove_file(&path);
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
