# 0013 — Clip library: mux read side, openh264 thumbnails, on-disk cache

## Status

Accepted.

## Context

The settings window becomes the rewynd app: a Library view listing saved clips with
thumbnails, playback/delete actions, and a per-clip upload flow (the per-upload title and
visibility override left open by the upload work). Thumbnails need one decoded frame per
clip, in a GUI that deliberately has no GPU stack (ADR 0006), from files our own muxer wrote
(H.264 in MP4, ADR 0002/0004).

## Decision

- **Read side in rewynd-mux** (`mux/src/read.rs`): `clip_summary` (dimensions + duration from
  the track headers) and `first_keyframe_annexb` (avcC SPS/PPS plus the first sync sample,
  AVCC length prefixes converted back to Annex-B start codes — the exact inverse of the write
  side). No third-party demuxer; the vendored `mp4` reader is already in the tree and its
  writer produced the files. Coverage-gated like the rest of the mux crate.

- **CPU decode with `openh264` 0.9** (settings crate only, decode-only): BSD-2-Clause,
  GPL-compatible, pure-CPU (fits the no-wgpu settings build), and ~3-6 ms per 1080p frame,
  which is plenty for one keyframe per clip on a background task. The default `source`
  feature builds Cisco's C++ source vendored by `openh264-sys2`; without nasm it silently
  falls back to plain C, so CI needs no new packages beyond its existing C/C++ toolchain.
  Patent note, considered and accepted: H.264 baseline decoding of our own recordings, via a
  from-source build (not Cisco's binary, so no BSD+patent grant), carries the usual residual
  H.264 pool exposure (US-relevant, winding down through 2027). Decode-only, in a GPL app
  that already encodes H.264 through the system's GPU driver.

- **Thumbnail cache, two layers**: in-memory per `(path, mtime)` for the session, plus PNGs
  under `~/.cache/rewynd/thumbs/<fnv1a64(path, mtime)>.png` (dirs crate) so restarts render
  the library instantly. FNV-1a because the key must be stable across processes (std's hasher
  is per-process seeded). Corrupt or undecodable clips get a placeholder card, never a crash,
  and no cache entry.

- **Clip store moves to rewynd-config** (`config/src/clips.rs`): listing, naming, and
  output-dir resolution live in the GPU-free config crate (which already owned the default
  output dir), and `rewynd-clip` keeps only the saver. A feature split inside `rewynd-clip`
  was tried first and rejected: `cargo build --workspace` unifies features, so any workspace
  build of the settings GUI would still link the saver's encode/wgpu tree (ADR 0006
  violation). Separate crates are immune to feature unification.

## Consequences

- The library and the recorder resolve the clip directory through one function; per-game
  subfolders and the `rewynd-<millis>-<seq>.mp4` contract stay in one crate.
- openh264 adds a one-time C/C++ build to the settings crate (~40 s cold); no runtime deps.
- Phase 2 (trim/crop, issue #51) can build on the same read side for frame-accurate preview.
