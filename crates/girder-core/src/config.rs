//! Configuration: entry lists, patch layers, and the `!!js` replacement.
//!
//! Upstream composes a profile from layered YAML. Each bundle ships a `cordis.patch.yml` whose
//! top-level value is a list of patch entries, and [`applyEntryPatches`] overlays them onto the
//! base entry list: a patch either replaces keys on the row matching its `id`, or appends rows
//! through `insert`. Later layers win per key.
//!
//! # The `!!js` problem
//!
//! Upstream patches embed JavaScript in YAML — 61 occurrences across the bundles, of the form
//! `disabled: !!js "!ctx.get('profileContext')"`. That is a general-purpose language evaluated at
//! load time, with access to `process.env`, `process.platform`, and the plugin context. Rust cannot
//! and should not carry it: shipping `!!js` means shipping a sandboxed interpreter and an
//! evaluation-order contract, for expressions that are all of one of four shapes.
//!
//! So the tag is rejected at parse time, and the four shapes get first-class syntax:
//!
//! | upstream | here |
//! |---|---|
//! | `!!js "!ctx.get('profileContext')"` | `when: { service: profileContext }` |
//! | `!!js "process.env.X ?? 'y'"` | `when: { env: X, default: y }` |
//! | `!!js "process.platform === 'darwin'"` | `when: { platform: darwin }` |
//! | `!!js "… ? a : b"` | `when: { …, then: a, else: b }` |
//!
//! A `!!js` tag in any file is an error naming the file and line, not a silently ignored value.
//! That is the whole point: a config that cannot be evaluated must fail loudly at load, because
//! silently treating the expression as false would disable a plugin the user believes is on.
//!
//! # What this module does not do
//!
//! It does not validate a plugin's own config against a schema. Node uses zod/schemastery per
//! plugin; Rust will use `serde` derives on each plugin's config type, which belongs with the
//! plugins rather than here. This module validates the *patch structure* — the part every bundle
//! shares.

use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;

/// A failure raised while loading or applying configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A `!!js` tag was found. Upstream evaluates it; this crate does not.
    #[error(
        "`!!js` is not supported (line {line}); \
         replace it with `when:` — see docs/migration-plan.md §2.6"
    )]
    JsExpression {
        /// 1-based line of the offending tag.
        line: usize,
    },

    /// The YAML could not be parsed.
    #[error("failed to parse YAML: {source}")]
    Yaml {
        /// The parser's error, which carries its own line and column.
        #[from]
        source: serde_norway::Error,
    },

    /// A patch entry was structurally invalid.
    #[error("invalid patch entry {index}: {reason}")]
    InvalidPatch {
        /// 1-based position in the patch list.
        index: usize,
        /// What was wrong.
        reason: String,
    },
}

/// One row of a profile: a plugin instance and its configuration.
///
/// Mirrors the loader's `EntryOptions`. `config` is an arbitrary mapping because each plugin
/// defines its own shape; the patch layer moves it around without interpreting it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// Stable identity. Patches address rows by this, so it must be unique across the tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Module specifier the loader resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Whether the row is skipped. Resolved from `when` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,

    /// When set, this row's `config` is a list of child entries rather than plugin config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<bool>,

    /// Plugin configuration, or the child list for a group row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigValue>,

    /// Services this row requires before it loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject: Option<ConfigValue>,

    /// Intercept config applied to plugins started below this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intercept: Option<ConfigValue>,

    /// Isolation labels for service lookups below this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolate: Option<ConfigValue>,

    /// Conditional activation, replacing the upstream `!!js` on `disabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<When>,
}

/// Plugin config, or a group's child list.
///
/// Kept as a tiny dynamic value rather than `serde_json::Value` so a group's children stay typed
/// and so round-tripping through YAML does not reorder or reformat plugin config.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigValue {
    /// A mapping of plugin settings.
    Map(serde_json::Map<String, serde_json::Value>),
    /// A group's child entries.
    Entries(Vec<Entry>),
    /// Anything else a plugin may accept (scalars, lists).
    Json(serde_json::Value),
}

impl ConfigValue {
    /// Borrows this value as a child entry list, when it is one.
    #[must_use]
    pub fn as_entries(&self) -> Option<&Vec<Entry>> {
        match self {
            Self::Entries(entries) => Some(entries),
            Self::Map(_) | Self::Json(_) => None,
        }
    }

    /// Borrows this value as child entries mutably, when it is one.
    pub fn as_entries_mut(&mut self) -> Option<&mut Vec<Entry>> {
        match self {
            Self::Entries(entries) => Some(entries),
            Self::Map(_) | Self::Json(_) => None,
        }
    }
}

