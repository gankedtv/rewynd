# ADR 0021: Windows game detection — reject non-games, and release a window that leaves fullscreen

## Status

Accepted (issue #207).

## Context

Windows game-only capture (`[capture] desktop = false`, the default) picks its target
once, at session start: the foreground window, if it is a valid capture target, is not
a desktop-shell process, and covers its whole monitor (ADR 0012). That window then
becomes the WGC capture item, and the session ends only when its HWND is destroyed.

Both halves are too permissive. A browser playing a video fullscreen, a chat client
showing a stream, a media player, and any maximized window on a monitor without a
taskbar all satisfy "covers its monitor" exactly as a borderless game does. And once
such a window is latched, leaving fullscreen changes nothing — the window still
exists, so the recorder keeps capturing it (now a windowed app), and a game started
afterwards is never even considered, because detection only runs between sessions.
Reported in the field as watching anime or using Discord before launching a game, then
finding the tray reading "Recording: Discord" for the rest of the session.

Linux and macOS never had this shape: they gate a monitor stream on a continuously
re-derived "focused ∧ fullscreen ∧ not a shell app" and so recover on their own.

## Decision

Four rules, all of them keeping the ADR 0012 stance — capture too little rather than
too much.

**1. A known-non-game process is never a capture target.** Next to the existing
shell list (`SHELL_PROCESSES`: the desktop, task switching, the lock screen, rewynd
itself) sits `NON_GAME_PROCESSES`: apps that routinely own a monitor-sized window and
are never the game — browsers, media players, chat and conferencing clients,
storefronts and launchers (Steam Big Picture included), remote-desktop viewers, other
recorders — plus any `*.scr` screensaver by suffix. Remote *play* clients (Parsec,
Moonlight) are deliberately absent: there the user is playing a game. A process whose
name cannot be read stays a candidate, because anti-cheat (Vanguard, EAC) denies
`OpenProcess` and that is a strong game signal; every process the lists guard against
is queryable.

**2. A maximized window that still shows its title bar is not a game.** Every engine's
fullscreen and borderless mode drops the caption (Unity, Unreal, GLFW/LWJGL, SDL,
Source, Godot, Qt, and Chromium — which goes fullscreen by stripping `WS_CAPTION |
WS_THICKFRAME` off a maximized window, so a fullscreen browser is style-identical to a
borderless game and only rule 1 can reject it). `WS_CAPTION` together with
`WS_MAXIMIZE` therefore means a windowed app spread over a taskbar-less monitor. The
caption is matched as both its bits, so a `WS_POPUP | WS_BORDER` borderless window is
not mistaken for a decorated one, and an unreadable style never disqualifies.

**3. A captured window is released once it stops being fullscreen.** The session
watchdog re-checks the latched window every `STOP_POLL` (200 ms) through a `keep_alive`
callback, against the **same `WindowState` the detector uses** — so a window can never
be kept under a rule that would not have latched it in the first place, which is what
stops a latched window from surviving by growing a title bar. Not being fullscreen for
`RELEASE_GRACE` (1 s) ends the session as a clean stop and hands control back to the
detector, which picks up the real game on its next poll. The grace rides out the
flicker of a display-mode switch.

A **minimized** window gets a much longer allowance, `MINIMIZED_GRACE` (30 s), because
WGC delivers no frames for one — nothing leaks — and exclusive-fullscreen games
minimize on every alt-tab, where releasing would cost the replay buffer's continuity
(ADR 0012: a clip never spans a gated-off gap, so a new stretch clears the rings). The
allowance is bounded rather than unlimited: a game left minimized while a second one
runs would otherwise hold the recorder for the rest of the session. Releasing a
minimized window is cheap — the rings are cleared when the *next* game starts, not
when a session ends, so the minimized game's footage stays saveable in the meantime,
exactly as on Linux. One clock runs for the whole stretch away from fullscreen, so
minimizing a window that had already left fullscreen buys the longer grace without
forgiving the time already spent.

The watchdog never asks which window is foreground, so merely losing focus cannot
release a session: a borderless game that yields focus to a chat window on the second
monitor keeps recording, like ShadowPlay. The timing lives in a pure `Latch` state
machine so the transitions are unit-tested without a window.

**4. No preemption.** A fullscreen window appearing in the foreground does not steal
an active session. The two cases — a game latched while a fullscreen video takes focus,
and a video latched while a game takes focus — are not distinguishable from focus
alone, and preempting on every switch would clear the rings each time. Rules 1-3
remove the reported cases before a bad latch happens at all.

The detector and the `game_probe` diagnostic share one ordered `classify` returning a
`Verdict`, so the probe prints the decision that was actually made, naming the rule
that rejected a window.

## Consequences

- A mode switch that keeps a window out of fullscreen for longer than the grace
  restarts the session, and with it the replay buffer. That is the same cost Linux
  pays per gated stretch, and the price of not staying stuck on a stale window.
- Windowed games maximized on a monitor without a taskbar are no longer captured
  (they matched by accident, and only there). They need borderless mode or the
  desktop-capture opt-in — which is what the documented policy already said about
  windowed games.
- An unlisted non-game that runs a caption-less fullscreen window on another monitor
  can still hold a session until it leaves fullscreen. Follow-up: a per-app
  "record / never record this app" override, or preemption with long hysteresis.
- **Cloud gaming played in a browser is no longer captured** (GeForce NOW, Xbox Cloud
  Gaming, Luna in a fullscreen tab), which does cut against the reason Parsec and
  Moonlight stay eligible: there too the user is playing. A browser cannot be told
  apart from one playing a video by process name, and the reported bug is exactly a
  fullscreen browser, so the list wins for now. Those sessions need the
  desktop-capture opt-in until the per-app override lands.
- A game left minimized for longer than `MINIMIZED_GRACE` releases the session. Its
  footage stays saveable until another game starts, but resuming play begins a new
  stretch.
- The lists are maintained by hand and match on executable name only. A wrong or
  missing entry costs a false negative for that app; the probe names the rule, so a
  report carries the evidence.
- Pre-existing and unchanged: while a captured game is minimized the recording gate
  stays open, so audio accrues while video stalls. A clip saved after a long minimize
  can drift. Follow-up alongside the gate work in ADR 0012.
