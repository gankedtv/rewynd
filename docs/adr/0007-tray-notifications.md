# ADR 0007 — System tray + notifications: ksni (not tray-icon) + notify-rust

- **Status:** Accepted (issue #17)
- **Supersedes / superseded by:** none
- **Relates to:** PLAN §6 (Phase 7 UX), ADR 0003/0004 (portal/D-Bus stack), issue #17

## Context

Issue #17 wants a tray icon, a recording indicator, and a "clip saved" toast for the recorder
(Linux/KDE Plasma Wayland). The recorder already runs a tokio multi-thread runtime, a PipeWire
capture loop, and the XDG ScreenCast + GlobalShortcuts portals via `ashpd` — i.e. it already speaks
D-Bus (zbus 5, pulled by ashpd). UI deps should be permissive and low-footprint.

## Decision

**Use `ksni` for the tray and `notify-rust` for the toast**, both on the existing zbus 5 / D-Bus
stack; no GTK. The tray runs as a background **task of the existing tokio runtime** (not a thread,
not a second event loop). It lives in `crates/app/src/tray.rs`; the recorder spawns it alongside —
and leaves untouched — the proven GlobalShortcuts hotkey loop. Menu clicks arrive as a `TrayCmd`
over an `mpsc` channel ("Save clip now" → the same `save_clip`, "Open settings" → launches the
sibling `rewynd-settings`, "Quit" → exits). The clip-saved toast fires from `save_clip`'s success
arm, so both the hotkey and the tray paths show it.

The tray icon is the gankedtv logo mark (`assets/tray.png`, generated from `logo-mark.svg`),
embedded via `include_bytes!` and decoded to ksni's ARGB32 with `image` (png feature only).

## Options evaluated

| Option | Renderer / transport | License | Verdict |
| --- | --- | --- | --- |
| **ksni** | pure-Rust SNI over D-Bus (zbus 5) | Unlicense | **chosen** — reuses our zbus stack, no GTK, no extra event loop, no CI build dep, KDE-native |
| `tray-icon` | libappindicator → SNI, needs a **GTK event loop** | MIT/Apache | rejected — pulls GTK + `libgtk-3-dev`/`libappindicator3-dev` build deps and a second event loop to coordinate with tokio |
| `notify-rust` (toast) | `org.freedesktop.Notifications` via zbus 5 | MIT/Apache | **chosen** for the toast |

## Rationale

- **No GTK, no second event loop:** `ksni` is a zbus service driven by our existing tokio runtime,
  so it composes with ashpd/PipeWire without the GTK-vs-tokio coordination (and known crashes) that
  `tray-icon` would add. It also matches the repo's deliberate no-GTK posture (the settings app uses
  `rfd`'s `xdg-portal` backend for the same reason — ADR 0006).
- **Zero new CI build deps:** ksni + notify-rust (zbus backend) are pure Rust over D-Bus.
- **Low risk to the hotkey path:** the tray is additive — the GlobalShortcuts loop is unchanged, so
  the core clip-on-hotkey behaviour can't regress from this change.

## Consequences

- New deps: `ksni` (**Unlicense** — public-domain-equivalent, permissive; noted as not literally
  MIT/Apache), `notify-rust` (MIT/Apache), `image` (MIT/Apache, png feature only).
- `ksni` is **Linux-only** — fine, the recorder is Linux-only at runtime; a Windows tray would use a
  different crate later, behind the same `TrayCmd` shape.
- The tray needs a live SNI host (KDE tray) to exercise, so it has no headless test and
  `app/src/` is excluded from the coverage gate (like the GPU/portal and iced-GUI code); it is
  validated live on the dev box.
- "Quit" uses `process::exit` (the OS reclaims the GPU/portal resources on exit); a graceful
  SIGTERM-driven shutdown is a possible future refinement (shared with the restart path).
