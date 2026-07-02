//! Clip thumbnails: pull the first keyframe out of the MP4 (rewynd-mux's read side), decode it
//! on the CPU with openh264 (docs/adr/0013), downscale, and cache. Everything here is blocking
//! and runs on a background task, never the UI thread.
//!
//! Two cache layers: the caller keeps decoded handles in memory per (path, mtime); this module
//! adds an on-disk PNG per (path, mtime) hash under the user cache dir, so a restart shows the
//! library instantly without re-decoding.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use iced::widget::image::Handle;

/// Thumbnails are decoded at source size and downscaled to this width (height follows the
/// clip's aspect) — about 2x the card's logical size, so hidpi screens stay sharp.
const THUMB_WIDTH: u32 = 480;

/// A loaded thumbnail plus what the summary read told us along the way.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub handle: Handle,
    pub duration: Duration,
}

/// Load the thumbnail for the clip at `path`: from the disk cache when the (path, mtime) key
/// hits, else by decoding the first keyframe (and refilling the cache). Blocking.
pub fn load(path: &Path, modified: SystemTime) -> Result<Loaded, String> {
    let summary = rewynd_mux::read::clip_summary(path).map_err(|e| e.to_string())?;
    let cache = cache_file(path, modified);
    if let Some(cached) = &cache
        && let Ok(bytes) = std::fs::read(cached)
    {
        return Ok(Loaded {
            handle: Handle::from_bytes(bytes),
            duration: summary.duration,
        });
    }

    let annexb = rewynd_mux::read::first_keyframe_annexb(path).map_err(|e| e.to_string())?;
    let (width, height, rgb) = decode_first_frame(&annexb)?;
    let frame = image::RgbImage::from_raw(width, height, rgb)
        .ok_or_else(|| "decoded frame size mismatch".to_owned())?;
    let (tw, th) = thumb_dims(width, height);
    let thumb = image::imageops::thumbnail(&frame, tw, th);

    if let Some(cached) = &cache {
        // Thumbnails are frames of screen recordings: keep the cache dir private, like the
        // clip directories themselves. Best-effort throughout; a failed cache write just
        // means a re-decode next start.
        if let Some(parent) = cached.parent() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            let _ = builder.create(parent);
        }
        let _ = thumb.save_with_format(cached, image::ImageFormat::Png);
    }

    let rgba = image::DynamicImage::ImageRgb8(thumb).into_rgba8();
    Ok(Loaded {
        handle: Handle::from_rgba(tw, th, rgba.into_raw()),
        duration: summary.duration,
    })
}

/// Decode a self-contained Annex-B keyframe (SPS/PPS + IDR) to RGB. openh264 may buffer the
/// first feed and only emit on flush, so both paths are taken.
fn decode_first_frame(annexb: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut decoder = openh264::decoder::Decoder::new().map_err(|e| e.to_string())?;
    match decoder.decode(annexb) {
        Ok(Some(yuv)) => return Ok(rgb_of(&yuv)),
        Ok(None) => {}
        Err(e) => return Err(e.to_string()),
    }
    let remaining = decoder.flush_remaining().map_err(|e| e.to_string())?;
    remaining
        .first()
        .map(rgb_of)
        .ok_or_else(|| "the decoder produced no frame".to_owned())
}

fn rgb_of(yuv: &openh264::decoder::DecodedYUV<'_>) -> (u32, u32, Vec<u8>) {
    use openh264::formats::YUVSource;
    let (width, height) = yuv.dimensions();
    let mut rgb = vec![0u8; width * height * 3];
    yuv.write_rgb8(&mut rgb);
    (width as u32, height as u32, rgb)
}

/// The on-disk cache path for a clip, or `None` when the platform has no cache dir.
fn cache_file(path: &Path, modified: SystemTime) -> Option<PathBuf> {
    Some(
        dirs::cache_dir()?
            .join("rewynd")
            .join("thumbs")
            .join(format!("{:016x}.png", cache_key(path, modified))),
    )
}

/// A stable 64-bit key over (path, mtime) — FNV-1a, so the cache file names survive restarts
/// (std's hasher is seeded per process) and a rewritten clip gets a fresh entry.
fn cache_key(path: &Path, modified: SystemTime) -> u64 {
    let nanos = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hash = fnv1a(0xcbf2_9ce4_8422_2325, path.as_os_str().as_encoded_bytes());
    hash = fnv1a(hash, &nanos.to_le_bytes());
    hash
}

fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Thumbnail dimensions: capped at [`THUMB_WIDTH`] keeping the aspect ratio; smaller frames
/// stay as they are.
fn thumb_dims(width: u32, height: u32) -> (u32, u32) {
    if width <= THUMB_WIDTH || width == 0 {
        return (width.max(1), height.max(1));
    }
    let th = (u64::from(height) * u64::from(THUMB_WIDTH) / u64::from(width)) as u32;
    (THUMB_WIDTH, th.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_is_stable_and_sensitive() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        let a = cache_key(Path::new("/clips/rewynd-1-0.mp4"), t);
        assert_eq!(
            a,
            cache_key(Path::new("/clips/rewynd-1-0.mp4"), t),
            "same inputs, same key across calls (and processes)"
        );
        assert_ne!(a, cache_key(Path::new("/clips/rewynd-2-0.mp4"), t));
        assert_ne!(
            a,
            cache_key(
                Path::new("/clips/rewynd-1-0.mp4"),
                t + Duration::from_secs(1)
            ),
            "a rewritten file gets a fresh cache entry"
        );
    }

    #[test]
    fn thumb_dims_cap_width_and_keep_aspect() {
        assert_eq!(thumb_dims(1920, 1080), (480, 270));
        assert_eq!(thumb_dims(3840, 2160), (480, 270));
        assert_eq!(thumb_dims(320, 180), (320, 180), "small frames stay as-is");
        assert_eq!(thumb_dims(0, 0), (1, 1), "degenerate input stays drawable");
        assert_eq!(thumb_dims(10_000, 2), (480, 1));
    }
}
