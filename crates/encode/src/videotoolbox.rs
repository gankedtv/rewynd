//! VideoToolbox H.264 encoder for macOS (docs/adr/0015): a CoreVideo pixel buffer in
//! (NV12, IOSurface-backed from ScreenCaptureKit), an Annex-B [`EncodedChunk`] out.
//!
//! Produces the exact stream shape the ring buffer and muxer require: Annex-B bytes,
//! inline SPS/PPS before every IDR, an accurate keyframe flag, and the caller's PTS
//! verbatim — the same contract as the other backends. VideoToolbox emits AVCC
//! (length-prefixed) samples with parameter sets in the format description, so the
//! output callback converts via [`crate::annexb`] before anything reaches the ring.

use std::ffi::c_void;
use std::sync::mpsc;
use std::time::Duration;

use cidre::vt::compression::{encoder_spec_keys, keys, profile_level};
use cidre::vt::compression_properties::frame_keys;
use cidre::{arc, cf, cm, cv, os, vt};
use rewynd_buffer::EncodedChunk;

use crate::{EncodeError, EncodeParams, annexb};

/// PTS timescale handed to VideoToolbox: microseconds, matching the muxer's clock.
const PTS_TIMESCALE: i32 = 1_000_000;
/// How long to wait for the output callback per submitted frame. The hardware encoder
/// answers in milliseconds; hitting this means the session is wedged.
const OUTPUT_TIMEOUT: Duration = Duration::from_secs(2);

// Not wrapped by cidre 0.16 for compression sessions (only the decompression analog is).
#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder: &'static cf::String;
}

/// One parsed output frame, fully converted on the VideoToolbox callback thread so no
/// CoreFoundation types cross the channel.
struct ParsedFrame {
    annexb: Vec<u8>,
    is_keyframe: bool,
}

/// `Ok(None)` = the encoder dropped the frame (no output for this submission).
type CallbackResult = Result<Option<ParsedFrame>, String>;

/// Heap-pinned context the session's C callback writes through.
struct CallbackState {
    tx: mpsc::Sender<CallbackResult>,
}

/// H.264 encoder backed by a `VTCompressionSession`.
///
/// Constructed and driven from one capture thread; every `encode_pixel_buf` call
/// submits a frame and blocks for its (in-order, reordering disabled) output.
pub struct VideoToolboxEncoder {
    session: arc::R<vt::CompressionSession>,
    rx: mpsc::Receiver<CallbackResult>,
    /// Owns the callback context the session holds a raw pointer to. Dropped after
    /// `Drop` invalidates the session, so no callback can observe a dangling pointer.
    _state: Box<CallbackState>,
    params: EncodeParams,
    /// Apple's software encoder buffers frames until nudged (it ignores
    /// `MaxFrameDelayCount`), so the software path flushes after every submission.
    /// The hardware encoder is one-in-one-out already and stays pipelined.
    flush_per_frame: bool,
}

// SAFETY: the raw session pointer is the only non-Send field. VTCompressionSession
// calls (encode/complete/invalidate/property access) are documented safe from any
// thread; `&mut self` serialises ours. The callback context is a heap allocation
// written only through the channel's Sender, which is Send.
unsafe impl Send for VideoToolboxEncoder {}

impl VideoToolboxEncoder {
    /// Build a session for `params`. `require_hardware` selects the Apple Silicon
    /// hardware encoder and fails if it's unavailable; `false` pins Apple's software
    /// encoder (the "cpu" preference path).
    pub fn new(params: EncodeParams, require_hardware: bool) -> Result<Self, EncodeError> {
        if params.width == 0 || params.height == 0 {
            return Err(EncodeError::Init("width and height must be > 0".to_owned()));
        }
        if params.framerate == 0 {
            return Err(EncodeError::Init("framerate must be > 0".to_owned()));
        }
        if params.idr_period == 0 {
            return Err(EncodeError::Init("idr_period must be > 0".to_owned()));
        }

        let (tx, rx) = mpsc::channel();
        let state = Box::new(CallbackState { tx });
        let ctx: *mut CallbackState = std::ptr::from_ref(&*state).cast_mut();

        let spec = encoder_spec(require_hardware);
        let mut session = vt::CompressionSession::new(
            params.width,
            params.height,
            cm::VideoCodec::H264,
            Some(&spec),
            None,
            None,
            Some(output_callback),
            ctx,
        )
        .map_err(|e| EncodeError::Init(format!("VTCompressionSessionCreate failed: {e}")))?;

        configure_session(&mut session, params)?;

        session
            .prepare()
            .map_err(|e| EncodeError::Init(format!("prepare-to-encode failed: {e}")))?;

        if require_hardware {
            log_hardware_usage(&session);
        }

        Ok(Self {
            session,
            rx,
            _state: state,
            params,
            flush_per_frame: !require_hardware,
        })
    }

