# 0024 — Clip names and favourites: a metadata store, not a file rename

## Status

Accepted.

## Context

The library shows every clip by its save time, which makes one clip look like the next. People
want to name the ones worth keeping and to star their best, then find those again.

ADR 0013 made the filesystem the model: a clip is a `rewynd-<millis>-<seq>.mp4` under the
output directory, and everything the library shows is derived from the directory listing. The
only per-clip state that survives a restart today is the upload history, a small JSON file
beside `config.toml`, keyed by file name plus size plus mtime so a record dies when the file is
rewritten.

## Decision

- **A name is metadata; the file on disk keeps its generated name.** Renaming the file would
  break four things at once: the `rewynd-*.mp4` filter that makes a file a clip at all, the
  millisecond stamp that dates it, the `rewynd://clip/<name>` deep link a save toast opens, and
  the upload history's identity. Windows also holds the file open while the player has it. The
  library shows the name, "Show in folder" still leads to the real file.

- **A second store beside `config.toml`: `clip-meta.json`.** A map of file name to
  `{ name, favourite }`, `BTreeMap`-ordered so the file is stable, entries at their default
  dropped rather than written. Same handling as the upload history: 0600, atomic temp plus
  rename, read-modify-write under an exclusive lock, unreadable or corrupt means empty. The
  private writer both stores use now lives in `paths.rs`.

- **Keyed by file name alone, unlike the upload history.** A name has to survive a clip moving
  between game folders and a trim that rewrites the file in place; mtime in the key would throw
  it away on exactly the edit a user expects to keep it. Recorded clips carry a millisecond
  stamp plus a per-process sequence, so they do not collide. Trimmed copies are the exception:
  they are named after their source, so a copy deleted and made again takes the same name back.
  Every write for a copy therefore replaces the whole entry rather than one field, so a stale
  name or star can never attach itself to a new file.

- **No sidecar file per clip.** Clips live in the user's Videos folder, which is theirs; a
  second file next to every recording is litter, and a copy of a clip elsewhere would leave its
  sidecar behind.

- **Edits apply in memory first, then write.** The grid updates on the keystroke; the write runs
  on a blocking task. A rescan landing mid-write keeps the in-memory value for clips whose write
  is still owed, so a directory watch cannot show someone their own rename undone. One write per
  clip is in flight at a time: a further edit marks the clip, and its write goes out when the
  running one reports back, carrying whatever the value is by then. Deleting a clip clears its
  entry through that same queue, so an edit still in flight cannot bring it back.

- **No manual ordering.** The grid is newest-first and grouped by game, both derived. A
  hand-placed order has nowhere to live in that model, and iced has no drag-and-drop to build it
  on. Favourites plus search answer "show me the good ones" without inventing a third ordering.

## Consequences

- A clip deleted outside the app leaves its entry behind. Harmless: entries are looked up by
  the clips actually on disk, so a stale one is never shown, and the only file name that can
  come back is a trimmed copy's, which is written over in full. In-app deletes clear it.
- The upload title now defaults to the clip's name when it has one, which is what a person
  would have typed anyway.
- A trimmed copy inherits "`<name>` (trimmed)", written before the rescan sees the new file, so
  the copy never flashes up as a bare date.
