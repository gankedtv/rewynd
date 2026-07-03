//! The software encoder's output must satisfy the ring-buffer cut + muxer invariants
//! (keyframe-first, inline SPS/PPS) and produce a real, decodable MP4 — all without a GPU.

use std::time::Duration;

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use rewynd_buffer::RingBuffer;
use rewynd_encode::{EncodeParams, I420Frame, SoftwareEncoder};
use rewynd_mux::Mp4Muxer;
use rewynd_mux::read::first_keyframe_annexb;

const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;
const FPS: u32 = 30;
const IDR_PERIOD: u32 = 4;

fn solid(tick: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    (
        vec![16u8.wrapping_add(tick); w * h],
        vec![128u8; (w / 2) * (h / 2)],
        vec![128u8; (w / 2) * (h / 2)],
    )
}

#[test]
fn software_output_muxes_and_decodes() {
    let params = EncodeParams {
        width: WIDTH,
        height: HEIGHT,
        framerate: FPS,
        bitrate_bps: 2_000_000,
        idr_period: IDR_PERIOD,
    };
    let mut enc = SoftwareEncoder::new(params).expect("constructs");
    let mut ring = RingBuffer::new(Duration::from_secs(10));

    for i in 0..(IDR_PERIOD * 3) {
        let (y, u, v) = solid(i as u8);
        let chunk = enc
            .encode_i420(
                I420Frame {
                    y: &y,
                    u: &u,
                    v: &v,
                    width: WIDTH,
                    height: HEIGHT,
                },
                i == 0,
                Duration::from_millis(u64::from(i) * 33),
            )
            .expect("encodes");
        ring.push(chunk);
    }

    // A cut from the ring starts on a keyframe...
    let chunks = ring.flush_last(Duration::from_secs(5)).expect("cut");
    assert!(chunks.first().expect("non-empty").is_keyframe);

    // ...muxes into a valid MP4 (write_mp4 enforces keyframe-first + inline SPS/PPS)...
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("clip.mp4");
    Mp4Muxer::new(WIDTH, HEIGHT, FPS)
        .write_mp4(&chunks, &path)
        .expect("mux");

    // ...and the first keyframe decodes back to the source dimensions.
    let annexb = first_keyframe_annexb(&path).expect("keyframe");
    let mut dec = Decoder::new().expect("decoder");
    let yuv = dec
        .decode(&annexb)
        .expect("decode")
        .expect("keyframe yields a frame");
    assert_eq!(yuv.dimensions(), (WIDTH as usize, HEIGHT as usize));
}
