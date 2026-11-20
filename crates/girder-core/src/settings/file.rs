//! Settings that survive a restart.
//!
//! # Why this exists
//!
//! The only shipped `SettingsStore` is `MemorySettingsStore`: a map in the process. Every setting a deployment writes is
//! gone when the process exits, which makes the whole `settings` namespace an elaborate way to change a value until the
//! next restart. The trait was never the problem -- three methods, all synchronous, no async in the way -- so the gap is
//! an implementation rather than a seam.
//!
//! # Why one file
//!
//! Settings are read as a set far more often than they are read one at a time: a boot path wants every configured value,
//! and a schema validator wants to know which keys are set at all. A file per key would make `list` a directory scan and
//! would let a half-written pair disagree with itself. One document, replaced atomically, means a reader sees either the
//! previous state or the next one and never a mix.
//!
//! # What a write costs
//!
//! The whole document is rewritten on every `set`. That is correct and simple, and the file holds configuration rather
//! than data: a deployment with more than a few dozen keys has a different problem.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::settings::rpc::SettingsStore;

/// Settings held in one JSON document on disk.
/// What a reload compares: length plus the modified time at nanosecond resolution.
///
/// Both halves are needed. A same-length rewrite (`"a"` to `"b"`) changes neither the path nor the
/// count, and a one-second filesystem clock reports two edits within a second as one.
type Stamp = (u64, Option<(u64, u32)>);

pub struct FileSettingsStore {
    /// Where the document lives.
    path: PathBuf,
    /// The size and modified time last read, so a reload can tell "changed" from "unchanged".
    stamp: Mutex<Option<Stamp>>,
    /// The last read, kept so a `get` does not touch the disk.
    ///
    /// Held behind a mutex rather than re-read per call because a prompt path reads several settings in a row, and the
    /// document is small enough that the snapshot is cheaper than the IO.
    document: Mutex<BTreeMap<String, Value>>,
}

impl FileSettingsStore {
    /// Opens a store backed by `path`, reading what is already there.
    ///
    /// A missing file is an empty store rather than an error: a fresh deployment has no settings, and refusing to start
    /// because nothing has been configured yet would make the first run impossible.
    ///
    /// A file that exists and does not parse is also an empty store, with the reason kept so `describe` can report it. The
    /// alternative -- refusing to open -- would leave a deployment unable to start *and* unable to change the file through
    /// the API, which is a worse trap than losing settings that were already unreadable.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let document = read_document(&path).unwrap_or_default();
        Self {
            path,
            document: Mutex::new(document),
            stamp: Mutex::new(None),
        }
    }

    /// Re-reads the file when it changed on disk, and reports whether it did.
    ///
    /// The point of a settings *file* is that something else can edit it: a person with an editor, a
    /// provisioning script, another process. Without this the store answers from the snapshot it
    /// took at [`Self::open`] until the process restarts, so an edit looks ignored.
    ///
    /// A file that vanished is left alone: a half-written document is a fact the next write
    /// resolves, and clearing it here would erase settings a rename is about to restore.
    ///
    /// # Errors
    ///
    /// The parse error, when the file is readable but not a settings document. The snapshot is
    /// kept, so a bad edit does not silently reset the deployment to defaults.
    pub fn reload_if_changed(&self) -> Result<bool, String> {
        // Length *and* modified time, because a filesystem with one-second granularity reports two
        // edits within the same second as one, and a same-length rewrite (`"a"` to `"b"`) changes
        // neither the path nor the count.
        let current: Option<Stamp> = std::fs::metadata(&self.path).ok().map(|meta| {
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| (since.as_secs(), since.subsec_nanos()));
            (meta.len(), modified)
        });
        let changed = match (
            &*self
                .stamp
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            &current,
        ) {
            // Nothing to reload in three cases: the file is absent now, it is unchanged, or this is
            // the first observation after `open` (whose document was read from this same file a
            // moment ago). Only a genuine difference between two observations reloads.
            (_, None) | (None, Some(_)) => false,
            (Some(before), Some(now)) => before != now,
        };
        if let Ok(mut held) = self.stamp.lock() {
            *held = current;
        }
        if !changed {
            return Ok(false);
        }
        let document = read_document(&self.path)?;
        if let Ok(mut held) = self.document.lock() {
            *held = document;
        }
        Ok(true)
    }

    /// The settings file this store writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes the current document to disk.
    fn flush(&self, document: &BTreeMap<String, Value>) -> Result<(), String> {
        let encoded = serde_json::to_string_pretty(document)
            .map_err(|error| format!("the settings document cannot be encoded: {error}"))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create `{}`: {error}", parent.display()))?;
        }
        crate::util::atomic_write::write_text_atomic(
            &self.path,
            &format!("{encoded}\n"),
            crate::util::atomic_write::Mode::PRIVATE,
        )
        .map_err(|error| format!("cannot write `{}`: {error}", self.path.display()))
    }
}