    /// The parameters this encoder was configured with.
    #[must_use]
    pub fn params(&self) -> EncodeParams {
        self.params
    }

    /// Encode one NV12 pixel buffer captured at `pts` (capture-relative, strictly
    /// increasing). Returns `Ok(None)` when VideoToolbox dropped the frame — the
    /// caller skips the ring push, keeping the "every chunk is a real frame"
    /// invariant without inventing output.
    pub fn encode_pixel_buf(
        &mut self,
        frame: &cv::PixelBuf,
        force_keyframe: bool,
        pts: Duration,
    ) -> Result<Option<EncodedChunk>, EncodeError> {
        let time = cm::Time::new(
            i64::try_from(pts.as_micros()).unwrap_or(i64::MAX),
            PTS_TIMESCALE,
        );
        let frame_props = force_keyframe.then(|| {
            cf::DictionaryOf::with_keys_values(
                &[frame_keys::force_key_frame()],
                &[cf::Boolean::value_true().as_ref()],
            )
        });

        let mut info_flags = None;
        self.session
            .encode_frame(
                frame,
                time,
                cm::Time::invalid(),
                frame_props.as_deref(),
                std::ptr::null_mut(),
                &mut info_flags,
            )
            .map_err(|e| EncodeError::Encode(format!("encode_frame submission failed: {e}")))?;

        if self.flush_per_frame {
            self.session
                .complete_frames(time)
                .map_err(|e| EncodeError::Encode(format!("completing frame failed: {e}")))?;
        }

        // Reordering is off, so exactly one callback fires per submission, in order.
        match self.rx.recv_timeout(OUTPUT_TIMEOUT) {
            Ok(Ok(Some(parsed))) => Ok(Some(EncodedChunk {
                bytes: parsed.annexb.into(),
                is_keyframe: parsed.is_keyframe,
                pts,
            })),
            Ok(Ok(None)) => {
                tracing::debug!(?pts, "VideoToolbox dropped a frame");
                Ok(None)
            }
            Ok(Err(msg)) => Err(EncodeError::Encode(msg)),
            Err(_) => Err(EncodeError::Encode(
                "timed out waiting for VideoToolbox output".to_owned(),
            )),
        }
    }
}

impl Drop for VideoToolboxEncoder {
    fn drop(&mut self) {
        // OBS teardown order: flush pending frames, then invalidate so no callback can
        // run once the context box (dropped after this) goes away.
        let _ = self.session.complete_all();
        self.session.invalidate();
    }
}

/// Whether a hardware H.264 encoder session can be created (cheap probe used by the
/// platform's encoder selection).
#[must_use]
pub fn hardware_h264_available() -> bool {
    let spec = encoder_spec(true);
    match vt::CompressionSession::new::<c_void>(
        1280,
        720,
        cm::VideoCodec::H264,
        Some(&spec),
        None,
        None,
        None,
        std::ptr::null_mut(),
    ) {
        Ok(mut session) => {
            session.invalidate();
            true
        }
        Err(err) => {
            tracing::debug!(%err, "no hardware H.264 encoder available");
            false
        }
    }
}

