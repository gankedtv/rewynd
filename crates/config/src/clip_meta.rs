//! What the user has told us about a clip: the name they gave it and whether they starred it.
//! A small JSON file beside `config.toml` (see docs/adr/0024), written 0600 like the upload
//! history, because a clip name can quote whatever was said in the recording.
//!
//! Keyed by file name alone, not by size or mtime: a clip's name has to survive moving between
//! game folders and a trim that rewrites the file in place. File names carry a millisecond
//! stamp plus a per-process sequence, so they are unique within the store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How long a clip name may be. Long enough for a sentence, short enough that a card's title
/// line stays one line.
pub const MAX_NAME_CHARS: usize = 80;

/// What we remember about one clip. An entry equal to this default is dropped from the store
/// rather than written out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub favourite: bool,
}

impl ClipMeta {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One change to a clip's entry. Carried as data rather than a closure so the same change can
/// be shown on screen now and applied to the stored entry later, under the store's lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipEdit {
    /// Name it, or (with `None`) take its name away.
    Name(Option<String>),
    Favourite(bool),
    /// Forget the clip: what a delete leaves behind, and what a trimmed copy starts from.
    Reset,
}

impl ClipEdit {
    fn apply_to(&self, meta: &mut ClipMeta) {
        match self {
            Self::Name(name) => meta.name.clone_from(name),
            Self::Favourite(on) => meta.favourite = *on,
            Self::Reset => *meta = ClipMeta::default(),
        }
    }
}

/// Every clip we know something about, keyed by file name. A `BTreeMap` so the file stays in a
/// stable order instead of reshuffling on every write.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClipMetaStore(BTreeMap<String, ClipMeta>);

impl ClipMetaStore {
    /// What we know about the clip at `path`, if anything.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<&ClipMeta> {
        self.0.get(file_name_of(path)?)
    }

    /// The name the user gave this clip, if they gave it one.
    #[must_use]
    pub fn name_of(&self, path: &Path) -> Option<&str> {
        self.get(path)?.name.as_deref()
    }

    /// Whether this clip is starred.
    #[must_use]
    pub fn is_favourite(&self, path: &Path) -> bool {
        self.get(path).is_some_and(|m| m.favourite)
    }

    /// Apply `edit` to this clip's entry, creating it if needed. An entry left at its default
    /// (no name, not starred) is removed, so clearing a name does not leave a husk behind.
    pub fn set(&mut self, file_name: &str, edit: impl FnOnce(&mut ClipMeta)) {
        let mut meta = self.0.get(file_name).cloned().unwrap_or_default();
        edit(&mut meta);
        if meta.is_empty() {
            self.0.remove(file_name);
        } else {
            self.0.insert(file_name.to_owned(), meta);
        }
    }

    /// Apply `edits` in order to this clip's entry.
    pub fn apply(&mut self, file_name: &str, edits: &[ClipEdit]) {
        self.set(file_name, |meta| {
            for edit in edits {
                edit.apply_to(meta);
            }
        });
    }

    /// Whether the store holds nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The store's key for a clip path: its file name.
#[must_use]
pub fn file_name_of(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

/// Characters that are invisible or reorder what follows them. A clip name is drawn as a card
/// title and a heading, so one of these could hide text or flip it right to left.
fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00ad}'                  // soft hyphen
            | '\u{061c}'                // Arabic letter mark
            | '\u{200b}'..='\u{200f}'   // zero width spaces, joiners, LTR/RTL marks
            | '\u{202a}'..='\u{202e}'   // bidi embedding and overrides
            | '\u{2060}'..='\u{2064}'   // word joiner and invisible operators
            | '\u{2066}'..='\u{2069}'   // bidi isolates
            | '\u{feff}'                // byte order mark
        )
}

/// A clip name as it will be stored: whitespace collapsed, invisible characters dropped, capped
/// at [`MAX_NAME_CHARS`]. `None` when nothing usable is left, which means "no name".
#[must_use]
pub fn clean_name(raw: &str) -> Option<String> {
    let mapped: String = raw
        .chars()
        .map(|c| if is_invisible(c) { ' ' } else { c })
        .collect();
    let mut name = String::with_capacity(mapped.len());
    for word in mapped.split_whitespace() {
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(word);
    }
    if name.chars().count() > MAX_NAME_CHARS {
        name = name.chars().take(MAX_NAME_CHARS).collect();
        name = name.trim_end().to_owned();
    }
    (!name.is_empty()).then_some(name)
}

