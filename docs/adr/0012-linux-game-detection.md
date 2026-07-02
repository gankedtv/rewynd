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
stays saveable indefinitely (alt-tab, then save, still works — like ShadowPlay). The
first frame after the gate reopens forces an IDR so every recorded stretch starts on
a cuttable keyframe.

"Fullscreen game" mirrors the Windows heuristic: the focused toplevel is fullscreen
and not a desktop-shell app id (`game::is_shell_app_id`) — capture too little rather
than too much; windowed games fall under the desktop-capture opt-in.

The focused-window signal comes from a `FocusWatcher` with a backend chain:

1. `org_kde_plasma_window_management` (KDE, gives pid) — bound if advertised, but
   recent KWin (observed: Plasma 6.7) hides this global from ordinary clients.
2. `zwlr_foreign_toplevel_management_v1` (sway, Hyprland, niri, COSMIC, Wayfire, ...).
3. **KWin scripting over DBus** (the kdotool / GPU Screen Recorder technique): load a
   ~20-line KWin script via `org.kde.kwin.Scripting` that reports the active window's
   app id / title / pid / fullscreen state back over DBus on focus and fullscreen
   changes. This is the path that actually fires on current KDE, and was validated
   live on the dev box (Plasma 6.7, Overwatch under Proton). All values ride as
   strings because KWin's `callDBus` marshals JS numbers as doubles.
4. Nothing available (GNOME): log it and record the shared monitor continuously —
   the pre-#60 behaviour.

The watcher also names **per-game clip subfolders** (`[output] game_folders`, default
on): Proton windows carry `app_id = steam_app_<appid>`, resolved to the installed
game's title via `steamlocate` (local `appmanifest_*.acf`, no network); non-Steam ids
fall back to a cleaned app id, then the window title. `ClipSaver` sanitizes the name
into a filesystem-safe folder.

## Consequences

- Titles are never logged (they leak documents/URLs/chat); app ids are.
- The gate is evaluated per delivered frame; a static screen delivers few frames, but
  any focus change repaints, so the gate reacts in practice immediately.
- Windows keeps its own audio gap (between games the WGC stream stops but audio keeps
  ringing); noted at the mixer call site, not addressed here.
- The KWin script executes inside the compositor; it is written 0600 to
  `XDG_RUNTIME_DIR` (never reusing an existing file) and unloaded on drop, with a
  stale-script replace on startup.
