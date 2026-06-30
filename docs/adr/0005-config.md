# ADR 0005 — Runtime configuration: a TOML file with environment overrides

- **Status:** Accepted (issue #16)
- **Supersedes / superseded by:** none
- **Relates to:** PLAN §6 (Phase 7 UX), §9 (parameters never hard-coded), issue #16

## Context

Until now the runtime parameters (resolution, framerate, bitrate, audio rate/channels/bitrate)
came from built-in defaults with a few `REWYND_*` environment overrides for dev use; the buffer
window, output directory, and hotkey trigger were hard-coded constants. Issue #16 needs these to
be user-configurable and to **load from a file and take effect**: buffer length, hotkey, output
directory, monitor — plus per-source audio gain (the reported quiet/centred-mic follow-up). A
settings GUI (#17) will read and write the same file.

## Decision

**A single TOML file deserialized with `serde`, layered between the built-in defaults and the
existing `REWYND_*` environment overrides.** Precedence, low → high:

> built-in defaults **<** `config.toml` **<** `REWYND_*` environment variables

The file lives at `$XDG_CONFIG_HOME/rewynd/config.toml` (falling back to
`$HOME/.config/rewynd/config.toml`) — matching the XDG handling already used for the ScreenCast
restore token (`$XDG_STATE_HOME`) and the desktop entry (`$XDG_DATA_HOME`). A commented default
file is written on first run so the settings are discoverable.

Lives in `crates/app/src/config.rs` (platform-agnostic and unit-tested on CI — the coverage gate
covers it, unlike `app/src/main.rs`). The schema mirrors the encode/audio params plus app knobs:
`[video]`, `[audio]` (incl. `mic_gain` / `system_gain`), `[buffer]`, `[output]`, `[hotkey]`,
`[capture]`. Unknown keys are rejected (`deny_unknown_fields`) so typos surface rather than being
silently ignored; a missing or malformed file degrades to defaults (logged) rather than blocking
startup.

## Options evaluated

| Option | Verdict |
| --- | --- |
| **TOML + serde** | **chosen** — idiomatic for Rust desktop apps, human-editable, trivially serde-derived; `toml` 1.x + `serde` are MIT/Apache |
| JSON / YAML | JSON has no comments (poor for a hand-edited file); YAML is heavier and footgun-prone |
| Custom / env-only | env-only doesn't satisfy "load from a file"; a bespoke format reinvents serde |

## Rationale

- **Monitor selection on Wayland** is owned by the ScreenCast portal, not us: the share-picker +
  persisted restore token choose the monitor. The config lever is `capture.always_prompt`, which
  ignores the saved token so the picker re-appears and a different monitor can be chosen; the new
  selection then persists. True per-output selection by config isn't possible through the portal.
- **Hotkey** feeds the GlobalShortcuts `preferred_trigger`; the compositor still owns the final
  binding (the user can rebind it in the desktop's shortcut settings), so this is a preference, not
  a guarantee.
- **Audio gain** is a per-source linear multiplier applied before mixing; the mixer already clamps
  on drain, so a gain above unity can't overflow. This is the configurable fix for a quiet mic.
- **Env overrides retained** on top so existing dev workflows (and tests) keep working.

## Consequences

- New permissive deps: `serde` (derive) and `toml` 1.x (both MIT/Apache).
- The settings GUI (#17) reads/writes this same file; the file is the single source of truth for
  configuration, so the GUI and the headless daemon stay in sync without extra IPC.
- `deny_unknown_fields` means a single stray/typo'd key fails the whole parse (logged) and falls
  back to defaults — a deliberate trade favouring a loud, actionable error over a silent partial
  apply.
- `capture.always_prompt` is a **portal-specific** knob (Wayland's ScreenCast portal owns monitor
  choice). A future Windows backend has no portal, so it will need a different mechanism — likely a
  concrete `[capture] monitor = "..."` selector — and the schema may diverge per-OS there.
- `buffer.seconds` is clamped to a generous ceiling (an hour) so a fat-fingered value degrades to a
  capped window rather than growing the in-memory ring buffer until it OOMs.
