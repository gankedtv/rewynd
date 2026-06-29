# Rewynd — Project Plan & Build Spec

*Rewynd — a lightweight instant-replay clip recorder: it keeps the last 60 seconds on the GPU and writes a clip on a hotkey.*

> **For the human:** drop this file in the repo root (or `docs/PLAN.md`) and hand it to Claude Code.
> **For Claude Code:** read this entire document before doing anything. It is the single source of truth for *what* we are building and *why* every major decision was made. Do not re-litigate the locked decisions in §3 without flagging it explicitly. Your **first task** is §10 (create the GitHub milestones and issues). After that, work phase by phase per §7, respecting the guardrails in §9.

---

## 1. Mission

**Rewynd** is a lightweight, native, cross-platform (**Linux + Windows**, macOS explicitly out of scope) "instant replay" clip recorder for gameplay. It continuously keeps the **last 60 seconds** of the screen in a GPU-encoded ring buffer; on a hotkey it flushes that buffer to an MP4. The whole point is **low resource usage** — it must be comfortable to run while gaming on lower-end PCs, which means the frame stays on the GPU from capture through hardware encode (zero-copy), never round-tripping through the CPU.

It is **general-purpose first**: it produces standalone video files and is not tied to any platform. Integration with **ganked.tv** (the author's clip-sharing platform) — auto-upload triggered from the UI — is a planned *later* feature, not a dependency and not part of the core identity. Build the general-purpose recorder first; add the ganked.tv connection afterward.

**This is a hobby project built with AI assistance.** The author has shipped many projects across different stacks (see github.com/Turbootzz). Be direct and technical about trade-offs.

---

## 2. Scope

### MVP (the bar for "this works and is fun")
- **Linux-first**, single monitor, **video only** (no audio yet), fixed 60s buffer, one global hotkey → MP4 written to disk, clip starts cleanly on a keyframe and is independently playable.

### Target quality
- **1080p at 60fps** is the primary target. This must **not** be hard-coded: resolution, framerate, and bitrate are parameters so other qualities can be added later (the goal is "1080p60 for now, other qualities possible").
- Phase 0's 720p result was only a smoke test; 1080p60 is the real target.
- The ring buffer holds **encoded H.264**, so ~60s at a sane 1080p60 bitrate is only tens of MB of RAM, not raw frames. Keep it that way — never buffer uncompressed frames.

### Full v1
- Windows parity, audio + A/V sync, configurable buffer length / hotkey / output dir, a minimal tray/overlay, optional ganked.tv upload.

### Non-goals (do **not** build these unless explicitly told)
- macOS / Apple Silicon support. (Rationale in §3.) Treat as "maybe someday, separate backend."
- A full OBS-style compositor (multiple sources, scenes, overlays mixing). We capture **one** source and encode it.
- Streaming/RTMP, editing UI, cloud features beyond a single upload call.
- CPU/software encoding paths as a primary mode.

---

## 3. Locked decisions (with rationale)

These were settled through deliberate analysis. Treat them as decided. If you believe one is wrong, **stop and raise it** rather than silently diverging.

### 3.1 Language: **Rust**
Author's strength, fits the low-level/low-resource goal, and the ecosystem now has the pieces we need (below).

### 3.2 GPU abstraction: **wgpu**
We are on wgpu, **not** raw `ash`, because our encode library rides on wgpu. Bonus: wgpu is cross-platform (Vulkan/DX12/Metal) and far more ergonomic than raw Vulkan. We only drop to `wgpu-hal` (the unsafe interop layer) for the capture-import seam (§6.1).

### 3.3 Encode: **`gpu-video` crate** (Software Mansion, part of the `smelter` repo)
- Hardware H.264 encode via **Vulkan Video → NVENC/VAAPI**. Frames stay in GPU memory (zero-copy) via wgpu integration. Works on Linux (NVIDIA/AMD) and Windows out of the box.
- **PROVEN on the author's hardware (RTX 3080 Ti, CachyOS):** Phase 0 spike succeeded — 120 NV12 frames in → valid playable 1280×720 H.264 out, device created on the NVIDIA driver, no software-encoder fallback exists in the crate (so the path is genuinely Vulkan Video → NVENC).
- **The crate exposes exactly the four building blocks the ring buffer needs** (verified by reading the source at the pinned version):
  - **wgpu-texture encoder:** `device.create_wgpu_textures_encoder_h264(&queue, params)` → `WgpuTexturesEncoderH264`; method `encode(frame: InputFrame<wgpu::Texture>, force_keyframe: bool) -> EncodedOutputChunk<Vec<u8>>`.
  - **On-demand forced IDR:** the `force_keyframe: bool` arg. Force one at the frame where a clip should begin.
  - **Configurable GOP:** `EncoderOutputParameters.idr_period: Option<NonZeroU32>` (default ~30).
  - **Inline SPS/PPS:** `inline_stream_params: Option<bool>` (default `true`) — prepends parameter sets before each IDR so a clip cut from the buffer is self-decodable. (Or fetch out-of-band via `.sps()` / `.pps()` / `.vps()` for MP4 headers.)
  - Constraints: the video device must be created **with** a wgpu device (else `VideoDeviceWithoutWgpu`), and the input texture must be `wgpu::TextureFormat::NV12`.

### 3.4 Why not the alternatives (so they don't get reconsidered)
- **FFmpeg/libav** — the pragmatic, more-mature option (encode + mux + audio in one, NVENC/VAAPI/VideoToolbox). **Deliberately not chosen** because the author wants the pure-Rust + Vulkan + zero-copy path as the project's point. **Keep FFmpeg as the documented fallback** if `gpu-video` proves too immature (it is young, H.264-only). FFmpeg is LGPL-by-default (dynamic linking is fine for distribution) and invoking the `ffmpeg` *binary* for muxing only is also license-clean.
- **Dennis de Vulder's `gpu-vulkan` (github.com/dennisdevulder/gpu-vulkan), `feat/vulkan-video-recorder` branch** — a working GPU replay buffer + hotkey MP4 + Vulkan Video encode, BUT it is Java/LWJGL, a RuneLite *plugin* (not a library), and its capture is coupled to RuneLite's own renderer. **It is a reference and a knowledgeable human to ask, not a dependency.** Consult it if Vulkan Video encode does something weird that `gpu-video` doesn't insulate us from.
- **`scap` crate** — nice cross-platform capture, but returns **CPU** BGRA frames by default; that GPU→CPU round-trip is exactly the overhead we are avoiding. Not used for the hot path.

### 3.5 Capture: **per-platform, GPU-resident**
There is no single cross-platform capture API. Capture is the one genuinely per-platform layer.
- **Linux (Wayland, the author's primary platform):** XDG screencast portal via **`ashpd`** to negotiate the session + **`pipewire`** (pipewire-rs) to receive the stream as a **DMA-BUF** (DRM PRIME) fd — not memmapped CPU pixels.
- **Windows:** Windows Graphics Capture (WGC) / DXGI Desktop Duplication → a D3D11 texture (shared NT handle). Use the `windows-capture` crate or raw `windows` (windows-rs).

### 3.6 macOS is out of scope — and why "Metal has ray tracing now" does **not** change that
Apple has hardware ray tracing (M3+), but RT is a *rendering* feature, independent of video encode. The relevant fact: **Vulkan Video is still not available on Apple** (MoltenVK does not expose the video-encode extensions; the request has been open since 2021), and `gpu-video` itself targets only Linux + Windows. So an Apple port would mean a **separate VideoToolbox backend**, not our `gpu-video`/Vulkan path. Out of scope for v1.

### 3.7 Licensing — cleared, with one ongoing obligation
- **`gpu-video` is MIT** (confirmed: `license = "MIT"` + a LICENSE file in `gpu-video/`). The *smelter product* as a whole has a restrictive source-available license (real-time use / SaaS / embedding-for-distribution requires a commercial license starting $1k/mo), **but that governs the compositor product, not the separately-MIT-licensed library crate we depend on.**
- **Therefore permitted:** building a UI around it, shipping a full app, distributing, selling, and feeding ganked.tv — all fine. **Only obligation:** include the MIT copyright/license notice in distributed builds (an "about"/`LICENSES` file is enough).
- **UI framework is not chosen yet** (decided at Phase 7, not before). When choosing, watch the license: pick MIT/Apache; **avoid GPL/commercial-encumbered toolkits like Slint** for a distributed app.
- **Name-collision warning:** there is an unrelated `gpu-video` by AdrianEddy (a Gyroflow ffmpeg refactor, also MIT/Apache, not ready). We use the **Software Mansion** one from the cloned `smelter` repo. Keep the dependency pointed at that, not crates.io by accident.
- Other deps (wgpu, ash, ashpd, pipewire, global-hotkey) are MIT/Apache — fine.

---

## 4. Architecture

### 4.1 Pipeline (everything GPU-resident until the encoded bytes come back)

```
[OS capture] → [import as wgpu::Texture] → [RGBA→NV12 (compute)] → [gpu-video encode]
      │                  │                          │                      │
  DMABUF / D3D11    wgpu-hal interop          wgpu compute pass     H.264 chunks (Vec<u8>)
                                                                          │
                                          [keyframe-aligned ring buffer (RAM, ~60s)]
                                                                          │
                                              hotkey ──► [mux: chunks + PTS → MP4] ──► disk ──► (ganked.tv upload)
```

### 4.2 Workspace layout (Cargo workspace of focused crates)

```
rewynd/
├── Cargo.toml                 # workspace
├── crates/
│   ├── capture/               # trait FrameSource + platform impls
│   │   ├── src/lib.rs         #   pub trait FrameSource { async fn next_frame() -> GpuFrame; ... }
│   │   ├── src/linux/         #   ashpd + pipewire → DMABUF
│   │   └── src/windows/       #   WGC/DXGI → D3D11 texture
│   ├── gpu/                   # wgpu device/queue setup shared with gpu-video; the interop import helpers
│   ├── encode/               # thin wrapper over gpu-video (wgpu::Texture in → H264 chunk out); RGBA→NV12
│   ├── buffer/                # ring buffer, keyframe-aware; the "interesting" pure-Rust core
│   ├── mux/                   # H.264 Annex-B + PTS → MP4 (our timestamps, not guessed fps)
│   ├── upload/                # ganked.tv client (later feature)
│   └── app/                   # the `rewynd` binary: wires it together, hotkey, config, tray/overlay
└── PLAN.md
```

**Crate naming:** the binary crate is `rewynd`; library members take the `rewynd-` prefix (`rewynd-capture`, `rewynd-encode`, …) — reserve those names on crates.io only for the ones you actually publish.

### 4.3 Key trait boundaries
- **`FrameSource`** (in `capture`): yields a `GpuFrame` (an imported `wgpu::Texture` + timestamp + format). Per-platform impls behind a common trait so `app` is platform-agnostic.
- **`Encoder`** (in `encode`): wraps `gpu-video`. Input `wgpu::Texture` (NV12) + `force_keyframe`; output `EncodedChunk { bytes, is_keyframe, pts }`. Owns the RGBA→NV12 conversion if the source texture isn't already NV12.
- **`RingBuffer`** (in `buffer`): stores encoded chunks with a cap of ~60s; tracks which chunks are IDRs; `flush_last(duration)` returns the byte range from the most recent IDR ≤ `duration` ago. Pure Rust, fully unit-testable, **no GPU or driver dependency** — this is where most of the satisfying logic lives.
- **`Muxer`** (in `mux`): Annex-B → AVCC, write an MP4 with correct PTS from capture timestamps.

---

## 5. The single most important early risk: wgpu version coordination

`gpu-video` v0.4.0 pins **wgpu 29.0.0 at git rev `1503796`**. Our capture-import (§6.1) needs wgpu-hal interop functions that landed on wgpu's trunk and **may be newer than that pinned rev**. If we use a *different* wgpu than `gpu-video` does, the `wgpu::Texture` types won't match and we cannot hand a captured frame to the encoder. **Same wgpu version end-to-end is mandatory.**

**Phase 1 must resolve this first.** Options, in order of preference:
1. Confirm the pinned rev already exposes the interop fns (`texture_from_dmabuf_fd`, D3D11 shared-handle import, `add_wait_semaphore`, `Adapter::open_with_callback`). If yes, done.
2. If not, check whether a newer `gpu-video` bumps its wgpu pin to a rev that has them, and use that.
3. If neither, fork/patch `gpu-video`'s wgpu dependency to a single shared rev that has both the video-encode support and the interop — and pin our whole workspace to that exact rev.
4. Last resort: do the import in raw `ash` and feed `gpu-video` through any lower-level (non-wgpu) encoder entry point, accepting more manual Vulkan plumbing.

Document the outcome in an ADR (`docs/adr/0001-wgpu-rev.md`).

---

## 6. The hard parts (known unknowns, with current tooling)

### 6.1 Capture → `wgpu::Texture` interop (the crux; recently de-risked)
This is the part neither `gpu-video` nor Dennis solves, and it's the biggest integration work. Good news: wgpu gained the needed hooks recently.
- **Linux (DMABUF):** `wgpu_hal::vulkan::Device::texture_from_dmabuf_fd()` imports a PipeWire DMA-BUF fd as a wgpu texture (feature flags `VULKAN_EXTERNAL_MEMORY_FD` / `VULKAN_EXTERNAL_MEMORY_DMA_BUF`).
- **Windows (D3D11):** import the WGC/DXGI D3D11 shared NT handle via `VK_KHR_external_memory_win32` (the "create texture from d3d11 shared handle" wgpu path).
- **Synchronization (do not skip):** importing memory isn't enough — you must wait for the capture to finish writing before encoding. Use `wgpu_hal::vulkan::Queue::add_wait_semaphore` (imported via `VK_KHR_external_semaphore_*`). Forgetting this gives intermittent corruption that looks like a codec bug but isn't.
- **Enabling extensions wgpu doesn't expose by default:** `wgpu_hal::vulkan::Adapter::open_with_callback` lets you edit the extension list / pNext chain before device creation.
- These are `wgpu-hal` / `as_hal` level (unsafe). Native external-texture-from-handle is intentionally out of wgpu's *safe* public API, so expect to work through hal.
- **Reference implementations to crib from:** `libscreencapture-wayland` (C++, modular portal→pipewire→encode, shows the DMABUF→GPU-encode pattern), RustDesk's Wayland capture (Rust, though it uses a CPU GStreamer appsink path), and the wgpu interop PRs/examples.

### 6.2 RGBA → NV12 conversion
The encoder wants `NV12`; capture is almost certainly BGRA/RGBA. Convert on the GPU with a small wgpu compute (or fragment) pass. **First check** whether `gpu-video`'s internal `wgpu_helpers/rgba_to_nv12` is publicly usable; if so, reuse it. Otherwise write a ~50-line shader. This is well-trodden GPU work (unlike the encode init) — low risk, AI-assistable.

### 6.3 Keyframe-aligned cutting (pure Rust)
The ring buffer must cut on IDR boundaries. Track per chunk whether it's an IDR; on flush, walk back to the most recent IDR within the window and start there. With `inline_stream_params: true`, that cut is self-decodable. This is the core algorithmic piece — design it carefully and unit-test it without a GPU.

### 6.4 A/V sync (deferred to Phase 5)
The classic pain. `gpu-video` returns raw bitstream with no timing — **you** stamp PTS from capture timestamps; the MP4 container carries the framerate (don't let players guess it). Audio via `cpal`, synced by timestamp. **Do not start audio before Phase 5.**

---

## 7. Phases → milestones & issues

Each phase is a GitHub **milestone**. Issues below are written to be created roughly as-is (title, labels, body). Each phase ends with a **verifiable artifact** and a **review gate** (§9). Do not start phase N+1 until phase N's gate passes.

### Phase 0 — Encode spike ✅ DONE (record as closed/proven)
**Outcome (already verified on RTX 3080 Ti / CachyOS):** `gpu-video` encodes NV12 frames to a valid, playable H.264 file via Vulkan Video → NVENC. The four ring-buffer building blocks (`force_keyframe`, `idr_period`, `inline_stream_params`, NV12 wgpu-texture encoder) are confirmed present. Create this milestone and immediately close its issue as documentation of the de-risk.

### Phase 1 — Foundations & the wgpu-rev question
- **#1 Resolve wgpu version coordination** (`area:gpu`, `risk:high`) — per §5. Deliverable: an ADR stating which wgpu rev the whole workspace pins and how `gpu-video` + interop coexist. **Blocks everything else.**
- **#2 Scaffold the Cargo workspace** (`area:infra`) — crates per §4.2 with stub traits (`FrameSource`, `Encoder`, `RingBuffer`, `Muxer`) that compile. CI: `cargo build` + `cargo clippy -- -D warnings` + `cargo fmt --check`. Add MIT-notice handling for `gpu-video` in the build (LICENSES dir).
- **#3 Stand up the shared wgpu device** (`area:gpu`) — create the `wgpu` instance/adapter/device that `gpu-video` will also use, with the interop extensions enabled via `open_with_callback`. Deliverable: a test that creates the device and the `gpu-video` encoder against the *same* device on the author's machine.

### Phase 2 — Capture spike (per platform, GPU-resident)
- **#4 Linux: portal + PipeWire → DMABUF** (`area:capture`, `os:linux`, `risk:high`) — use `ashpd` to open a screencast session and `pipewire` to receive frames as DMA-BUF fds. Deliverable: log node id, negotiated format, and that frames arrive as DMABUF (not memmap). **Expect NVIDIA + Wayland friction; this is the riskiest capture target — prove it early.**
- **#5 Linux: import DMABUF → `wgpu::Texture`** (`area:gpu`, `os:linux`, `risk:high`) — `texture_from_dmabuf_fd` + the wait-semaphore sync. Deliverable: import one captured frame, copy/blit it to a readback buffer, save a PNG that matches the screen. (Sanity that the import + sync are correct before going live.)
- **#6 Windows: WGC/DXGI → D3D11 texture** (`area:capture`, `os:windows`) — `windows-capture` or raw windows-rs. Deliverable: a D3D11 texture per frame with a shared NT handle.
- **#7 Windows: import D3D11 shared handle → `wgpu::Texture`** (`area:gpu`, `os:windows`) — `VK_KHR_external_memory_win32` path. Same PNG-readback sanity check.

### Phase 3 — Live pipeline (capture → convert → encode → file)
- **#8 RGBA→NV12 conversion** (`area:encode`) — reuse `gpu-video`'s helper if public, else a compute shader. Unit/visual test: convert a known RGBA frame, verify NV12 output.
- **#9 Wire capture → NV12 → encoder, write to file** (`area:encode`, `risk:high`) — feed the live captured `wgpu::Texture` into the `Encoder` wrapper, write the H.264 chunks straight to a `.h264` file (no buffer yet). Deliverable: a continuously-growing file that plays back the live screen. Validates the whole hot path end-to-end. Measure CPU/GPU overhead here — this is the "is it actually light?" checkpoint.

### Phase 4 — Ring buffer + hotkey (THE feature)
- **#10 Keyframe-aware ring buffer** (`area:buffer`) — pure Rust, unit-tested. Stores ~60s of chunks, drops oldest, tracks IDRs, `flush_last(60s)` returns the byte range from the most recent IDR. Set a sane `idr_period` so keyframes are frequent enough to cut on.
- **#11 Global hotkey** (`area:app`) — `global-hotkey` crate. Deliverable: pressing the hotkey triggers a flush.
- **#12 Minimal MP4 muxing with real PTS** (`area:mux`) — Annex-B → AVCC, write MP4 with PTS from capture timestamps (don't let players guess fps). Evaluate a Rust mp4 muxer crate vs. invoking the `ffmpeg` binary for muxing only (both license-clean). Deliverable: **hotkey → playable, correctly-timed 60s MP4 on disk that starts on a keyframe.** ← MVP COMPLETE.

### Phase 5 — Audio + A/V sync
- **#13 System audio capture** (`area:audio`) — `cpal`.
- **#14 A/V mux + sync** (`area:audio`, `area:mux`, `risk:high`) — interleave audio, sync by timestamp, correct PTS for both tracks.

### Phase 6 — Windows parity
- **#15 Bring Windows to MVP parity** (`os:windows`) — full capture→encode→buffer→mux on Windows, matching Linux behavior. Per-vendor encoder quirks (NVENC vs AMD vs Intel) shaken out here.

### Phase 7 — UX
- **#16 Config** (`area:app`) — buffer length, hotkey, output dir, monitor selection.
- **#17 Tray + recording/overlay indicator** (`area:app`) — cheap since we already have a wgpu context; a "clip saved" toast and a recording indicator. **Choose the UI framework here** (not before) — must be MIT/Apache-licensed (avoid Slint-style GPL/commercial). Evaluate the options at this point.

### Phase 8 — ganked.tv integration (stretch / differentiator)
- **#18 ganked.tv upload client** (`area:upload`) — hotkey → clip → auto-upload via the existing ganked.tv API. Encode H.264 (browser-compatible) for this path. End-to-end "press hotkey, it's on ganked.tv."

---

## 8. Out-of-the-box ideas (optional; evaluate, don't auto-build)

- **Reuse `gpu-video`'s decode path for an in-app preview/scrubber.** The crate decodes too, zero-copy into a `wgpu::Texture`. Before uploading, render the clip in a tiny preview window — trim points, "keep/discard" — using the same GPU stack you already have. High synergy, low extra dependency surface.
- **End-to-end ganked.tv is the long-term hook.** Medal/ShadowPlay are closed ecosystems; "hotkey → clip → instantly on *your own* platform" is genuinely distinct and ties the author's two projects together. **Sequencing is deliberate: ship the general-purpose recorder first, then add this as a later UI-triggered feature** (Phase 8) — don't pull it forward, but keep it in view as the differentiator.
- **Rolling input-event log for smart clip boundaries.** Keep a lightweight ring of input/timestamps alongside the frame buffer; later, auto-suggest where the "moment" was (kill, big play) rather than always grabbing a flat 60s.
- **Per-game profiles** keyed by focused window/title (resolution, bitrate, hotkey).
- **AV1 later** for much smaller clips (Vulkan Video supports it; `gpu-video` may add it). Keep **H.264** for ganked.tv browser playback now; offer AV1 as an option once supported.
- **Open-source the capture→`gpu-video` bridge as its own crate.** The ecosystem currently lacks a clean "OS capture → wgpu texture → Vulkan Video encode" glue. Given the author's OSS track record (Nimbus, freshdock), a focused MIT crate here could get real traction — and the work has to be done anyway.

---

## 9. Workflow, conventions & guardrails for Claude Code

**Research before you implement (important).** The crates here move fast (`ashpd`, `pipewire`, `wgpu`, `gpu-video`, `global-hotkey`, the capture APIs). Before implementing against any of them, **web-search the current version, changelog, and docs**, and code against what is actually current — do **not** rely on training-data memory, and do not trust the version/rev numbers in this document, which may already be stale. Verify the real API surface (function names, signatures, feature flags) against the live docs/source for the version you pin.

**Git / PR workflow:**
- Work is **issue-driven**: for each issue, create a branch off that issue and do the work there.
- **Issues are intentionally broad** to keep momentum — a single issue may touch many files (~50 file changes is fine and expected). Don't over-fragment; bundle related work into substantial issues.
- For each issue: implement → run the **`/review`** slash command → apply the changes the review surfaces → then push.
- Open a **PR** for the branch. **Never** put "generated by Claude" or any AI-attribution in the PR description. **Do not** add `Co-authored-by: Claude` (or any Claude trailer) to commits. Keep commit messages and PR descriptions clean and human-sounding.
- **The human merges** — Claude Code does not merge PRs.
- PRs are **also reviewed by CodeRabbit** (the human routes them), so write them expecting that automated review.

**Scope discipline:**
- **Don't pull work forward.** No audio before Phase 5. No UI framework chosen before Phase 7. No Windows work blocking the Linux MVP. Keep the hot path minimal until it's proven.
- **Each issue/phase produces a verifiable artifact** (a PNG, a playing file, a passing unit test, a measured overhead number) — not just "it compiles." Give a short status summary at each phase boundary.

**Code conventions:**
- Latest stable Rust edition; `thiserror` for library errors, `anyhow` only in the binary; `tracing` for logging (the `gpu-video` examples hardcode INFO and have no per-frame logs — add your own `tracing` spans in our crates so the pipeline is observable). `clippy -D warnings` and `fmt --check` in CI.
- **Keep `gpu-video` / `wgpu` versions pinned and identical across the workspace** (§5). Any bump is an ADR-worthy event.
- **Resolution / framerate / bitrate are parameters, never hard-coded** (target 1080p60, but other qualities must be addable). The ring buffer holds encoded H.264 only — never buffer uncompressed frames.
- **ADRs** under `docs/adr/` for: the wgpu rev (0001), the muxer choice (Rust crate vs ffmpeg binary), and any encoder-param decisions.
- **Licensing in builds:** ship the MIT notice for `gpu-video` (and any other deps requiring attribution) in a `LICENSES/` dir or about screen.

**Use, don't depend on, Dennis's repo.** If Vulkan Video encode misbehaves in a way `gpu-video` doesn't insulate, his `feat/vulkan-video-recorder` branch is the worked example to consult — and he's a person to ask.

---

## 10. ► Task 0 — do this first

Set up the project tracking before writing feature code:

1. **Create labels:** `area:capture`, `area:gpu`, `area:encode`, `area:buffer`, `area:mux`, `area:audio`, `area:app`, `area:upload`, `area:infra`, `os:linux`, `os:windows`, `risk:high`, `risk:med`.
2. **Create milestones** for Phases 0–8 (titles per §7).
3. **Create the issues** in §7 under their milestones, with the listed labels and bodies (expand each body with the acceptance criteria implied by its "Deliverable"). Wire up the dependency relationships (e.g. #1 blocks all; #5 depends on #4; #9 depends on #5/#8).
4. Mark **Phase 0** and its issue as **closed**, citing the verified Phase-0 result in §3.3 as the proof.
5. Open a tracking issue "**Phase 1 kickoff**" and start with **#1 (wgpu rev resolution)** — nothing else proceeds until that's settled.

Use `gh issue create` / `gh api` (the GitHub CLI). If `gh` isn't authenticated, stop and tell the human.

6. **Before implementing #1**, web-search the current `wgpu` and `gpu-video` versions and docs (per §9) — the rev numbers in this document may already be outdated. The per-issue working flow (branch → `/review` → apply → push → PR; human merges; CodeRabbit reviews) is in §9.

---

## 11. Reference index

- **Encode lib:** `gpu-video` (Software Mansion), inside `github.com/software-mansion/smelter` — MIT. *(Not* AdrianEddy's same-named crate.)
- **Linux capture:** `ashpd` (XDG portals, screencast) + `pipewire` (pipewire-rs). Consider `lamco-pipewire` (higher-level DMABUF capture, young) as an option to evaluate.
- **wgpu interop:** `wgpu-hal` Vulkan — `texture_from_dmabuf_fd`, D3D11 shared-handle import (`VK_KHR_external_memory_win32`), `Queue::add_wait_semaphore`, `Adapter::open_with_callback`.
- **Windows capture:** `windows-capture` crate or raw `windows` (windows-rs) for WGC/DXGI.
- **Hotkey:** `global-hotkey`. **Audio:** `cpal`. **UI (later):** egui / iced / Tauri (MIT/Apache; avoid Slint).
- **Architecture references:** `libscreencapture-wayland` (C++), RustDesk Wayland capture (Rust), GPU Screen Recorder / ReplaySorcery (C, lightweight instant-replay prior art).
- **Encode-half blueprint + human contact:** Dennis de Vulder — `github.com/dennisdevulder/gpu-vulkan`, branch `feat/vulkan-video-recorder` (reference only; Java/LWJGL, RuneLite-coupled).

---

*End of plan.*
