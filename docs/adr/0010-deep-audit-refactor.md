# ADR 0010 — Deep-audit refactor: shutdown funnel, clip crate, hardening, honest MSRV

- **Status:** Accepted (issue #65)
- **Supersedes / superseded by:** amends 0001 (adds a mechanical pin gate), 0004 (atomic clip
  writes), 0005 (salvage parsing, sanitized accessors), 0009 (streaming PUT lands)
- **Relates to:** the six-lens audit + dependency review behind issue #65

## Context

After Phase 7 + the upload flow landed, a full audit (architecture, robustness, security, code
quality, tests, product wiring — 84 findings) and a dependency review were run. This ADR records
the structural decisions; small fixes ride along without ceremony.

## Decisions

- **One shutdown funnel.** Tray Quit, SIGTERM, SIGINT and hotkey-session loss all end in the same
  ordered teardown (stop flag → portal close → capture join → audio joins → mixer drain + Opus
  flush). `process::exit` is gone; the capture stream gained a cooperative stop watchdog so
  shutdown no longer depends on a portal close succeeding. Fatal startup errors surface as a
  desktop notification (the recorder is windowless); a failed/lost hotkey degrades to tray-only
  saving with rebind attempts instead of killing the recorder.
- **`rewynd-clip`.** The save path (cut both rings → pick a path → mux) moved out of the
  CI-excluded binary into a small crate with a `ClipSaver` handle — one bundle instead of six
  loose parameters at three call sites, unit-tested, with save failures now user-visible
  (distinct "nothing to save" / "could not write" toasts). It also signals the mixer to drain
  its in-flight tail before the audio cut, and falls back to the newest on-disk clip after a
  restart for "Upload last clip".
- **Honest MSRV as security policy.** `rust-version` was 1.85 while the tree's real floor was
  1.88; MSRV-aware resolution therefore pinned the RUSTSEC-2026-0009 `time`. Policy: keep
  `rust-version` at the true floor (now 1.89). CI gained a `cargo-deny` job (advisories, a
  GPL-compatible license allowlist per PLAN §3.7, git-source pinning per ADR 0001) and runs the
  vendored mp4 fork's own tests; third-party actions are pinned to commit SHAs.
- **Zero-copy cuts.** Ring chunks hold `Arc<[u8]>`, so cutting a clip clones ref-counts instead
  of copying up to ~750 MB under the mutex the capture thread needs. The two ring buffers share
  one generic core. Clips are written atomically (`.mp4.part` + rename).
- **Capture negotiates the configured size** (was a hard-coded 1920×1080 preference, violating
  the resolution-is-a-parameter rule) and the NV12 pass scales to the encoder size — its
  fullscreen pass samples with normalized UVs through a linear sampler, verified by a live GPU
  test. The dead `FrameSource`/`GpuFrame` seam was deleted: the Windows backend will be designed
  against the real callback-shaped API, not a stub that never matched it. Probe entry points sit
  behind a `probes` feature.
- **Hardening set** (each small): panics can no longer unwind across PipeWire C callbacks
  (caught, stream stops cleanly); secrets never reach logs (`Debug` redaction) or hostile peers
  (no redirects, http only on loopback, bounded error-body reads, clamped device-grant values);
  first-run config is 0600 with a 0700 dir; the temp-fallback instance dir is owner-verified
  with `O_NOFOLLOW` locks; notification bodies escape server-provided text; `%` is escaped in
  desktop-entry Exec values; clip temp fallback is a 0700 per-user dir; settings signal the
  recorder via `libc::kill` instead of a PATH-resolved binary.
- **Config salvage parsing.** An unknown key in one section no longer silently resets the whole
  config to defaults: known sections parse independently and only the offending section defaults
  (strict parsing is kept for the settings editor). Config accessors sanitize hand-edited
  video/audio values (Opus rates, even dimensions, bounded rates) like `buffer_window` always did.

## Consequences

- New crate `rewynd-clip` (CI-covered); `rewynd-config` split into modules (schema/paths/lock/
  desktop/process) with an unchanged public surface plus `stop_recorder`, `sibling_binary`,
  `install_launcher_entry`, `non_empty_or`.
- New deps: `tokio-util` (streaming PUT body), `tempfile` (dev-only). `futures-util` is now a
  workspace dep. MSRV 1.89 unlocks let-chains, used where they simplify.
- Deliberately NOT done: a full `Recorder` orchestration struct beyond thread ownership (the
  wiring is thinner now; revisit with Windows parity), per-clip visibility (issue #51), tray
  menu-item enablement states, i18n string tables.
