# 0014 — Encoder fallback: capability probe + software (CPU) H.264 path

## Status

Accepted.

## Context

Encoding runs through the gpu-video (Vulkan Video) H.264 backend (ADR 0001), which needs a
GPU **and** driver that expose `VK_KHR_video_encode_h264`: NVIDIA Turing+, AMD RDNA2+ on a
recent Mesa, Intel Arc / recent Xe. On anything older, `GpuContext::new` finds no encode
adapter and the recorder dies with a cryptic init error and no fallback. rewynd should still
record on those machines, and where it genuinely can't, fail with an actionable message.

The stream a clip is cut from has hard invariants (ring buffer + muxer, ADR 0002/0004): a
fixed GOP the ring can cut on, a correct keyframe flag per chunk, Annex-B bytes, and inline
SPS/PPS before every IDR so a cut clip is self-decodable. Any fallback encoder must produce
exactly that shape.

## Decision

- **CPU H.264 via `openh264` 0.9 (encode)**, alongside the decode use already in the settings
  crate (ADR 0013). BSD-2-Clause (already allow-listed in `deny.toml`), pure-CPU, no new build
  deps (the `openh264-sys2` from-source build is already proven in CI). x264 was rejected: its
  GPL license is viral against a closed release. No mature pure-Rust H.264 encoder exists
  (rav1e is AV1). Output is Constrained Baseline — broadly decodable, and a bonus: the
  library's own openh264 thumbnail/player decoder can decode CPU-encoded clips directly
  (unlike the High-profile NVENC stream, see `settings/src/player.rs`).

- **Two-layer design for CI coverage.** The pure core `SoftwareEncoder`
  (`encode/src/software.rs`) takes I420 planes in host memory and emits `EncodedChunk`s; it is
  fully unit-tested without a GPU (construction/validation, keyframe + SPS/PPS on every IDR
  across GOPs, forced keyframes, PTS passthrough, an openh264 decode round-trip) plus an
  integration test that muxes its output into a real MP4. The `SoftwareTextureEncoder` adapter
  (`encode/src/software_texture.rs`) reads an NV12 `wgpu::Texture` back to host memory,
  deinterleaves UV into I420, and calls the core; it needs a GPU, so it is coverage-excluded
  and covered by an `#[ignore]`d GPU test, exactly like the gpu-video backend.

- **Encoder settings**: `RateControlMode::Bitrate`, `skip_frames(false)` (the ring wants one
  chunk per captured frame — libopenh264 warns it therefore can't hold the bitrate ceiling
  strictly; we accept quality drift over dropped frames, and the muxer's real-PTS sample
  timing absorbs a below-realtime encoder as variable frame pacing), `intra_frame_period` =
  the configured GOP, `ScreenContentRealTime`, `ConstantId`, `num_threads(0)` (auto), and
  `VuiConfig::bt709()` (limited range) — matching the BT.709-limited NV12 the converter emits.

- **SPS/PPS guarantee.** The core caches SPS/PPS from the first output that carries them and
  prepends them to any later IDR access unit that lacks them, so the "inline SPS/PPS before
  every IDR" invariant holds regardless of libopenh264's re-emission behavior.

- **Selection (config → auto → CPU).** A `[video] encoder` config value (`"auto"`, `"cpu"`,
  or `"gpu:<adapter name>"`, default `auto`, env `REWYND_ENCODER`) plus a startup capability
  probe (`GpuContext::probe_adapters`) choose the backend. Auto prefers a GPU that can encode
  and falls back to the CPU with a non-fatal notification; a pinned GPU that is missing or
  can't encode falls back to auto with a warning. The chosen backend and detected capability
  are logged at startup. The GUI exposes the choice as a "Recording method" dropdown, listing
  only encode-capable GPUs, and shows a live recording-state pill fed by a small status file
  the recorder writes next to its pid file.

- **Perf posture.** 1080p60 software H.264 is CPU-heavy. Resolution/fps/bitrate stay
  parameters (never hard-capped); the recorder logs a warning and the GUI shows a hint when
  the CPU path is active, rather than silently downscaling.

## Consequences

- Machines without Vulkan Video encode record via the CPU path; machines without any Vulkan
  adapter still fail, now with an actionable message.
- The CPU encoder is real CI-covered code (unlike the GPU backend), raising confidence in the
  invariants the muxer depends on.
- macOS stays out of scope (gpu-video is Vulkan-only; Metal is a separate upstream TODO). The
  software path is gated to the same targets as the rest of the GPU stack.
- H.264 patent exposure is unchanged in kind from ADR 0013 (from-source build, our own
  recordings), now on the encode side for machines that lack a hardware encoder.
