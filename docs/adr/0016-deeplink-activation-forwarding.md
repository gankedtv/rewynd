# ADR 0016 — Forwarding clip deep-links to a running settings window

- **Status:** Accepted (issue #131)
- **Supersedes / superseded by:** none
- **Relates to:** ADR 0008 (single-instance guard), ADR 0007 (tray + notifications)

## Context

Clicking a `rewynd://clip/<name>` "clip saved" toast launches the settings app with the link as
an argument. With no window open that works: the fresh instance resolves the link and opens the
clip. With a window already open, the second instance dies at the single-instance guard (ADR
0008) before it ever looks at its arguments — the user gets an "already open" notification and
the clip never opens. The refused instance needs a way to hand its link to the holder.

## Decision

An **activation file in the per-user instance dir**, consumed by a directory watch:

- The refused instance writes the raw link to `settings.activate` beside the lock files —
  write-aside then rename, atomic on unix and Windows, so a watcher never sees a partial file.
  A second hand-off before the first is consumed replaces it (last click wins).
- The running window watches the instance dir (`notify`, non-recursive, filtered to that one
  file name so recorder pid/status churn never wakes the UI) and consumes the file: read,
  delete, then re-validate through the same `clip_from_deeplink` path as a launch argument
  before opening the clip and raising the window.
- Consumption drops anything stale (mtime older than 30 s — a leftover from a crashed window
  must not replay an old clip at the next launch), oversized, or non-text. `App::load` also
  consumes a pending file, covering a hand-off that lands before the watch arms.
- If the hand-off fails, the refused instance falls back to today's "already open" notification.

## Options evaluated

| Option | Verdict |
| --- | --- |
| **activation file + dir watch** | **chosen** — one cross-platform implementation, no new FFI, fully CI-testable; the instance dir (0700-verified on unix, per-user on Windows) and `notify` are already load-bearing here |
| named pipe (Windows) + unix socket | rejected — positive delivery ack, but two hand-rolled platform paths (`CreateNamedPipeW` FFI incl. a restrictive DACL, socket lifecycle/stale-inode handling), none of it coverable in CI on the other OS |
| D-Bus / platform activation APIs | rejected — heavier than the problem and Linux-only; Windows would still need its own path |
| kernel event + side file | rejected — the existing `NamedEvent` is Windows-only, so unix would need a second mechanism anyway; the watch already gives cross-platform wakeups |

## Rationale

- **DRY:** the sender is ~10 lines over `std::fs`; the receiver reuses the settings app's
  existing `notify` subscription pattern and the existing deep-link validation. No unsafe.
- **Trust:** the file only ever acts as a *candidate* link. The receiver re-validates it
  exactly like a launch argument (scheme, name shape, traversal rejection), so a planted file
  can at worst open a clip the user already owns.
- **Self-cleaning:** every look at the file consumes it; freshness bounds replay.

## Consequences

- Delivery is fire-and-forget: the sender can't tell whether the holder actually consumed the
  link (it exits successfully once the file is placed). If the holder's watcher died, the click
  is lost silently — accepted; the watcher failing is loud in the logs and rare.
- The Windows settings lock now also creates the instance dir (unix always did), so the watch
  target exists as soon as the guard is held.
- The tray's "Open settings" with a window already open still shows the notification rather
  than raising the window; the same channel could carry a plain "raise" activation later (the
  refinement ADR 0008 already flagged).