/// An activation condition: the declarative replacement for upstream's `!!js` on `disabled`.
///
/// Every field is optional and they compose with `and`. The value evaluates against an
/// [`Environment`], so the same profile can be loaded against different hosts without rewriting
/// the YAML.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct When {
    /// True when the named service is published. Replaces `!ctx.get('x')` (negated by `not`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,

    /// True when the service is *not* published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_service: Option<String>,

    /// Reads an environment variable. True when it is set and, if `equals` is given, matches it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,

    /// With `env`: the value the variable must equal. Compared after applying `default` when the
    /// variable is absent, which is how `(env ?? 'x') === 'y'` reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,

    /// With `env`: the value used when the variable is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,

    /// Host platform, compared against `std::env::consts::OS` (`linux`, `macos`, `windows`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,

    /// True when the host platform is *not* this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_platform: Option<String>,

    /// Negates the whole condition.
    #[serde(default)]
    pub not: bool,
}

/// What a condition can observe about its host.
///
/// Passed in rather than read from `std::env` inside the evaluator, so a test can state the
/// environment it is testing instead of mutating the process's.
#[derive(Clone, Debug, Default)]
pub struct Environment {
    /// Services currently published, by id.
    pub services: Vec<String>,
    /// Environment variables. Missing entries read as absent.
    pub vars: HashMap<String, String>,
    /// `std::env::consts::OS` for the running host; tests override it.
    pub platform: String,
}

impl Environment {
    /// Builds an environment from the current process.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            services: Vec::new(),
            vars: std::env::vars().collect(),
            platform: std::env::consts::OS.to_owned(),
        }
    }

    /// Records the given services as published.
    #[must_use]
    pub fn with_services(mut self, services: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.services = services.into_iter().map(Into::into).collect();
        self
    }

    /// Sets one environment variable.
    #[must_use]
    pub fn with_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.vars.insert(key.into(), value.into());
        self
    }

    /// Overrides the platform string.
    #[must_use]
    pub fn with_platform(mut self, platform: impl Into<String>) -> Self {
        self.platform = platform.into();
        self
    }

    fn has_service(&self, name: &str) -> bool {
        self.services.iter().any(|service| service == name)
    }
}

impl When {
    /// Evaluates the condition.
    ///
    /// Returns `true` when the row should stay enabled. An unknown service reads as absent and an
    /// unset variable reads as absent, matching what `ctx.get()` returns upstream.
    #[must_use]
    pub fn evaluate(&self, env: &Environment) -> bool {
        let mut result = true;

        if let Some(service) = &self.service {
            result &= env.has_service(service);
        }
        if let Some(service) = &self.not_service {
            result &= !env.has_service(service);
        }
        if let Some(key) = &self.env {
            // `default` stands in for an absent variable, mirroring `??`. Without `equals` the
            // condition only asks whether the variable has a value at all (after the default).
            let actual = env
                .vars
                .get(key)
                .map(String::as_str)
                .or_else(|| self.default.as_ref().and_then(serde_json::Value::as_str));
            result &= match (&self.equals, actual) {
                (Some(expected), Some(actual)) => actual == expected,
                (Some(_), None) => false,
                (None, actual) => actual.is_some(),
            };
        }
        if let Some(platform) = &self.platform {
            result &= env.platform == *platform;
        }
        if let Some(platform) = &self.not_platform {
            result &= env.platform != *platform;
        }
        if self.not {
            result = !result;
        }
        result
    }
}

/// One patch: replace keys on the row with `id`, or append rows through `insert`.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    /// The row to modify. Required unless this patch only inserts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Rows to append to the root list, or to the group named by `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert: Option<Vec<Entry>>,

    /// Guard: a patch whose `name` does not match the target row is skipped with a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Replaces the target's config wholesale, as upstream does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigValue>,

    /// Sets the target's disabled flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,

    /// Marks or unmarks the target as a group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<bool>,
}

/// A warning raised while applying patches.
///
/// Upstream reports these through a loader warning and continues; a patch that targets a row the
/// tree does not have is not fatal, because one overlay is shared across surfaces that do not all
/// contain the same rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchWarning {
    /// What went wrong.
    pub message: String,
    /// The patch's position in the list.
    pub index: usize,
}

impl fmt::Display for PatchWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "patch {}: {}", self.index + 1, self.message)
    }
}

/// Which file or patch layer owns a row, for diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// The base file.
    File,
    /// A patch layer, by index.
    Patch(usize),
    /// An `insert` in a patch layer, by index.
    Insert(usize),
}

