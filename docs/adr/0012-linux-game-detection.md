# ADR 0012: Linux game detection — gate the monitor stream on the focused fullscreen window

## Status

Accepted (issue #60).

## Context

Game-only capture is the default (`[capture] desktop = false`): recording the whole
desktop can catch private content. Windows targets the game window directly
(WGC on the detected fullscreen foreground window). On Wayland that is impossible for
an unprivileged app: the XDG ScreenCast portal deliberately hides the window list, and
there is no programmatic "capture that window" without the user picking it in the
portal dialog per game.

## Decision

Keep the portal's **monitor** stream (one picker dialog ever, restore token persists),
and **gate the pipeline** instead: frames are dropped before import/encode unless a
fullscreen window is focused, and the audio mixer drops its packets over the same
flag. A fullscreen game covering the monitor means the monitor stream shows exactly
the game, so the recorded pixels match Windows' window capture in practice. While the
gate is closed nothing is pushed, so ring eviction stalls and the last game footage
stays saveable (alt-tab, then save, still works — like ShadowPlay).

The gate is **event-driven, never frame-driven**: the watcher publishes every
focused-game transition through one funnel (`Shared::publish`) which runs the app's
reaction (`game_gate::reaction`) on the watcher's own thread. That reaction flips the
shared `recording` flag (read by the frame callback and the audio mixer), resolves
the game's display name (steamlocate does file IO — deliberately off the PipeWire
callback, where a panic would unwind across FFI), and keeps the saver's folder
current. Frame delivery is damage-driven and can stop entirely (static desktop,
lock screen, focus moving to another monitor), so state must never wait for a frame.

**A clip never spans a gated-off gap.** The muxer writes audio packets back-to-back
and assumes contiguous capture; a pause inside a clip would freeze video and slide
all later audio early. So when a (new or resumed) game starts recording, both rings
are cleared: every saved clip is one contiguous stretch. The cost is deliberate:
footage from before an alt-tab is saveable *during* the pause, but gone once play
resumes. The first frame into an empty ring (and the first after a pause) forces an
IDR so every stretch starts on a cuttable keyframe.

**Fail closed.** If a watcher backend dies (Wayland error, wlr `finished`, KWin
leaving the bus) it publishes `None`: recording pauses rather than silently capturing
the desktop under a stale "game focused". The KWin backend watches
`NameOwnerChanged` for `org.kde.KWin` and reloads its script after a compositor
restart.

"Fullscreen game" mirrors the Windows heuristic: the focused toplevel is fullscreen
and not a desktop-shell app id (`game::is_shell_app_id`) — capture too little rather
than too much; windowed games fall under the desktop-capture opt-in.

The focused-window signal comes from a `FocusWatcher` with a backend chain:

1. `org_kde_plasma_window_management` (KDE, gives pid) — bound if advertised, but
   recent KWin (observed: Plasma 6.7) hides this global from ordinary clients.
2. `zwlr_foreign_toplevel_management_v1` (sway, Hyprland, niri, COSMIC, Wayfire, ...)
   — bound at version ≥ 2, where the `fullscreen` state entry appeared; on a v1
   server the gate could never open.
3. **KWin scripting over DBus** (the kdotool / GPU Screen Recorder technique): load a
   ~40-line KWin script via `org.kde.kwin.Scripting` that reports the active window's
   app id / title / pid / fullscreen state back over DBus on focus, fullscreen and
   geometry changes (deduplicated against the last payload, so window drags don't
   flood the bus). Hooks attach once per window via `windowAdded` + a sweep of
   existing windows. A window also counts as fullscreen when its frame covers its
   whole output — borderless-windowed games never set the compositor fullscreen
   state. This is the path that actually fires on current KDE, and was validated
   live on the dev box (Plasma 6.7, Overwatch under Proton). All values ride as
   strings because KWin's `callDBus` marshals JS numbers as doubles.
4. Nothing available (GNOME): log it and record the shared monitor continuously —
   the pre-detection behaviour.

The watcher also names **per-game clip subfolders** (`[output] game_folders`, default
on): Proton windows carry `app_id = steam_app_<appid>`, resolved to the installed
game's title via `steamlocate` (local `appmanifest_*.acf`, no network); non-Steam ids
fall back to a cleaned app id, then the window title. `ClipSaver` sanitizes the name
into a filesystem-safe folder.

## Consequences

- Titles are never logged (they leak documents/URLs/chat); app ids are.
- Windows adopts the same shared reaction: the WGC game-session callbacks drive the
  `recording` flag (audio now pauses between games too), the per-game folder, and the
  ring clears — behaviour matches Linux.
- A few desktop frames can slip in around an alt-tab: the compositor repaints before
  the watcher's event lands. Bounded to that latency (milliseconds), and those frames
  are dropped from the next stretch anyway by the ring clear.
- The KWin script executes inside the compositor; it is written 0600 to
  `XDG_RUNTIME_DIR` (never reusing an existing file) and unloaded on drop, with a
  stale-script replace on startup and a reload after a KWin restart.
- On wlroots compositors only the compositor fullscreen state gates (no geometry
  heuristic), so borderless-windowed games there need the desktop-capture opt-in.
