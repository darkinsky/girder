//! The loader's decision core: entry trees, patch overlays, and the three-way journal reconciliation
//! that upstream keeps in `vendor/loader` and `vendor/include`.
//!
//! # What upstream contains, measured
//!
//! `vendor/loader` (Cordis' plugin loader) and `vendor/include` (its file-backed entry tree) are
//! what `packages/boot` mounts plugins through: `app-boot` composes a profile, `Loader` turns the
//! composed entry list into running plugins, and `Include` keeps one entry list in a file. Measured
//! with comments and blank lines removed, file by file:
//!
//! | File | Lines | Effective | What it decides | Needs |
//! | --- | --- | --- | --- | --- |
//! | `loader/src/config/entry.ts` | 239 | 187 | option merge (a `null` deletes), key order, ancestor-aware `disabled` | Cordis fibers |
//! | `loader/src/config/tree.ts` | 205 | 145 | nested-id resolution, id assignment, change reporting | Cordis fibers |
//! | `loader/src/config/isolate.ts` | 176 | 139 | per-entry and per-label service realms | Cordis context |
//! | `loader/src/index.ts` | 219 | 131 | the `Loader` service, export unwrapping | Cordis |
//! | `loader/src/config/group.ts` | 89 | 69 | group children reconciliation by id | Cordis |
//! | `loader/src/internal.ts` | 144 | 87 | Node v1/v2 internal loader detection | `node:module` |
//! | `loader/src/resolve.ts` | 75 | 52 | writes `.cordis/resolve.mjs`, imports it | filesystem |
//! | `loader/src/config/diff.ts` | 45 | 31 | volatile-aware raw config equality | schemastery |
//! | `loader/src/config/utils.ts` | 32 | 23 | `!!js` evaluation | `new Function` |
//! | `include/src/index.ts` | 465 | 366 | read / parse / validate, atomic write, repair merge | filesystem, YAML |
//! | `include/src/patch.ts` | 261 | 206 | `applyPatches`, `PatchIndex`, `routeJournal` | — |
//! | `include/src/journal.ts` | 242 | 192 | journal, `applyJournal`, `reconcile` | — |
//!
//! 2,192 lines, 1,628 of them without comments. **None of it sorts plugins by dependency.** `inject`
//! is resolved by Cordis' fiber state machine, which parks a plugin as *pending* until its services
//! exist; `EntryGroup.update` reconciles children by id in one `Promise.all`, and `EntryTree.entries()`
//! walks a store. The "load in dependency order" shape one expects is not there, and inventing one
//! would be a different loader — so what is ported is the *id-keyed reconciliation* that is there.
//!
//! The split that matters is not line count but **what the code touches**, because that decides what
//! a Rust build can accept unchanged:
//!
//! | Capability | Files | Effective | Portable |
//! | --- | --- | --- | --- |
//! | Entry-list overlays (`applyPatches`, `PatchIndex`, `routeJournal`, `ensureIds`) | `include/src/patch.ts` | 206 | **yes** — pure list algebra |
//! | Change journal and three-way merge (`diffOptions`, `mergeRecords`, `record`, `applyJournal`, `reconcile`) | `include/src/journal.ts` | 192 | **yes** — pure map algebra |
//! | Flatten / place / detach over nested groups | both | 98 | **yes** |
//! | File lifecycle decisions (read stage, initial fallback, write plan, stale-write, rename retry) | `include/src/index.ts` | 55 | **yes**, once the file is factored out |
//! | Anonymous-id assignment by shape | `include/src/index.ts` | 48 | **yes**, with a supplied id source |
//! | Nested-id resolution, id generation, `disabled` chain, key order, export unwrapping | `loader/src/{config/tree,config/entry,index}.ts` | 96 | **yes** |
//! | Node internal loader classification | `loader/src/internal.ts` | 87 | no — `node:module` internals |
//! | `!!js` evaluation | `loader/src/config/utils.ts` | 23 | no — it is an interpreter |
//! | Volatile config equality | `loader/src/config/diff.ts` | 31 | no — schemastery metadata |
//! | Service realm swapping | `loader/src/config/isolate.ts` | 139 | no — `Context.isolate` symbols |
//! | Filesystem reads/writes, YAML, `import()` | `resolve.ts`, the `index.ts` body | 300+ | no — I/O |
//!
//! **There is no topological sort in either package.** The loader does not order plugins by
//! dependency: `inject` is resolved by Cordis' fiber state machine, which parks a plugin as *pending*
//! until its services exist rather than sorting a list. `EntryGroup.update` reconciles children by id
//! in one `Promise.all`, and `EntryTree.entries()` walks a store. So the "load by dependency order"
//! shape one might expect is simply not there; what is there is *id-keyed reconciliation*, which is
//! what this module ports.
//!
//! # What this build already covers
//!
//! [`crate::settings::config`] composes a profile from layered patches: `EntryList::apply` overlays layers,
//! records [`Owner::File`]/[`Owner::Patch`]/[`Owner::Insert`] per row, and rejects `!!js` at parse
//! time. That covers the *compose-a-profile* half of `include/src/patch.ts` — the half `app-boot`
//! needs. What it does **not** have, by grep across the workspace:
//!
//! - **The change journal.** No `journal`, `mergeRecords`, `record`, `applyJournal`; the 88 local
//!   hits for `journal` are session logs and a WAL, unrelated.
//! - **Three-way reconciliation.** No `reconcile` over `(base, theirs, journal)`, and therefore no
//!   "the file wins" conflict rules — the 33 hits are other merge helpers.
//! - **Live-tree patches.** No `routeJournal`, no owner routing back into a patch document, no
//!   `PatchIndex`. `config.rs` computes owners once, while overlaying; it cannot route a later change
//!   *back* to the patch that owns the key.
//! - **Nested addressing.** No `resolve("group:child")`, no `place`/`detach`/`flatten` over group
//!   children (the 6 `fn flatten` hits are lineage and message flattening).
//! - **The `Entry` lifecycle rules a loader needs.** No key ordering, no ancestor-aware `disabled`,
//!   no export unwrapping, no stage-classified config-file errors, no stale-write or rename-retry
//!   policy.
//!
//! # What is implemented here
//!
//! | Upstream symbol | Here |
//! | --- | --- |
//! | `flatten`, `detach`, `place`, `applyChanges` (`journal.ts`) | [`flatten`], [`detach`], [`place`], [`apply_changes`] |
//! | `diffOptions`, `mergeRecords`, `merge`, `record` | [`diff_options`], [`merge_records`], [`journal_merge`], [`record`] |
//! | `applyJournal`, `reconcile`, `Conflict` | [`apply_journal`], [`reconcile`], [`Conflict`] |
//! | `applyPatches`, `ensureIds`, `ensureInsertIds` | [`apply_patches`], [`ensure_ids`], [`ensure_insert_ids`] |
//! | `PatchIndex`, `EntryOwner`, `fileOwned` | [`PatchIndex`], [`EntryOwner`], [`PatchIndex::file_owned`] |
//! | `routeJournal` | [`route_journal`] |
//! | `sortKeys`, `takeEntries` (`entry.ts`) | [`sort_keys`] (returns the order) |
//! | `Entry.disabled`, `_resolveDisabled` | [`is_disabled`] |
//! | `EntryTree.resolve`, `resolveGroup`, `ensureId` | [`resolve`], [`resolve_group`], [`ensure_id`] |
//! | `Loader.unwrapExports` | [`unwrap_exports`] |
//! | `_assignIds` (`include/src/index.ts`) | [`assign_ids`] |
//! | `types` / extension check, `ConfigFileError`, `_write`'s decisions, `_writeText`'s retry | [`content_type`], [`ConfigFileError`], [`plan_write`], [`retry_delay`] |
//!
//! # What is deliberately absent
//!
//! - **`!!js` evaluation.** [`is_js_expression`] recognises the node so a caller can refuse it;
//!   [`is_disabled`] returns an error rather than a guess when an ancestor's `disabled` is an
//!   expression. The crate's standing rule is that an unevaluable config fails loudly
//!   ([`crate::settings::config`] rejects the tag at parse time), and a loader that silently reads
//!   `{"__jsExpr": "..."}` as *false* would start a plugin the user believes is off.
//! - **Filesystem and YAML.** [`plan_write`] returns the decision and the caller performs it;
//!   parsing and dumping are `serde_norway`'s job, already used by [`crate::settings::config`].
//! - **Service realms.** `isolate.ts` swaps symbols inside a Cordis `Context`. There is no Cordis
//!   context here, and inventing a realm table without the context it isolates would be a table
//!   nothing reads.
//! - **Node internals.** `internal.ts` reachable only through `--expose-internals` or a native
//!   addon; not a shape this build has.
//!
//! # Why every entry point takes its facts instead of reading them
//!
//! [`plan_write`] takes the cached text, the text currently on disk, and whether the file is
//! writable, instead of reading all three. [`assign_ids`] takes an id source. [`reconcile`] takes
//! the two flattened snapshots. A journal merge that reads the disk answers differently on a
//! different machine and cannot be tested at all, and getting it wrong loses a user's edit.
//!
//! # Why the crate root is spelled `girder_core`
//!
//! This module is not declared in `lib.rs` yet — wiring is a separate change. The test compiles this
//! exact source by `#[path]`, and under that inclusion `crate::` names the test binary, so every path
//! out of the module would have to go through the crate root's re-exports. Nothing here needs one:
//! the module imports `std` and `serde_json`, both already dependencies of the crate.

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};

