//! The read side: open a saved MP4 and pull out what a preview needs — the clip's
//! dimensions/duration and the first keyframe converted back to Annex-B (the inverse of the
//! write side), so a CPU decoder can render a thumbnail without a full demuxer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rewynd_buffer::{EncodedAudioChunk, EncodedChunk};
use thiserror::Error;

use crate::{AudioTrack, Mp4Muxer};

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

/// One video sample ready for a raw H.264 decoder: its presentation time and Annex-B bytes.
/// The first sample an iterator yields carries the track's SPS/PPS inline, so decoding can
/// start there without any other setup.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub pts: Duration,
    pub annexb: Vec<u8>,
}

/// A streaming reader over a clip's video samples, from the sync sample at or before a
/// requested time to the end of the track (in-app playback wants exactly this: a valid decode
/// chain without reading the whole clip up front). Corrupt data ends the iteration.
pub struct VideoFrames {
    reader: Reader,
    track_id: u32,
    timescale: f64,
    prefix: usize,
    /// SPS/PPS, prepended to the first yielded sample then taken.
    header: Option<Vec<u8>>,
    next: u32,
    count: u32,
}

/// Open the clip at `path` for streaming video reads starting at the sync sample at or before
/// `start`.
pub fn video_frames_from(path: &Path, start: Duration) -> Result<VideoFrames, ReadError> {
    catch_reader_panics(|| {
        let reader = open(path)?;
        let track_id = video_track(&reader)?;
        let (timescale, prefix, header, next, count) = {
            let track = &reader.tracks()[&track_id];
            let timescale = f64::from(track.timescale().max(1));
            let mut header = Vec::new();
            append_nal(&mut header, track.sequence_parameter_set()?);
            append_nal(&mut header, track.picture_parameter_set()?);
            (
                timescale,
                nal_length_size(track)?,
                header,
                sync_at_or_before(track, start, timescale),
                track.sample_count(),
            )
        };
        Ok(VideoFrames {
            reader,
            track_id,
            timescale,
            prefix,
            header: Some(header),
            next,
            count,
        })
    })
}

/// The last sync-sample id whose decode time is at or before `start` (else the first sync
/// sample, else sample 1). The stss table is ordered, so the scan can stop past `start`.
fn sync_at_or_before(track: &mp4::Mp4Track, start: Duration, timescale: f64) -> u32 {
    let Some(syncs) = track.sync_sample_ids() else {
        return 1;
    };
    let mut best = *syncs.first().unwrap_or(&1);
    for &id in syncs {
        let Ok((time, _)) = track.sample_time(id) else {
            continue;
        };
        if time as f64 / timescale <= start.as_secs_f64() {
            best = id;
        } else {
            break;
        }
    }
    best
}

impl Iterator for VideoFrames {
    type Item = VideoFrame;

    fn next(&mut self) -> Option<VideoFrame> {
        if self.next > self.count {
            return None;
        }
        let id = self.next;
        self.next += 1;
        // The vendored reader panics on inconsistent metadata; end the stream instead.
        let sample = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.reader.read_sample(self.track_id, id)
        }));
        let Ok(Ok(Some(sample))) = sample else {
            self.next = self.count + 1;
            return None;
        };
        let pts_units =
            (sample.start_time as i64 + i64::from(sample.rendering_offset)).max(0) as f64;
        let mut annexb = self.header.take().unwrap_or_default();
        if avcc_to_annexb(&sample.bytes, self.prefix, &mut annexb).is_err() {
            self.next = self.count + 1;
            return None;
        }
        Some(VideoFrame {
            pts: Duration::from_secs_f64(pts_units / self.timescale),
            annexb,
        })
    }
}