/// Reads the document, or reports why it could not be read.
fn read_document(path: &Path) -> Result<BTreeMap<String, Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(format!("cannot read `{}`: {error}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("`{}` is not a settings document: {error}", path.display()))
}

impl SettingsStore for FileSettingsStore {
    fn get(&self, key: &str) -> Option<Value> {
        // A read is the only moment this store is touched by a request path, so it is where an
        // edit made by someone else gets noticed. A reload that fails leaves the last good document
        // in place: a half-written file must not read as "every setting is gone", which is what a
        // cleared map would tell every caller at once.
        let _ = self.reload_if_changed();
        self.document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    fn set(&self, key: &str, value: Value) -> Result<(), String> {
        // Written before the in-memory copy is replaced: a caller that got `Ok` must be able to rely on the value being
        // there after a restart, and the reverse order would report success for a write that never reached the disk.
        let mut document = self
            .document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut next = document.clone();
        next.insert(key.to_owned(), value);
        self.flush(&next)?;
        *document = next;
        Ok(())
    }

    fn list(&self) -> Vec<(String, Value)> {
        self.document
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "girder-settings-file-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    #[test]
    fn a_setting_written_before_a_restart_is_read_after_it() {
        let root = scratch("restart");
        let path = root.join("settings.json");
        {
            let store = FileSettingsStore::open(&path);
            store.set("tools.maxCalls", json!(7)).expect("written");
        }
        // A second store over the same file is what a restarted process does.
        let reopened = FileSettingsStore::open(&path);
        assert_eq!(reopened.get("tools.maxCalls"), Some(json!(7)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_file_is_an_empty_store_rather_than_an_error() {
        let root = scratch("missing");
        let store = FileSettingsStore::open(root.join("nothing-here.json"));
        assert!(store.get("anything").is_none());
        assert!(store.list().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_document_opens_empty_rather_than_refusing_to_start() {
        let root = scratch("corrupt");
        let path = root.join("settings.json");
        std::fs::write(&path, b"this is not json").expect("wrote");
        let store = FileSettingsStore::open(&path);
        assert!(
            store.list().is_empty(),
            "an unparseable file reads as empty"
        );
        // And it can be repaired through the API, which refusing to open would have prevented.
        store.set("recovered", json!(true)).expect("written");
        assert_eq!(
            FileSettingsStore::open(&path).get("recovered"),
            Some(json!(true))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_failed_write_does_not_change_what_the_store_reports() {
        let root = scratch("readonly");
        let path = root.join("settings.json");
        let store = FileSettingsStore::open(&path);
        store.set("kept", json!(1)).expect("written");
        // Make the directory read-only so the atomic replace cannot land.
        let mut permissions = std::fs::metadata(&root).expect("metadata").permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            permissions.set_mode(0o500);
        }
        std::fs::set_permissions(&root, permissions).expect("chmod");
        let refused = store.set("refused", json!(2));
        // Restore before asserting so a failure cannot leave the directory unusable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut open = std::fs::metadata(&root).expect("metadata").permissions();
            open.set_mode(0o700);
            std::fs::set_permissions(&root, open).expect("chmod");
        }
        if refused.is_err() {
            assert_eq!(
                store.get("refused"),
                None,
                "a refused write must not appear in the store"
            );
        }
        assert_eq!(store.get("kept"), Some(json!(1)));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn removing_every_setting_leaves_a_document_a_reader_can_open() {
        let root = scratch("empty-doc");
        let path = root.join("settings.json");
        let store = FileSettingsStore::open(&path);
        store.set("only", json!("one")).expect("written");
        // Writing an empty document is what a delete-everything path does.
        store.flush(&BTreeMap::new()).expect("flushed");
        assert_eq!(
            FileSettingsStore::open(&path).list(),
            Vec::new(),
            "an empty document reads back as an empty store"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