/// One loader entry's options, keyed by field name — upstream `EntryOptions`.
pub type Entry = Map<String, Value>;

/// A list of entries. Every element is a JSON object.
pub type EntryList = Vec<Value>;

/// A per-key selection predicate — upstream's `(id: string, key: string | null) => boolean`.
///
/// `key` is `None` when asking about the entry as a whole (creation, removal, relocation) rather
/// than one of its keys.
pub type EntryFilter<'a> = dyn Fn(&str, Option<&str>) -> bool + 'a;

/// A per-key selection predicate that sees only the key.
pub type KeyFilter<'a> = dyn FnMut(&str) -> bool + 'a;

/// A per-key delta: `None` deletes the key, `Some(value)` writes it.
///
/// Upstream spells these `Dict` with `undefined` standing for "delete". JSON has no `undefined`, so
/// the outer `Option` carries it.
pub type Changes = Vec<(String, Option<Value>)>;

/// The group separator in a nested entry id — upstream `EntryTree.sep`.
pub const SEP: char = ':';

// ============================================================================================
// Reading the tree: flatten, place, detach
// ============================================================================================

/// Whether a JSON value is truthy in the JavaScript sense upstream tests with `if (value)`.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
    }
}

/// Whether an entry is a group — upstream `entry.group` used as a condition.
fn is_group(entry: &Entry) -> bool {
    truthy(entry.get("group"))
}

/// A group's child list, when the entry is a group with one.
fn children(entry: &Entry) -> Option<&EntryList> {
    if !is_group(entry) {
        return None;
    }
    entry.get("config").and_then(Value::as_array)
}

/// One entry as the flat index sees it — upstream `FlatEntry`.
#[derive(Clone, Debug, PartialEq)]
pub struct FlatEntry {
    /// The group holding the entry, `None` for the root list.
    pub parent: Option<String>,
    /// The entry's index within that group's child list.
    pub position: usize,
    /// A copy of the entry's options.
    pub options: Entry,
    /// Indices of the group entries between the root list and the entry's own list.
    path: Vec<usize>,
}

impl FlatEntry {
    /// The indices walked from the root list to the list holding this entry.
    #[must_use]
    pub fn path(&self) -> &[usize] {
        &self.path
    }
}

/// Index every entry, nested group members included, by id — upstream `flatten`.
///
/// An entry with no id is not indexed (nothing can address it), but a *group* with no id still has
/// its children indexed with `parent: None`, exactly as upstream's `options.id ?? null` recursion
/// does.
#[must_use]
pub fn flatten(data: &[Value]) -> BTreeMap<String, FlatEntry> {
    let mut result = BTreeMap::new();
    flatten_into(data, None, &mut Vec::new(), &mut result);
    result
}

fn flatten_into(
    data: &[Value],
    parent: Option<&str>,
    ancestors: &mut Vec<usize>,
    out: &mut BTreeMap<String, FlatEntry>,
) {
    for (position, value) in data.iter().enumerate() {
        let Some(entry) = value.as_object() else {
            continue;
        };
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            out.insert(
                id.to_owned(),
                FlatEntry {
                    parent: parent.map(str::to_owned),
                    position,
                    options: entry.clone(),
                    path: ancestors.clone(),
                },
            );
        }
        if let Some(children) = children(entry) {
            ancestors.push(position);
            flatten_into(
                children,
                entry.get("id").and_then(Value::as_str),
                ancestors,
                out,
            );
            ancestors.pop();
        }
    }
}

/// The list at `path`, walking group children. With `create`, a missing `config` becomes `[]`, which
/// is what upstream's `group.options.config ??= []` does.
fn list_at_mut<'a>(
    data: &'a mut EntryList,
    path: &[usize],
    create: bool,
) -> Option<&'a mut EntryList> {
    let mut current = data;
    for &index in path {
        let entry = current.get_mut(index)?.as_object_mut()?;
        if !entry.contains_key("config") {
            if !create {
                return None;
            }
            entry.insert("config".to_owned(), Value::Array(Vec::new()));
        }
        current = entry.get_mut("config")?.as_array_mut()?;
    }
    Some(current)
}

/// The entry at `(path, position)`.
fn entry_at_mut<'a>(
    data: &'a mut EntryList,
    path: &[usize],
    position: usize,
) -> Option<&'a mut Entry> {
    list_at_mut(data, path, false)?
        .get_mut(position)?
        .as_object_mut()
}

/// Remove the entry with `id` from wherever it lives — upstream `detach`.
///
/// Returns the removed options, which a caller moving the entry needs; `None` when no entry has
/// that id.
pub fn detach(data: &mut EntryList, id: &str) -> Option<Entry> {
    let flat = flatten(data).remove(id)?;
    let list = list_at_mut(data, &flat.path, false)?;
    if flat.position >= list.len() {
        return None;
    }
    let removed = list.remove(flat.position);
    removed.as_object().cloned()
}

/// Insert `options` under `parent` (a group id, or `None` for the root) at `position`.
///
/// The position is clamped to the group's length, as upstream's `splice` does. Returns `false` when
/// the parent does not exist or is not a group — upstream's `place` returns `false` for the same two
/// cases, and `applyJournal` turns that into a warning rather than an error.
pub fn place(data: &mut EntryList, options: Entry, parent: Option<&str>, position: usize) -> bool {
    let list = match parent {
        None => data,
        Some(parent) => {
            let flat = flatten(data);
            let Some(group) = flat.get(parent) else {
                return false;
            };
            if !is_group(&group.options) {
                return false;
            }
            let mut path = group.path.clone();
            path.push(group.position);
            match list_at_mut(data, &path, true) {
                Some(list) => list,
                None => return false,
            }
        }
    };
    let at = position.min(list.len());
    list.insert(at, Value::Object(options));
    true
}

/// Apply a per-key delta in place — upstream `applyChanges`.
///
/// `filter` restricts which keys apply; upstream passes `key => filter(id, key)` where `key` is
/// `null` for the entry as a whole, which [`apply_journal`] covers.
pub fn apply_changes(entry: &mut Entry, changes: &Changes, mut filter: Option<&mut KeyFilter<'_>>) {
    for (key, value) in changes {
        if let Some(filter) = filter.as_mut()
            && !(filter)(key)
        {
            continue;
        }
        match value {
            None => {
                entry.remove(key);
            }
            Some(value) => {
                entry.insert(key.clone(), value.clone());
            }
        }
    }
}

// ============================================================================================
// The journal — upstream `include/src/journal.ts`
// ============================================================================================

