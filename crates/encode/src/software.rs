//! Software (CPU) H.264 [`Encoder`] core, backed by libopenh264 (docs/adr/0014).
//!
//! Produces the exact stream shape the ring buffer and muxer require: Annex-B bytes,
//! inline SPS/PPS before every IDR, and a correct keyframe flag so a clip cut from the
//! buffer is self-decodable ([`Encoder`] contract, PLAN §3.3). Output is Constrained
//! Baseline, which is broadly decodable (including by the library's own thumbnail decoder).
//!
//! This module is the pure-CPU half: it takes I420 planes in host memory, so it is fully
//! unit-testable without a GPU. The [`Encoder`]-trait adapter that reads an NV12
//! `wgpu::Texture` back into these planes lives in `software_texture`.

use std::sync::Arc;
use std::time::Duration;

use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder as Openh264Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;
use rewynd_buffer::EncodedChunk;

use crate::{EncodeError, EncodeParams};

/// H.264 NAL unit types (ITU-T H.264 §7.4.1) the muxer needs to find inline before an IDR.
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
/// A canonical 4-byte Annex-B start code, used when re-emitting cached parameter sets.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Borrowed I420 (planar YUV 4:2:0) planes for one frame.
///
/// Planes are tightly packed: `y` is `width * height`, `u` and `v` are each
/// `(width / 2) * (height / 2)`. `width` and `height` must be even.
pub struct I420Frame<'a> {
    /// Luma plane, row-major, stride `width`.
    pub y: &'a [u8],
    /// Cb (blue-difference) chroma plane, stride `width / 2`.
    pub u: &'a [u8],
    /// Cr (red-difference) chroma plane, stride `width / 2`.
    pub v: &'a [u8],
    /// Frame width in pixels (even).
    pub width: u32,
    /// Frame height in pixels (even).
    pub height: u32,
}

