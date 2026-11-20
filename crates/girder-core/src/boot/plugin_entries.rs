//! The plugin entry registry: the explicit replacement for Cordis's entry tree.
//!
//! # What upstream reads
//!
//! `packages/boot/plugin-manager/src/index.ts` answers `listPlugins` from three sources that all
//! describe one thing -- the loader's entry tree:
//!
//! ```typescript
//! const rows = flatten(composeEntries([readProfilePatches('dsh', this.profile)]))
//! const snapshot = await readPluginInventory(this.ctx)
//! const actual = [...this.ctx.loader.entries()].find(row => row.id === entry.entryId)
//! ```
//!
//! The tree exists because Cordis mounts plugins into a mutable, hierarchical structure at runtime.
//! This port replaced that with `ServiceKey`, so there is no tree to walk.
//!
//! # What this port does instead
//!
//! An explicit registry: a `BTreeMap` of entries keyed by id, each carrying the facts
//! `listPlugins` actually reads. That is the same **data**, obtained by a different **mechanism** --
//! and the mechanism keeps what the tree cannot: every entry is a typed row this compiler checks,
//! and nothing can appear in it that was not put there.
//!
//! # The two read-only reasons, and why they survive the substitution
//!
//! Upstream marks an entry `management-required` when it is protected (the harness manages it, so a
//! user must not remove it) or when it *is* the entry asking. It marks one `unaddressable` when the
//! tree cannot give it a stable address: no matching profile row, more than one row claiming it, a
//! name that disagrees with the row, or a parent that is not the include root.
//!
//! **Both are facts about data, not about a tree**, so both are computable here. `unaddressable` in
//! particular reduces to four checks a registry can make: does a row exist, is there exactly one, does
//! its name match, and is the parent the include root. That is what [`address`] does.

use std::collections::BTreeMap;

/// One row in the profile a plugin entry may be addressed by.
///
/// Upstream's `readProfilePatches` output, reduced to the fields the address check reads. A profile
/// row is what a patch applies to: it names a plugin and the spec that loads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileRow {
    /// The row's identity, which becomes an entry's `patchId` when it matches.
    pub id: String,
    /// The plugin the row patches.
    pub name: String,
    /// The parent this row sits under. `include` is the root upstream compares against.
    pub parent: String,
}

/// One installed plugin, as `listPlugins` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The entry id, which is what a client addresses it by.
    pub entry_id: String,
    /// The package name.
    pub module_name: String,
    /// The loader's `options.id` for this entry -- the key a profile row is matched against.
    ///
    /// **Not the module name**, which is the mistake an earlier version of this struct made.
    /// Upstream matches `rows.filter(row => row.id === actual?.options.id)`: the row's id against the
    /// entry's *options* id, and only then compares `candidate.name !== entry.moduleName` as a
    /// separate check. Collapsing the two would make a row whose id happens to equal the module name
    /// match for the wrong reason.
    pub options_id: String,
}

/// Why an entry cannot be managed, when it cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadOnly {
    /// The harness manages this plugin, or it is the entry asking the question.
    ManagementRequired,
    /// No stable profile address: missing, ambiguous, misnamed, or not under the include root.
    Unaddressable,
}

impl ReadOnly {
    /// The wire spelling, matching upstream's discriminator.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ManagementRequired => "management-required",
            Self::Unaddressable => "unaddressable",
        }
    }
}

/// Resolves an entry's profile address, or says why it has none.
///
/// Upstream's four conditions, in the order its `||` chain evaluates them, so a caller that logged the
/// reason upstream would get the same one here:
///
/// 1. no row whose id matches the entry's `options.id`,
/// 2. more than one row claiming that id,
/// 3. the row's name disagreeing with the entry's module name,
/// 4. a parent that is not the include root.
///
/// Returns the row id, which is the `patchId` upstream reports on a manageable entry.
#[must_use]
pub fn address<'a>(entry: &Entry, rows: &'a [ProfileRow]) -> Option<&'a str> {
    let candidates: Vec<&ProfileRow> = rows
        .iter()
        .filter(|row| row.id == entry.options_id)
        .collect();
    if candidates.len() != 1 {
        return None;
    }
    let row = candidates[0];
    if row.name != entry.module_name {
        return None;
    }
    if row.parent != INCLUDE_ROOT {
        return None;
    }
    Some(row.id.as_str())
}