/// Where a journalled entry now lives — upstream's `parent?: string | null`.
///
/// Three states, and the third is the point: `None` means the tree did not *relocate* the entry, so
/// the file's own position stands. Folding this into `Option<Option<String>>` would make "not moved"
/// and "moved to the root" the same value, and a root-level move would then be silently dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The tree did not relocate the entry; the file's position stands.
    Unchanged,
    /// The entry now lives in the root list.
    Root,
    /// The entry now lives in the named group, or in the root when the name is empty.
    Group(String),
}

impl Placement {
    /// The group name this placement names, `None` for the root — including the `Unchanged` case,
    /// which callers that only need a target treat as the root.
    #[must_use]
    pub fn group(&self) -> Option<&str> {
        match self {
            Self::Unchanged | Self::Root => None,
            Self::Group(name) => Some(name.as_str()),
        }
    }

    /// Whether the tree relocated the entry at all.
    #[must_use]
    pub fn is_moved(&self) -> bool {
        !matches!(self, Self::Unchanged)
    }
}

/// One tree-side mutation that has not reached the file yet, keyed by entry id — upstream
/// `JournalRecord`.
#[derive(Clone, Debug, PartialEq)]
pub enum JournalRecord {
    /// The entry was removed from its group.
    Remove,
    /// The entry was created or updated.
    Upsert {
        /// The entry did not exist when the tree first reported it.
        created: bool,
        /// Where the tree placed the entry.
        parent: Placement,
        /// Index within `parent`; only meaningful alongside a `parent`.
        position: usize,
        /// Per-key delta relative to the options the entry had when the change was reported;
        /// `None` deletes a key. Collected at report time, because the live tree is reconciled
        /// asynchronously and may lag behind the file.
        changes: Changes,
    },
}

/// Pending tree-side changes, in the order each id was first reported — upstream `Journal`
/// (`Map<string, JournalRecord>`, whose iteration order is insertion order).
pub type Journal = Vec<(String, JournalRecord)>;

/// The record for `id`, if any.
#[must_use]
pub fn journal_get<'a>(journal: &'a Journal, id: &str) -> Option<&'a JournalRecord> {
    journal
        .iter()
        .find(|(key, _)| key == id)
        .map(|(_, record)| record)
}

/// Fold a later record into an earlier one for the same entry — upstream `merge`.
///
/// The merged record keeps the *earlier* creation flag and the *later* relocation and values, so a
/// key written twice costs one write and an entry created and removed before being written leaves
/// no trace at all.
pub fn merge_records(
    older: Option<&JournalRecord>,
    newer: &JournalRecord,
) -> Option<JournalRecord> {
    let Some(older) = older else {
        return Some(newer.clone());
    };
    match newer {
        JournalRecord::Remove => match older {
            // An entry created and removed before ever being written leaves no trace.
            JournalRecord::Upsert { created: true, .. } => None,
            _ => Some(JournalRecord::Remove),
        },
        JournalRecord::Upsert {
            parent,
            position,
            changes,
            ..
        } => match older {
            JournalRecord::Remove => Some(JournalRecord::Upsert {
                created: false,
                parent: parent.clone(),
                position: *position,
                changes: changes.clone(),
            }),
            JournalRecord::Upsert {
                created,
                parent: older_parent,
                position: older_position,
                changes: older_changes,
            } => {
                let relocated = parent.is_moved();
                Some(JournalRecord::Upsert {
                    created: *created,
                    parent: if relocated {
                        parent.clone()
                    } else {
                        older_parent.clone()
                    },
                    position: if relocated {
                        *position
                    } else {
                        *older_position
                    },
                    changes: merge_changes(older_changes, changes),
                })
            }
        },
    }
}

/// Later values win per key, and keep their first position — `{ ...older, ...newer }`.
fn merge_changes(older: &Changes, newer: &Changes) -> Changes {
    let mut merged = older.clone();
    for (key, value) in newer {
        match merged.iter_mut().find(|(existing, _)| existing == key) {
            Some(slot) => slot.1.clone_from(value),
            None => merged.push((key.clone(), value.clone())),
        }
    }
    merged
}

/// Fold `record` into the journal — upstream `merge(journal, id, record)`.
///
/// A record that merges to nothing (a created-then-removed entry) is dropped entirely.
pub fn journal_merge(journal: &mut Journal, id: &str, record: &JournalRecord) {
    let existing = journal.iter().position(|(key, _)| key == id);
    let older = existing.map(|index| &journal[index].1);
    match merge_records(older, record) {
        Some(merged) => match existing {
            Some(index) => journal[index].1 = merged,
            None => journal.push((id.to_owned(), merged)),
        },
        None => {
            if let Some(index) = existing {
                journal.remove(index);
            }
        }
    }
}

/// A tree-side change as the persistence layer sees it — upstream `EntryChange` plus the arguments
/// of `record`.
#[derive(Clone, Debug)]
pub struct EntryChange {
    /// The entry's id, local to its tree.
    pub id: String,
    /// The options after the change; `None` when the entry was removed.
    pub options: Option<Entry>,
    /// The options before the change; `None` when the entry was created.
    pub legacy: Option<Entry>,
    /// The group the entry now lives in (`None` for the root).
    pub parent: Option<String>,
    /// Whether the entry moved between groups in this tree.
    pub moved: bool,
    /// The entry's index within its group.
    pub position: usize,
}

/// Record one tree-side change — upstream `record`.
///
/// A creation records the entry's whole options as its delta (its keys, minus `id`); an update
/// records only what changed; a removal records nothing but the removal.
pub fn record(journal: &mut Journal, change: &EntryChange) {
    let Some(options) = &change.options else {
        journal_merge(journal, &change.id, &JournalRecord::Remove);
        return;
    };
    let Some(legacy) = &change.legacy else {
        let changes: Changes = options
            .iter()
            .filter(|(key, _)| key.as_str() != "id")
            .map(|(key, value)| (key.clone(), Some(value.clone())))
            .collect();
        journal_merge(
            journal,
            &change.id,
            &JournalRecord::Upsert {
                created: true,
                parent: match &change.parent {
                    Some(parent) => Placement::Group(parent.clone()),
                    None => Placement::Root,
                },
                position: change.position,
                changes,
            },
        );
        return;
    };
    journal_merge(
        journal,
        &change.id,
        &JournalRecord::Upsert {
            created: false,
            parent: if change.moved {
                match &change.parent {
                    Some(parent) => Placement::Group(parent.clone()),
                    None => Placement::Root,
                }
            } else {
                Placement::Unchanged
            },
            position: change.position,
            changes: diff_options(legacy, options),
        },
    );
}

/// The per-key difference between two option sets — upstream `diffOptions`.
///
/// `id` is never a delta: it identifies the record, so recording it would be recording itself.
#[must_use]
pub fn diff_options(legacy: &Entry, options: &Entry) -> Changes {
    let mut keys: BTreeSet<&str> = legacy.keys().map(String::as_str).collect();
    keys.extend(options.keys().map(String::as_str));
    let mut changes = Vec::new();
    for key in keys {
        if key == "id" {
            continue;
        }
        let before = legacy.get(key);
        let after = options.get(key);
        if before == after {
            continue;
        }
        changes.push((key.to_owned(), after.cloned()));
    }
    changes
}