/// The composed entry list.
///
/// Rows are addressed by id, including nested group children, so a later layer can patch a row a
/// group inserted.
#[derive(Clone, Debug, Default)]
pub struct EntryList {
    entries: Vec<Entry>,
    /// id → owner, for every row in the tree.
    owners: HashMap<String, Owner>,
}

impl EntryList {
    /// Creates a list from already-parsed rows, recording the base file as their owner.
    #[must_use]
    pub fn new(entries: Vec<Entry>) -> Self {
        let mut list = Self {
            entries,
            owners: HashMap::new(),
        };
        list.reindex();
        list
    }

    /// The rows, groups nested.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Consumes the list, returning the rows.
    #[must_use]
    pub fn into_entries(self) -> Vec<Entry> {
        self.entries
    }

    /// Whether any row carries `id`.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.owners.contains_key(id)
    }

    /// Who owns `id`.
    #[must_use]
    pub fn owner(&self, id: &str) -> Option<Owner> {
        self.owners.get(id).copied()
    }

    /// Finds a row by id, searching nested groups.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Entry> {
        find_entry(&self.entries, id)
    }

    /// Number of rows, counting group children.
    #[must_use]
    pub fn len(&self) -> usize {
        self.owners.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    /// Overlays `patches` in order.
    ///
    /// Returns the warnings for patches that matched nothing, so the caller can report them. A
    /// patch that matches nothing is not an error — see [`PatchWarning`].
    pub fn apply(&mut self, patches: &[Patch], layer_offset: usize) -> Vec<PatchWarning> {
        let mut warnings = Vec::new();

        for (offset, patch) in patches.iter().enumerate() {
            let index = layer_offset + offset;
            if let Some(insert) = &patch.insert {
                self.apply_insert(patch, insert.clone(), index, &mut warnings);
                continue;
            }

            let Some(id) = &patch.id else {
                warnings.push(PatchWarning {
                    message: "`id` is required for a patch that does not insert".to_owned(),
                    index,
                });
                continue;
            };

            let Some(target) = find_entry_mut(&mut self.entries, id) else {
                warnings.push(PatchWarning {
                    message: format!("no row with id `{id}`"),
                    index,
                });
                continue;
            };

            if let Some(expected) = &patch.name {
                if target.name.as_deref() != Some(expected.as_str()) {
                    warnings.push(PatchWarning {
                        message: format!(
                            "name mismatch for `{id}` (expected `{expected}`, found `{:?}`)",
                            target.name
                        ),
                        index,
                    });
                    continue;
                }
            }

            if let Some(config) = &patch.config {
                target.config = Some(config.clone());
            }
            if let Some(disabled) = patch.disabled {
                target.disabled = Some(disabled);
            }
            if let Some(group) = patch.group {
                target.group = Some(group);
            }
            self.owners.insert(id.clone(), Owner::Patch(index));
        }

        self.reindex();
        warnings
    }

    fn apply_insert(
        &mut self,
        patch: &Patch,
        mut rows: Vec<Entry>,
        index: usize,
        warnings: &mut Vec<PatchWarning>,
    ) {
        // Record ownership before moving the rows into the tree, so ids stay attributable.
        let inserted: Vec<String> = rows.iter().filter_map(|row| row.id.clone()).collect();
        match &patch.id {
            None => self.entries.append(&mut rows),
            Some(parent) => {
                let Some(target) = find_entry_mut(&mut self.entries, parent) else {
                    warnings.push(PatchWarning {
                        message: format!("insert target `{parent}` not found"),
                        index,
                    });
                    return;
                };
                if target.group != Some(true) {
                    warnings.push(PatchWarning {
                        message: format!("insert target `{parent}` is not a group"),
                        index,
                    });
                    return;
                }
                // A group's children live in its `config`. A group that has none gets an empty list.
                match &mut target.config {
                    Some(ConfigValue::Entries(children)) => children.append(&mut rows),
                    None => target.config = Some(ConfigValue::Entries(rows)),
                    Some(other) => {
                        warnings.push(PatchWarning {
                            message: format!("insert target `{parent}` has non-list config"),
                            index,
                        });
                        let _ = other;
                    }
                }
            }
        }
        for id in inserted {
            self.owners.insert(id, Owner::Insert(index));
        }
    }

    /// Rebuilds the id index, including nested group children.
    fn reindex(&mut self) {
        let mut owners = HashMap::new();
        collect_owners(&self.entries, &self.owners, &mut owners);
        self.owners = owners;
    }
}