/// The name a trimmed copy inherits from its source. Re-cleaned, so a name already at the cap
/// makes room for the suffix instead of overflowing it.
#[must_use]
pub fn trimmed_copy_name(name: &str) -> Option<String> {
    const SUFFIX: &str = " (trimmed)";
    let room = MAX_NAME_CHARS.saturating_sub(SUFFIX.chars().count());
    let stem: String = clean_name(name)?.chars().take(room).collect();
    clean_name(&format!("{}{SUFFIX}", stem.trim_end()))
}

/// The store's path, beside `config.toml`.
#[must_use]
pub fn store_path() -> Option<PathBuf> {
    Some(crate::config_path()?.with_file_name("clip-meta.json"))
}

/// Everything we know about every clip (empty when the file is missing or unreadable).
#[must_use]
pub fn load() -> ClipMetaStore {
    store_path().map(|p| load_at(&p)).unwrap_or_default()
}

/// Apply `edits` to one clip's entry and write the store back. The read-modify-write runs under
/// an exclusive file lock, and each edit touches only its own field of whatever the store holds
/// by then, so a second writer's rename and this one's star don't overwrite each other. An edit
/// that leaves the entry at its default removes it, which is how a deleted clip is forgotten.
pub fn update(file_name: &str, edits: &[ClipEdit]) -> std::io::Result<()> {
    let path = store_path().ok_or_else(no_path)?;
    with_store_lock(&path, || {
        let mut store = load_at(&path);
        store.apply(file_name, edits);
        save_at(&path, &store)
    })
}

fn with_store_lock<T>(
    path: &Path,
    body: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    crate::lock::with_exclusive_lock(&path.with_extension("json.lock"), body)
}

fn no_path() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no config directory to store clip names",
    )
}