/// Errors from [`trim_clip`].
#[derive(Debug, Error)]
pub enum TrimError {
    /// The source could not be read (missing, corrupt, or no video track).
    #[error(transparent)]
    Read(#[from] ReadError),
    /// Writing the trimmed clip failed.
    #[error("writing the trimmed clip failed: {0}")]
    Mux(#[from] crate::MuxError),
    /// The requested window selected no frames (empty, reversed, or past the clip's end).
    #[error("the trim range is empty")]
    EmptyRange,
}

/// What a completed trim produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrimSummary {
    /// Duration of the trimmed clip, from its (keyframe-aligned) start to the last kept frame.
    pub duration: Duration,
}

/// Losslessly trim the clip at `src` to the `[start, end]` window, writing a new MP4 at `dst`.
///
/// The start snaps back to the nearest keyframe at or before `start`, so the cut needs no re-encode
/// and the result stays decodable; every video frame through `end` is kept, and the audio packets in
/// the same window ride along. Timestamps rebase to zero, so the trimmed clip plays from its own
/// start. Keyframe granularity is the recorder's IDR interval (about one second at the defaults).
pub fn trim_clip(
    src: &Path,
    dst: &Path,
    start: Duration,
    end: Duration,
) -> Result<TrimSummary, TrimError> {
    if end <= start {
        return Err(TrimError::EmptyRange);
    }
    // The vendored reader `unwrap`s on inconsistent metadata; contain a hostile file as an error.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        trim_inner(src, dst, start, end)
    }))
    .unwrap_or(Err(TrimError::Read(ReadError::Corrupt)))
}