/// Overlay a journal onto `data` in place — upstream `applyJournal`.
///
/// `filter(id, key)` restricts which changes apply; `key` is `None` when asking about the entry as a
/// whole (creation, removal, relocation). A change that cannot be placed — its target group is gone —
/// is reported through `warn` and otherwise dropped, because a journal is repaired by the next file
/// read and a warning is how upstream keeps a stale journal from failing a write.
pub fn apply_journal(
    data: &mut EntryList,
    journal: &Journal,
    filter: Option<&EntryFilter<'_>>,
    warn: &mut dyn FnMut(&str),
) {
    for (id, record) in journal {
        if let Some(filter) = filter
            && !filter(id, None)
        {
            continue;
        }
        match record {
            JournalRecord::Remove => {
                detach(data, id);
            }
            JournalRecord::Upsert {
                parent,
                position,
                changes,
                ..
            } => {
                let target = parent.group().map(str::to_owned);
                match flatten(data).remove(id) {
                    None => {
                        let mut options = Entry::new();
                        options.insert("id".to_owned(), Value::String(id.clone()));
                        let mut key_filter = |key: &str| match filter {
                            Some(filter) => filter(id, Some(key)),
                            None => true,
                        };
                        apply_changes(&mut options, changes, Some(&mut key_filter));
                        if !place(data, options, target.as_deref(), *position) {
                            warn(&format!(
                                "cannot place entry {id}: group {} not found",
                                target.as_deref().unwrap_or("null")
                            ));
                        }
                    }
                    Some(flat) => {
                        let mut key_filter = |key: &str| match filter {
                            Some(filter) => filter(id, Some(key)),
                            None => true,
                        };
                        let mut updated = flat.options.clone();
                        apply_changes(&mut updated, changes, Some(&mut key_filter));
                        let relocated =
                            parent.is_moved() && target.as_deref() != flat.parent.as_deref();
                        if relocated {
                            detach(data, id);
                            if !place(data, updated, target.as_deref(), *position) {
                                warn(&format!(
                                    "cannot move entry {id}: group {} not found",
                                    target.as_deref().unwrap_or("null")
                                ));
                                // Put it back where it was, so a failed move loses nothing.
                                place(data, flat.options, flat.parent.as_deref(), flat.position);
                            }
                        } else if let Some(entry) = entry_at_mut(data, &flat.path, flat.position) {
                            *entry = updated;
                        }
                    }
                }
            }
        }
    }
}

/// A journal change the file contradicts — upstream `Conflict`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// Which entry.
    pub id: String,
    /// Why, in the words a person reads.
    pub reason: String,
}

/// Three-way reconciliation of a journal against a new file state — upstream `reconcile`.
///
/// The file wins every conflict: a conflicting key is dropped from the journal so the tree follows
/// the file, and every drop is reported. An entry the file never mentioned and the journal created is
/// not a conflict — there is nothing to contradict.
///
/// `file_owned(id, key)` scopes the check to what the file can actually express; patch-owned keys and
/// patch-inserted entries live elsewhere, so they are skipped. `key` is `None` for the entry as a
/// whole.
///
/// `base` is the snapshot the journal was recorded against, `theirs` what is on disk now.
pub fn reconcile(
    journal: &mut Journal,
    base: &BTreeMap<String, FlatEntry>,
    theirs: &BTreeMap<String, FlatEntry>,
    file_owned: &dyn Fn(&str, Option<&str>) -> bool,
) -> Vec<Conflict> {
    let mut conflicts = Vec::new();
    let mut kept: Journal = Vec::with_capacity(journal.len());
    for (id, record) in journal.iter() {
        let mut record = record.clone();
        let mut drop_record = false;
        if file_owned(id, None) {
            let before = base.get(id);
            let theirs = theirs.get(id);
            match &mut record {
                JournalRecord::Remove => {
                    if theirs.is_none() {
                        // The file removed it too; the journal has nothing left to persist.
                        drop_record = true;
                    } else if before.is_none()
                        || theirs.map(|t| &t.options) != before.map(|b| &b.options)
                    {
                        let reason = if before.is_some() {
                            "modified in file, removed at runtime"
                        } else {
                            "added in file, removed at runtime"
                        };
                        conflicts.push(Conflict {
                            id: id.clone(),
                            reason: reason.to_owned(),
                        });
                        drop_record = true;
                    }
                }
                JournalRecord::Upsert {
                    created,
                    parent,
                    changes,
                    ..
                } => {
                    let Some(theirs) = theirs else {
                        if before.is_some() {
                            conflicts.push(Conflict {
                                id: id.clone(),
                                reason: "removed in file, modified at runtime".to_owned(),
                            });
                            drop_record = true;
                        }
                        // else: created at runtime, nothing in the file to conflict with — the
                        // change stays queued and the next write creates the entry.
                        if drop_record {
                            continue;
                        }
                        kept.push((id.clone(), record));
                        continue;
                    };
                    // The file has the entry now, so the pending change is an update, not a
                    // creation: the next write must not re-create it as new.
                    *created = false;
                    let mut dropped = Vec::new();
                    for (key, value) in changes.iter() {
                        if !file_owned(id, Some(key)) {
                            continue;
                        }
                        let base_value = before.and_then(|before| before.options.get(key));
                        let file_value = theirs.options.get(key);
                        // Agreement, either with the snapshot or with the pending value, is not a
                        // conflict: there is nothing for the file to win.
                        if base_value == file_value || file_value == value.as_ref() {
                            continue;
                        }
                        conflicts.push(Conflict {
                            id: id.clone(),
                            reason: format!("key \"{key}\" modified both in file and at runtime"),
                        });
                        dropped.push(key.clone());
                    }
                    if !dropped.is_empty() {
                        changes.retain(|(key, _)| !dropped.contains(key));
                    }
                    if parent.is_moved()
                        && let Some(before) = before
                        && theirs.parent != before.parent
                        && theirs.parent.as_deref() != parent.group()
                    {
                        conflicts.push(Conflict {
                            id: id.clone(),
                            reason: "moved both in file and at runtime".to_owned(),
                        });
                        *parent = Placement::Unchanged;
                    }
                    let moved = parent.is_moved() && theirs.parent.as_deref() != parent.group();
                    if changes.is_empty() && !moved {
                        // Nothing left to persist: the file already says what the tree would write.
                        drop_record = true;
                    }
                }
            }
        }
        if !drop_record {
            kept.push((id.clone(), record));
        }
    }
    *journal = kept;
    conflicts
}
// ============================================================================================
// Patch overlays — upstream `include/src/patch.ts`
// ============================================================================================

/// One patch row — upstream `PatchOptions`.
///
/// `insert` and `id` are the only fields with meaning to [`apply_patches`]; everything else is an
/// override of the target entry's key of the same name (`name` doubles as the guard upstream checks
/// before overriding anything).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Patch {
    /// The entry to modify. Required unless the patch only inserts.
    pub id: Option<String>,
    /// Entries to append: to the root list, or to the group named by `id`.
    pub insert: Option<EntryList>,
    /// Guard. A patch whose `name` does not match the target's is skipped with a warning.
    pub name: Option<String>,
    /// Every other key, in the order the document listed it.
    pub overrides: Vec<(String, Value)>,
}

impl Patch {
    /// A patch that only inserts.
    #[must_use]
    pub fn inserting(insert: EntryList) -> Self {
        Self {
            insert: Some(insert),
            ..Self::default()
        }
    }

    /// The keys this patch overrides on its target — upstream's `omit(patch, ['id', 'insert', 'name'])`.
    #[must_use = "the keys are the patch\'s whole effect; dropping them silently drops the patch"]
    pub fn override_keys(&self) -> impl Iterator<Item = &str> {
        self.overrides.iter().map(|(key, _)| key.as_str())
    }
}

/// Give every entry an id, nested group members included — upstream `ensureIds`.
///
/// `used` reports whether an id is already taken (in the tree, not just in this list), and `next`
/// produces the candidate. Upstream's candidate is eight random hex digits; this takes the source as
/// an argument so a caller can keep its ids deterministic, and so a test can prove that a taken
/// candidate is retried rather than accepted twice.
pub fn ensure_ids(
    entries: &mut EntryList,
    used: &dyn Fn(&str) -> bool,
    next: &mut dyn FnMut() -> String,
) {
    // The ids already spent, kept current as we go: a list may name the same entry twice only by
    // mistake, and two anonymous entries must not end up sharing a fresh id either.
    let mut taken = entry_ids(entries);
    for value in entries.iter_mut() {
        let Some(entry) = value.as_object_mut() else {
            continue;
        };
        if !truthy(entry.get("id")) {
            let mut candidate = next();
            let mut attempts = 0u32;
            while used(&candidate) || taken.contains(&candidate) {
                candidate = next();
                attempts += 1;
                assert!(attempts < 1_000, "ensure_ids: id source exhausted");
            }
            taken.insert(candidate.clone());
            entry.insert("id".to_owned(), Value::String(candidate));
        }
        let group = is_group(entry);
        if group && let Some(children) = entry.get_mut("config").and_then(Value::as_array_mut) {
            ensure_ids(children, used, next);
        }
    }
}

