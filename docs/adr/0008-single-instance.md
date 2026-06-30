# ADR 0008 — Single-instance guard: advisory `flock`

- **Status:** Accepted (issue #55)
- **Supersedes / superseded by:** none
- **Relates to:** ADR 0005 (config + pid file), ADR 0006/0007 (settings app + tray), issue #55

## Context

Nothing stopped two recorders (or two settings windows) from running at once. A second recorder
means two ScreenCast sessions and two GlobalShortcuts bindings fighting over the same trigger, plus
wasted GPU; it is easy to hit with a leftover daemon from a previous session, or by opening the
settings from both `cargo` and the tray. The recorder already writes a pid file
(`$XDG_RUNTIME_DIR/rewynd/recorder.pid`) for the settings app's restart path.

## Decision

Guard both binaries with a **non-blocking exclusive advisory `flock`**, in `rewynd-config`:

- **Recorder:** lock the existing pid file at startup (before claiming the portals) and write the
  pid into it under the lock. If the lock is already held, log and exit cleanly.
- **Settings:** lock a separate `settings.lock`; a second window logs and exits.

The lock is taken via `libc::flock(LOCK_EX | LOCK_NB)` and held by keeping the `File` open for the
process's lifetime (an `InstanceLock` guard). The pid file is **not** removed on exit.

## Options evaluated

| Option | Verdict |
| --- | --- |
| **advisory `flock`** | **chosen** — kernel releases it on process death (including crash/SIGKILL), so no stale-lock recovery; the existing pid file doubles as the recorder's lock target |
| pid-file existence check | rejected — racy (TOCTOU between two launches) and needs stale-pid recovery after a crash |
| `std::fs::File::try_lock` | rejected — stabilised in Rust 1.89, past our 1.85 MSRV |
| abstract-socket / D-Bus name | rejected — heavier than the problem; we already have the pid file |

## Rationale

- **Self-cleaning:** the lock lives on the open file description, so process death frees it. The
  restart path (settings SIGTERMs the recorder, waits for `/proc/<pid>` to clear — escalating to
  SIGKILL if it outlives the wait — then relaunches) re-acquires it cleanly. The recorder retries
  the `flock` on `EINTR` so a stray signal can't be mistaken for a free lock.
- **No unlink race:** the pid file is left in place on exit. Unlinking it could let an incoming
  instance create and lock a fresh inode at the same path while the outgoing fd still holds a lock
  on the old one. A leftover pid is harmless — the settings app verifies it against `/proc`.
- **Minimal dep:** one `libc` FFI call (unix-only); no second event loop or IPC.

## Consequences

- New dep: `libc` (MIT/Apache), unix-only.
- **Linux/unix only.** Off unix the API stays total via a no-op `InstanceLock` stub (always
  "acquires"), so callers need no `#[cfg]`; a Windows build gets no real guard yet and would use a
  named mutex behind the same shape when Windows reaches parity (issue #15).
- The tray's "Open settings", when a window is already open, currently results in the second
  instance silently exiting rather than raising the existing window — a possible later refinement.
