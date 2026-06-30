# ADR 0004 — A/V mux: Opus audio track + sync, via a vendored `mp4` fork

- **Status:** Accepted (boxes verified byte-for-byte vs ffmpeg on the dev box, 2026-06-30)
- **Supersedes / superseded by:** extends ADR 0002 (MP4 muxer)
- **Relates to:** issue #14 (A/V mux + sync), PLAN §6.4; depends on #12 (video mux) and #13 (audio capture)

## Context

The MVP writes a video-only MP4 (ADR 0002, the `mp4` crate 0.14). Phase 5 adds the
captured system audio (ADR 0003) as a second, time-synced track. We need: an Opus audio
track in the MP4 (Opus is forced by the GPL-3 license — AAC/fdk-aac is GPL-incompatible),
per-sample timing on the same microsecond-accurate basis as video, and the two tracks kept
in lip-sync despite coming from two independent capture streams.

The `mp4` crate models codecs as a closed enum with no extension hook and has no Opus
support, and upstream (alfg/mp4-rust) is archived — there is nothing to upstream a patch to.

## Decision

1. **Vendor-fork the `mp4` crate in-tree at `vendor/mp4`** (a path dependency, *not* a
   workspace member) and add Opus support additively. In-tree vendoring (vs a `[patch]`
   git fork) is reproducible with no external repo to maintain, and the diff is reviewable
   in our own PRs. Keeping it out of `[workspace].members` means clippy/fmt/coverage gates
   skip the third-party source; `vendor/mp4/` is added to the coverage `--ignore-filename-regex`.
   The Opus additions: a new `Opus` sample entry + `dOps` box (`src/mp4box/opus.rs`), an
   `OpusConfig` + `MediaConfig::OpusConfig` variant, `MediaType::OPUS`, the `stsd`/`track`
   wiring, and a `set_track_edit_list` writer hook for the pre-skip edit list.

2. **Audio codec: Opus via the `opus` crate (libopus).** libopus is BSD-3 (GPL-3-compatible)
   and the `opus` bindings are MIT/Apache-2.0. Encode interleaved F32 at 48 kHz stereo,
   128 kbps VBR (bitrate is a parameter, not hard-coded — per CLAUDE.md). Audio is encoded
   in `rewynd-encode` (codec work) into bare Opus packets; `rewynd-mux` stays codec-agnostic.

3. **A/V sync via a single shared monotonic epoch.** Video and audio capture each stamped a
   PTS from their own `Instant::now()` epoch, which are not comparable. Both capture entry
   points now take a shared `epoch: Instant`, so every `EncodedChunk`/audio packet PTS is
   relative to the same `t0`. The clip rebases **both** tracks against the same base (the
   clip's first video chunk PTS); the audio track's small non-zero start offset is preserved
   (not zeroed), keeping lip-sync. Video samples carry µs durations (timescale 1_000_000);
   audio samples carry their Opus frame length in 48 kHz ticks (track timescale 48000).

4. **Opus pre-skip trim via an edit list (`elst`), now — not deferred.** `dOps.PreSkip` is
   set to the encoder lookahead (queried, not hard-coded). For correct playback on *all*
   players (including browsers/MSE, which ignore `PreSkip` and rely on `elst`), the audio
   track also carries a one-entry edit list: `media_time = PreSkip` (track timescale),
   `segment_duration = (total_samples − PreSkip)` converted to the movie timescale. ganked.tv
   re-encodes uploads to AV1, but raw clips are played/shared directly, so standalone
   correctness is the better default (user's call).

## Box layout (RFC 7845 §5, "Encapsulation of Opus in ISO-BMFF")

- `Opus` sample entry: an `AudioSampleEntry` (same prefix as `mp4a`) with `channelcount`,
  `samplesize = 16`, `samplerate = 48000 << 16` (Opus always decodes at 48 kHz, regardless
  of source rate), and a child `dOps`.
- `dOps` (`OpusSpecificBox`): **not a FullBox** — its leading byte is the box's own
  `version = 0`. All multi-byte fields big-endian (unlike Ogg's little-endian `OpusHead`):
  `OutputChannelCount`, `PreSkip` (u16), `InputSampleRate` (u32), `OutputGain` (i16, 0),
  `ChannelMappingFamily = 0` (no trailing mapping table for mono/stereo). 19 bytes total.
- One MP4 sample == one Opus packet, all `is_sync = true`, payload is the bare Opus packet.

## Validation

- Unit test asserts the `dOps` bytes exactly; cross-checked **byte-for-byte against
  `ffmpeg -c:a libopus`** output (identical `dOps`; the `Opus` entry prefix matches — ffmpeg
  only appends an optional `btrt` box we omit). A `rewynd-mux` write→`Mp4Reader` round-trip
  proves the two-track file parses back (video H.264 + audio Opus, sample counts, timing).
- A/V lip-sync is validated live on the dev box: ffprobe shows two in-spec streams with
  sane per-track `start_time`/duration, and a clip with a known clap/beep lines up audio to
  the video frame in VLC/mpv (and a browser `<video>`), with no leading priming click.

## Consequences

- `vendor/mp4` is third-party code we now own; the Opus additions are small, isolated, and
  tested. Bumping/replacing the muxer stays localized behind the `Muxer` surface (ADR 0002).
- `rewynd-buffer` gains an audio lane (Opus packets have no keyframe concept — a simple
  time-windowed ring), flushed for the same window as video.
- libopus becomes a build/runtime dependency (system `libopus`, added to CI apt deps).