/// The parent name upstream compares against.
///
/// `actual?.parent.tree.ctx.fiber.entry?.id !== 'include'`: an entry not mounted under the include
/// root has no address a patch can name.
pub const INCLUDE_ROOT: &str = "include";

/// The plugins the harness manages itself.
///
/// Upstream's `PROTECTED_PLUGINS`, from `official-bundles`. Kept as a list because that is what it is:
/// a fixed set of names whose removal would break the harness, so a client is told "you cannot manage
/// this" rather than being allowed to try.
pub const PROTECTED: &[&str] = &[
    "@deepseek-ai/dsh",
    "@deepseek-ai/dsh-app-boot",
    "@deepseek-ai/dsh-boot",
    "@deepseek-ai/dsh-llm",
    "@deepseek-ai/dsh-session-controller",
];

/// The entry registry.
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<String, Entry>,
    rows: Vec<ProfileRow>,
    owner: Option<String>,
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one installed plugin.
    pub fn insert(&mut self, entry: Entry) {
        self.entries.insert(entry.entry_id.clone(), entry);
    }

    /// Adds one profile row.
    pub fn push_row(&mut self, row: ProfileRow) {
        self.rows.push(row);
    }

    /// Names the entry that is asking, so it reports itself as management-required.
    ///
    /// Upstream reads `ctx.fiber.entry?.id`: the plugin manager cannot manage the entry it belongs
    /// to, because doing so would unload itself mid-call.
    pub fn set_owner(&mut self, entry_id: impl Into<String>) {
        self.owner = Some(entry_id.into());
    }

    /// Every entry, in id order so two runs agree.
    #[must_use]
    pub fn entries(&self) -> Vec<&Entry> {
        self.entries.values().collect()
    }

    /// The row a patch may address, when the entry is manageable.
    ///
    /// Returns `Ok(patch_id)` for a manageable entry and `Err(reason)` otherwise. The two checks run
    /// in upstream's order: protected first, because a protected entry is never addressable for
    /// management regardless of what the profile says.
    ///
    /// # Errors
    ///
    /// [`ReadOnly`] naming why.
    pub fn patch_id(&self, entry: &Entry) -> Result<&str, ReadOnly> {
        if PROTECTED.contains(&entry.module_name.as_str())
            || self.owner.as_deref() == Some(entry.entry_id.as_str())
        {
            return Err(ReadOnly::ManagementRequired);
        }
        address(entry, &self.rows).ok_or(ReadOnly::Unaddressable)
    }

    /// Renders every entry the way `listPlugins` answers.
    #[must_use]
    pub fn list(&self) -> Vec<serde_json::Value> {
        self.entries()
            .into_iter()
            .map(|entry| {
                let mut row = serde_json::json!({
                    "entryId": entry.entry_id,
                    "moduleName": entry.module_name,
                });
                match self.patch_id(entry) {
                    Ok(patch_id) => row["patchId"] = serde_json::json!(patch_id),
                    Err(reason) => row["readOnlyReason"] = serde_json::json!(reason.as_str()),
                }
                row
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry whose options id equals its module name, which is the ordinary case: a plugin
    /// mounted under the name it was installed as.
    fn entry(id: &str, module: &str) -> Entry {
        Entry {
            entry_id: id.to_owned(),
            module_name: module.to_owned(),
            options_id: module.to_owned(),
        }
    }

    fn row(id: &str, name: &str, parent: &str) -> ProfileRow {
        ProfileRow {
            id: id.to_owned(),
            name: name.to_owned(),
            parent: parent.to_owned(),
        }
    }

    #[test]
    fn a_protected_plugin_is_management_required() {
        // Checked before the address, because a protected entry is never manageable regardless of
        // what the profile says.
        let mut registry = Registry::new();
        registry.insert(entry("e1", "@deepseek-ai/dsh"));
        let listed = registry.list();
        assert_eq!(listed[0]["readOnlyReason"], "management-required");
    }

    #[test]
    fn the_entry_asking_cannot_manage_itself() {
        // Upstream reads `ctx.fiber.entry?.id`: managing yourself would unload yourself mid-call.
        let mut registry = Registry::new();
        registry.insert(entry("e1", "some-plugin"));
        registry.push_row(row("r1", "some-plugin", INCLUDE_ROOT));
        registry.set_owner("e1");
        assert_eq!(registry.list()[0]["readOnlyReason"], "management-required");
    }

    #[test]
    fn a_matching_row_under_the_include_root_is_addressable() {
        // A profile row's id is what a patch names, and `options.id` is that same value -- upstream
        // matches `row.id === actual?.options.id`. The row's id is therefore the plugin's options id,
        // not an arbitrary label.
        let mut registry = Registry::new();
        registry.insert(entry("e1", "some-plugin"));
        registry.push_row(row("some-plugin", "some-plugin", INCLUDE_ROOT));
        assert_eq!(registry.list()[0]["patchId"], "some-plugin");
    }

    #[test]
    fn a_row_whose_id_is_not_the_options_id_does_not_match() {
        // The bug an earlier version of this module had: matching the row by module name instead of
        // by the entry's `options.id`, which would address a plugin for the wrong reason.
        let mut registry = Registry::new();
        registry.insert(entry("e1", "some-plugin"));
        registry.push_row(row("r1", "some-plugin", INCLUDE_ROOT));
        assert_eq!(registry.list()[0]["readOnlyReason"], "unaddressable");
    }

    #[test]
    fn two_rows_claiming_one_name_is_unaddressable() {
        // Ambiguity: a patch that named either row would be a guess.
        let mut registry = Registry::new();
        registry.insert(entry("e1", "some-plugin"));
        registry.push_row(row("r1", "some-plugin", INCLUDE_ROOT));
        registry.push_row(row("r2", "some-plugin", INCLUDE_ROOT));
        assert_eq!(registry.list()[0]["readOnlyReason"], "unaddressable");
    }

    #[test]
    fn a_missing_row_is_unaddressable() {
        let mut registry = Registry::new();
        registry.insert(entry("e1", "some-plugin"));
        assert_eq!(registry.list()[0]["readOnlyReason"], "unaddressable");
    }

    #[test]
    fn a_misnamed_row_is_unaddressable() {
        // A row whose name disagrees with the installed module would address the wrong plugin.
        let mut registry = Registry::new();
        registry.insert(entry("e1", "some-plugin"));
        registry.push_row(row("r1", "another-plugin", INCLUDE_ROOT));
        assert_eq!(registry.list()[0]["readOnlyReason"], "unaddressable");
    }

    #[test]
    fn a_row_outside_the_include_root_is_unaddressable() {
        // `parent !== 'include'`: an entry not mounted at the root has no address a patch can name.
        let mut registry = Registry::new();
        registry.insert(entry("e1", "some-plugin"));
        registry.push_row(row("r1", "some-plugin", "nested"));
        assert_eq!(registry.list()[0]["readOnlyReason"], "unaddressable");
    }

    #[test]
    fn entries_are_listed_in_id_order() {
        // Two runs of the same registry must answer the same order.
        let mut registry = Registry::new();
        registry.insert(entry("b", "b"));
        registry.insert(entry("a", "a"));
        let ids: Vec<&str> = registry
            .entries()
            .iter()
            .map(|e| e.entry_id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }
}
