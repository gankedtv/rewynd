//! Issue #3 deliverable: a `gpu-video` H.264 encoder constructs on the **same** wgpu
//! device as the rest of the pipeline.
//!
//! This needs a Vulkan GPU with H.264 encode support, so it is `#[ignore]`d (the CI
//! runner has no GPU). Run it on the dev box with:
//!
//! ```sh
//! cargo test -p rewynd-encode --test shared_device -- --ignored
//! ```
#![cfg(vulkan)]

use rewynd_encode::{EncodeParams, GpuVideoEncoder};
use rewynd_gpu::GpuContext;

#[test]
#[ignore = "requires a Vulkan GPU with H.264 encode; run with --ignored on the dev box"]
fn encoder_constructs_on_shared_device() {
    let gpu = pollster::block_on(GpuContext::new()).expect("create shared wgpu device");
    let encoder = GpuVideoEncoder::new(&gpu, EncodeParams::default())
        .expect("construct gpu-video encoder on the shared wgpu device");

    let params = encoder.params();
    assert_eq!(params.width, 1920);
    assert_eq!(params.height, 1080);
    assert_eq!(params.framerate, 60);
}
