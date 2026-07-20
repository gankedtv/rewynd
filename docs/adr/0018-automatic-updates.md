# ADR 0018 — Automatic updates: background download, install at recorder start

- **Status:** Accepted (issue #161)
- **Supersedes / superseded by:** none
- **Relates to:** ADR 0008 (single-instance guard), ADR 0017 (branded Windows installer)

## Context

Velopack installs could already update, but only through the settings sidebar's manual
"Check for updates" button. A background recorder that most users never open a window for
should keep itself current. Constraints:

- The recorder must never die mid-session: Velopack's updater force-kills any process left
  in the install dir while applying, and a recorder mid-MP4-write (or a settings window
  mid-edit) must not go that way.
- Applying restarts the **package's main exe**, which is the GUI (`rewynd`), not the process
  that called apply. A naive recorder-side apply at boot would pop a settings window.
- `VelopackApp`'s `auto_apply_on_startup` **defaults to on** in the `velopack` crate: as soon
  as any flow leaves a downloaded update pending, the next launch of either binary would
  silently apply it — including a settings launch, whose apply would force-kill the running
  recorder without the stop-first handshake the manual flow performs.
- Package-manager installs (AUR) have no Velopack receipt; their updates belong to the
  package manager.

## Decision

**The recorder downloads in the background and installs only at its own start; both binaries
opt out of Velopack's implicit auto-apply.**

- A new `[updates] auto_install` config (default on, settings toggle shown only in Velopack
  installs) gates everything. No receipt ⇒ every entry point is inert, so dev runs and AUR
  installs never self-update regardless.
- On every recorder start, after the single-instance lock (ADR 0008) and before the capture
  pipeline exists, a **previously downloaded** update is applied — the process is replaced
  while nothing is buffering or writing. The apply passes `--recorder` as the restart
  argument, so the relaunched GUI hands straight off to the new recorder without a window.
- The apply is skipped while a settings window is open (probed via the settings
  single-instance guard) and on degraded lock-less starts: never yank a live peer.
- A detached recorder thread checks the feed shortly after start and then daily, and only
  **downloads**. The download installs at the next recorder start, or immediately when the
  user presses the sidebar button (which keeps its stop-recorder-first handshake).
- Both `VelopackApp` bootstraps call `set_auto_apply_on_startup(false)`; apply timing is
  always explicit.

## Consequences

- A fresh release installs at the next boot after its background download — typically the
  next gaming session. Nothing updates mid-session; the window between release and install
  is bounded by the daily check plus one recorder restart.
- The settings window keeps sole ownership of "update right now", with its existing
  stop-confirm-apply sequence.
- The boot-time apply briefly holds both the recorder and settings locks' attention: the
  settings probe acquires the lock for a microsecond, so a settings app launched in exactly
  that instant can see "already open" once — benign and retryable.
- If the user never reboots and never opens settings, the recorder still picks the update up
  at its next start (crash, logout, config-change restart) — there is deliberately no
  mid-session apply to close that gap.