/// Every id present in a list, nested group members included.
fn entry_ids(entries: &[Value]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for value in entries {
        let Some(entry) = value.as_object() else {
            continue;
        };
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            ids.insert(id.to_owned());
        }
        if let Some(children) = children(entry) {
            ids.extend(entry_ids(children));
        }
    }
    ids
}

/// Give every *inserted* entry an id — upstream `ensureInsertIds`.
///
/// Runtime changes are routed back to an inserted entry by id, so an insert without one would be
/// unreachable from the journal.
pub fn ensure_insert_ids(
    patches: &mut [Patch],
    used: &dyn Fn(&str) -> bool,
    next: &mut dyn FnMut() -> String,
) {
    for patch in patches {
        if let Some(insert) = patch.insert.as_mut() {
            ensure_ids(insert, used, next);
        }
    }
}

/// Overlay `patches` onto a clone of `data` — upstream `applyPatches`.
///
/// The input is not modified: upstream clones so that the tree owns every object it mounts, and a
/// caller that kept a snapshot for reconciliation must see it stay as it was. Returns the patched
/// list and the warnings for patches that matched nothing.
///
/// An insert patch clones its rows and appends them; a patch with an `id` is an override, checked
/// against `name` first and then applied key by key. Both failure modes warn and skip, because one
/// overlay list is shared across surfaces that do not all contain the same rows.
#[must_use]
pub fn apply_patches(data: &[Value], patches: &[Patch]) -> (EntryList, Vec<String>) {
    let mut data = data.to_vec();
    let mut warnings = Vec::new();

    for patch in patches {
        if let Some(insert) = &patch.insert {
            let mut cloned = insert.clone();
            match &patch.id {
                Some(id) => {
                    let index = index_entries(&data);
                    let Some(target) = index.get(id) else {
                        warnings.push(format!("patch insert: entry {id} not found"));
                        continue;
                    };
                    if !is_group(&target.options) {
                        warnings.push(format!("patch insert: entry {id} is not a group"));
                        continue;
                    }
                    let mut path = target.path.clone();
                    path.push(target.position);
                    let Some(list) = list_at_mut(&mut data, &path, true) else {
                        warnings.push(format!("patch insert: entry {id} has no child list"));
                        continue;
                    };
                    list.append(&mut cloned);
                }
                None => data.append(&mut cloned),
            }
            continue;
        }

        let Some(id) = &patch.id else {
            warnings.push("patch: id is required for non-insert patches".to_owned());
            continue;
        };
        let index = index_entries(&data);
        let Some(target) = index.get(id) else {
            warnings.push(format!("patch: entry {id} not found"));
            continue;
        };
        if let Some(expected) = &patch.name {
            let actual = target.options.get("name").and_then(Value::as_str);
            if actual != Some(expected.as_str()) {
                warnings.push(format!(
                    "patch: name mismatch for {id} (expected {expected}, got {})",
                    actual.unwrap_or("nothing")
                ));
                continue;
            }
        }
        let path = target.path.clone();
        let position = target.position;
        let Some(entry) = entry_at_mut(&mut data, &path, position) else {
            warnings.push(format!("patch: entry {id} not found"));
            continue;
        };
        for (key, value) in &patch.overrides {
            if key == "id" {
                continue;
            }
            entry.insert(key.clone(), value.clone());
        }
    }

    (data, warnings)
}

/// [`flatten`], spelled separately so the patch code reads like the journal code.
fn index_entries(data: &[Value]) -> BTreeMap<String, FlatEntry> {
    flatten(data)
}

/// Who owns an entry, or one key of it, in the patched tree — upstream `EntryOwner`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryOwner {
    /// The file.
    File,
    /// A patch that overrides that key; the last one wins.
    Patch {
        /// Index into the patch list.
        index: usize,
    },
    /// The patch that inserted the whole entry.
    Insert {
        /// Index into the patch list.
        index: usize,
    },
}

/// Which patch owns what — upstream `PatchIndex`.
///
/// An inserted entry is owned by the patch that inserted it, as a whole *and* per key: overriding a
/// key of an inserted entry would otherwise leave the entry in one document and half its values in
/// another. Entries the file declares stay [`EntryOwner::File`] unless a patch names them.
#[derive(Clone, Debug, Default)]
pub struct PatchIndex {
    keys: BTreeMap<String, BTreeMap<String, usize>>,
    inserts: BTreeMap<String, usize>,
}

impl PatchIndex {
    /// Build the index over a patch list.
    #[must_use]
    pub fn new(patches: &[Patch]) -> Self {
        let mut keys: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        let mut inserts = BTreeMap::new();
        for (index, patch) in patches.iter().enumerate() {
            if let Some(insert) = &patch.insert {
                index_inserts(insert, index, &mut inserts);
                continue;
            }
            let Some(id) = &patch.id else {
                continue;
            };
            let entry = keys.entry(id.clone()).or_default();
            for key in patch.override_keys() {
                entry.insert(key.to_owned(), index);
            }
        }
        Self { keys, inserts }
    }

    /// Owner of the entry as a whole — `None` is the root and always the file's.
    #[must_use]
    pub fn entry(&self, id: Option<&str>) -> EntryOwner {
        let Some(id) = id else {
            return EntryOwner::File;
        };
        match self.inserts.get(id) {
            Some(index) => EntryOwner::Insert { index: *index },
            None => EntryOwner::File,
        }
    }

    /// Owner of one key of an entry.
    #[must_use]
    pub fn key(&self, id: &str, key: &str) -> EntryOwner {
        if let Some(index) = self.inserts.get(id) {
            return EntryOwner::Insert { index: *index };
        }
        match self.keys.get(id).and_then(|keys| keys.get(key)) {
            Some(index) => EntryOwner::Patch { index: *index },
            None => EntryOwner::File,
        }
    }

    /// Whether the file owns the entry (`key: None`) or one of its keys.
    ///
    /// This is what [`reconcile`] is scoped by: a key a patch owns cannot be contradicted by the
    /// file, so the journal keeps it.
    #[must_use]
    pub fn file_owned(&self, id: &str, key: Option<&str>) -> bool {
        match key {
            None => self.entry(Some(id)) == EntryOwner::File,
            Some(key) => self.key(id, key) == EntryOwner::File,
        }
    }
}

fn index_inserts(insert: &[Value], index: usize, out: &mut BTreeMap<String, usize>) {
    for value in insert {
        let Some(entry) = value.as_object() else {
            continue;
        };
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            out.insert(id.to_owned(), index);
        }
        if let Some(children) = children(entry) {
            index_inserts(children, index, out);
        }
    }
}

