# ADR 0002 — MP4 muxer for the clip writer

- **Status:** Accepted (verified on the Linux dev box — CachyOS / RTX 3080 Ti, 2026-06-29)
- **Supersedes / superseded by:** none
- **Relates to:** issue #12 (minimal MP4 muxing with real PTS)

## Context

The encoder emits raw H.264 **Annex-B** byte chunks (start-code-delimited NAL units), and
the ring buffer hands us a clip — a ~60 s slice that starts on an IDR, each chunk tagged
with a real capture-relative PTS. We need to write a standard, broadly-playable `.mp4` in
which **we** control per-sample timing, so players don't guess the framerate from a fixed
default (the capture is damage-driven / variable-rate, so a guessed CFR would be wrong).

Requirements: per-sample timestamps we set; the clip starting on a sync sample (keyframe);
Annex-B → AVCC (length-prefixed) conversion plus an `avcC` config from the SPS/PPS;
pure-Rust + lightweight (the project's whole point is low overhead) and a GPL-3-compatible
license.

## Options evaluated (current versions, mid-2026)

| Option | Per-sample PTS | Annex-B | Deps | License | Verdict |
| --- | --- | --- | --- | --- | --- |
| **`mp4` (alfg/mp4-rust) 0.14** | yes (explicit per-sample duration + `stss`) | we convert to AVCC; SPS/PPS passed separately | pure-Rust, 6 small deps | MIT | **chosen** |
| `muxide` 0.2.x | yes | ingests Annex-B, builds `avcC` itself, fast-start | pure-Rust **but pulls `clap`/`indicatif`** | MIT/Apache | rejected |
| `mp4e` 1.0.x | **no explicit per-sample PTS** | ingests Annex-B | zero-dep | MIT | rejected |
| `ffmpeg` binary (`-c copy`) | **no** — synthesizes from an assumed constant fps | yes | heavyweight external runtime | n/a (separate process) | rejected |

## Decision

**Use the `mp4` crate (alfg/mp4-rust) 0.14** and own the small Annex-B → AVCC conversion.

- It gives explicit per-sample **duration** (we use a microsecond timescale, so capture-PTS
  deltas are written exactly) and an `is_sync` flag that populates the `stss` sync-sample
  table — exactly the "don't guess fps" + "starts on a keyframe" requirements.
- It is mature/widely-used ISO-BMFF, pure-Rust, MIT, and vendorable, with **lightweight,
  sensible deps** — unlike `muxide`, which drags a CLI framework (`clap` + `indicatif`)
  into a muxing library.
- We convert Annex-B → AVCC ourselves (replace start codes with 4-byte NAL lengths) and
  pull the SPS/PPS out of the first IDR (gpu-video emits them inline) to build the `avcC`.
  This is ~40 lines, unit-tested, and gives full control over the bitstream.

**Rejected:**
- **ffmpeg binary** — dealbreaker: a raw Annex-B stream carries no timestamps, so the CLI
  synthesizes them from a constant `-r` (default 25 fps). No way to inject per-frame PTS —
  precisely the failure this issue exists to avoid. Also a heavy external runtime dep.
- **mp4e** — zero-dep and Annex-B-friendly, but doesn't expose the explicit per-sample PTS
  control we need.
- **muxide** — the nicest API (Annex-B in, `avcC` + fast-start handled), but pre-1.0,
  single-author, ~6 months old, and pulls `clap`/`indicatif` into the dependency tree,
  which conflicts with the lightweight goal. Reconsider once it's more proven and gates its
  CLI deps behind a feature.

## Consequences

- We own `nal_units` / `annexb_to_avcc` / SPS-PPS extraction in `rewynd-mux`, all unit-tested
  (including a write → `Mp4Reader` round-trip) on CI without a GPU.
- The `mp4` 0.14 writer emits **mdat-first / moov-last** (no fast-start). That is fine for a
  local replay clip (players handle moov-last for on-disk files); web/streaming fast-start
  would need a post-shuffle pass and is out of MVP scope.
- `mp4` is **unmaintained upstream since 2023**. Mitigation: it's MIT/pure-Rust/vendorable,
  the writer surface we use is small and tested, and maintained API-compatible forks
  (`re_mp4`, `flowly-mp4`) exist as a drop-in if a fix is ever needed. Bumping/replacing the
  muxer is a localized change behind the `Muxer` trait.
