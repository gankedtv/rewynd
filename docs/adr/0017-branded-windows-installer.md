# ADR 0017 — Branded Windows installer: our own front, Velopack underneath

- **Status:** Accepted (issue #139)
- **Supersedes / superseded by:** none
- **Relates to:** ADR 0016 (deep-link forwarding), docs/design/arena.md, docs/code-signing-policy.md

## Context

The stock Velopack `Setup.exe` with `--splashImage` installs correctly (per-user, no admin,
auto-launch) but the experience is a splash that flashes by: no deliberate start, no branded
surface — nothing like the launcher-style installers users know from game tooling. The goal is
an install in rewynd's own Arena design that starts with an explicit **Install** click and ends
in the app's first-run wizard, without touching the Velopack update contract (Update.exe,
deltas, the in-app one-click update).

## Decision

Ship **`rewynd-win-Installer.exe`** (crate `crates/installer`): a borderless Arena-styled iced
window — brand mark, version, one Install button, an indeterminate progress sweep — that stages
the **real `Setup.exe` embedded in itself** and runs it with `--silent`. Setup.exe still does
every bit of the actual install, so the on-disk result is byte-for-byte what Velopack expects.
In silent mode Setup.exe skips its own end-of-install launch (verified in its source:
`commands::install` guards `start_package` behind `!dialogs::get_silent()`), so the installer
starts `%LocalAppData%\rewynd\current\rewynd.exe` itself; exit code 0/1 is the success signal,
and the `--log` tail feeds the failure screen.

The release workflow builds the installer *after* `vpk pack`, pointing `REWYND_SETUP_EXE` at
the fresh Setup.exe for `build.rs` to embed; dev builds embed nothing and fall back to a
Setup.exe sitting beside the exe. The plain `rewynd-win-Setup.exe` stays published for
unattended installs and as the winget candidate.

Deliberate non-features: no install-location choice (per-user `%LocalAppData%` keeps the
update contract and the no-admin constraint; `Setup.exe --installto` remains for power users)
and no license/options pages — choices live in the first-run wizard, not the installer.

## Options evaluated

| Option | Verdict |
| --- | --- |
| **own exe wrapping `Setup.exe --silent`** | **chosen** — unlimited branding, the install itself stays Velopack's, maintainer-endorsed pattern (velopack/velopack#177) |
| `vpk pack --msi` | rejected — a stock WiX-5 wizard with two swappable bitmaps (banner 493×58, logo 493×312); enterprise chrome, not a launcher, and its default `Either` scope invites a UAC path |
| splash only (status quo) | rejected as the end state — no Install button, no cancel, no progress (upstream velopack/velopack#102); kept as the unattended fallback |
| re-implementing install via `Update.exe apply` | rejected — we would own Velopack's on-disk contract (shortcuts, registry, `sq.version`) and break updates the first time it drifts |

## Rationale

- **Update contract untouched:** the wrapper never writes into the install dir; only Setup.exe
  does. The `velopack` Rust crate is updates-only and cannot perform a fresh install, so
  driving the real Setup.exe is the only supported route.
- **Progress honesty:** Setup.exe exposes no progress IPC, so the UI shows an indeterminate
  sweep rather than a fabricated percentage; failures surface the log tail.
- **Software rendering:** the installer renders with `tiny-skia` only — it runs before we know
  anything about the machine's GPU, and a static window needs no wgpu.

## Consequences

- A second user-launched executable: added to the code-signing policy's artifact list, and it
  ships unsigned until SignPath (#137) lands — as a brand-new exe it starts with zero
  SmartScreen reputation, which is the accepted cost of moving the download link to it.
- The README's primary Windows download becomes the branded installer; Setup.exe remains
  published beside it (unattended installs, winget #123).
- The installer duplicates a hand-kept slice of the Arena palette from
  `crates/settings/src/theme.rs`; a shared theme crate is not worth the split for one extra
  window, revisit if a third window appears.
- `installer/src/main.rs` (the iced window) joins the coverage ignore list; the Setup.exe
  driver in `setup.rs` stays covered.