/// Route a journal to its owners — upstream `routeJournal`.
///
/// File-owned changes are applied to `data`, patch-owned ones to `patches`; both are mutated in
/// place, so pass clones. Returns whether any patch changed — a caller that persists only the file
/// needs to know that a round trip through the parent is required for the rest.
///
/// A key that a patch owns is written into that patch rather than the file: writing it into the file
/// would create a second source of truth for a value the patch exists to override, and the next
/// overlay would silently undo the user's edit.
pub fn route_journal(
    journal: &Journal,
    data: &mut EntryList,
    patches: &mut [Patch],
    warn: &mut dyn FnMut(&str),
) -> bool {
    let index = PatchIndex::new(patches);
    let mut touched = false;

    for (id, record) in journal {
        let owner = index.entry(Some(id));
        match record {
            JournalRecord::Remove => match &owner {
                EntryOwner::Insert { .. } => touched |= remove_inserted(patches, id),
                _ => {
                    detach(data, id);
                }
            },
            JournalRecord::Upsert {
                parent,
                position,
                changes,
                ..
            } => {
                let mut current = match &owner {
                    // An inserted entry lives in the patch, so the file is not where its current
                    // options are; reading them from `data` would treat every update as a creation.
                    EntryOwner::Insert { index } => inserted_entry(patches, *index, id),
                    _ => flatten(data).remove(id),
                };
                let Some(mut current) = current.take() else {
                    let mut options = Entry::new();
                    options.insert("id".to_owned(), Value::String(id.clone()));
                    apply_changes(&mut options, changes, None);
                    touched |=
                        place_at(&mut options, parent, *position, &index, data, patches, warn);
                    continue;
                };
                let relocated = parent.is_moved() && parent.group() != current.parent.as_deref();
                if relocated {
                    // Moving across owners carries the full options over, not just the delta.
                    if matches!(owner, EntryOwner::Insert { .. }) {
                        touched |= remove_inserted(patches, id);
                    } else {
                        detach(data, id);
                    }
                    apply_changes(&mut current.options, changes, None);
                    touched |= place_at(
                        &mut current.options,
                        parent,
                        *position,
                        &index,
                        data,
                        patches,
                        warn,
                    );
                    continue;
                }
                if matches!(owner, EntryOwner::Insert { .. }) {
                    apply_changes(&mut current.options, changes, None);
                    touched |= write_inserted(patches, id, &current.options);
                    continue;
                }
                let mut by_patch: BTreeMap<usize, Vec<(String, Value)>> = BTreeMap::new();
                let mut options = current.options.clone();
                let mut split = |key: &str| match index.key(id, key) {
                    EntryOwner::Patch { index } => {
                        by_patch
                            .entry(index)
                            .or_default()
                            .push((key.to_owned(), changes_value(changes, key)));
                        false
                    }
                    _ => true,
                };
                apply_changes(&mut options, changes, Some(&mut split));
                // `split` borrows `by_patch`; the last use was the call above, so the map is free
                // to move below without an explicit drop.
                let _ = &split;
                if let Some(entry) = list_at_mut(data, &current.path, false)
                    .and_then(|slot| slot.get_mut(current.position))
                    .and_then(Value::as_object_mut)
                {
                    *entry = options;
                }
                for (index, changed) in by_patch {
                    for (key, value) in changed {
                        if let Some(patch) = patches.get_mut(index) {
                            set_override(patch, &key, value);
                        }
                        touched = true;
                    }
                }
            }
        }
    }

    touched
}

/// The value a delta carries for `key`; a delta built from `changes` always has it.
fn changes_value(changes: &Changes, key: &str) -> Value {
    changes
        .iter()
        .find(|(existing, _)| existing == key)
        .and_then(|(_, value)| value.clone())
        .unwrap_or(Value::Null)
}

/// Put `options` where the journal says — into a patch's insert list when a patch owns the parent
/// group, into the file otherwise.
fn place_at(
    options: &mut Entry,
    parent: &Placement,
    position: usize,
    index: &PatchIndex,
    data: &mut EntryList,
    patches: &mut [Patch],
    warn: &mut dyn FnMut(&str),
) -> bool {
    let id = options
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("(anonymous)")
        .to_owned();
    let Some(parent) = parent.group() else {
        // No group named: the root list, at the journal's index.
        let at = position.min(data.len());
        data.insert(at, Value::Object(std::mem::take(options)));
        return false;
    };
    if let EntryOwner::Insert { index: patch_index } = index.entry(Some(parent)) {
        if let Some(list) = patches
            .get_mut(patch_index)
            .and_then(|patch| patch.insert.as_mut())
        {
            let at = position.min(list.len());
            list.insert(at, Value::Object(std::mem::take(options)));
        }
        return true;
    }
    if !place(data, std::mem::take(options), Some(parent), position) {
        warn(&format!(
            "cannot place entry {id}: group {parent} not found"
        ));
    }
    false
}

/// Set (or replace) one override on a patch.
fn set_override(patch: &mut Patch, key: &str, value: Value) {
    match patch
        .overrides
        .iter_mut()
        .find(|(existing, _)| existing == key)
    {
        Some(slot) => slot.1 = value,
        None => patch.overrides.push((key.to_owned(), value)),
    }
}

/// Drop an inserted entry from every patch, keeping its nested group config consistent.
fn remove_inserted(patches: &mut [Patch], id: &str) -> bool {
    let mut removed = false;
    for patch in patches.iter_mut() {
        if let Some(insert) = patch.insert.as_mut()
            && remove_entry(insert, id)
        {
            removed = true;
        }
    }
    removed
}

fn remove_entry(list: &mut EntryList, id: &str) -> bool {
    if let Some(position) = list
        .iter()
        .position(|value| value.get("id").and_then(Value::as_str) == Some(id))
    {
        list.remove(position);
        return true;
    }
    for value in list.iter_mut() {
        if let Some(entry) = value.as_object_mut()
            && let Some(children) = entry.get_mut("config").and_then(Value::as_array_mut)
            && remove_entry(children, id)
        {
            return true;
        }
    }
    false
}

/// Write an inserted entry's new options back into the patch that inserted it.
fn write_inserted(patches: &mut [Patch], id: &str, options: &Entry) -> bool {
    for patch in patches.iter_mut() {
        if let Some(insert) = patch.insert.as_mut()
            && replace_entry(insert, id, options)
        {
            return true;
        }
    }
    false
}

/// An inserted entry as the index sees it: it lives at the root of a patch's insert list.
fn inserted_entry(patches: &[Patch], index: usize, id: &str) -> Option<FlatEntry> {
    let insert = patches.get(index)?.insert.as_ref()?;
    let flat = flatten(insert);
    flat.get(id).cloned()
}

fn replace_entry(list: &mut EntryList, id: &str, options: &Entry) -> bool {
    for value in list.iter_mut() {
        let matches = value.get("id").and_then(Value::as_str) == Some(id);
        if matches {
            *value = Value::Object(options.clone());
            return true;
        }
        if let Some(entry) = value.as_object_mut()
            && let Some(children) = entry.get_mut("config").and_then(Value::as_array_mut)
            && replace_entry(children, id, options)
        {
            return true;
        }
    }
    false
}

// ============================================================================================
// The entry rules a loader applies — upstream `loader/src/config/entry.ts`, `tree.ts`, `index.ts`
// ============================================================================================

/// The order upstream rewrites an entry's keys into: `id` and `name` first, `config` last, the rest
/// alphabetically — upstream `sortKeys`.
///
/// This is not cosmetic. The loader writes its options back to the file, and a stable order is what
/// keeps an edit to one plugin from reordering a file the user shares with other tools — a diff of
/// one line, not a diff of the whole row.
///
/// The *order* is returned rather than applied because `serde_json::Map` in this crate is
/// `BTreeMap`-backed (`preserve_order` is off), so the map itself is always alphabetical and no
/// in-place reordering is expressible. A serializer that writes the file honours the returned order,
/// which is where the guarantee has to live anyway: the map is an intermediate, the file is the
/// artifact the user sees. Upstream sorts with `localeCompare`; this sorts by code unit, which agrees
/// for the ASCII keys a loader entry uses and is stated rather than locale-dependent.
#[must_use]
pub fn sort_keys(entry: &Entry) -> Vec<String> {
    const PREPEND: [&str; 2] = ["id", "name"];
    const APPEND: &str = "config";
    let mut ordered = Vec::with_capacity(entry.len());
    for key in PREPEND {
        if entry.contains_key(key) {
            ordered.push(key.to_owned());
        }
    }
    let mut rest: Vec<&String> = entry
        .keys()
        .filter(|key| !PREPEND.contains(&key.as_str()) && key.as_str() != APPEND)
        .collect();
    rest.sort();
    ordered.extend(rest.into_iter().cloned());
    if entry.contains_key(APPEND) {
        ordered.push(APPEND.to_owned());
    }
    ordered
}

/// Whether a JSON value is a serialized `!!js` node — upstream `isJsExpr`.
#[must_use]
pub fn is_js_expression(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.contains_key("__jsExpr"))
}

/// Why a `disabled` chain could not be decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnevaluableDisabled {
    /// The entry whose `disabled` is a `!!js` expression.
    pub id: String,
    /// The expression text, so a person can see what was skipped.
    pub expression: String,
}

impl fmt::Display for UnevaluableDisabled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "entry {} is disabled by the expression `{}`, which this build does not evaluate",
            self.id, self.expression
        )
    }
}

impl std::error::Error for UnevaluableDisabled {}

