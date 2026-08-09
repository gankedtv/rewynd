# ADR 0020 — Boot-time update check: one boot to the newest release

- **Status:** Accepted
- **Supersedes / superseded by:** none (amends ADR 0018)
- **Relates to:** ADR 0008 (single-instance guard), ADR 0018 (automatic updates)

## Context

ADR 0018 split updating in two: a detached thread that only **downloads** (2 minutes after start,
then daily) and an apply that only happens at a **recorder start**. Correct, but it costs two
boots. A user who boots the machine the day a release ships gets:

- boot 1 — nothing is pending, so the recorder starts on the old version; two minutes later the
  background check downloads the new package and leaves it pending;
- boot 2 — the pending package is applied and the recorder finally runs the new version.

Between those two boots the recorder keeps running the old build, which is exactly the state the
user notices ("a newer version has been out for days and I have rebooted since"). Machines that
are shut down between sessions never close the gap any faster, and each boot only ever installs
what the *previous* boot downloaded.

Shortening it by applying mid-session is not an option: ADR 0018's central constraint is that
Velopack's apply force-kills every process in the install dir, and a recorder holding a live
capture pipeline (or a settings window mid-edit) must not go that way. The only safe place to
apply is still the same one — start, after the single-instance lock, before capture exists.

## Decision

**At start, the recorder does a bounded check-and-download before it goes on to build the capture
pipeline, and applies whatever that produces at the same safe point.**

- The gates from ADR 0018 come first and are unchanged: `[updates] auto_install` on, and a
  Velopack receipt present. No receipt ⇒ inert (dev runs, AUR).
- A package already pending from an earlier session is applied exactly as before, and boot ends
  there — the process restarts into the new version with `--recorder`.
- With nothing pending, a detached thread runs check + download while the main thread waits — in
  two phases, because the two steps fail differently. The feed answer gets **10 s**
  (`BOOT_CHECK_WAIT`), so a blackholed request costs ten seconds, not ninety; only once the thread
  reports an update available does the budget stretch to the full **90 s** (`BOOT_UPDATE_WAIT`,
  measured from the start of the wait). A download that finishes inside it is applied on the spot,
  so a release that shipped since the last session is running after **one** boot.
- On timeout, no update, or a failed check the boot simply carries on. The thread is not
  cancelled: a slow download still lands and installs at the next start, which is precisely
  ADR 0018's behaviour. An offline boot costs no wait worth measuring — the feed request fails
  fast, and the failure stays at debug level because offline boots are routine.
- The fresh check is skipped entirely on a `--restart` relaunch (the tray's microphone toggle, a
  hotkey rebind). Those are mid-session restarts wearing a start's clothes, and mid-session must
  never wait on the network; the daily check covers them. A package left pending by an earlier
  session is still applied — that is the pre-existing behaviour, and it is instant.
- The wait is skipped entirely when a settings window is open, and abandoned if one opens while it
  runs (the main thread rechecks every second). The apply would be deferred anyway (never yank a
  live peer), so there is nothing to wait for.
- The daily cadence is untouched: the same check + download thread from ADR 0018 still runs
  2 minutes after start and every 24 h, and covers machines that stay up for days.
- Overlapping the boot check with the daily one is safe: Velopack's `download_updates` takes an
  exclusive lock on the packages dir and returns early when the target package already exists.

## Consequences

- A release installs at the first boot after it ships instead of the second — worst case, one
  90 s-capped delay on a boot that had an update waiting to be fetched.
- Boot can now take up to 90 s longer before the recorder publishes its `Starting` status. Only
  Velopack installs with the setting on and a download actually running pay that; a feed that never
  answers costs 10 s, a `--restart` relaunch nothing, and a settings window open (the onboarding
  wizard included) skips the wait entirely — but the settings app's own start-the-recorder
  handshake has a 60 s confirm timeout, so any future flow that waits on a recorder start without
  holding the settings lock has to account for this cap.
- Nothing about mid-session behaviour changes: the recorder still never dies under a live capture,
  and the settings window keeps sole ownership of "update right now".
- One gap stays open: a settings app that could not take its lock opens anyway (lock-less degraded
  mode), and `settings_running` cannot see it. Its wizard can therefore still meet a boot download,
  which is why the 90 s cap matters — it bounds the collision instead of removing it.
- The failure mode is the old one. Every path that does not complete inside the window degrades to
  ADR 0018's "installs at the next start", so a flaky or slow network costs freshness, never a
  boot.
