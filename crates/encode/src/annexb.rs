//! Byte-level AVCC → Annex-B conversion helpers.
//!
//! VideoToolbox emits AVCC samples (big-endian length-prefixed NALUs, parameter sets
//! out-of-band in the format description), but the ring buffer and muxer expect
//! Annex-B access units with inline SPS/PPS before every IDR ([`crate::Encoder`]
//! contract, PLAN §3.3). These helpers are pure byte logic — no platform types — so
//! they compile and are unit-tested everywhere.

use thiserror::Error;

/// The canonical 4-byte Annex-B start code emitted before every NALU.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Errors from walking an AVCC (length-prefixed) buffer.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AvccError {
    /// The NAL length prefix size must be 1–4 bytes.
    #[error("invalid nal_length_size {0}: must be 1..=4")]
    InvalidNalLengthSize(usize),
    /// The buffer ended in the middle of a length prefix.
    #[error("truncated NALU length prefix at offset {0}")]
    TruncatedLengthPrefix(usize),
    /// A length prefix pointed past the end of the buffer.
    #[error("truncated NALU at offset {offset}: length {len} exceeds remaining {remaining} bytes")]
    TruncatedNalu {
        /// Offset of the NALU's length prefix.
        offset: usize,
        /// Payload length the prefix declared.
        len: usize,
        /// Bytes actually remaining after the prefix.
        remaining: usize,
    },
    /// A length prefix declared a zero-length NALU.
    #[error("zero-length NALU at offset {0}")]
    ZeroLengthNalu(usize),
}

/// Re-emit an AVCC buffer (`nal_length_size`-byte big-endian length prefix per NALU) as
/// Annex-B: a 4-byte `00 00 00 01` start code followed by the payload, for each NALU.
/// Appends to `out`; an empty input appends nothing.
pub fn annexb_from_avcc(
    avcc: &[u8],
    nal_length_size: usize,
    out: &mut Vec<u8>,
) -> Result<(), AvccError> {
    if !(1..=4).contains(&nal_length_size) {
        return Err(AvccError::InvalidNalLengthSize(nal_length_size));
    }

    let mut offset = 0;
    while offset < avcc.len() {
        if avcc.len() - offset < nal_length_size {
            return Err(AvccError::TruncatedLengthPrefix(offset));
        }
        let mut len = 0usize;
        for &byte in &avcc[offset..offset + nal_length_size] {
            len = (len << 8) | usize::from(byte);
        }
        if len == 0 {
            return Err(AvccError::ZeroLengthNalu(offset));
        }
        let payload_start = offset + nal_length_size;
        let remaining = avcc.len() - payload_start;
        if len > remaining {
            return Err(AvccError::TruncatedNalu {
                offset,
                len,
                remaining,
            });
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&avcc[payload_start..payload_start + len]);
        offset = payload_start + len;
    }
    Ok(())
}

