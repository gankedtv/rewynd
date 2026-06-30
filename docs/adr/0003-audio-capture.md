# ADR 0003 — System-audio capture via PipeWire (not cpal)

- **Status:** Accepted (verified on the Linux dev box — CachyOS / RTX 3080 Ti / PipeWire 1.6.7, 2026-06-30)
- **Supersedes / superseded by:** none
- **Relates to:** issue #13 (system audio capture), PLAN §9

## Context

Phase 5 adds system-audio capture so a clip carries what the user *hears* (the desktop
output mix), to be Opus-encoded and muxed alongside the H.264 video. PLAN §9 named `cpal`
as the capture library. We need: the **default sink's monitor** (output, not a mic),
interleaved `f32` PCM at a known rate/channels (Opus wants 48 kHz), and a per-buffer
capture timestamp on the **same monotonic clock** as video so the two tracks can be synced.

## Decision

**Capture with `pipewire` (pipewire-rs), attaching to the default sink's monitor**, and
record this deviation from PLAN's `cpal`.

A normal PipeWire session (`Context::connect(None)` — no portal/fd, unlike the ScreenCast
video path) with the `stream.capture.sink = "true"` property attaches the stream to the
default sink's monitor ports. We pin an `EnumFormat` of interleaved `F32LE` at the
requested rate/channels (48 kHz / stereo by default; both are parameters, never hard-coded,
matching the video posture); PipeWire's audioconvert resamples/remixes to match, so each
delivered buffer is exactly the layout downstream Opus expects. Each buffer is stamped with
a monotonic capture-relative PTS at dequeue — the same discipline as the video
`DmabufFrame::pts`.

Lives in `crates/capture/src/linux/audio.rs` (reuses the crate's existing `pipewire` dep +
Linux gating); public surface is `AudioParams` + `capture_system_audio(params, on_samples)`.

## Options evaluated

| Option | Sink monitor (output) | Format control | Deps | Verdict |
| --- | --- | --- | --- | --- |
| **pipewire-rs sink monitor** | native (`stream.capture.sink`) | pins F32LE/rate/channels; audioconvert matches | already a dep (video) | **chosen** |
| `cpal` | ALSA backend on Linux can't cleanly select the PipeWire monitor | host/device enumeration | adds a new audio stack | rejected |
| `libpulse` binding | yes (PA monitor source) | yes | adds a PulseAudio dep | rejected |

## Rationale

- **cpal on Linux is ALSA-only** (no native PipeWire backend at the pinned version): selecting
  the default sink's *monitor* is awkward/brittle and goes through the Pulse/ALSA compat
  shims rather than the graph we already drive for video.
- **Consistency + zero new deps:** the capture crate already depends on `pipewire` for video;
  audio reuses it, the same main-loop/stream/format-pod patterns, and the same monotonic-PTS
  discipline — which matters for A/V sync in #14.
- **Windows** audio will use `cpal`/WASAPI later (like the video capture split), behind the
  same `capture_system_audio` shape.

## Consequences

- Linux-only, like the rest of the capture backend; runtime-validated by the `audio_probe`
  example (no GPU needed, but needs a live PipeWire session, so it isn't a CI unit test). The
  pure pieces (format-pod construction, `F32LE` decode) are unit-tested on CI.
- A sink with no playback client can suspend; during true silence the monitor may deliver no
  buffers. That is fine for the clip use case (no audio = nothing to record) but means #14's
  A/V interleave must tolerate audio gaps rather than assume a continuous stream.
- The callback runs on the PipeWire main loop (no `RT_PROCESS`), so a consumer that locks a
  mutex (the #14 audio ring buffer) can't trigger an audio xrun; the trade-off is that heavy
  callback work would stall capture, so the callback must stay cheap.

## Validation (dev box, 2026-06-30)

`audio_probe` against the live sink monitor while a 440 Hz tone played: negotiated
`F32LE / 48000 / 2ch` as requested; buffers of 2048 frames each; per-buffer PTS advanced
monotonically at ≈ 42.67 ms (= 2048 / 48000); peak ≈ 0.088 with RMS ≈ 0.062 — matching a
sine's amplitude/√2, confirming the interleaved decode and channel framing. Non-silent
capture confirmed; clean exit.