/// Encoder-specification dictionary: require the hardware encoder, or disable it to
/// pin Apple's software encoder.
fn encoder_spec(require_hardware: bool) -> arc::R<cf::Dictionary> {
    let (key, value) = if require_hardware {
        (
            encoder_spec_keys::require_hw_accelerated_video_encoder(),
            cf::Boolean::value_true(),
        )
    } else {
        (
            encoder_spec_keys::enable_hw_accelerated_video_encoder(),
            cf::Boolean::value_false(),
        )
    };
    cf::Dictionary::with_keys_values(&[key.as_ref()], &[value.as_ref()])
        .expect("CFDictionaryCreate with static keys")
}

/// Apply session properties. Ring-buffer-critical knobs (no frame reordering — the
/// muxer assumes decode order == presentation order — plus rate control and the fixed
/// GOP the buffer cuts on) fail construction; tuning knobs only log.
fn configure_session(
    session: &mut vt::CompressionSession,
    params: EncodeParams,
) -> Result<(), EncodeError> {
    let bitrate = cf::Number::from_i64(i64::from(params.bitrate_bps));
    let gop_frames = cf::Number::from_i64(i64::from(params.idr_period));

    let mut critical = cf::DictionaryMut::with_capacity(3);
    critical.insert(keys::allow_frame_reordering(), cf::Boolean::value_false());
    critical.insert(keys::avarage_bit_rate(), &bitrate);
    critical.insert(keys::max_key_frame_interval(), &gop_frames);
    session
        .set_props(&critical)
        .map_err(|e| EncodeError::Init(format!("setting critical session properties: {e}")))?;

    let framerate = cf::Number::from_i64(i64::from(params.framerate));
    // ScreenCaptureKit is VFR (no frames while the screen is idle), so a frame-count
    // GOP can stretch in wall time; the duration limit keeps cut points bounded.
    let gop_seconds =
        cf::Number::from_f64(f64::from(params.idr_period) / f64::from(params.framerate));

    let optional: [(&str, &cf::String, &cf::Type); 7] = [
        ("RealTime", keys::real_time(), cf::Boolean::value_true()),
        (
            "ProfileLevel",
            keys::profile_lvl(),
            profile_level::h264::high_auto_lvl(),
        ),
        ("ExpectedFrameRate", keys::expected_frame_rate(), &framerate),
        (
            "MaxKeyFrameIntervalDuration",
            keys::max_key_frame_interval_duration(),
            &gop_seconds,
        ),
        (
            "ColorPrimaries",
            keys::color_primaries(),
            cv::image_buf_attachment::color_primaries::itu_r_709_2(),
        ),
        (
            "TransferFunction",
            keys::transfer_fn(),
            cv::image_buf_attachment::transfer_fn::itu_r_709_2(),
        ),
        (
            "YCbCrMatrix",
            keys::ycbcr_matrix(),
            cv::image_buf_attachment::ycbcr_matrix::itu_r_709_2(),
        ),
    ];
    for (name, key, value) in optional {
        if let Err(err) = session.set_prop(key, Some(value)) {
            tracing::warn!(%err, property = name, "optional VideoToolbox property rejected");
        }
    }
    Ok(())
}

/// Read back whether the session actually got the hardware encoder (informational —
/// creation already failed if `require_hardware` couldn't be satisfied).
fn log_hardware_usage(session: &vt::CompressionSession) {
    let key = unsafe { kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder };
    match session.prop(key) {
        Ok(Some(value)) => {
            // CFBoolean values are singletons, so pointer identity is the comparison.
            let got: &cf::Type = &value;
            let wanted: &cf::Type = cf::Boolean::value_true();
            if std::ptr::eq(got, wanted) {
                tracing::info!("VideoToolbox session using the hardware encoder");
            } else {
                tracing::info!("VideoToolbox session not confirmed on the hardware encoder");
            }
        }
        Ok(None) => tracing::info!("VideoToolbox session not confirmed on the hardware encoder"),
        Err(err) => tracing::debug!(%err, "could not query hardware-encoder usage"),
    }
}

extern "C" fn output_callback(
    ctx: *mut CallbackState,
    _src_frame_ref_con: *mut c_void,
    status: os::Status,
    info_flags: vt::EncodeInfoFlags,
    sample_buf: Option<&cm::SampleBuf>,
) {
    // SAFETY: `ctx` points at the encoder's boxed CallbackState, which outlives the
    // session (invalidated before the box drops).
    let Some(state) = (unsafe { ctx.as_ref() }) else {
        return;
    };
    // The encoder thread may have timed out and gone away; a dead channel is fine.
    let _ = state.tx.send(parse_output(status, info_flags, sample_buf));
}

