# ADR 0001 — wgpu version coordination

- **Status:** Proposed (pending local build verification on the Linux dev box — see [Verification gate](#verification-gate))
- **Date:** 2026-06-29
- **Issue:** [#1 Resolve wgpu version coordination](https://github.com/gankedtv/rewynd/issues/1)
- **Deciders:** Turbootzz

## Context

rewynd keeps every frame GPU-resident from OS capture through hardware H.264 encode (zero-copy). The encoder is Software Mansion's `gpu-video` crate (Vulkan Video → NVENC/VAAPI), which takes input frames as `wgpu::Texture` in `NV12`. Our capture layer must import OS-captured GPU memory (a PipeWire DMA-BUF fd on Linux, a D3D11 shared NT handle on Windows) **into a `wgpu::Texture`** and hand it to that encoder.

The decisive constraint (PLAN §5): for a captured texture to be accepted by `gpu-video`, **both crates must compile against the exact same `wgpu` source**. If rewynd builds against a different `wgpu` than `gpu-video`, the `wgpu::Texture` / `wgpu::Device` types are distinct Rust types and will not interoperate — the pipeline cannot be wired at all. This blocks every other issue.

The import functions we need live in `wgpu-hal` (the unsafe Vulkan interop layer):

- `wgpu_hal::vulkan::Device::texture_from_dmabuf_fd` — Linux DMA-BUF import
- `wgpu_hal::vulkan::Device::texture_from_d3d11_shared_handle` — Windows D3D11 shared-handle import
- `wgpu_hal::vulkan::Queue::add_wait_semaphore` — external-semaphore sync (so we wait for capture to finish writing before encoding)
- `wgpu::Device::create_texture_from_hal` — wrap the imported hal texture back into a `wgpu::Texture`

## Findings (verified against primary sources, mid-2026)

Researched via multi-source web probes + an adversarial re-fetch of raw GitHub source and the GitHub API. Load-bearing facts were re-verified directly in source, not from changelog/summarizer text (the wgpu CHANGELOG mis-attributes the DMABUF feature — see [Notes](#notes)).

### `gpu-video` pins wgpu differently in the published crate vs git master

| Source | wgpu dependency | Has interop fns? |
| --- | --- | --- |
| `gpu-video` **0.4.0** (crates.io) | crates.io `wgpu = "^29.0.0"` → resolves to **29.0.3** | **No** — 29.0.3's `wgpu-hal` Vulkan `Device` exposes only `texture_from_raw` |
| `gpu-video` **git master** (smelter) | `wgpu = { git = "https://github.com/gfx-rs/wgpu", rev = "1503796" }` | **Yes** — see below |

The switch to the git rev happened **after** 0.4.0 was published: smelter PR #2025 *"Use wgpu from master branch"* (2026-05-27); 0.4.0 release was PR #1984 (2026-05-12). A later device/adapter API change landed in smelter PR #2039 (2026-06-18).

### The pinned wgpu rev `1503796` already contains every interop fn

`wgpu` git rev **`1503796`** (full SHA `1503796ad4cf16f1a8010fbcf2fa09e626b198f6`, trunk commit dated 2026-05-26, wgpu in-dev version `29.0.0`). Confirmed present in source at that exact rev:

| Function | Location @ `1503796` | Notes |
| --- | --- | --- |
| `texture_from_dmabuf_fd(fd: OwnedFd, desc, drm_modifier: u64, stride: u64, offset: u64) -> Result<Texture, DeviceError>` | `wgpu-hal/src/vulkan/device.rs` | `#[cfg(unix)]`; needs `Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF`; **single-plane DMA-bufs only** |
| `texture_from_d3d11_shared_handle(HANDLE, desc) -> Result<Texture, DeviceError>` | `wgpu-hal/src/vulkan/device.rs` | `#[cfg(windows)]`; needs `Features::VULKAN_EXTERNAL_MEMORY_WIN32` |
| `Queue::add_wait_semaphore(semaphore, Option<u64>, stage)` (+ `remove_wait_semaphore`) | `wgpu-hal/src/vulkan/mod.rs` | `None` = binary, `Some(v)` = timeline |
| `texture_from_raw(vk_image, desc, drop_callback, memory)` | `wgpu-hal/src/vulkan/device.rs` | also in 29.0.3; used internally by `gpu-video` |
| `create_texture_from_hal::<Vulkan>(hal_texture, desc, initial_state)` | wgpu-core | **breaking** (PR #9496): now requires explicit `initial_state: wgt::TextureUses`; pass the real imported state, not `UNINITIALIZED`, or zero-copy content is discarded |

`gpu-video`'s own encoder uses the same surface (`as_hal::<Vulkan>()` + `texture_from_raw` + `create_texture_from_hal`), so the interop contract is shared at this rev. `git compare 1503796...trunk` is `behind_by: 0` (a clean ancestor of trunk).

### `gpu-video` API surface we will call (confirmed on 0.4.0 docs + master source)

- `create_wgpu_textures_encoder_h264(&self, queue: &wgpu::Queue, parameters: EncoderParametersH264) -> Result<WgpuTexturesEncoderH264, _>` (confirm the `queue` arg on the exact pinned commit)
- `WgpuTexturesEncoderH264::encode(InputFrame<wgpu::Texture>, force_keyframe: bool) -> Result<EncodedOutputChunk<Vec<u8>>, _>` — input must be `wgpu::TextureFormat::NV12` (else `NotNV12Texture`)
- `EncoderOutputParameters { idr_period: Option<NonZeroU32>, inline_stream_params: Option<bool>, .. }`
- `sps()` / `pps()` → Annex-B NAL units (out-of-band param sets for MP4 headers)
- `WgpuRgbaToNv12Converter` (public struct) for RGBA→NV12 — **there is no public free `rgba_to_nv12` fn**; the `wgpu_helpers` module is `pub(crate)`. Relevant to issue #8.

## Decision

**Pin the entire rewynd workspace to a single `wgpu` source: git rev `1503796`, and consume `gpu-video` from git (a pinned commit ≥ smelter PR #2025), not the published 0.4.0 crate.** Add a `[patch.crates-io]` block redirecting all wgpu crates to that rev so no transitive dependency can pull a second, incompatible `wgpu`.

```toml
# workspace Cargo.toml

[workspace.dependencies]
# gpu-video from git — the published 0.4.0 uses crates.io wgpu 29.0.x and lacks the interop fns.
gpu-video = { git = "https://github.com/software-mansion/smelter", rev = "<pin a commit >= PR #2025; see open questions>" }
wgpu      = { git = "https://github.com/gfx-rs/wgpu", rev = "1503796" }

# Force every wgpu/naga crate in the tree to the same source as gpu-video, so wgpu::Texture types unify.
[patch.crates-io]
wgpu       = { git = "https://github.com/gfx-rs/wgpu", rev = "1503796" }
wgpu-hal   = { git = "https://github.com/gfx-rs/wgpu", rev = "1503796" }
wgpu-core  = { git = "https://github.com/gfx-rs/wgpu", rev = "1503796" }
wgpu-types = { git = "https://github.com/gfx-rs/wgpu", rev = "1503796" }
naga       = { git = "https://github.com/gfx-rs/wgpu", rev = "1503796" }
```

**Rejected: crates.io `wgpu` 29.0.x (and the published `gpu-video` 0.4.0).** 29.0.3 exposes only `texture_from_raw`; the DMA-BUF / D3D11 / semaphore imports are trunk-only. That path both lacks the functions and mismatches the git-rev type identity. This is strategy (1) *"pinned rev already has interop"* from PLAN §5 — but specifically via git `gpu-video`, not the crate.

This is an **ADR-worthy pin**: any future bump of `wgpu` or `gpu-video` must be done in lockstep and recorded here.

## Verification gate

The web proves the functions exist *in source* at the rev. It cannot prove the workspace **builds and links** on the target hardware. This Claude environment is macOS (out of scope, no Vulkan Video), so the following **must be run by the author on the Linux box (CachyOS / RTX 3080 Ti)** — they belong with issue #2 (scaffold) and #3 (shared device):

- [ ] `cargo build` / `cargo check` the scaffolded workspace against `wgpu` rev `1503796` + git `gpu-video`.
- [ ] `cargo tree -i wgpu` (and `wgpu-hal`, `wgpu-core`, `wgpu-types`, `naga`) shows **exactly one** source for each — no crates.io `wgpu` sneaking in transitively.
- [ ] Pin the exact `gpu-video` commit (≥ PR #2025 so it uses wgpu `1503796`); decide whether to include PR #2039's device/adapter API change, and confirm the `create_wgpu_textures_encoder_h264` signature on that commit.
- [ ] Confirm the wgpu device is created with `VULKAN_EXTERNAL_MEMORY_DMA_BUF` / `_FD` (Linux) — and that the NVIDIA adapter actually advertises them — before relying on import at runtime (relevant to #3, #5). Confirm whether `Adapter::open_with_callback` (PLAN §6.1) is still the path to enable extensions on the post-#2039 API.

## Consequences

**Positive**
- A single, internally-consistent `wgpu` across capture import and `gpu-video` encode — the types unify, the pipeline can be wired (unblocks #2–#18).
- No fork/patch of `gpu-video`'s own deps needed (strategy 3 avoided); we ride its existing pin.
- `WgpuRgbaToNv12Converter` exists, so #8 likely needs no hand-written shader.

**Negative / risks**
- **Unreleased APIs.** The interop fns are trunk-only; we cannot move to a crates.io `wgpu` release until they land in one. Exit path = a future wgpu release that ships these imports.
- **Moving target.** `gpu-video` master is ahead of 0.4.0 and already changed its device/adapter API (#2039). Pin a specific commit; bump deliberately.
- **Single-plane DMA-buf limit.** `texture_from_dmabuf_fd` is single-plane only; PipeWire NV12/multi-plane frames need per-plane handling or a conversion step (integration constraint for #5/#8).
- **`create_texture_from_hal` breaking change.** Import code must pass the correct `initial_state` or zero-copy content is discarded.
- **Windows path source-confirmed only.** The D3D11 import can't be validated on the Linux dev box (#7 will shake it out).

## Notes

- The wgpu CHANGELOG mis-attributes DMA-BUF import to PR #9412 (`By @TODO`); #9412 is actually the SHADER_I16 PR. The real DMA-BUF PR is **#9366** (merged 2026-04-09). `add_wait_semaphore` = PR #9461 (2026-04-29). D3D11 shared-handle = PR #6161. Do not trust changelog links here; source presence at the rev is what matters.
- Reference for the capture→import data contract: `libscreencapture-wayland`'s `DmaBufFrame` maps ~1:1 onto `texture_from_dmabuf_fd`'s args (DRM fourcc + modifier + per-plane fd/offset/stride).
