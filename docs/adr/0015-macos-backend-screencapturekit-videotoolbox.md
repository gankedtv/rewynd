# 0015 — macOS backend: ScreenCaptureKit + VideoToolbox

## Status

Accepted. Amends ADR 0014's "macOS stays out of scope" note and PLAN §1/§2/§3.6.

## Context

The recorder's pipeline rides wgpu + gpu-video's Vulkan Video encode (ADR 0001), and PLAN
§3.6 kept macOS out of scope for exactly that reason. Nothing on Apple's side has changed:
MoltenVK exposes none of the Vulkan Video extensions (requested since 2021, wontfix in
practice — Metal has no codec API to layer them on), and the latest Metal (Metal 4, macOS
26) added graphics and ML features only. Apple Silicon's hardware encoder lives on the
media engine, exposed exclusively through VideoToolbox. A macOS port therefore cannot
reuse the Vulkan pipeline at any layer; it needs the separate VideoToolbox backend PLAN
§3.6 predicted.

What macOS does offer is a well-matched pair. ScreenCaptureKit composites, scales, and
pixel-converts in WindowServer on the GPU and hands out NV12 (`420v`) IOSurface-backed
CVPixelBuffers with the cursor already composited; VideoToolbox encodes them zero-copy
over unified memory on the media engine — a fixed-function block separate from the GPU
cores, so gaming performance is unaffected. H.264 hardware encode exists on every M-series
chip (AV1 encode on none, as of 2026). The stream feeding the ring has the same hard
invariants as ADR 0014: a fixed cuttable GOP, a correct per-chunk keyframe flag, Annex-B
bytes, and inline SPS/PPS before every IDR.

## Decision

- **Scope: Apple Silicon only, macOS 15 floor.** Every M-series Mac runs 15, the SCK
  microphone API needs 15, and Apple's security-patch window is 26/15/14. Intel Macs are
  not targeted.

- **The macOS pipeline bypasses wgpu/gpu-video entirely.** `SCStream` (NV12 `420v`,
  hardware-scaled, cursor composited server-side) delivers IOSurface-backed CVPixelBuffers
  straight into a `VTCompressionSession`. The frame never becomes a `wgpu::Texture`, so
  the ADR 0001 pin is untouched by the port.

- **VT session config:** `RealTime=true` + `ExpectedFrameRate`, `AllowFrameReordering=false`
  (no B-frames, DTS == PTS), H.264 High AutoLevel, `AverageBitRate`, and
  `MaxKeyFrameInterval=idr_period` *plus* `MaxKeyFrameIntervalDuration` — SCK is
  change-driven/VFR (no frames while the screen is static), so a frame-count GOP alone can
  stretch arbitrarily in wall time; the duration key bounds it. BT.709 color end to end.
  Resolution/framerate/bitrate/idr_period stay parameters. VT emits AVCC; a pure converter
  rewrites it to Annex-B with SPS/PPS inlined before every IDR, so the existing
  `EncodedChunk`/ring/muxer contracts are unchanged.

- **The `"cpu"` encoder preference maps to VT with hardware disabled** (Apple's software
  encoder) instead of openh264: one output path, same properties, no second bitstream
  shape on macOS.

- **Bindings: `cidre`, pinned `=0.16.1`** (MIT, crates.io; pinned exact because 0.x minors
  break). Its `sc-record` example is this exact SCK→VT pipeline, and it is
  production-proven (Cap, StreamChamp). Rejected: the objc2-* suite — fully viable and the
  designated migration target if cidre churn bites, but ~500–1000 more lines of unsafe
  glue (`define_class!` delegates, block plumbing, hand-built VT property dictionaries);
  doom-fish `screencapturekit` + `videotoolbox` — a Swift-toolchain build dependency and
  an explicitly experimental VT half; hand-rolled `extern "C"` — obsoleted by generated
  bindings.

- **Audio through SCK too:** `capturesAudio` for system audio (rides the Screen Recording
  grant, no driver or kext), `captureMicrophone` (15+) for the mic. The Opus encoder,
  mixer, and audio rings are reused unchanged.

- **Tray, hotkey, app loop:** tray-icon (NSStatusItem) and global-hotkey (Carbon
  `RegisterEventHotKey` — fires over fullscreen games and needs no Accessibility
  permission; macOS 15 rejects combos whose only modifiers are Shift/Option). The AppKit
  event loop owns the main thread, which both require; capture/encode/audio stay on plain
  threads. App Nap is suppressed with an NSProcessInfo activity held for the capture
  lifetime.

- **Game detection:** NSWorkspace frontmost-app polling + a CoreGraphics window-bounds
  fullscreen check, shaped as a FocusWatcher like Linux (ADR 0012) — the gate stays
  outside the capture path.

## Consequences

- TCC is the ugly part. The Screen Recording grant is mandatory; unbundled dev runs from a
  terminal attach the grant to the *terminal*, and macOS 26 requires a signed .app bundle
  for the app to appear in the privacy pane at all. Users on 15+ get a monthly-ish
  re-approval nag with no public opt-out. Shipping therefore requires bundle + signing +
  notarization; that packaging (and Velopack's osx target) is deferred to a follow-up —
  until then macOS is build-from-source.
- cidre is a single-maintainer 0.x crate: churn risk accepted, mitigated by the exact pin
  and the mapped objc2 migration path.
- VFR pacing is structural: SCK delivers nothing while content is static. Real-PTS muxing
  absorbs it, but wall-time cut granularity is bounded by the duration GOP key, not the
  frame-count one.
- CI gains a `macos` job (clippy/build/test on Apple Silicon; gates releases via
  `workflow_call`). Coverage excludes `capture/src/macos/` and `encode/src/videotoolbox.rs`
  — validated by `#[ignore]`d tests against live SCK/VT on a Mac — while the pure Annex-B
  converter stays CI-covered.
- A third per-platform pipeline to maintain, and the first with no wgpu in it at all.