/// Runs on VideoToolbox's callback thread, in submission order (reordering is off):
/// converts one output sample to a ready-to-push Annex-B access unit.
fn parse_output(
    status: os::Status,
    info_flags: vt::EncodeInfoFlags,
    sample_buf: Option<&cm::SampleBuf>,
) -> CallbackResult {
    if let Some(err) = status.error() {
        return Err(format!("encode callback reported {err}"));
    }
    if info_flags.contains(vt::EncodeInfoFlags::FRAME_DROPPED) {
        return Ok(None);
    }
    let Some(sample) = sample_buf else {
        return Ok(None);
    };

    // Sync iff the attachments don't say NotSync (absent array/key means sync).
    let is_keyframe = sample.is_key_frame();

    let format_desc = sample
        .format_desc()
        .ok_or_else(|| "output sample has no format description".to_owned())?;
    let (param_set_count, nal_length_size) = format_desc
        .h264_params_count_and_header_len()
        .map(|(count, header_len)| (count, usize::try_from(header_len).unwrap_or(4)))
        .unwrap_or((0, 4));

    let mut annexb = Vec::new();
    if is_keyframe {
        // SPS then PPS out of the format description, inline before the IDR so a clip
        // cut here is self-decodable.
        let mut sets = Vec::with_capacity(param_set_count);
        for index in 0..param_set_count {
            let set = format_desc
                .h264_param_set_at(index)
                .map_err(|e| format!("reading parameter set {index}: {e}"))?;
            sets.push(set.to_vec());
        }
        if sets.is_empty() {
            return Err("keyframe sample carries no parameter sets".to_owned());
        }
        annexb::prepend_parameter_sets(&sets, &mut annexb);
    }

    let data = sample
        .data_buf()
        .ok_or_else(|| "output sample has no data buffer".to_owned())?;
    let total_len = data.data_len();
    if data.is_range_contiguous(0, total_len) {
        let avcc = data.as_slice().map_err(|e| e.to_string())?;
        annexb::annexb_from_avcc(avcc, nal_length_size, &mut annexb)
    } else {
        let mut copied = vec![0u8; total_len];
        data.copy_to(0, &mut copied).map_err(|e| e.to_string())?;
        annexb::annexb_from_avcc(&copied, nal_length_size, &mut annexb)
    }
    .map_err(|e| e.to_string())?;

    if annexb.is_empty() {
        return Err("output sample converted to an empty access unit".to_owned());
    }

    Ok(Some(ParsedFrame {
        annexb,
        is_keyframe,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annexb::{annexb_from_avcc, prepend_parameter_sets};

    fn params(width: u32, height: u32) -> EncodeParams {
        EncodeParams {
            width,
            height,
            framerate: 30,
            bitrate_bps: 2_000_000,
            idr_period: 10,
        }
    }

    #[test]
    fn new_rejects_bad_params() {
        assert!(VideoToolboxEncoder::new(params(0, 720), false).is_err());
        assert!(VideoToolboxEncoder::new(params(1280, 0), false).is_err());
        let zero_fps = EncodeParams {
            framerate: 0,
            ..params(1280, 720)
        };
        assert!(VideoToolboxEncoder::new(zero_fps, false).is_err());
        let zero_gop = EncodeParams {
            idr_period: 0,
            ..params(1280, 720)
        };
        assert!(VideoToolboxEncoder::new(zero_gop, false).is_err());
    }

    /// Byte offsets and NAL types of every 4-byte start code in `data`.
    fn nal_types(data: &[u8]) -> Vec<u8> {
        data.windows(4)
            .enumerate()
            .filter(|(_, w)| *w == [0, 0, 0, 1])
            .map(|(i, _)| data[i + 4] & 0x1F)
            .collect()
    }

    /// Fill an NV12 pixel buffer with a solid tone whose luma tracks `tick`.
    fn solid_nv12_frame(width: usize, height: usize, tick: u8) -> arc::R<cv::PixelBuf> {
        let mut buf = cv::PixelBuf::new(
            width,
            height,
            cv::PixelFormat::_420_YP_CB_CR_8_BI_PLANAR_VIDEO_RANGE,
            None,
        )
        .expect("pixel buffer");
        let plane_dims = [(width, height), (width, height / 2)];
        let values = [16u8.wrapping_add(tick), 128u8];
        // SAFETY: lock/unlock bracket the writes; each write stays inside its plane.
        unsafe {
            buf.lock_base_addr(cv::pixel_buffer::LockFlags::DEFAULT)
                .result()
                .expect("lock");
            for (plane, (&(w, h), value)) in plane_dims.iter().zip(values).enumerate() {
                let stride = buf.plane_bytes_per_row(plane);
                let base = buf.plane_base_address(plane).cast_mut();
                for row in 0..h {
                    std::ptr::write_bytes(base.add(row * stride), value, w);
                }
            }
            buf.unlock_lock_base_addr(cv::pixel_buffer::LockFlags::DEFAULT)
                .result()
                .expect("unlock");
        }
        buf
    }

    /// Live end-to-end check on this machine's VideoToolbox: needs macOS media
    /// services, so it's ignored in CI like the GPU tests.
    #[test]
    #[ignore]
    fn live_encode_produces_annexb_with_inline_parameter_sets() {
        live_encode_roundtrip(false);
    }

    #[test]
    #[ignore]
    fn live_hardware_encode_produces_annexb_with_inline_parameter_sets() {
        live_encode_roundtrip(true);
    }

    fn live_encode_roundtrip(require_hardware: bool) {
        let params = EncodeParams {
            width: 640,
            height: 360,
            framerate: 30,
            bitrate_bps: 1_500_000,
            idr_period: 5,
        };
        let mut enc = VideoToolboxEncoder::new(params, require_hardware).expect("constructs");

        let mut keyframes = 0;
        for i in 0..12u64 {
            let frame = solid_nv12_frame(640, 360, u8::try_from(i * 8).unwrap());
            let pts = Duration::from_micros(i * 33_333);
            let force = i == 7;
            let chunk = enc
                .encode_pixel_buf(&frame, force, pts)
                .expect("encodes")
                .expect("no drops for a fed session");
            assert_eq!(chunk.pts, pts, "pts must ride through verbatim");
            let types = nal_types(&chunk.bytes);
            assert!(!types.is_empty(), "frame {i}: no NALUs");
            if chunk.is_keyframe {
                keyframes += 1;
                assert!(types.contains(&7), "keyframe {i} must carry SPS: {types:?}");
                assert!(types.contains(&8), "keyframe {i} must carry PPS: {types:?}");
                assert!(types.contains(&5), "keyframe {i} must carry an IDR slice");
            } else {
                assert!(
                    !types.contains(&5),
                    "delta frame {i} must not contain an IDR"
                );
            }
            if i == 0 || force {
                assert!(chunk.is_keyframe, "frame {i} was forced to be an IDR");
            }
        }
        // Frame 0, the forced frame 7, and the idr_period=5 cadence.
        assert!(keyframes >= 3, "expected several IDRs, got {keyframes}");
    }

    #[test]
    #[ignore]
    fn live_hardware_probe_runs() {
        // Result depends on the machine; just prove the probe doesn't wedge or crash.
        let available = hardware_h264_available();
        println!("hardware H.264 encoder available: {available}");
    }

    #[test]
    fn composed_keyframe_layout_matches_contract() {
        // Mirrors parse_output's keyframe path with synthetic bytes (no VT needed).
        let sps = vec![0x67u8, 0x42];
        let pps = vec![0x68u8, 0xCE];
        let idr = [0u8, 0, 0, 2, 0x65, 0xFF];
        let mut out = Vec::new();
        prepend_parameter_sets(&[sps, pps], &mut out);
        annexb_from_avcc(&idr, 4, &mut out).expect("converts");
        assert_eq!(nal_types(&out), [7, 8, 5]);
    }
}