fn trim_inner(
    src: &Path,
    dst: &Path,
    start: Duration,
    end: Duration,
) -> Result<TrimSummary, TrimError> {
    let mut reader = open(src)?;
    let video_id = video_track(&reader)?;

    // All track metadata up front, so the immutable borrows are dropped before the &mut read_sample
    // loops below.
    struct VideoMeta {
        timescale: f64,
        prefix: usize,
        sps: Vec<u8>,
        pps: Vec<u8>,
        count: u32,
        width: u32,
        height: u32,
        duration: Duration,
    }
    struct AudioMeta {
        id: u32,
        channels: u8,
        sample_rate: u32,
        count: u32,
    }
    let (vmeta, ameta) = {
        let vt = &reader.tracks()[&video_id];
        let vmeta = VideoMeta {
            timescale: f64::from(vt.timescale().max(1)),
            prefix: nal_length_size(vt)?,
            sps: vt
                .sequence_parameter_set()
                .map_err(ReadError::from)?
                .to_vec(),
            pps: vt
                .picture_parameter_set()
                .map_err(ReadError::from)?
                .to_vec(),
            count: vt.sample_count(),
            width: u32::from(vt.width()),
            height: u32::from(vt.height()),
            duration: vt.duration(),
        };
        // Opus audio tracks, in track-id order (the recorder writes the mix as track 1, the mic as
        // track 2); preserve that order in the trimmed clip.
        let mut ameta: Vec<AudioMeta> = reader
            .tracks()
            .iter()
            .filter(|(_, t)| t.media_type().is_ok_and(|m| m == mp4::MediaType::OPUS))
            .map(|(id, t)| AudioMeta {
                id: *id,
                channels: t
                    .trak
                    .mdia
                    .minf
                    .stbl
                    .stsd
                    .opus
                    .as_ref()
                    .map_or(2, |o| o.dops.output_channel_count.max(1)),
                sample_rate: t.timescale().max(1),
                count: t.sample_count(),
            })
            .collect();
        ameta.sort_by_key(|a| a.id);
        (vmeta, ameta)
    };

    // The muxer only uses the frame rate for the final frame's duration; estimate it from the source.
    let fps = if vmeta.duration.as_secs_f64() > 0.0 {
        (f64::from(vmeta.count) / vmeta.duration.as_secs_f64())
            .round()
            .clamp(1.0, 240.0) as u32
    } else {
        60
    };
    let frame = Duration::from_secs_f64(1.0 / f64::from(fps));

    // Read every video sample: its presentation time, sync flag, and Annex-B bytes.
    struct Vid {
        pts: Duration,
        sync: bool,
        annexb: Vec<u8>,
    }
    let mut vids: Vec<Vid> = Vec::with_capacity(vmeta.count as usize);
    for id in 1..=vmeta.count {
        let Some(sample) = reader.read_sample(video_id, id).map_err(ReadError::from)? else {
            break;
        };
        let pts_units =
            (sample.start_time as i64 + i64::from(sample.rendering_offset)).max(0) as f64;
        let mut annexb = Vec::new();
        avcc_to_annexb(&sample.bytes, vmeta.prefix, &mut annexb)?;
        vids.push(Vid {
            pts: Duration::from_secs_f64(pts_units / vmeta.timescale),
            sync: sample.is_sync,
            annexb,
        });
    }
    if vids.is_empty() {
        return Err(TrimError::EmptyRange);
    }

    // Start on the last keyframe at or before `start` (else the first keyframe); end on the last
    // frame at or before `end`.
    let in_idx = vids
        .iter()
        .enumerate()
        .filter(|(_, v)| v.sync && v.pts <= start)
        .map(|(i, _)| i)
        .next_back()
        .or_else(|| vids.iter().position(|v| v.sync))
        .unwrap_or(0);

    // A window that starts at or after the clip's end selects nothing: reject it rather than
    // emit the trailing GOP.
    let clip_end = vids.last().map_or(Duration::ZERO, |v| v.pts + frame);
    if start >= clip_end {
        return Err(TrimError::EmptyRange);
    }
    let Some(out_idx) = vids
        .iter()
        .enumerate()
        .filter(|(i, v)| *i >= in_idx && v.pts <= end)
        .map(|(i, _)| i)
        .next_back()
    else {
        return Err(TrimError::EmptyRange);
    };

    let in_pts = vids[in_idx].pts;
    let out_pts = vids[out_idx].pts;
    let audio_hi = out_pts + frame;

    // The first kept video chunk must carry SPS/PPS inline: the writer reads them there to build the
    // avcC. Subsequent chunks are the sample NALs as-is.
    let mut video_chunks: Vec<EncodedChunk> = Vec::with_capacity(out_idx - in_idx + 1);
    for (i, v) in vids.iter().enumerate().take(out_idx + 1).skip(in_idx) {
        let bytes = if i == in_idx {
            let mut b = Vec::with_capacity(v.annexb.len() + vmeta.sps.len() + vmeta.pps.len() + 8);
            append_nal(&mut b, &vmeta.sps);
            append_nal(&mut b, &vmeta.pps);
            b.extend_from_slice(&v.annexb);
            b
        } else {
            v.annexb.clone()
        };
        video_chunks.push(EncodedChunk {
            bytes: bytes.into(),
            is_keyframe: v.sync,
            pts: v.pts,
        });
    }

    // Audio packets whose presentation time lands in the same window as the kept video.
    let mut audio_store: Vec<Vec<EncodedAudioChunk>> = Vec::with_capacity(ameta.len());
    for a in &ameta {
        let mut chunks = Vec::new();
        for id in 1..=a.count {
            let Some(sample) = reader.read_sample(a.id, id).map_err(ReadError::from)? else {
                break;
            };
            let pts = Duration::from_secs_f64(sample.start_time as f64 / f64::from(a.sample_rate));
            if pts > audio_hi {
                break;
            }
            if pts < in_pts {
                continue;
            }
            chunks.push(EncodedAudioChunk {
                bytes: sample.bytes.to_vec().into(),
                frames: sample.duration,
                pts,
            });
        }
        audio_store.push(chunks);
    }
    let audio_tracks: Vec<AudioTrack> = ameta
        .iter()
        .zip(&audio_store)
        // A mid-stream cut has no encoder priming at its first packet, so pre_skip is 0.
        .map(|(a, chunks)| AudioTrack {
            chunks,
            channels: a.channels,
            sample_rate: a.sample_rate,
            pre_skip: 0,
        })
        .collect();

    let muxer = Mp4Muxer::new(vmeta.width, vmeta.height, fps);
    if audio_tracks.iter().all(|t| t.chunks.is_empty()) {
        muxer.write_mp4(&video_chunks, dst)?;
    } else {
        muxer.write_mp4_with_audio_tracks(&video_chunks, &audio_tracks, dst)?;
    }

    Ok(TrimSummary {
        duration: out_pts.saturating_sub(in_pts) + frame,
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

    /// A ten-frame video (keyframes at 0/4/8) plus one Opus track of 20 ms packets, for trim tests.
    fn av_clip() -> TempMp4 {
        let video: Vec<_> = (0..10u64)
            .map(|i| {
                let tag = i as u8;
                if i % 4 == 0 {
                    chunk(annexb(&[&SPS, &PPS, &[0x65, 0x88, tag]]), true, i * 16_667)
                } else {
                    chunk(annexb(&[&[0x41, 0x9a, tag]]), false, i * 16_667)
                }
            })
            .collect();
        let audio: Vec<EncodedAudioChunk> = (0..8u64)
            .map(|i| EncodedAudioChunk {
                bytes: vec![0xFC, 0xFF, 0xFE].into(),
                frames: 960,
                pts: Duration::from_micros(i * 20_000),
            })
            .collect();
        let track = AudioTrack {
            chunks: &audio,
            channels: 2,
            sample_rate: 48_000,
            pre_skip: 0,
        };
        let out = TempMp4::new();
        Mp4Muxer::new(1920, 1080, 60)
            .write_mp4_with_audio(&video, &track, &out.0)
            .expect("write av clip");
        out
    }

    #[test]
    fn video_frames_start_on_the_enclosing_keyframe() {
        let clip = multi_keyframe_clip();
        // 70 ms is past the 66.7 ms keyframe at frame 4: the stream starts there.
        let frames: Vec<_> = video_frames_from(&clip.0, Duration::from_millis(70))
            .expect("open")
            .collect();
        assert_eq!(frames.len(), 6, "samples 5..=10");
        assert!(
            contains(&frames[0].annexb, &SPS),
            "SPS/PPS on the first frame"
        );
        assert!(
            contains(&frames[0].annexb, &[0x65, 0x88, 4]),
            "frame 4's IDR"
        );
        assert!(!contains(&frames[1].annexb, &SPS), "header only once");
        assert!(
            frames.windows(2).all(|w| w[0].pts < w[1].pts),
            "presentation times increase"
        );
    }

    #[test]
    fn video_frames_from_zero_cover_the_whole_clip() {
        let clip = multi_keyframe_clip();
        let frames: Vec<_> = video_frames_from(&clip.0, Duration::ZERO)
            .expect("open")
            .collect();
        assert_eq!(frames.len(), 10);
        assert!(contains(&frames[0].annexb, &SPS));
    }

    #[test]
    fn video_frames_errors_and_empties_are_contained() {
        assert!(matches!(
            video_frames_from(Path::new("/nonexistent/clip.mp4"), Duration::ZERO),
            Err(ReadError::Io { .. })
        ));
        let empty = raw_video_mp4(&[]);
        let frames: Vec<_> = video_frames_from(&empty.0, Duration::ZERO)
            .expect("open")
            .collect();
        assert!(frames.is_empty(), "no samples yields nothing, no panic");
    }

    #[test]
    fn trim_snaps_start_back_to_the_enclosing_keyframe() {
        let clip = multi_keyframe_clip();
        let out = TempMp4::new();
        // Start at 70 ms (past the 66.7 ms keyframe at frame 4) through 120 ms: the start snaps back
        // to that keyframe, and the last kept frame is the one at or before 120 ms.
        let summary = trim_clip(
            &clip.0,
            &out.0,
            Duration::from_millis(70),
            Duration::from_millis(120),
        )
        .expect("trim");

        let frame = first_keyframe_annexb(&out.0).expect("keyframe");
        assert!(
            contains(&frame, &[0x65, 0x88, 4]),
            "snapped to frame 4's keyframe"
        );
        assert!(
            !contains(&frame, &[0x65, 0x88, 0]),
            "not the original first keyframe"
        );
        // Frames 4..=7 kept ⇒ about four 16.67 ms frames.
        assert!(
            summary.duration >= Duration::from_millis(60)
                && summary.duration <= Duration::from_millis(72),
            "duration {:?}",
            summary.duration
        );
    }

    #[test]
    fn trim_from_the_start_keeps_the_first_keyframe() {
        let clip = multi_keyframe_clip();
        let out = TempMp4::new();
        let summary =
            trim_clip(&clip.0, &out.0, Duration::ZERO, Duration::from_millis(40)).expect("trim");
        let frame = first_keyframe_annexb(&out.0).expect("keyframe");
        // Starts on an IDR from the first GOP (tag byte omitted: frame 0's is 0x00, which the write
        // side strips as NAL trailing padding), and not the 66.7 ms keyframe at frame 4.
        assert!(contains(&frame, &[0x65, 0x88]), "starts on a keyframe");
        assert!(
            !contains(&frame, &[0x65, 0x88, 4]),
            "not frame 4's keyframe"
        );
        // Frames 0..=2 kept ⇒ about three 16.67 ms frames.
        assert!(
            summary.duration <= Duration::from_millis(56),
            "duration {:?}",
            summary.duration
        );
    }

    #[test]
    fn trim_rejects_an_empty_or_reversed_range() {
        let clip = multi_keyframe_clip();
        let out = TempMp4::new();
        assert!(matches!(
            trim_clip(
                &clip.0,
                &out.0,
                Duration::from_millis(80),
                Duration::from_millis(20)
            ),
            Err(TrimError::EmptyRange)
        ));
        assert!(matches!(
            trim_clip(
                &clip.0,
                &out.0,
                Duration::from_millis(50),
                Duration::from_millis(50)
            ),
            Err(TrimError::EmptyRange)
        ));
    }

    #[test]
    fn trim_window_past_the_clip_end_is_empty() {
        // The ten-frame clip is ~0.17 s; a window well past that selects no frame.
        let clip = multi_keyframe_clip();
        let out = TempMp4::new();
        assert!(matches!(
            trim_clip(
                &clip.0,
                &out.0,
                Duration::from_millis(500),
                Duration::from_millis(600),
            ),
            Err(TrimError::EmptyRange)
        ));
    }

    #[test]
    fn trim_missing_source_is_a_read_error() {
        let out = TempMp4::new();
        assert!(matches!(
            trim_clip(
                Path::new("/nonexistent/clip.mp4"),
                &out.0,
                Duration::ZERO,
                Duration::from_millis(50),
            ),
            Err(TrimError::Read(ReadError::Io { .. }))
        ));
    }

    #[test]
    fn trim_carries_the_audio_track_along() {
        let clip = av_clip();
        let out = TempMp4::new();
        trim_clip(
            &clip.0,
            &out.0,
            Duration::from_millis(70),
            Duration::from_millis(140),
        )
        .expect("trim");

        let reader = open(&out.0).expect("open trimmed");
        let has_opus = reader
            .tracks()
            .values()
            .any(|t| t.media_type().is_ok_and(|m| m == mp4::MediaType::OPUS));
        assert!(has_opus, "the trimmed clip keeps its Opus track");
        // And the video still starts on the enclosing keyframe.
        let frame = first_keyframe_annexb(&out.0).expect("keyframe");
        assert!(contains(&frame, &[0x65, 0x88, 4]));
    }
}