fn load_at(path: &Path) -> ClipMetaStore {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_at(path: &Path, store: &ClipMetaStore) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(store)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::paths::write_private_atomic(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(store: &mut ClipMetaStore, file: &str, name: &str) {
        store.set(file, |m| m.name = clean_name(name));
    }

    #[test]
    fn set_stores_and_compacts_entries() {
        let mut store = ClipMetaStore::default();
        named(&mut store, "rewynd-1-0.mp4", "Clutch ace");
        store.set("rewynd-1-0.mp4", |m| m.favourite = true);
        let clip = Path::new("/clips/Elden Ring/rewynd-1-0.mp4");
        assert_eq!(store.name_of(clip), Some("Clutch ace"));
        assert!(
            store.is_favourite(clip),
            "the folder is not part of the key"
        );

        // Clearing both fields drops the entry rather than leaving an empty husk.
        store.set("rewynd-1-0.mp4", |m| m.name = None);
        assert!(!store.is_empty(), "still starred");
        store.set("rewynd-1-0.mp4", |m| m.favourite = false);
        assert!(store.is_empty());
        assert_eq!(store.name_of(clip), None);
        assert!(!store.is_favourite(clip));
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clip-meta.json");
        assert!(load_at(&path).is_empty(), "missing file loads empty");

        let mut store = ClipMetaStore::default();
        named(&mut store, "rewynd-2-0.mp4", "Triple kill");
        store.set("rewynd-3-0.mp4", |m| m.favourite = true);
        save_at(&path, &store).expect("save");
        assert_eq!(load_at(&path), store);

        // Only the fields that carry information are written.
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("Triple kill"), "{text}");
        assert!(!text.contains("\"favourite\": false"), "{text}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "clip names are owner-only");
        }
    }

    #[test]
    fn garbage_loads_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, b"not json").expect("write");
        assert!(load_at(&bad).is_empty());
    }

    #[test]
    fn with_store_lock_runs_the_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clip-meta.json");
        let out = with_store_lock(&path, || {
            let mut store = ClipMetaStore::default();
            named(&mut store, "rewynd-1-0.mp4", "Locked");
            save_at(&path, &store)?;
            Ok::<_, std::io::Error>(7)
        })
        .expect("locked body");
        assert_eq!(out, 7);
        assert_eq!(
            load_at(&path).name_of(Path::new("rewynd-1-0.mp4")),
            Some("Locked")
        );
        #[cfg(unix)]
        assert!(path.with_extension("json.lock").exists());
    }

    #[test]
    fn clearing_an_entry_through_update_forgets_the_clip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clip-meta.json");
        let mut store = ClipMetaStore::default();
        named(&mut store, "rewynd-1-0.mp4", "Gone soon");
        named(&mut store, "rewynd-2-0.mp4", "Stays");
        save_at(&path, &store).expect("save");

        // What the delete handler does: clear the entry.
        let mut store = load_at(&path);
        store.apply("rewynd-1-0.mp4", &[ClipEdit::Reset]);
        save_at(&path, &store).expect("save");

        let after = load_at(&path);
        assert_eq!(after.name_of(Path::new("rewynd-1-0.mp4")), None);
        assert_eq!(after.name_of(Path::new("rewynd-2-0.mp4")), Some("Stays"));
    }

    #[test]
    fn clean_name_drops_invisible_and_direction_flipping_characters() {
        // A name is drawn as a heading, so nothing in it may hide text or reverse it.
        let sneaky = "Ace\u{202e}gpj.exe\u{200b} \u{061c}\u{feff}clip";
        let cleaned = clean_name(sneaky).expect("name");
        assert_eq!(cleaned, "Ace gpj.exe clip");
        assert!(!cleaned.chars().any(is_invisible), "{cleaned:?}");
        assert_eq!(clean_name("\u{200b}\u{202e}\u{feff}"), None);
    }

    #[test]
    fn an_edit_leaves_every_other_field_alone() {
        let mut store = ClipMetaStore::default();
        store.apply(
            "rewynd-1-0.mp4",
            &[ClipEdit::Name(Some("Clutch ace".to_owned()))],
        );
        store.apply("rewynd-1-0.mp4", &[ClipEdit::Favourite(true)]);
        let clip = Path::new("rewynd-1-0.mp4");
        assert_eq!(store.name_of(clip), Some("Clutch ace"));
        assert!(store.is_favourite(clip));

        // Starring a clip someone else renamed in the meantime keeps their name: the edit is
        // applied to the entry as it stands, not to a snapshot taken before the lock.
        store.apply(
            "rewynd-1-0.mp4",
            &[ClipEdit::Name(Some("Their name".to_owned()))],
        );
        store.apply("rewynd-1-0.mp4", &[ClipEdit::Favourite(false)]);
        assert_eq!(store.name_of(clip), Some("Their name"));
        assert!(!store.is_favourite(clip));

        // Reset drops the whole entry, whatever is in it.
        store.apply("rewynd-1-0.mp4", &[ClipEdit::Reset]);
        assert!(store.is_empty());

        // Several edits in one go run in order.
        store.apply(
            "rewynd-2-0.mp4",
            &[
                ClipEdit::Favourite(true),
                ClipEdit::Name(Some("Kept".to_owned())),
                ClipEdit::Favourite(false),
            ],
        );
        assert_eq!(store.name_of(Path::new("rewynd-2-0.mp4")), Some("Kept"));
        assert!(!store.is_favourite(Path::new("rewynd-2-0.mp4")));
    }

    #[test]
    fn clean_name_tidies_and_caps() {
        assert_eq!(
            clean_name("  Clutch   ace  "),
            Some("Clutch ace".to_owned())
        );
        assert_eq!(
            clean_name("line\nbreak\tand\u{0}nul"),
            Some("line break and nul".to_owned())
        );
        assert_eq!(clean_name("   "), None);
        assert_eq!(clean_name(""), None);

        // The cap counts characters, not bytes, so a multibyte name is never cut mid-character.
        let long = "é".repeat(MAX_NAME_CHARS + 20);
        let capped = clean_name(&long).expect("name");
        assert_eq!(capped.chars().count(), MAX_NAME_CHARS);
        // A name capped mid-word does not keep a dangling space.
        let words = format!("{} tail", "x".repeat(MAX_NAME_CHARS - 1));
        assert_eq!(
            clean_name(&words).expect("name"),
            "x".repeat(MAX_NAME_CHARS - 1)
        );
    }

    #[test]
    fn trimmed_copy_name_fits_under_the_cap() {
        assert_eq!(
            trimmed_copy_name("Clutch ace"),
            Some("Clutch ace (trimmed)".to_owned())
        );
        let long = trimmed_copy_name(&"x".repeat(MAX_NAME_CHARS)).expect("name");
        assert!(long.chars().count() <= MAX_NAME_CHARS, "{long}");
        assert!(long.ends_with(" (trimmed)"), "{long}");
        assert_eq!(trimmed_copy_name("   "), None);
    }

    #[test]
    fn file_name_of_is_the_key() {
        assert_eq!(
            file_name_of(Path::new("/clips/Elden Ring/rewynd-5-0.mp4")),
            Some("rewynd-5-0.mp4")
        );
        assert_eq!(file_name_of(Path::new("/")), None);
    }
}