/// Emit raw parameter-set payloads (no start codes in the input) as Annex-B NALUs,
/// each prefixed with a 4-byte start code. Appended to `out` — call before
/// [`annexb_from_avcc`] so SPS/PPS precede the IDR slice in the access unit.
pub fn prepend_parameter_sets(parameter_sets: &[Vec<u8>], out: &mut Vec<u8>) {
    for set in parameter_sets {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(set);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an AVCC buffer from payloads with the given prefix size.
    fn avcc(payloads: &[&[u8]], nal_length_size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for p in payloads {
            let len = u32::try_from(p.len()).expect("test payload fits u32");
            out.extend_from_slice(&len.to_be_bytes()[4 - nal_length_size..]);
            out.extend_from_slice(p);
        }
        out
    }

    #[test]
    fn converts_multiple_nalus_with_4_byte_lengths() {
        let idr: &[u8] = &[0x65, 0xAA, 0xBB];
        let sei: &[u8] = &[0x06, 0x01];
        let input = avcc(&[sei, idr], 4);
        let mut out = Vec::new();
        annexb_from_avcc(&input, 4, &mut out).expect("converts");
        assert_eq!(out, [&START_CODE[..], sei, &START_CODE[..], idr].concat());
    }

    #[test]
    fn converts_1_and_2_byte_lengths() {
        for nal_length_size in [1usize, 2] {
            let a: &[u8] = &[0x41, 0x01, 0x02];
            let b: &[u8] = &[0x41, 0x03];
            let input = avcc(&[a, b], nal_length_size);
            let mut out = Vec::new();
            annexb_from_avcc(&input, nal_length_size, &mut out)
                .unwrap_or_else(|e| panic!("size {nal_length_size}: {e}"));
            assert_eq!(
                out,
                [&START_CODE[..], a, &START_CODE[..], b].concat(),
                "nal_length_size {nal_length_size}"
            );
        }
    }

    #[test]
    fn empty_input_appends_nothing() {
        let mut out = vec![0xFFu8];
        annexb_from_avcc(&[], 4, &mut out).expect("empty ok");
        assert_eq!(out, [0xFF]);
    }

    #[test]
    fn appends_after_existing_content() {
        let mut out = vec![1u8, 2, 3];
        let input = avcc(&[&[0x65, 0x00]], 4);
        annexb_from_avcc(&input, 4, &mut out).expect("converts");
        assert_eq!(out[..3], [1, 2, 3]);
        assert_eq!(out[3..], [0, 0, 0, 1, 0x65, 0x00]);
    }

    #[test]
    fn rejects_invalid_nal_length_size() {
        let mut out = Vec::new();
        assert_eq!(
            annexb_from_avcc(&[0], 0, &mut out),
            Err(AvccError::InvalidNalLengthSize(0))
        );
        assert_eq!(
            annexb_from_avcc(&[0], 5, &mut out),
            Err(AvccError::InvalidNalLengthSize(5))
        );
    }

    #[test]
    fn rejects_truncated_length_prefix() {
        // One complete NALU, then 2 stray bytes that can't hold a 4-byte prefix.
        let mut input = avcc(&[&[0x65, 0x01]], 4);
        input.extend_from_slice(&[0, 0]);
        let mut out = Vec::new();
        assert_eq!(
            annexb_from_avcc(&input, 4, &mut out),
            Err(AvccError::TruncatedLengthPrefix(6))
        );
    }

    #[test]
    fn rejects_truncated_nalu() {
        // Prefix says 5 bytes, only 2 present.
        let input = [0u8, 0, 0, 5, 0x65, 0x01];
        let mut out = Vec::new();
        assert_eq!(
            annexb_from_avcc(&input, 4, &mut out),
            Err(AvccError::TruncatedNalu {
                offset: 0,
                len: 5,
                remaining: 2,
            })
        );
    }

    #[test]
    fn rejects_zero_length_nalu() {
        let input = [0u8, 0, 0, 0, 0x65];
        let mut out = Vec::new();
        assert_eq!(
            annexb_from_avcc(&input, 4, &mut out),
            Err(AvccError::ZeroLengthNalu(0))
        );
    }

    #[test]
    fn error_messages_name_the_cause() {
        assert_eq!(
            AvccError::InvalidNalLengthSize(7).to_string(),
            "invalid nal_length_size 7: must be 1..=4"
        );
        assert_eq!(
            AvccError::ZeroLengthNalu(3).to_string(),
            "zero-length NALU at offset 3"
        );
    }

    #[test]
    fn emits_parameter_sets_with_start_codes() {
        let sps = vec![0x67u8, 0x64, 0x00];
        let pps = vec![0x68u8, 0xEE];
        let mut out = Vec::new();
        prepend_parameter_sets(&[sps.clone(), pps.clone()], &mut out);
        assert_eq!(out, [&START_CODE[..], &sps, &START_CODE[..], &pps].concat());
    }

    #[test]
    fn no_parameter_sets_emits_nothing() {
        let mut out = Vec::new();
        prepend_parameter_sets(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn idr_chunk_layout_is_sps_pps_then_slices() {
        // Compose exactly what the VideoToolbox backend does for a keyframe:
        // parameter sets first, then the converted AVCC sample.
        let sps = vec![0x67u8, 0x64, 0x00, 0x2A];
        let pps = vec![0x68u8, 0xEE, 0x3C];
        let idr: &[u8] = &[0x65, 0x88, 0x84, 0x00];
        let sample = avcc(&[idr], 4);

        let mut chunk = Vec::new();
        prepend_parameter_sets(&[sps.clone(), pps.clone()], &mut chunk);
        annexb_from_avcc(&sample, 4, &mut chunk).expect("converts");

        let expected = [
            &START_CODE[..],
            &sps,
            &START_CODE[..],
            &pps,
            &START_CODE[..],
            idr,
        ]
        .concat();
        assert_eq!(chunk, expected);

        // NAL types in order: SPS (7), PPS (8), IDR (5) — a self-decodable clip start.
        let types: Vec<u8> = chunk
            .windows(4)
            .enumerate()
            .filter(|(_, w)| *w == START_CODE)
            .map(|(i, _)| chunk[i + 4] & 0x1F)
            .collect();
        assert_eq!(types, [7, 8, 5]);
    }
}