/// Whether an entry is disabled, including through any owning group — upstream `Entry.disabled`.
///
/// A group is never disabled by its own flag (`if (this.options.group) return false`): a group is a
/// container, and disabling a container is spelled by disabling each child. The chain walks
/// ancestors, so a child of a disabled group is disabled even though its own flag is absent.
///
/// `ancestors` are the enclosing groups, outermost last, as ids paired with their options — the
/// caller knows the tree, and passing the chain in keeps this a function of its arguments. An
/// expression anywhere in the chain is an error rather than a guess: reading `{"__jsExpr": "..."}`
/// as false would start a plugin the user believes is off.
pub fn is_disabled(
    entry: &Entry,
    ancestors: &[(&str, &Entry)],
) -> Result<bool, UnevaluableDisabled> {
    if is_group(entry) {
        return Ok(false);
    }
    let first = resolve_disabled(entry.get("id").and_then(Value::as_str), entry)?;
    if first {
        return Ok(true);
    }
    for (id, ancestor) in ancestors {
        if resolve_disabled(Some(id), ancestor)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn resolve_disabled(id: Option<&str>, entry: &Entry) -> Result<bool, UnevaluableDisabled> {
    match entry.get("disabled") {
        Some(value) if is_js_expression(value) => {
            let expression = value
                .get("__jsExpr")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Err(UnevaluableDisabled {
                id: id.unwrap_or("(anonymous)").to_owned(),
                expression,
            })
        }
        value => Ok(truthy(value)),
    }
}

/// The entry an id names, nested ids separated by [`SEP`] — upstream `EntryTree.resolve`.
///
/// An id may itself contain the separator; the last segment names the entry, the segments before it
/// name the groups to walk through. Either a missing group or a missing entry is an error, because
/// the caller asked for a specific entry and a silent `None` would hide a typo.
pub fn resolve<'a>(data: &'a [Value], id: &str) -> Result<&'a Entry, ResolveError> {
    let segments: Vec<&str> = id.split(SEP).collect();
    let mut list = data;
    let mut walked = String::new();
    for segment in &segments[..segments.len() - 1] {
        if !walked.is_empty() {
            walked.push(SEP);
        }
        walked.push_str(segment);
        let group = list
            .iter()
            .filter_map(Value::as_object)
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(*segment));
        let Some(group) = group else {
            return Err(ResolveError::NotFound(walked));
        };
        let Some(children) = children(group) else {
            return Err(ResolveError::NotAGroup(walked));
        };
        list = children;
    }
    let tail = segments[segments.len() - 1];
    let entry = list
        .iter()
        .filter_map(Value::as_object)
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(tail));
    match entry {
        Some(entry) => Ok(entry),
        None => Err(ResolveError::NotFound(id.to_owned())),
    }
}

/// Why a nested id did not resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// No entry or group has that id.
    NotFound(String),
    /// An id on the path names an entry that is not a group.
    NotAGroup(String),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "cannot resolve entry {id}"),
            Self::NotAGroup(id) => write!(f, "entry {id} is not a group"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// The children of the group `id` names, `None` naming the root — upstream `resolveGroup`.
pub fn resolve_group<'a>(data: &'a [Value], id: Option<&str>) -> Result<&'a [Value], ResolveError> {
    let Some(id) = id else {
        return Ok(data);
    };
    let entry = resolve(data, id)?;
    match children(entry) {
        Some(children) => Ok(children),
        None => Err(ResolveError::NotAGroup(id.to_owned())),
    }
}

/// Assign an id when the entry has none — upstream `EntryTree.ensureId`.
///
/// `next` produces candidates and `used` reports whether the tree already has one; the loop is what
/// upstream's `do … while (this.store[id])` is, restated so that a source which keeps producing a
/// taken id fails loudly instead of spinning.
pub fn ensure_id(
    options: &mut Entry,
    used: &dyn Fn(&str) -> bool,
    next: &mut dyn FnMut() -> String,
) -> Result<String, IdExhausted> {
    if let Some(id) = options.get("id").and_then(Value::as_str)
        && !id.is_empty()
    {
        return Ok(id.to_owned());
    }
    for _ in 0..1_000 {
        let candidate = next();
        if used(&candidate) {
            continue;
        }
        options.insert("id".to_owned(), Value::String(candidate.clone()));
        return Ok(candidate);
    }
    Err(IdExhausted)
}

/// The id source produced a thousand taken candidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdExhausted;

impl fmt::Display for IdExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("id source produced no unused id in 1000 attempts")
    }
}

impl std::error::Error for IdExhausted {}

// ============================================================================================
// The config file's decisions — upstream `include/src/index.ts`
// ============================================================================================

/// The media type a config file's extension maps to — upstream `types`.
///
/// Upstream throws on an unknown extension at construction, which is the right moment: a tree whose
/// file it cannot read or write is a tree that silently keeps the user's edits in memory forever.
#[must_use]
pub fn content_type(filename: &str) -> Option<&'static str> {
    match extension(filename)? {
        ".json" => Some("application/json"),
        ".yaml" | ".yml" => Some("application/yaml"),
        _ => None,
    }
}

/// The extension of a path's last segment, `.` included — upstream `node:path`'s `extname`.
///
/// A leading dot is part of the name, not an extension (`.gitignore` has none), and a dot in a
/// directory is not an extension either (`a.b/config` has none), which is why this reads only the
/// last segment.
#[must_use]
pub fn extension(filename: &str) -> Option<&str> {
    let name = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None;
    }
    Some(&name[dot..])
}

/// Which step of reading a config file failed — upstream `ConfigStage`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigStage {
    /// The file could not be read.
    Read,
    /// The bytes were not the declared format.
    Parse,
    /// The parsed document was not a top-level array.
    Validate,
}

impl ConfigStage {
    /// The stage's name, as the message spells it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Parse => "parse",
            Self::Validate => "validate",
        }
    }
}

/// A config file failure that says which stage failed — upstream `ConfigFileError`.
///
/// The stage is not decoration. [`initial_or_fail`] acts on exactly one of them: a *read* failure
/// whose cause is "no such file" is the normal first run, and everything else is a real error that
/// must not be papered over by writing a fresh default over a file the user was mid-edit on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFileError {
    /// Which step failed.
    pub stage: ConfigStage,
    /// The file.
    pub path: String,
    /// The underlying problem, in the words of whatever detected it.
    pub cause: String,
}

impl ConfigFileError {
    /// Build a failure for `stage` on `path`.
    #[must_use]
    pub fn new(stage: ConfigStage, path: impl Into<String>, cause: impl Into<String>) -> Self {
        Self {
            stage,
            path: path.into(),
            cause: cause.into(),
        }
    }
}

impl fmt::Display for ConfigFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "failed to {} config file {}",
            self.stage.label(),
            self.path
        )
    }
}

impl std::error::Error for ConfigFileError {}

/// What to do when the config file could not be read — upstream `Include`'s `Service.init` fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingFile {
    /// The file does not exist and `initial` is set: write the default and read it back.
    WriteInitial,
    /// The file does not exist and no default was given: fail.
    Fail,
    /// The read failed for any other reason: fail, and do not touch the file.
    Propagate,
}

/// Decide what a failed first read means — the whole of upstream's `ENOENT` branch, as a decision.
///
/// Only a missing file falls back to `initial`; an unreadable or truncated one is an error. The
/// distinction is what keeps a first run from needing a default and a crash from looking like one.
#[must_use]
pub fn initial_or_fail(stage: ConfigStage, missing_cause: bool, has_initial: bool) -> MissingFile {
    if stage != ConfigStage::Read || !missing_cause {
        return MissingFile::Propagate;
    }
    if has_initial {
        MissingFile::WriteInitial
    } else {
        MissingFile::Fail
    }
}

/// The outcome of preparing a write — upstream `_write`'s sequence of checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WritePlan {
    /// Nothing is pending, or nothing has been read yet: nothing to do.
    Idle,
    /// The file changed under us: re-read first, then come back with the merge.
    Reread,
    /// The file is gone. The pending changes stay in memory and are reported.
    Missing,
    /// The file is read-only. The pending changes stay in memory and are reported.
    ReadOnly,
    /// Write `text` to `target`, through `temp` and a rename, and only while the file still holds
    /// `expected`.
    Write {
        /// The file to replace.
        target: String,
        /// The temporary file to write first.
        temp: String,
        /// The text to write.
        text: String,
        /// The content the target must still hold, when the write is conditional.
        expected: Option<String>,
    },
}