fn collect_owners(
    entries: &[Entry],
    previous: &HashMap<String, Owner>,
    out: &mut HashMap<String, Owner>,
) {
    for entry in entries {
        if let Some(id) = &entry.id {
            let owner = previous.get(id).copied().unwrap_or(Owner::File);
            out.insert(id.clone(), owner);
        }
        if let Some(ConfigValue::Entries(children)) = &entry.config {
            if entry.group == Some(true) {
                collect_owners(children, previous, out);
            }
        }
    }
}

fn find_entry<'a>(entries: &'a [Entry], id: &str) -> Option<&'a Entry> {
    for entry in entries {
        if entry.id.as_deref() == Some(id) {
            return Some(entry);
        }
        if entry.group == Some(true) {
            if let Some(ConfigValue::Entries(children)) = &entry.config {
                if let Some(found) = find_entry(children, id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn find_entry_mut<'a>(entries: &'a mut [Entry], id: &str) -> Option<&'a mut Entry> {
    for entry in entries {
        if entry.id.as_deref() == Some(id) {
            return Some(entry);
        }
        if entry.group == Some(true) {
            if let Some(ConfigValue::Entries(children)) = &mut entry.config {
                if let Some(found) = find_entry_mut(children, id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

// ---- YAML loading -------------------------------------------------------------------------------

impl<'de> Deserialize<'de> for ConfigValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        // A group's children are a YAML list; anything else is plugin config. The two are told
        // apart by shape alone, which is what upstream does too.
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Array(_) => {
                // Only a group's children are entry lists; a plugin's list config (a list of
                // strings, say) must stay JSON. Entries are distinguished by having an `id` or
                // `name`, which a plain scalar list does not.
                let looks_like_entries = match &value {
                    serde_json::Value::Array(items) => items.iter().all(|item| {
                        item.is_object() && (item.get("id").is_some() || item.get("name").is_some())
                    }),
                    _ => false,
                };
                if looks_like_entries {
                    let entries: Vec<Entry> =
                        serde_json::from_value(value).map_err(D::Error::custom)?;
                    Ok(Self::Entries(entries))
                } else {
                    Ok(Self::Json(value))
                }
            }
            serde_json::Value::Object(map) => Ok(Self::Map(map)),
            other => Ok(Self::Json(other)),
        }
    }
}

/// Parses a base entry list from YAML.
///
/// # Errors
///
/// Returns [`ConfigError::JsExpression`] when the document contains a `!!js` tag. The tag is found
/// before parsing, so the error names the line rather than surfacing as a type mismatch.
pub fn parse_entries(yaml: &str) -> Result<Vec<Entry>, ConfigError> {
    reject_js(yaml)?;
    let entries: Vec<Entry> = serde_norway::from_str(yaml)?;
    Ok(entries)
}

/// Parses a patch list from YAML.
///
/// # Errors
///
/// Same as [`parse_entries`].
pub fn parse_patches(yaml: &str) -> Result<Vec<Patch>, ConfigError> {
    reject_js(yaml)?;
    let patches: Vec<Patch> = serde_norway::from_str(yaml)?;
    Ok(patches)
}

/// Scans for a `!!js` tag and reports its line.
///
/// A line scan rather than a parser hook: the parser would have to be given a tag resolver, and a
/// resolver that errors produces a message without a reliable line. Scanning is exact for the one
/// thing being rejected.
fn reject_js(yaml: &str) -> Result<(), ConfigError> {
    for (index, line) in yaml.lines().enumerate() {
        // Ignore occurrences inside a comment, and require the tag to start a value.
        let code = line.split(" #").next().unwrap_or(line);
        let trimmed = code.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        if code.contains("!!js") {
            return Err(ConfigError::JsExpression { line: index + 1 });
        }
    }
    Ok(())
}

// ---- value interpolation ------------------------------------------------------------------------

/// A config value that may contain `${...}` interpolations or be a conditional object.
///
/// Replaces the `!!js` forms that produce *values* rather than booleans. Two shapes are supported,
/// both evaluated at load time so a broken reference fails before any plugin starts:
///
/// - **String interpolation** — `"${env.DSH_HOME:-/tmp}/sessions"`, `"${cwd}"`, `"${home}"`,
///   `"${platform}"`. The `:-` suffix supplies a default when the variable is absent.
/// - **Conditional** — `{ when: {...}, then: <value>, else: <value> }`, resolved against the same
///   [`Environment`] as `when` on an entry.
///
/// A bare string with no `${` is returned unchanged, and so is any scalar that is not a string.
#[derive(Clone, Debug, PartialEq)]
pub struct Template(pub serde_json::Value);

impl Template {
    /// Resolves the template against `env`.
    ///
    /// Interpolations whose variable is absent and has no default resolve to an empty string, which
    /// matches how an unset shell variable expands — and is visible, unlike a silently dropped key.
    #[must_use]
    pub fn resolve(&self, env: &Environment) -> serde_json::Value {
        resolve_value(&self.0, env)
    }
}

fn resolve_value(value: &serde_json::Value, env: &Environment) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(interpolate(text, env)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|item| resolve_value(item, env)).collect())
        }
        serde_json::Value::Object(map) => {
            // A conditional is recognised by its shape: `when` plus `then`.
            if let (Some(when), Some(then)) = (map.get("when"), map.get("then")) {
                if let Ok(condition) = serde_json::from_value::<When>(when.clone()) {
                    let branch = if condition.evaluate(env) {
                        then
                    } else {
                        map.get("else").unwrap_or(&serde_json::Value::Null)
                    };
                    return resolve_value(branch, env);
                }
            }
            serde_json::Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), resolve_value(item, env)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// Expands `${...}` references in `text`.
///
/// Supported forms: `${env.NAME}`, `${env.NAME:-default}`, `${cwd}`, `${home}`, `${platform}`.
/// An unknown form is left verbatim rather than blanked, so a typo is visible in the output instead
/// of silently producing an empty path.
fn interpolate(text: &str, env: &Environment) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Unterminated reference: keep the remainder as-is.
            out.push_str(&rest[start..]);
            return out;
        };
        let reference = &after[..end];
        out.push_str(&expand_reference(reference, env).unwrap_or_else(|| {
            // Unknown reference: preserve it so the mistake reaches the user.
            format!("${{{reference}}}")
        }));
        rest = &after[end + 1..];
    }

    out.push_str(rest);
    out
}