/// CPU H.264 encoder. Consumes I420 frames, emits [`EncodedChunk`]s the ring buffer can cut.
pub struct SoftwareEncoder {
    inner: Openh264Encoder,
    params: EncodeParams,
    /// Parameter sets (each `START_CODE ++ payload`) cached from the first output that
    /// carries them, so we can guarantee they precede every IDR even if libopenh264
    /// stops re-emitting them on later IDRs.
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl SoftwareEncoder {
    /// Configure the encoder for `params`. Rejects zero/odd dimensions and a zero GOP,
    /// mirroring the GPU backend's up-front validation.
    pub fn new(params: EncodeParams) -> Result<Self, EncodeError> {
        if params.width == 0 || params.height == 0 {
            return Err(EncodeError::Init("width and height must be > 0".to_owned()));
        }
        if !params.width.is_multiple_of(2) || !params.height.is_multiple_of(2) {
            return Err(EncodeError::Init(
                "width and height must be even for I420".to_owned(),
            ));
        }
        if params.idr_period == 0 {
            return Err(EncodeError::Init("idr_period must be > 0".to_owned()));
        }

        // Bitrate rate control with frame-skip disabled: the ring buffer's PTS math wants a
        // chunk per captured frame, and a fixed GOP it can cut on. BT.709 limited-range VUI
        // matches the NV12 the converter produces upstream.
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(params.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(params.framerate as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .skip_frames(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(params.idr_period))
            .usage_type(UsageType::ScreenContentRealTime)
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .num_threads(0)
            .vui(VuiConfig::bt709());

        let inner = Openh264Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| EncodeError::Init(e.to_string()))?;

        Ok(Self {
            inner,
            params,
            sps: Vec::new(),
            pps: Vec::new(),
        })
    }

    /// The parameters this encoder was configured with.
    #[must_use]
    pub fn params(&self) -> EncodeParams {
        self.params
    }

    /// Encode one I420 frame captured at `pts`. `force_keyframe` forces an IDR here.
    pub fn encode_i420(
        &mut self,
        frame: I420Frame<'_>,
        force_keyframe: bool,
        pts: Duration,
    ) -> Result<EncodedChunk, EncodeError> {
        if force_keyframe {
            self.inner.force_intra_frame();
        }

        let (w, h) = (frame.width as usize, frame.height as usize);
        let slices = YUVSlices::new((frame.y, frame.u, frame.v), (w, h), (w, w / 2, w / 2));

        // The bitstream borrows the encoder, so pull owned bytes + type out before touching
        // any encoder-owned state again.
        let (mut bytes, is_keyframe) = {
            let bs = self
                .inner
                .encode_at(&slices, timestamp_from(pts))
                .map_err(|e| EncodeError::Encode(e.to_string()))?;
            (bs.to_vec(), bs.frame_type() == FrameType::IDR)
        };

        if bytes.is_empty() {
            return Err(EncodeError::Encode("encoder produced no output".to_owned()));
        }

        // Remember parameter sets whenever they appear so a later bare IDR can be repaired.
        if let Some(sps) = extract_nal(&bytes, NAL_SPS) {
            self.sps = sps;
        }
        if let Some(pps) = extract_nal(&bytes, NAL_PPS) {
            self.pps = pps;
        }
        if is_keyframe {
            bytes = ensure_parameter_sets(bytes, &self.sps, &self.pps);
        }

        Ok(EncodedChunk {
            bytes: Arc::from(bytes),
            is_keyframe,
            pts,
        })
    }
}

/// libopenh264 stamps timestamps in whole milliseconds; the exact `Duration` still rides on
/// the returned chunk, so this millisecond value only feeds the encoder's internal pacing.
fn timestamp_from(pts: Duration) -> openh264::Timestamp {
    openh264::Timestamp::from_millis(u64::try_from(pts.as_millis()).unwrap_or(u64::MAX))
}

/// Deinterleave an NV12 UV plane (`U,V,U,V,…`) into separate U and V planes, reusing the
/// destination buffers' allocations.
pub(crate) fn deinterleave_uv(uv: &[u8], u: &mut Vec<u8>, v: &mut Vec<u8>) {
    u.clear();
    v.clear();
    u.reserve(uv.len() / 2);
    v.reserve(uv.len() / 2);
    for pair in uv.chunks_exact(2) {
        u.push(pair[0]);
        v.push(pair[1]);
    }
}

/// If `bytes` (an IDR access unit) is missing SPS or PPS, prepend the cached ones so a clip
/// starting here is self-decodable. A no-op when both are already inline.
fn ensure_parameter_sets(bytes: Vec<u8>, sps: &[u8], pps: &[u8]) -> Vec<u8> {
    if contains_nal(&bytes, NAL_SPS) && contains_nal(&bytes, NAL_PPS) {
        return bytes;
    }
    let mut out = Vec::with_capacity(sps.len() + pps.len() + bytes.len());
    out.extend_from_slice(sps);
    out.extend_from_slice(pps);
    out.extend_from_slice(&bytes);
    out
}

/// Byte offsets of every Annex-B start-code prefix (`00 00 01`; a 4-byte `00 00 00 01`
/// matches at its trailing three bytes, so the NAL header still sits at `offset + 3`).
fn start_codes(data: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            offsets.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    offsets
}

/// `nal_unit_type` of the NAL beginning at start code `sc`.
fn nal_type_at(data: &[u8], sc: usize) -> Option<u8> {
    data.get(sc + 3).map(|b| b & 0x1f)
}

/// Whether an Annex-B stream contains a NAL of the given type.
fn contains_nal(data: &[u8], nal_type: u8) -> bool {
    start_codes(data)
        .into_iter()
        .any(|sc| nal_type_at(data, sc) == Some(nal_type))
}

/// Extract the first NAL of `nal_type` as `START_CODE ++ payload`, trailing zero bytes
/// trimmed (RBSP always ends in a non-zero stop bit, so this only sheds the next start
/// code's leading padding).
fn extract_nal(data: &[u8], nal_type: u8) -> Option<Vec<u8>> {
    let codes = start_codes(data);
    for (idx, &sc) in codes.iter().enumerate() {
        if nal_type_at(data, sc) != Some(nal_type) {
            continue;
        }
        let payload_start = sc + 3;
        let payload_end = codes.get(idx + 1).copied().unwrap_or(data.len());
        let mut payload = &data[payload_start..payload_end];
        while payload.last() == Some(&0) {
            payload = &payload[..payload.len() - 1];
        }
        let mut out = START_CODE.to_vec();
        out.extend_from_slice(payload);
        return Some(out);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(width: u32, height: u32, idr_period: u32) -> EncodeParams {
        EncodeParams {
            width,
            height,
            framerate: 30,
            bitrate_bps: 2_000_000,
            idr_period,
        }
    }

    /// A solid-colour I420 frame whose luma tracks `tick`, so successive frames differ and
    /// the encoder does real work rather than collapsing everything to skips.
    fn solid_frame(width: u32, height: u32, tick: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let (w, h) = (width as usize, height as usize);
        let y = vec![16u8.wrapping_add(tick); w * h];
        let u = vec![128u8; (w / 2) * (h / 2)];
        let v = vec![128u8; (w / 2) * (h / 2)];
        (y, u, v)
    }

    fn frame<'a>(
        planes: &'a (Vec<u8>, Vec<u8>, Vec<u8>),
        width: u32,
        height: u32,
    ) -> I420Frame<'a> {
        I420Frame {
            y: &planes.0,
            u: &planes.1,
            v: &planes.2,
            width,
            height,
        }
    }

    #[test]
    fn new_rejects_bad_params() {
        assert!(SoftwareEncoder::new(params(0, 64, 4)).is_err());
        assert!(SoftwareEncoder::new(params(65, 64, 4)).is_err());
        assert!(SoftwareEncoder::new(params(64, 64, 0)).is_err());
    }

    #[test]
    fn new_succeeds_and_reports_params() {
        let enc = SoftwareEncoder::new(params(64, 64, 4)).expect("constructs");
        assert_eq!(enc.params().width, 64);
        assert_eq!(enc.params().idr_period, 4);
    }

    #[test]
    fn first_chunk_is_keyframe_with_parameter_sets() {
        let mut enc = SoftwareEncoder::new(params(64, 64, 4)).expect("constructs");
        let planes = solid_frame(64, 64, 0);
        let chunk = enc
            .encode_i420(frame(&planes, 64, 64), false, Duration::ZERO)
            .expect("encodes");
        assert!(chunk.is_keyframe);
        assert!(contains_nal(&chunk.bytes, NAL_SPS), "IDR must carry SPS");
        assert!(contains_nal(&chunk.bytes, NAL_PPS), "IDR must carry PPS");
    }

    #[test]
    fn every_idr_carries_parameter_sets_across_gops() {
        let idr_period = 4;
        let mut enc = SoftwareEncoder::new(params(64, 64, idr_period)).expect("constructs");
        for i in 0..(idr_period * 3) {
            let planes = solid_frame(64, 64, i as u8);
            let pts = Duration::from_millis(u64::from(i) * 33);
            let chunk = enc
                .encode_i420(frame(&planes, 64, 64), false, pts)
                .expect("encodes");
            if i % idr_period == 0 {
                assert!(chunk.is_keyframe, "frame {i} should be an IDR");
                assert!(contains_nal(&chunk.bytes, NAL_SPS), "IDR {i} needs SPS");
                assert!(contains_nal(&chunk.bytes, NAL_PPS), "IDR {i} needs PPS");
            } else {
                assert!(!chunk.is_keyframe, "frame {i} should be a delta");
            }
        }
    }

    #[test]
    fn force_keyframe_mid_gop_emits_idr() {
        let mut enc = SoftwareEncoder::new(params(64, 64, 100)).expect("constructs");
        // Prime the stream (frame 0 is always an IDR).
        let p0 = solid_frame(64, 64, 0);
        enc.encode_i420(frame(&p0, 64, 64), false, Duration::ZERO)
            .expect("encodes");
        let p1 = solid_frame(64, 64, 1);
        let delta = enc
            .encode_i420(frame(&p1, 64, 64), false, Duration::from_millis(33))
            .expect("encodes");
        assert!(!delta.is_keyframe);
        let p2 = solid_frame(64, 64, 2);
        let forced = enc
            .encode_i420(frame(&p2, 64, 64), true, Duration::from_millis(66))
            .expect("encodes");
        assert!(forced.is_keyframe);
        assert!(contains_nal(&forced.bytes, NAL_SPS));
        assert!(contains_nal(&forced.bytes, NAL_PPS));
    }

    #[test]
    fn pts_is_passed_through_verbatim() {
        let mut enc = SoftwareEncoder::new(params(64, 64, 4)).expect("constructs");
        let planes = solid_frame(64, 64, 0);
        let odd = Duration::from_nanos(16_666_667);
        let chunk = enc
            .encode_i420(frame(&planes, 64, 64), false, odd)
            .expect("encodes");
        assert_eq!(chunk.pts, odd);
    }

    #[test]
    fn deinterleave_uv_splits_planes() {
        let uv = [10u8, 20, 11, 21, 12, 22];
        let mut u = Vec::new();
        let mut v = Vec::new();
        deinterleave_uv(&uv, &mut u, &mut v);
        assert_eq!(u, [10, 11, 12]);
        assert_eq!(v, [20, 21, 22]);
    }

    #[test]
    fn nal_scanner_handles_both_start_code_lengths() {
        // 3-byte start code + SPS (type 7), then 4-byte start code + PPS (type 8).
        let data = [0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB];
        assert!(contains_nal(&data, NAL_SPS));
        assert!(contains_nal(&data, NAL_PPS));
        assert!(!contains_nal(&data, 5));
        let sps = extract_nal(&data, NAL_SPS).expect("has sps");
        assert_eq!(sps, [0, 0, 0, 1, 0x67, 0xAA]);
    }

    #[test]
    fn ensure_parameter_sets_prepends_only_when_missing() {
        let sps = [0u8, 0, 0, 1, 0x67, 0xAA];
        let pps = [0u8, 0, 0, 1, 0x68, 0xBB];
        // A bare IDR (type 5) gets both prepended.
        let bare = vec![0, 0, 0, 1, 0x65, 0xCC];
        let fixed = ensure_parameter_sets(bare, &sps, &pps);
        assert!(contains_nal(&fixed, NAL_SPS));
        assert!(contains_nal(&fixed, NAL_PPS));
        assert_eq!(&fixed[..6], &sps);
        // An already-complete access unit is returned untouched.
        let complete = {
            let mut b = Vec::new();
            b.extend_from_slice(&sps);
            b.extend_from_slice(&pps);
            b.extend_from_slice(&[0, 0, 0, 1, 0x65, 0xCC]);
            b
        };
        let same = ensure_parameter_sets(complete.clone(), &sps, &pps);
        assert_eq!(same, complete);
    }

    #[test]
    fn output_decodes_back_to_the_source_dimensions() {
        use openh264::decoder::Decoder;
        use openh264::formats::YUVSource;

        let mut enc = SoftwareEncoder::new(params(64, 64, 4)).expect("constructs");
        let mut dec = Decoder::new().expect("decoder");
        let mut decoded_any = false;
        for i in 0..8u8 {
            let planes = solid_frame(64, 64, i);
            let pts = Duration::from_millis(u64::from(i) * 33);
            let chunk = enc
                .encode_i420(frame(&planes, 64, 64), false, pts)
                .expect("encodes");
            if let Some(yuv) = dec.decode(&chunk.bytes).expect("decodes") {
                let (w, h) = yuv.dimensions();
                assert_eq!((w, h), (64, 64));
                decoded_any = true;
            }
        }
        assert!(decoded_any, "decoder should yield at least one frame");
    }
}
