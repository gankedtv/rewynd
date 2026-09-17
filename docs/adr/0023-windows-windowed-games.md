# ADR 0023: Windows game-only capture records known windowed games

## Status

Accepted (issue #214).

## Context

Windows game-only capture (ADR 0012, ADR 0021) accepts the foreground window as the
game only when it covers its whole monitor without a title bar: how exclusive and
borderless fullscreen games present. Games that are commonly played in a window,
Minecraft first among them, never qualified, and a maximized Minecraft is rejected
outright as a decorated window. Minecraft Bedrock could not qualify even fullscreen:
a UWP app's top-level window belongs to `ApplicationFrameHost.exe`, which is on the
shell list.

The fullscreen rule exists because a windowed app cannot be told from a windowed game
by geometry or style. Any relaxation must therefore name games.

## Decision

**Known windowed games qualify at any size.** `WindowedGames` holds two kinds of
rule:

- *Built in*: Minecraft Java (a `javaw.exe`/`java.exe` whose title starts with
  "Minecraft", so an IDE on the same JVM does not match) and Minecraft Bedrock
  (`Minecraft.Windows.exe`). A built-in rule carries the game's name, which labels the
  clip folder and the tray instead of `javaw`.
- *Configured*: `[capture] windowed_games`, a list in the settings app shown when
  desktop capture is off. An entry ending in `.exe` matches the process name; anything
  else is a case-insensitive substring of the window title. A named process beats the
  shell and non-game lists: it is the user's word. A title fragment does not, because
  titles are ambiguous: "balatro" also matches a browser tab on the game's wiki and the
  Steam store page, and latching onto those is the very thing game-only capture exists to
  avoid.

A matched window is `Capturable` while visible and not minimized, whatever its size or
decoration; the latch of ADR 0021 keeps it under that same rule, so shrinking the window
does not release it while minimizing still does after its grace. Everything else keeps
the fullscreen rule.

**A UWP frame is judged by the app it hosts.** When the foreground process is
`ApplicationFrameHost.exe`, the `Windows.UI.Core.CoreWindow` child's process stands in
for it, for the exclusion lists, the rules and the game name. A frame with no such child
(a suspended app) stays excluded as the host.

**WGC captures the window as it is.** The window's size is whatever it is and may
change; the shareable-slot pool already recreates on a size change, windows-capture
recreates its frame pool, and the NV12 pass letterboxes any aspect into the encode size
(ADR 0019). A decorated window's title bar is part of the capture.

**Process names keep their dotted stems.** `Minecraft.Windows.exe` names the game
`Minecraft.Windows`, not `Windows`: only reverse-DNS ids without `.exe` take their last
segment.

## Consequences

- A windowed game is recorded with whatever overlaps it, since WGC window capture draws
  the window's own surface: other windows on top are not included, the title bar is.
- The lists stay short on purpose. A game not covered plays fullscreen or gets an entry;
  the settings hint says how.
- Linux and macOS ignore the list: they gate a monitor stream on a fullscreen focus and
  have no window to capture. The settings field is hidden there.
- The probe (`game_probe`) reports a windowed match with its own verdict, and shows the
  hosted process for UWP frames.
