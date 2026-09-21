# 0025 — Cropping happens at export, not at record time

## Status

Accepted (issue #220).

## Context

Ultrawide is the reason. A 21:9 clip was uploaded whole to ganked.tv and cropped there, after
the fact, because the part worth watching was never the full 3440 px. Doing that in the app
means a crop rectangle over the clip's own frame, with the frame in front of you.

ADR 0019 chose letterbox over crop for the *recording* path: a clip is a record of what was on
screen, and cropping it silently deletes part of that record. That still holds, and nothing
here changes it. A crop chosen in the editor is a different act — an edit, made afterwards, on
a clip that already exists, by someone looking at the frame and deciding which part of it to
keep. Save as copy leaves the original untouched.

The editor grows a crop overlay on the preview with Original / 16:9 / 21:9 / 4:3 / 9:16 / Free
presets, keyboard control (arrows move, shift+arrows resize, esc resets) and a readout of the
output size. Save, Save as copy and Upload all apply it.

## Decision

- **The crop is applied by the bundled ffmpeg, in one re-encode with the trim.** ffmpeg is
  already shipped beside our binaries on Linux and Windows for in-app playback — a BtbN GPL
  autobuild, so it carries libx264. The invocation (`export.rs`) is `-ss` *before* `-i` so the
  re-encode makes the seek frame-accurate, `-t` after it for the kept length,
  `-vf crop=W:H:X:Y`, `-map 0:v:0 -map 0:a?` (a clip recorded with everything muted has no
  audio track and must still export), `libx264` at CRF 18 / preset `veryfast` with libx264's
  own profile pick, `-pix_fmt yuv420p`, `-c:a copy -strict -2` (clips carry Opus, which older
  ffmpeg builds still call experimental in MP4), `-movflags +faststart`, `-f mp4`. Cutting and
  cropping in one pass means the frames are decoded and encoded once, not twice.

- **The crop rect reaches ffmpeg in even integer pixels — origin as well as size.** yuv420p
  subsamples chroma 2×2, so an odd `X`/`Y` has no chroma sample to start from, not just an odd
  `W`/`H`. `crop::pixel_rect` snaps all four, keeps a 2 px floor, and shrinks rather than
  slides a rect that would leave the frame.

- **Quality is a parameter struct, not constants in the argv builder.** `export::VideoQuality`
  (crf, preset, profile) is the single place a config field can slot into later. CRF rather
  than a bitrate because a crop changes the frame area: the same bitrate would mean something
  different for every rectangle, while a CRF means the same picture quality whatever is left.
  CRF 18 at `veryfast` is visually lossless for game footage and fast enough not to hold the
  window. The profile is left to libx264 (High) — verified to decode through the library's
  openh264 thumbnail path, the same way the recorder's NVENC output does.

- **Trim on its own stays lossless.** No crop still means `rewynd_mux::read::trim_clip`, an
  MP4 stream copy with no encoder in sight. So the two paths differ in where a clip starts: a
  stream-copied trim snaps back to the enclosing keyframe (up to about a second, the
  recorder's IDR interval), a cropped export starts exactly where the range says. The panel
  says so, rather than leaving people to notice.

- **The rectangle is ephemeral editor state, like the trim range.** It resets when the clip is
  opened and is never written to `clip-meta.json`. That store (ADR 0024) holds what a clip
  *is* — its name, its star — not what someone was in the middle of doing to it, and a
  persisted crop would have to be reconciled with a file that changed underneath it.

- **Upload crops locally first.** The same shape as the upload trim: write a throwaway sibling
  temp, send that, delete it. The server receives a finished file and needs to know nothing
  about cropping.

- **The GUI still does not depend on `rewynd-encode`.** ffmpeg is a spawned process behind the
  same `player::ffmpeg_command`; no new crate, no wgpu, no encoder in the window's dependency
  tree (ADR 0006).

- **macOS says so instead of failing.** No ffmpeg is bundled there, so the crop controls show
  a "needs ffmpeg" state exactly as in-app playback already does — up front, not at the moment
  someone hits Save. The bundled-or-`PATH` ffmpeg is re-probed every time a clip is opened
  until one is found, so installing it while the app is open is enough; once found, the answer
  is kept.

- **The overlay is drawn, not shaded.** Scrims and handles are painted over the full
  letterboxed frame using `crop::contain_rect`, the same contain fit the video shader uses, so
  the overlay and the picture agree on where the frame is. The rect itself is normalized to
  the source frame (0..1), so it survives a rescale of the preview and converts to pixels only
  at the argv. The detail page switches its preview from Cover to Contain, because an overlay
  on a cropped-to-fill picture would be pointing at pixels the export will not use.

## Alternatives rejected

- **Crop at record time.** Contradicts ADR 0019 outright: it would change what is recorded,
  and the deleted pixels are gone for good. The whole point is that this decision is made
  later, with the frame visible.
- **A pure-Rust re-encode through openh264.** The crate we already link is
  decode-plus-Constrained Baseline-encode; it cannot even decode the recorder's High-profile
  delta frames, let alone re-encode them at a quality anyone would keep. It is CPU-only and
  would drag an encoder into the GUI process.
- **Crop server-side only.** Leaves every local file uncropped, and uploads the bars anyway —
  the upload is the slow part.
- **`h264_nvenc` through ffmpeg.** A plausible follow-up for long clips on an NVIDIA box, but
  it means per-platform encoder selection and a fallback path for every machine without one.
  libx264 works everywhere ffmpeg does; v1 does not need the other.

## Consequences

- Crop makes ffmpeg a hard dependency for that one action, where playback only degrades to the
  system player without it. The AUR package already depends on `ffmpeg`; the installers
  already bundle it.
- A crop export costs real time on a long clip — it is an encode, not a copy. ffmpeg's
  `-progress pipe:1` drives a progress bar, and the Cancel beside it drops the task, which
  kills ffmpeg and removes the partial file.
- Exported clips get their library thumbnail from openh264 decoding libx264's output. High
  profile decodes today; `VideoQuality.profile` is the escape hatch if a future build of
  either disagrees.
- `export.rs` is unit-tested on its argv (the whole invocation is asserted verbatim) plus an
  `#[ignore]`d end-to-end test that synthesises a clip, exports it, and reads the result back
  with our own MP4 reader and thumbnail decoder.
