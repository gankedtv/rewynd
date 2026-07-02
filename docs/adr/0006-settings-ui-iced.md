# ADR 0006 — Settings UI: a standalone iced app (tiny-skia, no wgpu)

- **Status:** Accepted (issue #17)
- **Supersedes / superseded by:** none
- **Relates to:** PLAN §6 (Phase 7 UX), §3.6/§9 (licensing, low footprint), ADR 0001 (wgpu pin), ADR 0005 (config), issue #17

## Context

Phase 7 needs a way for users to view and edit settings as a **real application window** (not
only a tray), accessible to non-technical users. PLAN §6 deferred the UI-framework choice to this
point and required an MIT/Apache UI (no Slint-style GPL/commercial). The recorder also pins **wgpu
to a git rev (v29)** via a workspace-global `[patch.crates-io]` (ADR 0001), which rewrites *every*
wgpu/naga crate in the workspace.

## Decision

**A standalone `rewynd-settings` binary built with `iced` 0.14, using the `tiny-skia` software
renderer (no wgpu), as a normal workspace member.** (Renamed to `rewynd` for v1.0 — it is the
GUI a user launches; the recorder became `rewynd-recorder`.) It reads and writes the same
`config.toml` as the recorder (ADR 0005) — the file is the single source of truth, so there is no
IPC. Changes apply on the recorder's next clip / restart; the window says so after saving
(live-reload is a future refinement).

- **Renderer:** `iced = { default-features = false, features = ["tiny-skia", "tokio", "wayland",
  "x11"] }`. The `tiny-skia` feature selects iced's software backend; with `wgpu`/`wgpu-bare` off,
  the dependency tree contains **no wgpu/naga crate**, so the ADR-0001 patch can't bite and the
  crate builds as a normal workspace member. `wayland`/`x11` wire winit's and softbuffer's Linux
  backends (dropping iced's defaults dropped them). Software rendering is ample for a settings form
  and keeps the footprint low — no second GPU stack.
- **Decoupling:** the GUI depends only on the GPU-free `rewynd-config` crate (ADR 0005), never on
  `rewynd-encode` (which pulls wgpu/gpu-video unconditionally).
- **Folder picker:** `rfd` with the `xdg-portal` backend (no GTK build dependency), consistent with
  our portal-based capture. The async dialog future is driven by iced's tokio executor.

## Options evaluated

| Option | Renderer | License | Verdict |
| --- | --- | --- | --- |
| **iced 0.14 (tiny-skia)** | software | MIT | **chosen** — polished/app-like, no wgpu clash, lean |
| egui (egui-wgpu) | wgpu 29 | MIT/Apache | shares our wgpu device, but immediate-mode "dev-tool" look; user preferred iced's app feel |
| iced (wgpu backend) | wgpu 27 | MIT | conflicts with the wgpu-v29 patch (would need a workspace exclusion) |
| Tauri / Dioxus | OS webview | MIT/Apache | bundles a webview (WebKitGTK blank-window issues on NVIDIA/KWin); against the low-footprint goal |
| gtk4-rs | GTK | bindings MIT, **GTK is LGPL** | not MIT/Apache; heavy Windows deployment |
| Slint | — | GPL/commercial | excluded by PLAN §6 |

## Rationale

- **App feel + accessibility:** the user wants a polished, modern window that non-technical people
  can navigate — iced (retained, Elm-style, used by System76's COSMIC) fits better than egui's
  immediate-mode aesthetic. A custom dark `Theme`/`Palette` (charcoal + indigo accent) avoids the
  default look; the colours are top-of-file constants, one-line retunable to a future ganked.tv
  house style.
- **No wgpu clash, low footprint:** the tiny-skia path sidesteps the wgpu-pin conflict entirely and
  doesn't spin up a GPU stack for a settings window. During gaming only the light recorder daemon
  runs; the settings window is opened on demand.

## Consequences

- New permissive deps: `iced` (MIT) + `rfd` (MIT). CI gains one build dep, `libxkbcommon-dev`.
- The GUI needs a display to run, so `run` has no headless test; the pure mapping helpers are
  unit-tested and `settings/src/` is excluded from the coverage gate (like the GPU/portal code).
- A **tray icon + "clip saved" toast** was deferred here and has since landed — superseded on
  this point by ADR 0007, which chose `ksni` (no GTK) over the `tray-icon` path sketched below.
- Sanity-check on dependency drift: `cargo tree -p rewynd | grep -i wgpu` must stay empty (the GUI crate, formerly `rewynd-settings`).
  If iced later moves to wgpu-only, hold iced at this line or revisit per ADR 0001.
