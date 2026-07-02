//! Clip thumbnails: pull the first keyframe out of the MP4 (rewynd-mux's read side), decode it
//! on the CPU with openh264 (docs/adr/0013), downscale, and cache. Everything here is blocking
//! and runs on a background task, never the UI thread.
//!
//! Two cache layers: the caller keeps decoded handles in memory per (path, mtime); this module
//! adds an on-disk PNG per (path, mtime) hash under the user cache dir, so a restart shows the
//! library instantly without re-decoding. The cache dir gets the clip store's private-dir bar
//! (thumbnails are frames of the same screen recordings); when it can't be verified as ours,
//! thumbnails stay in-memory only for the run.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use iced::widget::image::Handle;

/// Thumbnails are decoded at source size and downscaled to this width (height follows the
/// clip's aspect) — about 2x the card's logical size, so hidpi screens stay sharp.
const THUMB_WIDTH: u32 = 480;

/// The PNG file signature; a cached file that doesn't start with it is discarded.
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// A loaded thumbnail plus what the summary read told us along the way.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub handle: Handle,
    pub duration: Duration,
}

/// Load the thumbnail for the clip at `path`: from the disk cache when the (path, mtime) key
/// hits, else by decoding the first keyframe (and refilling the cache). Blocking.
pub fn load(path: &Path, modified: SystemTime) -> Result<Loaded, String> {
    let cache = cache_file(path, modified);
    if let Some(cached) = &cache
        && let Some(bytes) = read_cached_png(cached)
    {
        let summary = rewynd_mux::read::clip_summary(path).map_err(|e| e.to_string())?;
        return Ok(Loaded {
            handle: Handle::from_bytes(bytes),
            duration: summary.duration,
        });
    }

    // One open + parse for both the summary and the keyframe.
    let (summary, annexb) = rewynd_mux::read::clip_preview(path).map_err(|e| e.to_string())?;
    let (width, height, rgb) = decode_first_frame(&annexb)?;
    let frame = image::RgbImage::from_raw(width, height, rgb)
        .ok_or_else(|| "decoded frame size mismatch".to_owned())?;
    let (tw, th) = thumb_dims(width, height);
    let thumb = image::imageops::thumbnail(&frame, tw, th);

    if let Some(cached) = &cache {
        // Best-effort throughout; a failed cache write just means a re-decode next start.
        write_png_atomically(&thumb, cached);
    }

    let rgba = image::DynamicImage::ImageRgb8(thumb).into_rgba8();
    Ok(Loaded {
        handle: Handle::from_rgba(tw, th, rgba.into_raw()),
        duration: summary.duration,
    })
}

/// Drop the cached PNG for a clip the user deleted (best effort).
pub fn remove_cached(path: &Path, modified: SystemTime) {
    if let Some(cached) = cache_file(path, modified) {
        let _ = std::fs::remove_file(cached);
    }
}

/// A cache hit's bytes, when the file exists and carries the PNG signature. Anything else in
/// the slot is removed and treated as a miss, so a corrupt or planted file self-heals into a
/// fresh decode instead of being handed to the image decoder forever.
fn read_cached_png(cached: &Path) -> Option<Vec<u8>> {
    let bytes = std::fs::read(cached).ok()?;
    if bytes.starts_with(&PNG_MAGIC) {
        return Some(bytes);
    }
    tracing::warn!(path = %cached.display(), "cached thumbnail is not a PNG; discarding it");
    let _ = std::fs::remove_file(cached);
    None
}

/// Encode `thumb` and write it atomically: PNG to memory, then a 0600 temp file in the cache
/// dir, then a rename over the final name — a concurrent reader never sees a partial file,
/// and the bytes are never group/world readable.
fn write_png_atomically(thumb: &image::RgbImage, cached: &Path) {
    use std::io::Write;

    let mut bytes = Vec::new();
    if thumb
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .is_err()
    {
        return;
    }
    let (Some(dir), Some(name)) = (cached.parent(), cached.file_name().and_then(|n| n.to_str()))
    else {
        return;
    };
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&tmp)
        .and_then(|mut file| file.write_all(&bytes));
    if written.is_ok() && std::fs::rename(&tmp, cached).is_ok() {
        return;
    }
    let _ = std::fs::remove_file(&tmp);
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

/// The on-disk cache path for a clip, or `None` when the platform has no cache dir or ours
/// can't be verified as private (then this run caches in memory only).
fn cache_file(path: &Path, modified: SystemTime) -> Option<PathBuf> {
    Some(thumbs_dir()?.join(format!("{:016x}.png", cache_key(path, modified))))
}

/// The private thumbnail cache dir, (re-)verified on every use like the clip store's fallback
/// dir: created 0700 when missing, and only trusted while it is a real directory owned by us
/// with no group/world access.
fn thumbs_dir() -> Option<PathBuf> {
    let base = dirs::cache_dir()?.join("rewynd");
    let dir = base.join("thumbs");
    if rewynd_config::ensure_private_dir(&base) && rewynd_config::ensure_private_dir(&dir) {
        return Some(dir);
    }
    tracing::warn!(
        dir = %dir.display(),
        "thumbnail cache dir is not safely ours; keeping thumbnails in memory only"
    );
    None
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

    #[test]
    fn a_non_png_cache_entry_is_discarded_and_missed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = dir.path().join("bad.png");
        std::fs::write(&bad, b"not a png").expect("write");
        assert_eq!(read_cached_png(&bad), None);
        assert!(!bad.exists(), "the corrupt entry self-heals by deletion");
        assert_eq!(read_cached_png(&bad), None, "missing file is a miss");
    }

    #[test]
    fn atomic_writes_produce_a_private_png() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("thumb.png");
        let thumb = image::RgbImage::from_pixel(2, 2, image::Rgb([0, 128, 255]));
        write_png_atomically(&thumb, &out);
        let bytes = read_cached_png(&out).expect("valid PNG on disk");
        assert!(bytes.starts_with(&PNG_MAGIC));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&out).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "cache files are owner-only");
        }
        assert!(
            std::fs::read_dir(dir.path()).unwrap().count() == 1,
            "no temp file is left behind"
        );
    }
}
