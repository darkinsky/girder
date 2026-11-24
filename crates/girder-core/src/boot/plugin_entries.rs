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

/// Desired enablement, and the file it is read back from after a restart.
struct Enablement {
    /// Explicit choices. An entry that is absent is enabled, which is how a compile-time plugin starts.
    values: BTreeMap<String, bool>,
    /// Where a change is written. Absent in a process that only answers from memory.
    store: Option<std::path::PathBuf>,
    /// Exemptions kept in memory when no profile directory has been named.
    compatibility: crate::boot::install::CompatibilityFile,
}

impl Default for Enablement {
    fn default() -> Self {
        Self {
            values: BTreeMap::new(),
            store: None,
            compatibility: crate::boot::install::CompatibilityFile {
                exemptions: BTreeMap::new(),
                warnings: Vec::new(),
                rewritable: true,
            },
        }
    }
}

/// The entry registry.
#[derive(Default)]
pub struct Registry {
    entries: BTreeMap<String, Entry>,
    rows: Vec<ProfileRow>,
    owner: Option<String>,
    enabled: std::sync::Mutex<Enablement>,
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

    /// Loads enablement from `path` and writes later changes back there.
    ///
    /// A missing file is the first run. A file that is not the map this method writes is ignored,
    /// so a damaged record cannot turn every plugin off.
    pub fn load_enablement(&mut self, path: impl Into<std::path::PathBuf>) {
        let path = path.into();
        let mut state = self
            .enabled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(values) = serde_json::from_slice::<BTreeMap<String, bool>>(&bytes) {
                state.values = values;
            }
        }
        state.store = Some(path);
    }

    /// Records whether `id` should run, and reports the outcome `setPluginEnabled` returns.
    ///
    /// This build has no hot reload, so a change that was saved answers `restart-required`. The
    /// plugin stays mounted until the next start, and [`Self::list`] already reports the saved choice.
    #[must_use]
    pub fn set_plugin_enabled(&self, id: &str, enabled: bool) -> serde_json::Value {
        let Some(entry) = self.entries.get(id).cloned() else {
            return failed_enable(id, enabled, "unknown-plugin");
        };
        if let Err(reason) = self.patch_id(&entry) {
            return failed_enable(id, enabled, reason.as_str());
        }
        let mut state = self
            .enabled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = state.values.get(id).copied().unwrap_or(true);
        let changed = previous != enabled;
        if changed {
            if let Some(path) = state.store.clone() {
                let mut next = state.values.clone();
                next.insert(id.to_owned(), enabled);
                if let Err(error) = write_enablement(&path, &next) {
                    return serde_json::json!({
                        "changed": false,
                        "application": "failed",
                        "stage": "enable",
                        "target": id,
                        "enabled": enabled,
                        "error": { "code": "operation-error", "diagnostic": error },
                    });
                }
            }
            state.values.insert(id.to_owned(), enabled);
        }
        serde_json::json!({
            "changed": changed,
            "application": "restart-required",
            "restartReason": "no-hmr",
            "stage": "enable",
            "target": id,
            "enabled": enabled,
        })
    }

    /// The exemptions `listVersionExemptions` answers.
    ///
    /// A named profile is read from `compatibility.json` beside the enablement file. Without one,
    /// the answer is the grants recorded in this process.
    #[must_use]
    pub fn version_exemptions(&self) -> serde_json::Value {
        let state = self
            .enabled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let file = compatibility_of(&state);
        serde_json::json!({
            "exemptions": file.exemptions,
            "warnings": file.warnings,
        })
    }

    /// Grants or revokes one exact package/runtime exemption, as `setVersionExemption` answers.
    ///
    /// A saved change waits for the next start. The running version is this crate's version, so a
    /// grant for any other runtime is refused rather than recorded.
    #[must_use]
    pub fn set_version_exemption(
        &self,
        package_version: &str,
        runtime_version: &str,
        enabled: bool,
        accept_risk: bool,
    ) -> serde_json::Value {
        let mut state = self
            .enabled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let file = compatibility_of(&state);
        let before = file.exemptions.clone();
        let next = match crate::boot::install::update_profile_compatibility(
            &file,
            package_version,
            runtime_version,
            enabled,
            accept_risk,
            env!("CARGO_PKG_VERSION"),
        ) {
            Ok(next) => next,
            Err(error) => {
                return failed_exemption(package_version, enabled, &error.to_string());
            }
        };
        let changed = next != before;
        if changed {
            if let Err(error) = store_compatibility(&mut state, &next) {
                return failed_exemption(package_version, enabled, &error);
            }
        }
        serde_json::json!({
            "changed": changed,
            "application": "restart-required",
            "restartReason": "no-hmr",
            "stage": "enable",
            "target": package_version,
            "enabled": enabled,
        })
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
                let enabled = self
                    .enabled
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .values
                    .get(&entry.entry_id)
                    .copied()
                    .unwrap_or(true);
                let mut row = serde_json::json!({
                    "entryId": entry.entry_id,
                    "moduleName": entry.module_name,
                    "enabled": enabled,
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

fn compatibility_of(state: &Enablement) -> crate::boot::install::CompatibilityFile {
    match profile_dir(state) {
        Some(dir) => crate::boot::install::read_profile_compatibility(&dir),
        None => state.compatibility.clone(),
    }
}

fn profile_dir(state: &Enablement) -> Option<std::path::PathBuf> {
    state
        .store
        .as_ref()
        .and_then(|path| path.parent())
        .map(std::path::Path::to_path_buf)
        .filter(|dir| !dir.as_os_str().is_empty())
}

fn store_compatibility(
    state: &mut Enablement,
    exemptions: &BTreeMap<String, Vec<String>>,
) -> Result<(), String> {
    if let Some(dir) = profile_dir(state) {
        let path = dir.join(crate::boot::install::PROFILE_COMPATIBILITY_FILENAME);
        let body = crate::boot::install::render_compatibility(exemptions);
        return write_private(&path, body.as_bytes());
    }
    state.compatibility.exemptions = exemptions.clone();
    state.compatibility.warnings.clear();
    state.compatibility.rewritable = true;
    Ok(())
}

fn write_private(path: &std::path::Path, body: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension("json.tmp");
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        use std::io::Write;
        file.write_all(body).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
    }
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

fn failed_exemption(target: &str, enabled: bool, diagnostic: &str) -> serde_json::Value {
    serde_json::json!({
        "changed": false,
        "application": "failed",
        "stage": "enable",
        "target": target,
        "enabled": enabled,
        "error": { "code": "operation-error", "diagnostic": diagnostic },
    })
}

fn failed_enable(id: &str, enabled: bool, code: &str) -> serde_json::Value {
    serde_json::json!({
        "changed": false,
        "application": "failed",
        "stage": "enable",
        "target": id,
        "enabled": enabled,
        "error": { "code": code },
    })
}

fn write_enablement(path: &std::path::Path, values: &BTreeMap<String, bool>) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(values).map_err(|error| error.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, body).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
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

    fn manageable(id: &str) -> Registry {
        let mut registry = Registry::new();
        registry.insert(entry(id, "some-plugin"));
        registry.push_row(row("some-plugin", "some-plugin", INCLUDE_ROOT));
        registry
    }

    #[test]
    fn disabling_a_plugin_is_saved_and_listed() {
        let registry = manageable("e1");
        let first = registry.set_plugin_enabled("e1", false);
        assert_eq!(first["changed"], true);
        assert_eq!(first["application"], "restart-required");
        assert_eq!(first["restartReason"], "no-hmr");
        assert_eq!(first["enabled"], false);
        assert_eq!(registry.list()[0]["enabled"], false);

        let again = registry.set_plugin_enabled("e1", false);
        assert_eq!(again["changed"], false);
        assert_eq!(again["application"], "restart-required");

        let restored = registry.set_plugin_enabled("e1", true);
        assert_eq!(restored["changed"], true);
        assert_eq!(registry.list()[0]["enabled"], true);
    }

    #[test]
    fn an_unknown_or_protected_plugin_is_not_changed() {
        let registry = manageable("e1");
        let missing = registry.set_plugin_enabled("missing", false);
        assert_eq!(missing["application"], "failed");
        assert_eq!(missing["error"]["code"], "unknown-plugin");

        let mut protected = Registry::new();
        protected.insert(entry("core", "@deepseek-ai/dsh"));
        let refused = protected.set_plugin_enabled("core", false);
        assert_eq!(refused["error"]["code"], "management-required");
        assert_eq!(protected.list()[0]["enabled"], true);
    }

    #[test]
    fn enablement_survives_a_new_registry() {
        let dir = std::env::temp_dir().join(format!(
            "girder-plugin-enable-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("plugin-enablement.json");
        let mut registry = manageable("e1");
        registry.load_enablement(&path);
        assert_eq!(registry.set_plugin_enabled("e1", false)["changed"], true);

        let mut restored = manageable("e1");
        restored.load_enablement(&path);
        assert_eq!(restored.list()[0]["enabled"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_version_exemption_needs_consent_and_this_runtime() {
        let runtime = env!("CARGO_PKG_VERSION");
        let registry = Registry::new();
        let refused = registry.set_version_exemption("some-plugin@1.2.3", runtime, true, false);
        assert_eq!(refused["application"], "failed");
        assert!(
            refused["error"]["diagnostic"]
                .as_str()
                .unwrap_or_default()
                .contains("acceptRisk"),
            "{refused}"
        );
        let stale = registry.set_version_exemption("some-plugin@1.2.3", "9.9.9", true, true);
        assert!(
            stale["error"]["diagnostic"]
                .as_str()
                .unwrap_or_default()
                .contains("9.9.9"),
            "{stale}"
        );
        let granted = registry.set_version_exemption("some-plugin@1.2.3", runtime, true, true);
        assert_eq!(granted["changed"], true);
        assert_eq!(granted["application"], "restart-required");
        assert_eq!(granted["restartReason"], "no-hmr");
        assert_eq!(
            registry.version_exemptions()["exemptions"]["some-plugin@1.2.3"][0],
            runtime
        );
        assert_eq!(
            registry.set_version_exemption("some-plugin@1.2.3", runtime, true, true)["changed"],
            false
        );
        assert_eq!(
            registry.set_version_exemption("some-plugin@1.2.3", runtime, false, false)["changed"],
            true
        );
        assert!(
            registry.version_exemptions()["exemptions"]
                .as_object()
                .expect("map")
                .is_empty()
        );
    }

    #[test]
    fn a_damaged_compatibility_file_is_left_alone() {
        let dir = std::env::temp_dir().join(format!(
            "girder-plugin-compat-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let mut registry = Registry::new();
        registry.load_enablement(dir.join("plugin-enablement.json"));
        let path = dir.join("compatibility.json");
        std::fs::write(&path, "{not json").expect("damaged");
        let before = std::fs::read(&path).expect("read");
        let refused = registry.set_version_exemption(
            "some-plugin@1.2.3",
            env!("CARGO_PKG_VERSION"),
            true,
            true,
        );
        assert_eq!(refused["application"], "failed");
        assert_eq!(std::fs::read(&path).expect("unchanged"), before);
        assert!(
            !registry.version_exemptions()["warnings"]
                .as_array()
                .expect("warnings")
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