/// Decide what a write should do, from facts the caller already has.
///
/// The order is the interesting part and mirrors upstream exactly: a file that vanished is *not* an
/// error (it is reported and the changes are kept), a file that changed since the snapshot must be
/// re-read before writing (otherwise the write would clobber the other writer), and a read-only file
/// keeps its pending changes in memory rather than failing the tree. `temp` is derived from the
/// target so that no reader ever sees a half-written file.
///
/// `pending` is the number of unsaved changes, `cached` the text last read or written (`None` before
/// the first read), `current` what is on disk now (`None` when the file is missing).
#[must_use]
pub fn plan_write(
    target: &str,
    temp: &str,
    pending: usize,
    cached: Option<&str>,
    current: Option<&str>,
    writable: bool,
    text: &str,
) -> WritePlan {
    if pending == 0 || cached.is_none() {
        return WritePlan::Idle;
    }
    let Some(current) = current else {
        return WritePlan::Missing;
    };
    if Some(current) != cached {
        return WritePlan::Reread;
    }
    if !writable {
        return WritePlan::ReadOnly;
    }
    WritePlan::Write {
        target: target.to_owned(),
        temp: temp.to_owned(),
        text: text.to_owned(),
        expected: cached.map(str::to_owned),
    }
}

/// The name of the temporary file a write goes through — upstream `_writeText`'s `tmp`.
///
/// Derived from the target rather than placed elsewhere, so the rename is within one filesystem and
/// therefore atomic; `pid` and `seq` keep two writers from colliding.
#[must_use]
pub fn temp_path(target: &str, pid: u32, seq: u64) -> String {
    let (dir, name) = match target.rfind(['/', '\\']) {
        Some(index) => (&target[..=index], &target[index + 1..]),
        None => ("", target),
    };
    format!("{dir}.{name}.{pid}.{seq}.tmp")
}

/// Whether a rename failure is worth retrying — upstream `isTransientRenameError`.
///
/// On Windows a rename over a file another process holds open (typically an antivirus scanning what
/// was just written) fails with one of these, and the lock is normally gone within milliseconds. The
/// codes are matched on every platform because the retry is cheaper than the special case.
#[must_use]
pub fn is_transient_rename_error(code: &str) -> bool {
    matches!(code, "EACCES" | "EPERM" | "EBUSY")
}

/// The maximum number of rename retries — upstream `RENAME_RETRIES`.
pub const RENAME_RETRIES: u32 = 10;

/// The backoff step in milliseconds — upstream `RENAME_BACKOFF`.
pub const RENAME_BACKOFF_MS: u64 = 20;

/// How long to wait before rename attempt `attempt` (zero-based), or `None` to give up.
///
/// Upstream sleeps `20 * (attempt + 1)` milliseconds and stops after ten retries; the delay is
/// returned instead of slept so the policy is testable and the caller keeps its own clock.
#[must_use]
pub fn retry_delay(attempt: u32, code: &str) -> Option<Duration> {
    if attempt >= RENAME_RETRIES || !is_transient_rename_error(code) {
        return None;
    }
    Some(Duration::from_millis(
        RENAME_BACKOFF_MS * u64::from(attempt + 1),
    ))
}

/// What to do when a write is refused or disagrees with the file — upstream `_writeText`'s
/// `StaleWriteError` branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleWrite {
    /// The target still holds the expected content: rename.
    Rename,
    /// Someone else wrote it: re-read, merge, and try again.
    Reread,
}

/// Decide whether a conditional write may go ahead.
///
/// `expected` is what the caller read; `current` is what is on disk now. A missing file is *not* a
/// green light — it is a change like any other, because a file deleted between the check and the
/// rename is a file the caller's snapshot no longer describes.
#[must_use]
pub fn check_expected(expected: Option<&str>, current: Option<&str>) -> StaleWrite {
    if expected == current {
        StaleWrite::Rename
    } else {
        StaleWrite::Reread
    }
}

/// Assign ids to entries the file leaves anonymous — upstream `_assignIds`.
///
/// Entries the file leaves anonymous get an id before the loader sees them, and an entry that looks
/// exactly like one named on the previous read *keeps that id*, matched by parent and by options.
/// That is what keeps an edit to a neighbour from restarting an anonymous plugin: the file never
/// named it, so the only stable identity available is its shape.
///
/// `previous` is the id-to-(parent, options) map from the last read; `used` reports ids already spent
/// elsewhere in the tree. Returns the entries (mutated in place) and the new map. Ids are inserted
/// first, so [`sort_keys`]'s order survives the round trip.
pub fn assign_ids(
    data: &mut EntryList,
    previous: &BTreeMap<String, (Option<String>, Entry)>,
    used: &dyn Fn(&str) -> bool,
    next: &mut dyn FnMut() -> String,
) -> BTreeMap<String, (Option<String>, Entry)> {
    let mut spent = entry_ids(data);
    let mut pending: Vec<(Vec<usize>, usize, Option<String>, Entry)> = Vec::new();
    collect_anonymous(data, &mut Vec::new(), None, &mut pending);

    let mut previous = previous.clone();
    let mut assigned = BTreeMap::new();
    for (path, position, parent, options) in pending {
        let matched =
            previous
                .iter()
                .find_map(|(candidate, (candidate_parent, candidate_options))| {
                    (candidate_parent == &parent && candidate_options == &options)
                        .then(|| candidate.clone())
                });
        let id = match matched {
            Some(candidate) => {
                previous.remove(&candidate);
                Some(candidate)
            }
            None => None,
        };
        let id = if let Some(id) = id {
            id
        } else {
            let mut candidate = next();
            let mut attempts = 0u32;
            while spent.contains(&candidate) || used(&candidate) {
                candidate = next();
                attempts += 1;
                assert!(attempts < 1_000, "assign_ids: id source exhausted");
            }
            candidate
        };
        spent.insert(id.clone());
        assigned.insert(id.clone(), (parent.clone(), options.clone()));
        if let Some(entry) = entry_at_mut(data, &path, position) {
            let mut with_id = Entry::new();
            with_id.insert("id".to_owned(), Value::String(id));
            for (key, value) in entry.iter() {
                with_id.insert(key.clone(), value.clone());
            }
            *entry = with_id;
        }
    }
    assigned
}

fn collect_anonymous(
    data: &[Value],
    path: &mut Vec<usize>,
    parent: Option<&str>,
    out: &mut Vec<(Vec<usize>, usize, Option<String>, Entry)>,
) {
    for (position, value) in data.iter().enumerate() {
        let Some(entry) = value.as_object() else {
            continue;
        };
        if !truthy(entry.get("id")) {
            out.push((
                path.clone(),
                position,
                parent.map(str::to_owned),
                entry.clone(),
            ));
        }
        if let Some(children) = children(entry) {
            path.push(position);
            collect_anonymous(children, path, entry.get("id").and_then(Value::as_str), out);
            path.pop();
        }
    }
}

/// The module's plugin export, unwrapped from its ESM/CJS shapes — upstream `unwrapExports`.
///
/// Two shapes have to be seen through. A namespace object carries the plugin under `default`; an
/// esbuild-interop object additionally carries `__esModule` and puts the real export under a second
/// `default` (`evanw/esbuild#2623`). A value that is neither is returned unchanged, because a plugin
/// may legitimately be a function, an object with `apply`, or a class.
#[must_use]
pub fn unwrap_exports(exports: Value) -> Value {
    let mut exports = match exports {
        Value::Object(object) if object.contains_key("default") => object
            .get("default")
            .cloned()
            .expect("the key was just found"),
        other => return other,
    };
    if !truthy(exports.get("__esModule")) {
        return exports;
    }
    match exports {
        Value::Object(ref mut object) => match object.get("default") {
            Some(default) => default.clone(),
            None => exports,
        },
        _ => exports,
    }
}