fn expand_reference(reference: &str, env: &Environment) -> Option<String> {
    if let Some(name) = reference.strip_prefix("env.") {
        let (key, default) = match name.split_once(":-") {
            Some((key, default)) => (key, Some(default)),
            None => (name, None),
        };
        return Some(match env.vars.get(key) {
            Some(value) => value.clone(),
            None => default.unwrap_or("").to_owned(),
        });
    }
    match reference {
        "cwd" => std::env::current_dir()
            .ok()
            .map(|path| path.display().to_string()),
        "home" => std::env::var("HOME").ok(),
        "platform" => Some(env.platform.clone()),
        _ => None,
    }
}

// ---- resolving conditions into the row ----------------------------------------------------------

impl Entry {
    /// Whether this row should load, given `env`.
    ///
    /// An explicit `disabled: false` wins over a `when` that would disable the row: an operator who
    /// wrote `disabled: false` means it. Otherwise the condition decides, and a row with no
    /// condition is enabled.
    #[must_use]
    pub fn is_enabled(&self, env: &Environment) -> bool {
        match self.disabled {
            Some(false) => true,
            Some(true) => false,
            None => self
                .when
                .as_ref()
                .is_none_or(|condition| condition.evaluate(env)),
        }
    }
}

impl EntryList {
    /// Ids of the rows that would load, in tree order.
    #[must_use]
    pub fn enabled_ids(&self, env: &Environment) -> Vec<&str> {
        let mut out = Vec::new();
        collect_enabled(&self.entries, env, &mut out);
        out
    }

    /// Collects warnings for rows that would be skipped, naming each one.
    ///
    /// Resolution is deliberately separate from application: the caller decides whether a disabled
    /// row is normal (a bundle that does not apply here) or a mistake (a typo'd service name).
    #[must_use]
    pub fn disabled_ids(&self, env: &Environment) -> Vec<&str> {
        let mut out = Vec::new();
        collect_disabled(&self.entries, env, &mut out);
        out
    }
}

fn collect_enabled<'a>(entries: &'a [Entry], env: &Environment, out: &mut Vec<&'a str>) {
    for entry in entries {
        if entry.is_enabled(env) {
            if let Some(id) = &entry.id {
                out.push(id);
            }
            if entry.group == Some(true) {
                if let Some(ConfigValue::Entries(children)) = &entry.config {
                    collect_enabled(children, env, out);
                }
            }
        }
    }
}

fn collect_disabled<'a>(entries: &'a [Entry], env: &Environment, out: &mut Vec<&'a str>) {
    for entry in entries {
        if entry.is_enabled(env) {
            if entry.group == Some(true) {
                if let Some(ConfigValue::Entries(children)) = &entry.config {
                    collect_disabled(children, env, out);
                }
            }
        } else if let Some(id) = &entry.id {
            out.push(id);
        }
    }
}
