# 0019 — Recording resolution: follow the display, never stretch

## Status

Accepted.

## Context

Until now the recording resolution was a single stored `(width, height)` pair defaulting to
1920×1080, and the settings window offered four presets — 720p/1080p/1440p/2160p — every one of
them 16:9. Three things followed from that, and all three were wrong on any other panel:

- **The default ignored the display.** A fresh install recorded 1920×1080 whether the screen was
  1920×1080, 3440×1440 or 5120×1440.
- **The picture was stretched, silently.** The record path's only scaler is the RGBA→NV12 pass
  (`encode/src/gpu_video_backend.rs`), which samples the source across the whole output through a
  linear sampler. A 21:9 capture written into a 16:9 frame came out horizontally squashed — not
  letterboxed, not cropped, just wrong. macOS had the same shape via SCK's server-side scale,
  which never set an aspect flag.
- **There was no way out.** A non-16:9 size could only be set by hand-editing the TOML, and the
  dropdown then showed a blank selection.

Nothing in the tree ever asked the OS how big the screen was. The information was within reach on
every platform — the ScreenCast portal already stored a stream size it never read, WGC's `Monitor`
already answered `refresh_rate()` next to an unused `width()`, SCK's display list was already
enumerated to pick a display — but it was never used to size the encode.

PLAN §9 requires an ADR for encoder-parameter decisions, and resolution is the parameter it names
first.

## Decision

### 1. The config stores an intent, not just a size

`[video]` gains `match_display` (default `true`), and `0` becomes the "auto" value for
`width`/`height`. `ResolutionMode` (`config/src/resolution.rs`) reads the trio as one of three
things, and the recorder resolves it against the measured display:

| Stored | Mode | Records |
|---|---|---|
| `match_display = true`, `height = 0` | `MatchDisplay` | The display's native size, scaled down only if it exceeds ~4K worth of pixels |
| `match_display = true`, `height = N` | `Height(N)` | N lines at the display's aspect ratio, never upscaled |
| `match_display = false`, both non-zero | `Fixed` | Exactly that size, letterboxed if the shapes differ |

Two properties fall out of this that a separate "auto" flag would not have given:

- **Fresh installs match the display.** The shipped template is `width = 0` / `height = 0`, so an
  ultrawide records 3440×1440 out of the box.
- **Existing configs are fixed without a migration step.** Every config written before this
  contains `width = 1920` / `height = 1080` and no `match_display` key, which serde defaults to
  `true` — so they read as `Height(1080)` and start following the display's aspect ratio at the
  quality the user already had. 16:9 users see no change at all.

The one config this quietly changes behavior for: a TOML hand-edited to a non-16:9 pin before
this landed (e.g. `width = 2560` / `height = 1080`, the only way to get a non-16:9 size at all
under the old scheme) has no `match_display` key either, so it is read the same way — as
`Height(1080)`, not `Fixed`. The pinned width is discarded and the recording follows the
display's aspect ratio at 1080 lines instead. That's a silent change for exactly the users the
"no way out" bullet above describes, though arguably the better default now that there's a real
`Fixed` mode to switch to.

The 4K cap on `MatchDisplay` keeps an 8K panel from asking every encoder for a frame size it
cannot take (and a bitrate nobody wants). `Height` never upscales: extra lines cost bitrate and
encoder time without adding a pixel of detail.

`REWYND_WIDTH` keeps meaning "record exactly this" — naming a width in the environment implies
`match_display = false` when there is a height to pair it with. `REWYND_MATCH_DISPLAY` overrides
that either way.

### 2. Measuring the display is per-platform, and best-effort everywhere

| Platform | Source | Notes |
|---|---|---|
| Windows | `Monitor::width()/height()` before the WGC session | WGC always captures at native size, so this is exactly what will arrive |
| macOS | `SCDisplay` size × the current display mode's backing scale | `SCDisplay` reports **points**; SCK's config is **pixels**, so a Retina panel needs the scale or it captures at half resolution |
| Linux | The Wayland output containing the portal's reported origin, at its current mode | The portal's own stream size is compositor-space, so it is the right *shape* but the wrong number under fractional scaling; it stays as the fallback |

Every one returns `Option` and degrades — an unmeasurable display leaves the recorder on the
1080p reference, which is also `EncodeParams::default()`.

Linux is the only platform where this can't happen before the pipeline is built: the monitor isn't
chosen until the ScreenCast portal has run. Rather than reorder startup around a modal dialog, the
recorder recomputes its parameters after the portal returns and tells the already-constructed
`ClipSaver` the final dimensions (`set_dimensions`).

The resolved size is also held inside the chosen adapter's advertised `max_width`/`max_height` —
data the probe has always collected and `choose_encoder` never consulted. That mattered little when
the default was 1080p; with "match display" it decides whether a 4K panel stays on the GPU or
silently falls back to the CPU.

### 3. A mismatched aspect ratio is letterboxed, never stretched

Deriving the size from the display makes the common case match, but a pinned size or a mid-session
display-mode change can still disagree. When they do, `Nv12Converter::convert` first draws the
frame into a black output-sized target with a "contain" fit, then runs the existing RGBA→NV12 pass
over that. Matched aspects (the normal case) skip the pre-pass entirely — no extra texture, no
extra pass, no change to the hot path.

Letterbox over crop: a clip is a record of what was on screen, and cropping silently deletes part
of it. The bars are honest about the mismatch, and the shader is the one the clip player already
uses for playback (`settings/src/video.rs`), so recording and playback agree on what "fit" means.

macOS gets this from SCK instead of a shader: `preservesAspectRatio` + `scalesToFit`.

The arithmetic lives outside the GPU glue — `ResolutionMode`/`fit_width` in `rewynd-config`,
`contain_scale`/`aspect_matches` in `rewynd-encode`'s `fit` module — so all of it is unit-tested on
CI, and the coverage exclusions did not have to change.

### 4. The settings window offers heights, not 16:9 sizes

The dropdown becomes `Match display / 2160p / 1440p / 1080p / 720p / Custom`, each labelled with
the size it actually resolves to on this display ("1080p (Full HD) — 2560x1080"). Presets taller
than the display are hidden rather than shown all resolving to the same number. `Custom` reveals a
width × height pair and stores a pinned size.

The window is deliberately wgpu-free (ADR 0006) and cannot enumerate displays itself, so the
recorder publishes the measured geometry in `status.json` (`display_width`/`display_height`,
optional and defaulted, so an older status file still reads). With no recorder running the presets
resolve against the 1080p reference — the same answer the recorder would reach in that situation.

## Consequences

- Ultrawide, 16:10, 4:3 and portrait displays are recorded at their own shape, by default.
- A clip is never geometrically distorted, on any platform, under any setting.
- Recording at native resolution costs more bitrate and encoder time than the old fixed 1080p
  default. The quality slider is unchanged and still independent of resolution; tuning bitrate as a
  function of resolution is left for a later ADR, as `gpu_video_backend.rs` already flags for the
  rate-control constants.
- `Config::video()` now means "resolved with no display detected"; the recorder calls
  `video_for_display` instead. The lockstep test between the config defaults and
  `EncodeParams::default()` is unchanged in spirit — with no display, auto still resolves to
  1920×1080.
- The macOS backing-scale read goes through two hand-declared CoreGraphics functions
  (`CGDisplayCopyDisplayMode` / `CGDisplayModeGetPixelWidth`), following the existing
  `CGGetActiveDisplayList` precedent in `capture/src/macos/focus.rs`, because cidre binds display
  bounds but not display modes.
