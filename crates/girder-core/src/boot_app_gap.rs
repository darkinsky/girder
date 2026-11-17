//! `app-boot`'s launcher decisions — profile composition inputs, the `.env` layers, bundle
//! activation, startup judgement — and `config-editor`'s edit plan, as pure functions.
//!
//! # What upstream `packages/boot/app-boot` is, measured
//!
//! 22 production sources, 5,449 lines. Grouped by the *decision* each file family makes, with the
//! Cordis column read off whether the file hangs work on a `Context` or a `Fiber`:
//!
//! | Family | Files | Lines | Cordis | What it decides |
//! | --- | --- | --- | --- | --- |
//! | launcher glue (`index.ts`) | 1 | 1,286 | yes | `.env` layers, bootpatch loading, config dump, fail-loud guards, `boot()`, startup audit |
//! | Node ESM/CJS interception (`profile-resolution/*`) | 4 | 1,265 | partly | how a bare specifier resolves for plugins; `resolver.ts` alone is 1,072 |
//! | profile discovery and composition (`profile.ts`) | 1 | 762 | no | bundle layers, retired bundles, user patch layer |
//! | config schema projection (`config-schema/*`) | 6 | 1,083 | no | Schemastery graph → JSON Schema, `ajv` validation of defaults |
//! | package metadata (`package-meta.ts`) | 1 | 200 | no | locale display text and icons through the Node resolver |
//! | compatibility preflight (`compatibility-preflight.ts`) | 1 | 187 | yes | deny an incompatible row before the tree mounts |
//! | profile compatibility (`profile-compatibility.ts`) | 1 | 141 | no | the exemption file |
//! | plugin inventory (`profile-plugins.ts`) | 1 | 133 | no | installed dependencies and bundle activation |
//! | plugin compatibility (`plugin-compatibility.ts`) | 1 | 117 | no | `engines.dsh`, else dsh peers |
//! | startup diagnostics (`startup-diagnostics.ts`) | 1 | 104 | yes | the bounded pre-teardown snapshot |
//! | bundle platform (`bundle-platform.ts`) | 1 | 35 | no | host admission |
//! | sanitize (`profile-sanitize.ts`) | 1 | 34 | no | patch backup and bundle rewrite |
//! | profile context (`profile-context.ts`) | 1 | 85 | yes | telemetry patch, layer order |
//!
//! # What this workspace already covers, and where the holes are
//!
//! [`crate::boot_gap`] (2,102 lines) ports the *admission* family: install specs, registries,
//! failure classification, the exemption file (`readProfileCompatibility`,
//! `validatePluginVersionExemption`, `isExactPluginVersion`), semver precedence, and
//! `assertBundlePlatform`/`isWsl`. [`crate::boot_loader_gap`] (1,908) ports the *loader* family:
//! entry trees, patch overlays (`applyPatches`, `PatchIndex`, `routeJournal`), the change journal,
//! `reconcile`, `assignIds`, and the config-file write plan. [`crate::config`] composes layered
//! entry lists. A symbol grep across the workspace finds **no** counterpart for `parseEnv`,
//! `loadLayeredEnv`, `isBootstrapOnly`, `loadOptionalPatches`/`loadOverlayPatches`,
//! `bundlePatchFiles`, `resolveProfileDir`, `initProfile`, `PROFILE_PATCH_FILENAME`,
//! `RETIRED_BUNDLES`/`PROFILE_TEMPLATES`, `resolveTelemetryPatch`, `readProfilePlugins`,
//! `reconcileProfilePlugins`, `auditStartupEntries`, `requiredStartupEntryIds`,
//! `evaluatePluginCompatibility`, `captureStartupComposition`, or for `config-editor`'s edit plan.
//!
//! The largest hole is not a gap in size but in *kind*: `app-boot`'s 5,449 lines are 1,265 lines of
//! Node module interception (`resolver.ts` and its service — patching `Module._resolveFilename`
//! and `module.registerHooks`; **not portable at all**, it is the host's loader), 1,083 lines of
//! schema projection that calls `ajv` and native Schemastery validators, ~700 lines of `Context`
//! plumbing (`boot`, `mountRootInclude`, `installFailLoud`, `captureStartupComposition`,
//! `prepareProfileEntries`), and ~1,900 lines of ordinary decisions: name policy, layer order,
//! bundle lists, dependency activation, and diagnostic text. Those ~1,900 lines are what is
//! portable, and they are the part this module takes.
//!
//! # What is implemented here
//!
//! | Upstream symbol | Here |
//! | --- | --- |
//! | `parseEnv` (`node:util`), used by `readEnvLayer` | [`parse_env`] |
//! | `isBootstrapOnly`, `isHomeLayerRouteName`, `BOOTSTRAP_NAMES`, `BOOTSTRAP_PREFIXES` | [`is_bootstrap_only`], [`is_home_layer_route_name`], [`BOOTSTRAP_NAMES`], [`BOOTSTRAP_PREFIXES`] |
//! | `readEnvLayer`'s rejection | [`check_env_layer`], [`EnvError`], [`EnvErrorKind`] |
//! | `loadLayeredEnv` + `createLaunchEnvironmentSnapshot` | [`layer_env`], [`EnvSnapshot`], [`EnvLayer`], [`EnvSource`] |
//! | `resolveConfigPath` | [`resolve_config_path`] |
//! | `parsePatchList`, `loadOptionalPatches`, `loadOverlayPatches` | [`validate_patch_document`], [`patch_file_policy_for_missing`], [`PatchFilePolicy`] |
//! | `anchorInsertedPluginNames` | [`anchor_inserted_plugin_names`] |
//! | `bundlePatchFiles`, `bundlePatchPaths` | [`bundle_patch_files`], [`bundle_patch_paths`] |
//! | `readProfileManifest`/`writeProfileManifest`'s body | [`manifest_bundles`], [`with_bundles`] |
//! | `PROFILE_TEMPLATES`, `INSTALLATION_OWNED_PROFILE_TUPLES`, `RETIRED_BUNDLES` | [`profile_templates`], [`installation_owned_profile_tuples`], [`retired_bundles`] |
//! | `dropRetiredBundles`, `normalizeShippedProfile` | [`drop_retired_bundles`], [`normalize_shipped_profile`] |
//! | `withBundles` | [`with_bundles`] |
//! | `resolveTelemetryPatch`, the layer order of `readProfilePatches` | [`resolve_telemetry_patch`], [`layer_profile_patches`], [`ProfileLayerKind`] |
//! | `readProfilePlugins`, `reconcileProfilePlugins`, `writeProfileBundles`'s body | [`read_profile_plugins`], [`reconcile_profile_plugins`], [`manifest_bundles`], [`with_bundles`] |
//! | `unmetRequirements`, `evaluatePluginCompatibility`, `pluginCompatibilityWarning` | [`unmet_requirements`], [`evaluate_plugin_compatibility`], [`plugin_compatibility_warning`] |
//! | `requiredStartupEntryIds`, `auditStartupEntries`, `startupDiagnostic`, `activationDiagnostic` | [`REQUIRED_STARTUP_ENTRY_IDS`], [`audit_startup_entries`], [`startup_diagnostic`], [`activation_diagnostic`] |
//! | `captureStartupComposition`'s bound and ordering | [`bound_composition`] |
//! | `ConfigEditor.entries`, `configuration`, `edit`'s document change | [`addressable_rows`], [`configuration_layers`], [`plan_config_edit`], [`EditPlan`] |
//!
//! # What is deliberately absent
//!
//! - **`resolver.ts` (1,072 lines).** It installs an ESM loader hook and rewrites
//!   `Module._resolveFilename`; it is the Node host, not a decision about data. Same for
//!   `installRuntimeInterception`, `loader.resolveSync` in `package-meta.ts`, and the `import()`
//!   pipeline in `collect.ts`.
//! - **`config-schema/*`.** `projector.ts` (455 lines) is a Schemastery-graph walker whose every
//!   interesting branch asks `ajv` whether a default validates (`validator.validate(core,
//!   fallback.value)`), and `pattern.ts` asks `@eslint-community/regexpp` for an AST. Porting the
//!   walker without either dependency would not be the same walker; [`crate::config_schema_gap`]
//!   already owns validation semantics, which is the part a backend needs.
//! - **Everything that touches `Context`/`Fiber`.** `boot`, `mountRootInclude`,
//!   `installFailLoud`, `prepareProfileEntries`, `captureStartupComposition`, `reconcile`.
//!   [`crate::boot`] and [`crate::boot_stages`] are this build's own answer to the same problem in
//!   its own vocabulary, and `captureStartupComposition` reads `ctx.reflect.store` symbols — a
//!   Cordis realm table. What is portable from it is the *bound and the ordering*, which is
//!   [`bound_composition`].
//! - **The file lock and the atomic write.** `config-editor` wraps its edit in
//!   `withFileLock(join(dir, 'package.json'))` and `writeFileAtomic`; both already exist here as
//!   [`crate::atomic_write`]. [`plan_config_edit`] returns the document change; the caller that owns
//!   the profile directory performs it under that lock.
//! - **`!!js` evaluation and YAML.** No entry point here parses or dumps YAML. `config-editor`'s
//!   `visit` rewrites a `{__jsExpr: "..."}` node into a `!!js` scalar on the way out; that is a
//!   writer's job, so [`plan_config_edit`] reports which paths are expression markers
//!   ([`EditPlan::js_paths`]) and the caller tags them. This crate's standing rule is that an
//!   unevaluable config fails loudly ([`crate::config`] rejects the tag at parse time).
//!
//! # Why every entry point takes its facts instead of reading them
//!
//! [`check_env_layer`] takes the file's text and whether it is the Harness home, not `$DSH_HOME`;
//! [`layer_env`] takes the three layers already parsed; [`reconcile_profile_plugins`] takes the
//! inventory before and after the package operation and a platform predicate;
//! [`audit_startup_entries`] takes the inactive rows the loader reported. A decision that reads the
//! world answers differently on a different machine and cannot be tested at all — and each of these
//! decides whether a plugin may run or what a person is told went wrong.
//!
//! # Why the crate root is spelled `girder_core`
//!
//! This module is not declared in `lib.rs` yet — wiring is a separate change. The test compiles this
//! exact source by `#[path]`, and under that inclusion `crate::` names the test binary, so every
//! path out of the module would have to go through the crate root's re-exports. Nothing here needs
//! one: the module imports `std` and `serde_json`, both already dependencies of the crate.

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};

use serde_json::{Map, Value, json};

// ============================================================================================
// `.env` name policy — upstream `index.ts`
// ============================================================================================

/// Exact names no discovered `.env` file may set — upstream `BOOTSTRAP_NAMES`.
///
/// The set is the reason this policy exists: a `.env` travels with a repository, so a clone that
/// could set any of these would choose how the process starts, where its code and instructions load
/// from, or where its requests go.
pub const BOOTSTRAP_NAMES: &[&str] = &[
    // Process launch and module resolution.
    "PATH",
    "HOME",
    "USERPROFILE",
    "SHELL",
    "NODE_OPTIONS",
    "NODE_PATH",
    "NODE_EXTRA_CA_CERTS",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    // Interpreter startup hooks.
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PERL5OPT",
    "PERL5LIB",
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "RUBYOPT",
    "RUBYLIB",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
    "JDK_JAVA_OPTIONS",
    "PYTHONHOME",
    // Version-control hooks, config redirects, and ambient command selectors.
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "GIT_EXTERNAL_DIFF",
    "GIT_PAGER",
    "GIT_EDITOR",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_COUNT",
    "EDITOR",
    "VISUAL",
    "PAGER",
    "BROWSER",
    // Network reach and trust.
    "DEEPSEEK_BASE_URL",
    "DEEPSEEK_SEARCH_BASE_URL",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "NODE_TLS_REJECT_UNAUTHORIZED",
];

/// Name prefixes no discovered `.env` file may set — upstream `BOOTSTRAP_PREFIXES`.
pub const BOOTSTRAP_PREFIXES: &[&str] = &["DSH_", "XDG_", "DYLD_", "BASH_FUNC_"];

/// Proxy names the Harness-home `.env` may set in any letter case — upstream
/// `HOME_LAYER_PROXY_NAMES`. Proxy clients read both spellings, so the rejection matches both.
const HOME_LAYER_PROXY_NAMES: &[&str] = &["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"];

/// `DeepSeek` endpoint names the Harness-home `.env` may set only as spelled here — upstream
/// `HOME_LAYER_ENDPOINT_NAMES`. Their consumers look them up by exact name on POSIX, so another
/// spelling would load without effect.
const HOME_LAYER_ENDPOINT_NAMES: &[&str] = &["DEEPSEEK_BASE_URL", "DEEPSEEK_SEARCH_BASE_URL"];

/// Whether the Harness-home `.env` alone may set a bootstrap-only name — upstream
/// `isHomeLayerRouteName`.
///
/// A proxy counts in any letter case; an endpoint counts only in its exact spelling.
#[must_use]
pub fn is_home_layer_route_name(name: &str) -> bool {
    HOME_LAYER_PROXY_NAMES.contains(&name.to_uppercase().as_str())
        || HOME_LAYER_ENDPOINT_NAMES.contains(&name)
}

/// Whether a variable may come only from the inherited process environment — upstream
/// `isBootstrapOnly`.
///
/// Both the exact names and the prefixes are matched case-insensitively, because Windows folds the
/// case of an environment name and a file that spelled `Path` would otherwise slip through.
#[must_use]
pub fn is_bootstrap_only(name: &str) -> bool {
    let upper = name.to_uppercase();
    BOOTSTRAP_NAMES.contains(&upper.as_str())
        || BOOTSTRAP_PREFIXES
            .iter()
            .any(|prefix| upper.starts_with(prefix))
}

/// One `.env` file's parsed variables, with the path they came from — upstream
/// `readEnvLayer`'s `{ path, values }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvLayer {
    /// The file the values were parsed from.
    pub path: String,
    /// The variables, in declaration order (a later duplicate replaces an earlier one).
    pub values: BTreeMap<String, String>,
}

/// Why a `.env` layer may not be applied — upstream's thrown `Error` inside `readEnvLayer`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvError {
    /// The variable the file was not allowed to set.
    pub name: String,
    /// The file that set it.
    pub path: String,
    /// The remedy clause, after the semicolon.
    pub remedy: String,
    /// Which rule refused it, so a caller can sort a policy refusal from an I/O failure it also
    /// reports through this type.
    pub kind: EnvErrorKind,
}

/// Which rule refused a `.env` layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvErrorKind {
    /// The file set a name only the launching environment may set.
    Refused,
    /// The file could not be read at all.
    Unreadable,
}

impl fmt::Display for EnvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sentence = match self.kind {
            EnvErrorKind::Refused => format!(
                "{} sets \"{}\", which only the launching environment may set (it decides how this \
                 process starts, where its code and instructions load from, or how it reaches the \
                 network); {}",
                self.path, self.name, self.remedy
            ),
            EnvErrorKind::Unreadable => format!("{} cannot be read: {}", self.path, self.remedy),
        };
        write!(formatter, "{sentence}")
    }
}

impl std::error::Error for EnvError {}

/// The remedy clause upstream prints for one refused name.
///
/// Three cases, and they differ because the names differ: a route name set in the user's own home
/// file is allowed, so the remedy is to use that file; an endpoint spelled in another case would
/// load without effect, so the remedy names the exact spelling; every other bootstrap name has only
/// the launching environment.
fn env_remedy(name: &str, home_env: &str) -> String {
    if is_home_layer_route_name(name) {
        return format!(
            "export {name}, or put it in {home_env}, which does not travel with a repository"
        );
    }
    let upper = name.to_uppercase();
    if HOME_LAYER_ENDPOINT_NAMES.contains(&upper.as_str()) {
        return format!(
            "spell it {upper}, then export it or put it in {home_env}, which does not travel with a \
             repository"
        );
    }
    format!("export {name} instead of putting it in a .env file")
}

/// Parse one `.env` document exactly as Node's `util.parseEnv` does.
///
/// The grammar is small and this is the whole of it: carriage returns are removed first (so a CRLF
/// file and a lone CR are both handled); each remaining line is left-trimmed; a blank line or a `#`
/// line is skipped; a leading `export ` is stripped when what remains still names a variable; the
/// key is everything before the first `=` (trimmed, and an empty key skips the line); and the value
/// is either a quoted string — `"…"` to the next `"`, `'…'` to the next `'`, in both cases across
/// line breaks — or the rest of the line up to a `#`, trimmed. Inside double quotes the two-character
/// sequence `\n` becomes a newline and every other backslash sequence is literal; that is Node's
/// whole escape rule, and it is why `"a\\nb"` keeps both backslashes before the newline.
///
/// Two shapes are outside this port, and both are degenerate rather than merely unusual: a line
/// whose value begins with `=` once the name is gone (`=x` with no other `=` on the line) and a
/// `#`-comment line whose `=` appears after a quote character. Upstream's own reader has no
/// documented behaviour for either; every shape a real `.env` contains — the 3,000-document corpus
/// the tests replay, plus this crate's two fuzz corpora — is answered identically to Node
#[must_use]
pub fn parse_env(content: &str) -> BTreeMap<String, String> {
    parse_env_ordered(content).into_iter().collect()
}

/// [`parse_env`], keeping the order the file listed the variables in.
///
/// The order is observable: upstream refuses the *first* forbidden name in a file, so a diagnostic
/// names the variable the author wrote first. A duplicate name keeps its first position and its last
/// value, which is what assigning into a JavaScript object does.
#[must_use]
pub fn parse_env_ordered(content: &str) -> Vec<(String, String)> {
    // Carriage returns go first, so a CRLF file and one with a lone CR both read line by line.
    let document: String = content
        .chars()
        .filter(|character| *character != '\r')
        .collect();
    let mut values: Vec<(String, String)> = Vec::new();
    let mut cursor = 0usize;
    while cursor < document.len() {
        let line_end = document[cursor..]
            .find('\n')
            .map_or(document.len(), |offset| cursor + offset);
        let raw = &document[cursor..line_end];
        let line = raw.trim_start();
        // A quoted value moves the cursor past the line its closing quote is on.
        let consumed = if line.is_empty() || line.starts_with('#') {
            line_end + 1
        } else {
            let start = cursor + (raw.len() - line.len());
            match read_env_assignment(line, start, &document) {
                Some((key, value, resume)) => {
                    match values.iter_mut().find(|(name, _)| *name == key) {
                        Some(slot) => slot.1 = value,
                        None => values.push((key, value)),
                    }
                    resume
                }
                None => line_end + 1,
            }
        };
        cursor = consumed.min(document.len());
    }
    values
}

/// Read one assignment at `start`, returning the key, the value, and where reading resumes.
///
/// The resume position is past the line holding the value's last character: an unquoted value ends
/// at its own line, and a quoted one at the line its closing quote is on. Everything after a closed
/// quote on that line is discarded, which is how Node reads `A="x" junk`.
fn read_env_assignment(
    line: &str,
    start: usize,
    document: &str,
) -> Option<(String, String, usize)> {
    let line_resume = document[start..]
        .find('\n')
        .map_or(document.len(), |offset| start + offset + 1);
    // A name may not be empty, and Node reads a line that begins with `=` by dropping the empty
    // segment: `=a=1` is `a=1`, `=x` is not an assignment at all, and a line of nothing but `=` and
    // whitespace is the one case that keeps the empty name.
    let mut body = line;
    while let Some(rest) = body.strip_prefix('=') {
        let rest = rest.trim_start();
        if rest.is_empty() {
            return Some((String::new(), String::new(), line_resume));
        }
        body = rest;
    }
    let body_start = start + (line.len() - body.len());
    if body.starts_with('#') {
        return None;
    }
    // `export` is a keyword only when a variable follows it: `export =1` names a variable called
    // `export`, so the prefix stays when what follows it has no key.
    let exported = match body.strip_prefix("export ") {
        Some(rest)
            if rest
                .split_once('=')
                .is_some_and(|(key, value)| !key.trim().is_empty() && !value.trim().is_empty()) =>
        {
            rest.trim_start()
        }
        _ => body,
    };
    let body_start = body_start + (body.len() - exported.len());
    let equals = exported.find('=')?;
    let key = exported[..equals].trim();
    if key.is_empty() {
        return None;
    }
    let raw_value = &exported[equals + 1..];
    let value = raw_value.trim_start();
    let value_start = body_start + equals + 1 + (raw_value.len() - value.len());
    // A quoted value runs to the next quote *anywhere in the document*, line breaks included, and
    // everything after the closing quote on that line is discarded.
    for quote in ['"', '\''] {
        if !value.starts_with(quote) {
            continue;
        }
        if let Some(offset) = document[value_start + 1..].find(quote) {
            let close = value_start + 1 + offset;
            let text = &document[value_start + 1..close];
            // Double quotes decode exactly one escape: `\n`. Every other backslash sequence —
            // `\t`, `\r`, `\"`, `\\` — stays literally two characters, Node's whole rule.
            let text = if quote == '"' {
                text.replace("\\n", "\n")
            } else {
                text.to_owned()
            };
            let resume = document[close + 1..]
                .find('\n')
                .map_or(document.len(), |offset| close + 1 + offset + 1);
            return Some((key.to_owned(), text, resume));
        }
        // A quote that never closes is not a quote: the raw text through the end of its line stands,
        // `#` included. Node keeps that text's trailing whitespace unless nothing but whitespace
        // follows on later lines, which is the only sense in which the quote ever "ends".
        let line_end = document[value_start..]
            .find('\n')
            .map_or(document.len(), |offset| value_start + offset);
        let text = &document[value_start..line_end];
        let trailing = document[line_end..].trim().is_empty();
        return Some((
            key.to_owned(),
            if trailing {
                text.trim_end().to_owned()
            } else {
                text.to_owned()
            },
            line_resume,
        ));
    }
    // A value that is *only* whitespace continues onto the next line that holds something: Node
    // reads `B= \nx y` as `B=x y`, and a comment or an empty remainder as an empty value. A value
    // that is truly empty does not continue — `B=` stays empty whatever follows.
    if value.is_empty() && raw_value.is_empty() {
        return Some((key.to_owned(), String::new(), line_resume));
    }
    if value.is_empty() {
        let mut resume = line_resume;
        while resume < document.len() {
            let end = document[resume..]
                .find('\n')
                .map_or(document.len(), |offset| resume + offset);
            let next = document[resume..end].trim();
            if !next.is_empty() {
                if next.starts_with('#') {
                    return Some((key.to_owned(), String::new(), resume));
                }
                let (text, consumed) = next_value(next, resume, document);
                return Some((key.to_owned(), text, consumed));
            }
            resume = (end + 1).min(document.len());
        }
        return Some((key.to_owned(), String::new(), document.len()));
    }
    let text = value.split('#').next().unwrap_or(value).trim_end();
    Some((key.to_owned(), text.to_owned(), line_resume))
}

/// The value a continued line supplies, and where reading resumes after it.
///
/// A continued line is a value, never an assignment, so `A B=1` on it stays those six characters.
/// A quoted run consumes through the line its closing quote is on, which is why `A= \n""B=1`
/// yields an empty `A` and no `B` at all.
fn next_value(line: &str, start: usize, document: &str) -> (String, usize) {
    for quote in ['"', '\''] {
        if !line.starts_with(quote) {
            continue;
        }
        let line_end = document[start..]
            .find('\n')
            .map_or(document.len(), |offset| start + offset + 1);
        if let Some(offset) = document[start + 1..].find(quote) {
            let close = start + 1 + offset;
            let text = &document[start + 1..close];
            let text = if quote == '"' {
                text.replace("\\n", "\n")
            } else {
                text.to_owned()
            };
            let resume = document[close + 1..]
                .find('\n')
                .map_or(document.len(), |offset| close + 1 + offset + 1);
            return (text, resume);
        }
        return (line.to_owned(), line_end);
    }
    let text = line.split('#').next().unwrap_or(line).trim_end().to_owned();
    let line_end = document[start..]
        .find('\n')
        .map_or(document.len(), |offset| start + offset + 1);
    (text, line_end)
}

/// Parse one `.env` layer and refuse any name only the launching environment may set — upstream
/// `readEnvLayer`.
///
/// `home` is the resolved Harness home and `dir` the directory whose `.env` this is; the layer is
/// the home layer when the two name the same directory, and only then may it set a route name.
///
/// # Errors
///
/// Returns [`EnvError`] for the first refused name in declaration order, naming the file, the
/// variable, and the remedy. Validation runs over the parsed entries, so a rejection applies nothing.
pub fn check_env_layer(dir: &str, home: &str, content: &str) -> Result<EnvLayer, EnvError> {
    let normalized = join_path("", dir);
    let home_dir = join_path("", home);
    let is_home = normalized == home_dir;
    let path = join_path(&normalized, ".env");
    let home_env = join_path(&home_dir, ".env");
    let ordered = parse_env_ordered(content);
    for (name, _) in &ordered {
        if !is_bootstrap_only(name) {
            continue;
        }
        if is_home && is_home_layer_route_name(name) {
            continue;
        }
        return Err(EnvError {
            remedy: env_remedy(name, &home_env),
            path,
            name: name.clone(),
            kind: EnvErrorKind::Refused,
        });
    }
    Ok(EnvLayer {
        path,
        values: ordered.into_iter().collect(),
    })
}

/// Which layer supplied a variable — upstream `LaunchEnvironmentSnapshot`'s `source`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EnvSource {
    /// The inherited process environment; outranks every file.
    Process,
    /// The invoking directory's `.env`.
    ProjectEnv,
    /// The Harness home's `.env`.
    UserEnv,
}

/// One variable's value and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvValue {
    /// The value in effect.
    pub value: String,
    /// The layer that supplied it.
    pub source: EnvSource,
    /// The file, for a file layer.
    pub path: Option<String>,
}

/// This run's frozen environment — upstream `createLaunchEnvironmentSnapshot`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvSnapshot {
    /// Every variable the launch saw, by name.
    values: BTreeMap<String, Vec<EnvValue>>,
}

impl EnvSnapshot {
    /// The value in effect for `name`, from the highest-ranked layer that set it.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&EnvValue> {
        self.values.get(name).and_then(|layers| layers.first())
    }

    /// The value from one of `sources`, ignoring the layers above it — upstream `getFrom`.
    ///
    /// This is how a caller asks *whether the file set it*, rather than whether the process ended
    /// up with it.
    #[must_use]
    pub fn get_from(&self, name: &str, sources: &[EnvSource]) -> Option<&EnvValue> {
        self.values
            .get(name)
            .and_then(|layers| layers.iter().find(|layer| sources.contains(&layer.source)))
    }

    /// Every name the snapshot holds.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }
}

/// Layer the inherited environment over the project and Harness-home `.env` files — upstream
/// `loadLayeredEnv`.
///
/// A name already set by a higher-ranked layer keeps that layer's value: the process outranks the
/// invoking directory, which outranks the Harness home. Both files are validated by the caller
/// ([`check_env_layer`]) before either is applied, so a rejection cannot leave one file half
/// applied; `user` is `None` when the home *is* the invoking directory, which upstream expresses by
/// not reading the same file twice.
#[must_use]
pub fn layer_env(
    inherited: &BTreeMap<String, String>,
    project: Option<&EnvLayer>,
    user: Option<&EnvLayer>,
) -> EnvSnapshot {
    let mut snapshot = EnvSnapshot::default();
    for (name, value) in inherited {
        snapshot
            .values
            .entry(name.clone())
            .or_default()
            .push(EnvValue {
                value: value.clone(),
                source: EnvSource::Process,
                path: None,
            });
    }
    for (layer, source) in [(project, EnvSource::ProjectEnv), (user, EnvSource::UserEnv)] {
        let Some(layer) = layer else { continue };
        for (name, value) in &layer.values {
            snapshot
                .values
                .entry(name.clone())
                .or_default()
                .push(EnvValue {
                    value: value.clone(),
                    source,
                    path: Some(layer.path.clone()),
                });
        }
    }
    snapshot
}

// ============================================================================================
// Config path — upstream `resolveConfigPath`
// ============================================================================================

/// The basename a replay run reads instead of the requested one.
const REPLAY_CONFIG_NAME: &str = "cordis.snapshot.yml";

/// Resolve the config to boot — upstream `resolveConfigPath`.
///
/// Replay (`snapshot_mode == "replay"`) swaps a `cordis.yml`/`cordis.yaml` basename for
/// `cordis.snapshot.yml` in the same directory; every other mode, and every other basename, keeps
/// the path. An absolute `config_path` ignores `cwd`, as path resolution itself does.
#[must_use]
pub fn resolve_config_path(config_path: &str, snapshot_mode: Option<&str>, cwd: &str) -> String {
    let absolute = join_path(cwd, config_path);
    if snapshot_mode != Some("replay") {
        return absolute;
    }
    let (directory, base) = split_last(&absolute);
    let replay = match base
        .strip_suffix("cordis.yml")
        .or_else(|| base.strip_suffix("cordis.yaml"))
    {
        Some(stem) => format!("{stem}{REPLAY_CONFIG_NAME}"),
        None => base.to_owned(),
    };
    join_path(&directory, &replay)
}

/// Split a path into its directory (no trailing separator, empty when it is the root) and basename.
fn split_last(path: &str) -> (String, &str) {
    match path.rfind('/') {
        Some(0) => ("/".to_owned(), &path[1..]),
        Some(index) => (path[..index].to_owned(), &path[index + 1..]),
        None => (String::new(), path),
    }
}

/// Join `name` onto `base` and normalise `.`/`..`, as path resolution does — upstream `resolve`.
///
/// An absolute `name` discards `base` entirely, which is the rule that makes a patch file's absolute
/// plugin path independent of where the patch sits. Otherwise `base`'s components come first, then
/// `name`'s, `.` segments vanish, and `..` pops one — or, at the root, vanishes itself, because
/// there is nothing above `/`. The result keeps a leading separator when either input had one.
///
/// Kept here rather than taken from `std::path` because the answer is a diagnostic string in the
/// same spelling as its input: no backslash rewriting, and a Windows-shaped `C:\…` stays one
/// component rather than being split.
#[must_use]
pub fn join_path(base: &str, name: &str) -> String {
    let name_absolute = is_absolute_like(name);
    let rooted = name_absolute || base.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    let inputs: &[&str] = if name_absolute {
        &[name]
    } else {
        &[base, name]
    };
    for input in inputs {
        for part in input.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    if parts.last().is_some_and(|last| *last != "..") {
                        parts.pop();
                    } else if !rooted {
                        parts.push("..");
                    }
                }
                _ => parts.push(part),
            }
        }
    }
    let joined = parts.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

/// Whether a path is absolute in either the POSIX or the Windows spelling.
fn is_absolute_like(path: &str) -> bool {
    if path.starts_with('/') {
        return true;
    }
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

/// Percent-encode a path into a `file:` URL — the shape `pathToFileURL` produces for the paths a
/// patch file may name.
///
/// The safe set is Node's `URL` path codec rather than RFC 3986's unreserved set: `~` is encoded
/// (`%7E`) while the sub-delimiters `!$&'()*+,;=` and `:@` are not. Every other byte is
/// percent-encoded from its UTF-8 form, which is what makes `absolute #100%.mjs` a loadable URL
/// rather than a fragment.
#[must_use]
pub fn file_url(path: &str) -> String {
    const KEEP: &[u8] = b"-._/:@!$&'()*+,;=";
    // A run of leading separators names no deeper root: Node's URL parser collapses it.
    let path = if path.starts_with("//") {
        format!("/{}", path.trim_start_matches('/'))
    } else {
        path.to_owned()
    };
    let mut url = String::from("file://");
    for byte in path.as_bytes() {
        if byte.is_ascii_alphanumeric() || KEEP.contains(byte) {
            url.push(*byte as char);
        } else {
            // `write!` into the string avoids a temporary per encoded byte.
            let _ = write!(url, "%{byte:02X}");
        }
    }
    url
}

// ============================================================================================
// Patch files — upstream `loadOptionalPatches` / `loadOverlayPatches` / `parsePatchList`
// ============================================================================================

/// Whether a patch file's absence is "no layer" or a misconfiguration — the only difference between
/// upstream's `loadOptionalPatches` and `loadOverlayPatches`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchFilePolicy {
    /// A user patch layer: a missing file means no layer.
    Optional,
    /// A bundle's or `--patch` overlay: the caller named this file, so its absence is an error.
    Overlay,
}

impl PatchFilePolicy {
    /// What to call this list in diagnostics.
    fn label(self) -> &'static str {
        match self {
            Self::Optional => "patches",
            Self::Overlay => "overlay",
        }
    }
}

/// The decision for a patch file that could not be read — upstream's two `catch` blocks.
///
/// `io_error` is `None` for `ENOENT`. Returns `None` when there is no layer at all (only the
/// optional policy may answer that) and the diagnostic line otherwise.
#[must_use]
pub fn patch_file_policy_for_missing(
    policy: PatchFilePolicy,
    bin_name: &str,
    file: &str,
    io_error: Option<&str>,
) -> Option<String> {
    match (policy, io_error) {
        (PatchFilePolicy::Optional, None) => None,
        (PatchFilePolicy::Optional, Some(error)) => Some(format!(
            "{bin_name}: failed to read patches {file}: {error}"
        )),
        (PatchFilePolicy::Overlay, error) => Some(format!(
            "{bin_name}: failed to read overlay {file}: {}",
            error.unwrap_or("file does not exist")
        )),
    }
}

/// Validate one parsed patch document — upstream `parsePatchList` after its `yaml.load`.
///
/// A patch list is a top-level array whose every element is a mapping; anything else is a
/// misconfiguration and fails loud, because a layer that cannot apply at all would otherwise be
/// silently skipped. A patch whose *target row* is absent is not this function's business: that
/// stays a per-entry warning, so one overlay shared across surfaces need not match every tree.
///
/// # Errors
///
/// Returns the diagnostic line for a non-array document or a non-mapping element.
pub fn validate_patch_document(
    bin_name: &str,
    file: &str,
    policy: PatchFilePolicy,
    parsed: &Value,
) -> Result<Vec<Value>, String> {
    let label = policy.label();
    let Some(rows) = parsed.as_array() else {
        return Err(format!(
            "{bin_name}: {label} {file} must be a top-level YAML array of loader patch entries"
        ));
    };
    for (index, row) in rows.iter().enumerate() {
        if !row.is_object() {
            return Err(format!(
                "{bin_name}: {label} entry {} in {file} must be a mapping (a loader patch entry)",
                index + 1
            ));
        }
    }
    Ok(rows.clone())
}

/// Convert inserted filesystem paths to file URLs, anchored beside the patch — upstream
/// `anchorInsertedPluginNames`.
///
/// Only `insert` rows are rewritten, and only when their `name` is absolute or starts with `./` or
/// `../` (or is a Windows absolute path). A non-insert patch's `name` is an *assertion* about the
/// row it targets, not a specifier, so rewriting it would make the patch match nothing. Group
/// children are visited because a group's rows are themselves inserted plugins.
///
/// A bare package name (including a `cordis:` builtin or an already-absolute `file:` URL) is left
/// exactly as written.
pub fn anchor_inserted_plugin_names(rows: &mut [Value], file: &str) {
    let (base, _) = split_last(&join_path("", file));
    for patch in rows.iter_mut() {
        let Some(insert) = patch.get_mut("insert").and_then(Value::as_array_mut) else {
            continue;
        };
        for row in insert.iter_mut() {
            anchor_row(row, &base);
        }
    }
}

/// Anchor one inserted row's name, then its group children's.
fn anchor_row(row: &mut Value, base: &str) {
    let Some(object) = row.as_object_mut() else {
        return;
    };
    if let Some(name) = object.get("name").and_then(Value::as_str) {
        let relative = name.starts_with("./") || name.starts_with("../");
        if relative || is_absolute_like(name) {
            let resolved = if is_absolute_like(name) {
                name.to_owned()
            } else {
                join_path(base, name)
            };
            object.insert("name".to_owned(), Value::String(file_url(&resolved)));
        }
    }
    let is_group = object.get("group").is_some_and(Value::is_boolean)
        || object.get("name").and_then(Value::as_str) == Some("cordis:group");
    if !is_group {
        return;
    }
    if let Some(children) = object.get_mut("config").and_then(Value::as_array_mut) {
        for child in children.iter_mut() {
            anchor_row(child, base);
        }
    }
}

// ============================================================================================
// Profile bundles — upstream `profile.ts`
// ============================================================================================

/// The user patch layer's filename inside a profile directory — upstream
/// `PROFILE_PATCH_FILENAME`.
pub const PROFILE_PATCH_FILENAME: &str = "cordis.patch.yml";

/// The directory under the Harness home holding every profile — upstream `PROFILES_DIR`.
pub const PROFILES_DIR: &str = "profiles";

/// The bundles a shipped profile is created with — upstream `PROFILE_TEMPLATES`.
///
/// The order is the application order: `@deepseek-ai/dsh-base` first, so every other bundle patches
/// a tree that already has the base layer.
#[must_use]
pub fn profile_templates() -> BTreeMap<&'static str, Vec<&'static str>> {
    BTreeMap::from([
        (
            "acp",
            vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-acp-app"],
        ),
        (
            "headless",
            vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-headless"],
        ),
        (
            "sdk",
            vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-sdk-app"],
        ),
        ("sdk-minimal", vec!["@deepseek-ai/dsh-sdk-minimal"]),
        (
            "web",
            vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"],
        ),
    ])
}

/// Bundle tuples an earlier release wrote into a shipped profile — upstream
/// `INSTALLATION_OWNED_PROFILE_TUPLES`.
///
/// Only an *exact* match is replaced by the template, so a profile the user edited keeps its list.
#[must_use]
pub fn installation_owned_profile_tuples() -> BTreeMap<&'static str, Vec<&'static str>> {
    BTreeMap::from([(
        "headless",
        vec![
            "@deepseek-ai/dsh-base",
            "@deepseek-ai/dsh-web-app",
            "@deepseek-ai/dsh-headless",
        ],
    )])
}

/// Bundles an earlier release shipped and this installation no longer carries — upstream
/// `RETIRED_BUNDLES`.
#[must_use]
pub fn retired_bundles() -> BTreeSet<&'static str> {
    BTreeSet::from([
        "@deepseek-ai/dsh-experimental-schedule-bundle",
        "@deepseek-ai/dsh-experimental-agent-team-profile",
    ])
}

/// The patch files a bundle declares, in application order — upstream `bundlePatchFiles`.
///
/// `dsh.bundle.patch` is one path or an ordered list of paths. A list is *not* flattened
/// recursively and an entry that is not a string is refused, because a bundle that declared
/// `patch: [1]` would otherwise load no layer at all while appearing to be selected.
///
/// # Errors
///
/// Returns upstream's message for a declaration that is neither a string nor a list of strings.
pub fn bundle_patch_files(bundle: &Value) -> Result<Vec<String>, String> {
    let declared = match bundle.get("patch") {
        Some(Value::String(single)) => return Ok(vec![single.clone()]),
        Some(Value::Array(items)) => items,
        _ => {
            return Err("dsh.bundle.patch must be a file path or a list of file paths".to_owned());
        }
    };
    let mut files = Vec::with_capacity(declared.len());
    for item in declared {
        let Some(file) = item.as_str() else {
            return Err("dsh.bundle.patch must be a file path or a list of file paths".to_owned());
        };
        files.push(file.to_owned());
    }
    Ok(files)
}

/// Resolve a bundle declaration to its ordered absolute patch files — upstream
/// `bundlePatchPaths`.
///
/// # Errors
///
/// Same as [`bundle_patch_files`].
pub fn bundle_patch_paths(package_dir: &str, bundle: &Value) -> Result<Vec<String>, String> {
    Ok(bundle_patch_files(bundle)?
        .iter()
        .map(|file| join_path(package_dir, file))
        .collect())
}

/// Resolve a profile's directory under the Harness home — upstream `resolveProfileDir`.
///
/// The name becomes one path component, so it may not contain a separator, be `.`/`..`, or be
/// `node_modules` (which Node reserves for dependency lookup — a profile named that would have its
/// own `node_modules` looked *through* by every import).
///
/// # Errors
///
/// Returns upstream's message for a name that is not one component.
pub fn resolve_profile_dir(name: &str, home: &str) -> Result<String, String> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name == ".."
        || name == "node_modules"
    {
        return Err(format!("dsh: invalid profile name {name:?}"));
    }
    Ok(join_path(&join_path(home, PROFILES_DIR), name))
}

/// `manifest` with `dsh.profile.bundles` replaced, every other field preserved — upstream
/// `withBundles`.
///
/// Preserved means *not normalised*: a manifest that had no `dsh` section gains one, and a manifest
/// whose other `dsh` keys this build does not understand keeps them.
#[must_use]
pub fn with_bundles(manifest: &Value, bundles: &[String]) -> Value {
    let mut root = manifest.as_object().cloned().unwrap_or_default();
    let mut dsh = root
        .get("dsh")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut profile = dsh
        .get("profile")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    profile.insert(
        "bundles".to_owned(),
        Value::Array(bundles.iter().cloned().map(Value::String).collect()),
    );
    dsh.insert("profile".to_owned(), Value::Object(profile));
    root.insert("dsh".to_owned(), Value::Object(dsh));
    Value::Object(root)
}

/// The `dsh.profile.bundles` list a manifest carries, in order — upstream's
/// `manifest.dsh?.profile?.bundles ?? []`.
#[must_use]
pub fn manifest_bundles(manifest: &Value) -> Vec<String> {
    manifest
        .get("dsh")
        .and_then(|dsh| dsh.get("profile"))
        .and_then(|profile| profile.get("bundles"))
        .and_then(Value::as_array)
        .map(|bundles| {
            bundles
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Remove the [`retired_bundles`] entries from a bundle list — upstream `dropRetiredBundles`.
///
/// Returns `None` when nothing was retired, which is how the caller knows not to rewrite the
/// manifest. Duplicates and order are otherwise preserved: the list is the user's.
#[must_use]
pub fn drop_retired_bundles(bundles: &[String]) -> Option<Vec<String>> {
    let retired = retired_bundles();
    let kept: Vec<String> = bundles
        .iter()
        .filter(|bundle| !retired.contains(bundle.as_str()))
        .cloned()
        .collect();
    (kept.len() != bundles.len()).then_some(kept)
}

/// Replace an exact installation-owned bundle tuple with its shipped template — upstream
/// `normalizeShippedProfile`.
///
/// Returns `None` when the profile is not a shipped name, has no template, carries no bundle list,
/// or its list is not an exact match for the tuple that release wrote. A list the user edited is
/// left alone even when only its order differs.
#[must_use]
pub fn normalize_shipped_profile(name: &str, bundles: &[String]) -> Option<Vec<String>> {
    let templates = profile_templates();
    let template = templates.get(name)?;
    let owned = installation_owned_profile_tuples();
    let retired_tuple = owned.get(name)?;
    let matches = bundles.len() == retired_tuple.len()
        && bundles
            .iter()
            .zip(retired_tuple.iter())
            .all(|(bundle, expected)| bundle == expected);
    if !matches {
        return None;
    }
    Some(template.iter().map(|bundle| (*bundle).to_owned()).collect())
}

// ============================================================================================
// Profile patch layers — upstream `profile-context.ts`
// ============================================================================================

/// Where one patch layer came from, in application order — upstream `readProfilePatches`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProfileLayerKind {
    /// A selected bundle's `cordis.patch.yml`.
    Bundle,
    /// The profile's own patch file.
    Profile,
    /// `$DSH_HOME/cordis.patch.yml`.
    Home,
    /// A launcher layer: a `--patch` file or a flag-derived patch.
    Overlay,
    /// The telemetry opt-out patch, appended last so nothing can re-enable the row.
    Telemetry,
}

/// The id of the telemetry row the opt-out switch disables — upstream `TELEMETRY_ROW_ID`.
pub const TELEMETRY_ROW_ID: &str = "session-telemetry-otel";

/// Resolve the telemetry opt-out switch into its boot patch — upstream `resolveTelemetryPatch`.
///
/// *Any* non-empty value disables, including `0` and `false`: a privacy switch prefers
/// off-by-mistake over on-by-mistake. A composition without the telemetry row exports nothing, so
/// the switch is then trivially satisfied and no patch is generated — a custom profile need not
/// mount telemetry to run with the switch set.
#[must_use]
pub fn resolve_telemetry_patch(disabled_env: Option<&str>, has_row: bool) -> Option<Value> {
    if disabled_env.unwrap_or("").is_empty() || !has_row {
        return None;
    }
    Some(json!({ "id": TELEMETRY_ROW_ID, "disabled": true }))
}

/// Assemble the ordered patch list a launch applies — upstream `readProfilePatches`.
///
/// The order is the whole contract: bundle layers in `dsh.profile.bundles` order, then the
/// profile's own file, then the Harness-home file, then the launcher's overlays, then the telemetry
/// patch. Later layers win per key, so a home file overrides the profile and a `--patch` overrides
/// both.
///
/// `has_telemetry_row` is computed from the composition *before* the telemetry patch is added, and
/// `layers` is taken in call order — each element is one source's patch list.
#[must_use]
pub fn layer_profile_patches(
    layers: &[(ProfileLayerKind, Vec<Value>)],
    telemetry: Option<Value>,
) -> Vec<Value> {
    let mut ordered: Vec<Value> = layers
        .iter()
        .filter(|(kind, _)| *kind != ProfileLayerKind::Telemetry)
        .flat_map(|(_, patches)| patches.iter().cloned())
        .collect();
    if let Some(patch) = telemetry {
        ordered.push(patch);
    }
    ordered
}

// ============================================================================================
// Profile plugin inventory — upstream `profile-plugins.ts`
// ============================================================================================

/// One dependency under its package-manager key — upstream `ProfilePluginDependency`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfilePluginDependency {
    /// The dependency key (an npm alias keeps its own key).
    pub name: String,
    /// The installed version, or the declared spec when no installed metadata is readable.
    pub version: String,
    /// Whether the package the bundle resolver selects declares `dsh.bundle.patch`.
    pub bundle: bool,
    /// Whether the dependency appears in the profile's active bundle list.
    pub enabled: bool,
}

/// One profile's dependencies and bundle list — upstream `ProfilePluginInventory`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfilePluginInventory {
    /// The profile manifest as read.
    pub manifest: Value,
    /// Dependencies in `package.json` order.
    pub dependencies: Vec<ProfilePluginDependency>,
}

/// The outcome of reconciling bundle activation — upstream `ProfilePluginReconciliation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfilePluginReconciliation {
    /// The inventory after reconciliation, with `enabled` recomputed.
    pub plugins: ProfilePluginInventory,
    /// Newly added ordinary (non-bundle) dependencies, for caller-owned diagnostics.
    pub added_plain_dependencies: Vec<String>,
}

/// Read installed versions and bundle declarations — upstream `readProfilePlugins`'s pure half.
///
/// `dependencies` is the manifest's `dependencies` in *document* order, as `(key, spec)` pairs: the
/// key is the package-manager key, which an npm alias keeps (`alias: file:../pkg`), and upstream
/// lists them in the order `package.json` wrote them. `installed_version` answers one dependency's
/// installed `version` (or `None` when its metadata is missing or malformed) and `declares_bundle`
/// answers whether the resolved package declares `dsh.bundle.patch`. An unreadable installed
/// manifest leaves the dependency visible with its declared spec, so a broken package can still be
/// repaired or removed — and `spec` is what that fallback prints, not the installed metadata.
#[must_use]
pub fn read_profile_plugins(
    manifest: &Value,
    dependencies: &[(String, String)],
    installed_version: &dyn Fn(&str) -> Option<String>,
    declares_bundle: &dyn Fn(&str) -> bool,
) -> ProfilePluginInventory {
    let bundles = manifest_bundles(manifest);
    let dependencies = dependencies
        .iter()
        .map(|(name, spec)| ProfilePluginDependency {
            name: name.clone(),
            version: installed_version(name).unwrap_or_else(|| spec.clone()),
            bundle: declares_bundle(name),
            enabled: bundles.contains(name),
        })
        .collect();
    ProfilePluginInventory {
        manifest: manifest.clone(),
        dependencies,
    }
}

/// Reconcile bundle activation after a successful package operation — upstream
/// `reconcileProfilePlugins`.
///
/// The rule, which is the whole function: a bundle entry the profile listed *before* or *after* is
/// dropped when its package no longer declares a bundle (it was removed, or it stopped being one);
/// every entry the profile did not list is kept, so template entries and hand-written rows survive
/// in order and duplicates and all. Then each installed bundle that is not explicitly disabled is
/// appended, after its platform declaration admits the host.
///
/// `preserve_disabled` keeps explicitly disabled bundles disabled: they stay out of the list they
/// were already out of. Everything else — the manifest read, the dependency walk, the write — stays
/// with the caller.
///
/// # Errors
///
/// Returns the platform admission diagnostic for a bundle that would be activated on a host it
/// excludes. The selection is then not written at all: installed dependencies remain, no bundle
/// selection changes are.
pub fn reconcile_profile_plugins(
    before: &ProfilePluginInventory,
    after: &ProfilePluginInventory,
    preserve_disabled: bool,
    assert_platform: &dyn Fn(&str) -> Result<(), String>,
) -> Result<ProfilePluginReconciliation, String> {
    let before_names: BTreeSet<&str> = before
        .dependencies
        .iter()
        .map(|dependency| dependency.name.as_str())
        .collect();
    let after_names: BTreeSet<&str> = after
        .dependencies
        .iter()
        .map(|dependency| dependency.name.as_str())
        .collect();
    let bundle_names: BTreeSet<&str> = after
        .dependencies
        .iter()
        .filter(|dependency| dependency.bundle)
        .map(|dependency| dependency.name.as_str())
        .collect();
    let disabled: BTreeSet<&str> = if preserve_disabled {
        before
            .dependencies
            .iter()
            .filter(|dependency| dependency.bundle && !dependency.enabled)
            .map(|dependency| dependency.name.as_str())
            .collect()
    } else {
        BTreeSet::new()
    };
    let previous = manifest_bundles(&after.manifest);
    let mut bundles: Vec<String> = previous
        .iter()
        .filter(|name| {
            !(before_names.contains(name.as_str()) || after_names.contains(name.as_str()))
                || bundle_names.contains(name.as_str())
        })
        .cloned()
        .collect();
    for dependency in &after.dependencies {
        if !dependency.bundle
            || disabled.contains(dependency.name.as_str())
            || bundles.contains(&dependency.name)
        {
            continue;
        }
        assert_platform(&dependency.name)?;
        bundles.push(dependency.name.clone());
    }
    let changed = bundles != previous;
    let manifest = if changed {
        with_bundles(&after.manifest, &bundles)
    } else {
        after.manifest.clone()
    };
    let enabled: BTreeSet<&str> = bundles.iter().map(String::as_str).collect();
    Ok(ProfilePluginReconciliation {
        plugins: ProfilePluginInventory {
            manifest,
            dependencies: after
                .dependencies
                .iter()
                .map(|dependency| ProfilePluginDependency {
                    enabled: enabled.contains(dependency.name.as_str()),
                    ..dependency.clone()
                })
                .collect(),
        },
        added_plain_dependencies: after
            .dependencies
            .iter()
            .filter(|dependency| {
                !dependency.bundle && !before_names.contains(dependency.name.as_str())
            })
            .map(|dependency| dependency.name.clone())
            .collect(),
    })
}

// ============================================================================================
// Plugin compatibility — upstream `plugin-compatibility.ts`
// ============================================================================================

/// Unmet dsh requirements and the exact plugin/runtime exemption decision — upstream
/// `PluginCompatibility`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginCompatibility {
    /// The plugin's package name.
    pub name: String,
    /// The plugin's exact version.
    pub version: String,
    /// The running dsh version.
    pub runtime_version: String,
    /// Only the requirements the running version does not satisfy: `engines.dsh`, or dsh peer names.
    pub requirements: BTreeMap<String, String>,
    /// Whether the profile exempts this exact plugin version on this exact runtime.
    pub exempted: bool,
}

/// The `@deepseek-ai/dsh` peer range spellings that mean "the running runtime" — upstream's
/// `workspace:` list for peers.
const WORKSPACE_RANGES: &[&str] = &["workspace:^", "workspace:~", "workspace:*"];

/// Whether a requirement is met — upstream `plugin-compatibility.ts`'s `satisfies` wrapper.
///
/// The blank check is upstream's own, and it is not redundant: a range evaluator is entitled to
/// read `""` as `*` (an unconstrained range), and an empty requirement in a manifest is a mistake,
/// not a promise. So a blank or whitespace-only range is unmet whatever the evaluator thinks.
fn requirement_met(
    runtime_version: &str,
    range: &str,
    satisfies: &dyn Fn(&str, &str) -> bool,
) -> bool {
    !range.trim().is_empty() && satisfies(runtime_version, range)
}

/// Whether a manifest object field is a JSON object — upstream `objectOf`.
fn object_of<'a>(value: &'a Value, field: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{field} must be an object"))
}

/// Read one identity field that must be a non-empty string — upstream `identityField`.
fn identity_field(manifest: &Map<String, Value>, field: &str) -> Result<String, String> {
    match manifest.get(field).and_then(Value::as_str) {
        Some(value) if !value.trim().is_empty() => Ok(value.to_owned()),
        _ => Err(format!(
            "Plugin manifest {field} must be a non-empty string when dsh requirements are unmet"
        )),
    }
}

/// Whether a peer package name is a DSH package — upstream's `name !== '@deepseek-ai/dsh' &&
/// !name.startsWith('@deepseek-ai/dsh-')` filter.
fn is_dsh_package(name: &str) -> bool {
    name == "@deepseek-ai/dsh" || name.starts_with("@deepseek-ai/dsh-")
}

/// The requirements a runtime does not satisfy — upstream `unmetRequirements`.
///
/// A plugin's own `engines.dsh` range decides alone and its peer dependencies are then ignored;
/// otherwise every `@deepseek-ai/dsh` / `@deepseek-ai/dsh-*` peer must match. For peers only,
/// `workspace:^`, `workspace:~`, and `workspace:*` refer to the current runtime; any other empty or
/// invalid range — and any `workspace:` spelling in `engines.dsh` — stays unmet.
///
/// `satisfies` is the range evaluator, taken as a parameter because it is a semver grammar and not
/// a decision about this file; a caller that has one (whether `node-semver`-compatible like
/// [`crate::market_compat_gap`]'s or its own) supplies it, and one that does not cannot silently
/// answer "compatible".
///
/// # Errors
///
/// Returns upstream's message for a non-object `engines`/`peerDependencies`, a non-string range, or
/// a non-string `engines.dsh`.
pub fn unmet_requirements(
    manifest: &Value,
    runtime_version: &str,
    satisfies: &dyn Fn(&str, &str) -> bool,
) -> Result<BTreeMap<String, String>, String> {
    let fields = object_of(manifest, "Plugin manifest")?;
    let engines = match fields.get("engines") {
        Some(engines) => object_of(engines, "Plugin manifest engines")?,
        None => return unmet_peers(fields, runtime_version, satisfies),
    };
    if let Some(range) = engines.get("dsh") {
        let Some(range) = range.as_str() else {
            return Err("Plugin manifest engines.dsh must be a string".to_owned());
        };
        if requirement_met(runtime_version, range, satisfies) {
            return Ok(BTreeMap::new());
        }
        return Ok(BTreeMap::from([(
            "engines.dsh".to_owned(),
            range.to_owned(),
        )]));
    }
    unmet_peers(fields, runtime_version, satisfies)
}

/// The peer-dependency branch of [`unmet_requirements`].
fn unmet_peers(
    fields: &Map<String, Value>,
    runtime_version: &str,
    satisfies: &dyn Fn(&str, &str) -> bool,
) -> Result<BTreeMap<String, String>, String> {
    let Some(peers) = fields.get("peerDependencies") else {
        return Ok(BTreeMap::new());
    };
    let peers = object_of(peers, "Plugin manifest peerDependencies")?;
    let mut unmet = BTreeMap::new();
    for (name, range) in peers {
        let Some(range) = range.as_str() else {
            return Err(format!(
                "Plugin manifest peerDependencies[{name:?}] must be a string"
            ));
        };
        if !is_dsh_package(name) {
            continue;
        }
        let requirement = if WORKSPACE_RANGES.contains(&range) {
            runtime_version
        } else {
            range
        };
        if !requirement_met(runtime_version, requirement, satisfies) {
            unmet.insert(name.clone(), range.to_owned());
        }
    }
    Ok(unmet)
}

/// Check a plugin's dsh requirements against the runtime — upstream
/// `evaluatePluginCompatibility`.
///
/// Returns `None` when every requirement is met, and the unmet set (with the exemption verdict)
/// otherwise. `exemptions` maps `name@version` keys to the exact DSH versions the profile accepted.
/// An exempted plugin is still reported: consent records the risk, it does not make the range met.
///
/// # Errors
///
/// Returns upstream's message for a malformed manifest, or for a missing identity when the
/// requirements are unmet — a refused-and-unnameable plugin must not be reported as a package with
/// no name.
pub fn evaluate_plugin_compatibility(
    manifest: &Value,
    exemptions: &BTreeMap<String, Vec<String>>,
    runtime_version: &str,
    satisfies: &dyn Fn(&str, &str) -> bool,
) -> Result<Option<PluginCompatibility>, String> {
    if !is_semantic_version(runtime_version) {
        return Err(format!(
            "Invalid dsh runtime version: {runtime_version:?}; expected a semantic version"
        ));
    }
    let fields = object_of(manifest, "Plugin manifest")?;
    let requirements = unmet_requirements(manifest, runtime_version, satisfies)?;
    if requirements.is_empty() {
        return Ok(None);
    }
    let name = identity_field(fields, "name")?;
    let version = identity_field(fields, "version")?;
    let key = format!("{name}@{version}");
    let exempted = exemptions
        .get(&key)
        .is_some_and(|versions| versions.iter().any(|value| value == runtime_version));
    Ok(Some(PluginCompatibility {
        name,
        version,
        runtime_version: runtime_version.to_owned(),
        requirements,
        exempted,
    }))
}

/// The longest string `node-semver` will parse (`MAX_LENGTH`); anything longer is invalid.
const SEMVER_MAX_LENGTH: usize = 256;

/// The largest value a numeric component may take (`Number.MAX_SAFE_INTEGER`).
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Whether `value` is an exact semantic version, in the sense `semver.valid` means it.
///
/// Only validity is reported: `semver.valid` also *returns* the version with its build metadata
/// stripped, and the one caller — a runtime version check — only asks whether the answer is null.
/// The grammar is the strict one: an optional leading `v`, surrounding whitespace allowed, three
/// numeric components with no leading zeros, each within `Number.MAX_SAFE_INTEGER`, then optional
/// `.`-separated prerelease identifiers (numeric ones without leading zeros) and `.`-separated
/// build metadata. `1.2`, `01.2.3`, `1.2.3-01`, `=1.2.3`, and a 300-character string are all
/// invalid, which is upstream's answer for each.
#[must_use]
pub fn is_semantic_version(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > SEMVER_MAX_LENGTH {
        return false;
    }
    let trimmed = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let without_build = match trimmed.split_once('+') {
        Some((core, build)) => {
            if !build.split('.').all(is_build_identifier) {
                return false;
            }
            core
        }
        None => trimmed,
    };
    let (core, prerelease) = match without_build.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (without_build, None),
    };
    let mut components = core.split('.');
    let numeric = (0..3).all(|_| components.next().is_some_and(is_numeric_component));
    if !numeric || components.next().is_some() {
        return false;
    }
    match prerelease {
        None => true,
        Some(pre) => !pre.is_empty() && pre.split('.').all(is_prerelease_identifier),
    }
}

/// A numeric version component: digits only, no leading zero, within `MAX_SAFE_INTEGER`.
fn is_numeric_component(value: &str) -> bool {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    if value.len() > 1 && value.starts_with('0') {
        return false;
    }
    value
        .parse::<u64>()
        .is_ok_and(|number| number <= MAX_SAFE_INTEGER)
}

/// A prerelease identifier: alphanumerics and hyphens, and a numeric one carries no leading zero.
fn is_prerelease_identifier(value: &str) -> bool {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return false;
    }
    let numeric = value.bytes().all(|byte| byte.is_ascii_digit());
    !(numeric && value.len() > 1 && value.starts_with('0'))
}

/// A build-metadata identifier: alphanumerics and hyphens, with no leading-zero rule.
fn is_build_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Describe unmet requirements, their risk, and the exact-version remedy — upstream
/// `pluginCompatibilityWarning`.
///
/// The text is kept word for word: it is what a person reads before an install, and it names the
/// command that records consent.
#[must_use]
pub fn plugin_compatibility_warning(issue: &PluginCompatibility) -> String {
    let key = format!("{}@{}", issue.name, issue.version);
    let requirements = Value::Object(
        issue
            .requirements
            .iter()
            .map(|(name, range)| (name.clone(), Value::String(range.clone())))
            .collect(),
    );
    let requirements = serde_json::to_string(&requirements).unwrap_or_default();
    format!(
        "Plugin {key} is incompatible with dsh {}: requirements {requirements}. Running it may \
         cause crashes or data loss. Update the plugin or install a plugin version compatible with \
         this dsh runtime. To accept this risk explicitly, grant the exact-version exemption for \
         {key} on dsh {} with `dsh plugin allow-version` or the plugin manager, then retry the \
         installation or restart dsh. Exact-version exemption: {}.",
        issue.runtime_version,
        issue.runtime_version,
        if issue.exempted {
            "active"
        } else {
            "not active"
        }
    )
}

// ============================================================================================
// Startup judgement — upstream `index.ts` `auditStartupEntries`
// ============================================================================================

/// Entry ids whose presence defines a usable DSH application — upstream
/// `requiredStartupEntryIds`.
///
/// The list is global rather than profile metadata. A missing or disabled id does not affect
/// startup; an *enabled* listed entry must activate.
pub const REQUIRED_STARTUP_ENTRY_IDS: &[&str] = &[
    "agent-loop",
    "tool-executor-native",
    "tool-executor-ptc",
    "webserver",
    "web-runtime",
    "modules",
    "connection",
    "headless-runner",
    "acp",
    "sdk-jsonrpc-server",
];

/// Why an enabled entry did not activate — upstream `InactiveEntry['outcome']`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InactiveOutcome {
    /// The entry raised, or its import failed, or its disabled expression threw.
    Failed {
        /// The activation phase, when the loader reported one (`failed to import`, `disabled
        /// expression failed`).
        phase: Option<String>,
        /// The rendered error detail.
        detail: String,
    },
    /// The entry is parked waiting for services that no provider registered.
    Pending {
        /// The missing service names, in injection order.
        missing: Vec<String>,
    },
}

/// One enabled entry that did not activate — upstream's `InactiveEntry`, without its `Entry`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InactiveEntry {
    /// The entry id.
    pub id: String,
    /// The plugin module specifier.
    pub module: String,
    /// Whether this entry is the bootstrap include or a globally required id.
    pub required: bool,
    /// What went wrong.
    pub outcome: InactiveOutcome,
}

/// One line for an inactive entry — upstream `inactiveDiagnostic`.
///
/// The shape is `id (module): detail`, and the detail is a pending clause or the failure's phase and
/// error. `stacks` says whether the failure text carries stack frames: the reload text that people
/// read in the UI does not.
#[must_use]
pub fn inactive_diagnostic(entry: &InactiveEntry) -> String {
    let detail = match &entry.outcome {
        InactiveOutcome::Failed { phase, detail } => match phase {
            Some(phase) => format!("{phase}: {detail}"),
            None => detail.clone(),
        },
        InactiveOutcome::Pending { missing } => format!(
            "pending (waiting for {}: {})",
            if missing.len() == 1 {
                "service"
            } else {
                "services"
            },
            if missing.is_empty() {
                "unknown".to_owned()
            } else {
                missing.join(", ")
            }
        ),
    };
    format!("{} ({}): {detail}", entry.id, entry.module)
}

/// The optional-only warning — upstream `activationDiagnostic`.
///
/// One line of count, then one line per inactive entry. The trailing newline is part of the shape:
/// the caller writes this as one message.
#[must_use]
pub fn activation_diagnostic(bin_name: &str, failures: &[InactiveEntry]) -> String {
    let noun = if failures.len() == 1 {
        "entry"
    } else {
        "entries"
    };
    let mut text = format!(
        "{bin_name}: warning: {} {noun} did not activate\n",
        failures.len()
    );
    for failure in failures {
        text.push_str(&inactive_diagnostic(failure));
        text.push('\n');
    }
    text
}

/// The fatal startup message — upstream `startupDiagnostic`.
///
/// Failed plugins come first with their package and indented detail; plugins waiting for services
/// follow in a two-column table whose first column is padded to the longest label. Required entries
/// are marked `(required)` and sort first among the waiters, because that is the line a person needs
/// when startup says a required plugin is missing. Both sections are omitted when empty.
#[must_use]
pub fn startup_diagnostic(bin_name: &str, failures: &[InactiveEntry]) -> String {
    let required = failures.iter().filter(|entry| entry.required).count();
    let mut lines = vec![format!(
        "{bin_name}: startup failed: {required} required {} did not activate",
        if required == 1 { "plugin" } else { "plugins" }
    )];
    let failed: Vec<&InactiveEntry> = failures
        .iter()
        .filter(|entry| matches!(entry.outcome, InactiveOutcome::Failed { .. }))
        .collect();
    let mut pending: Vec<&InactiveEntry> = failures
        .iter()
        .filter(|entry| matches!(entry.outcome, InactiveOutcome::Pending { .. }))
        .collect();
    pending.sort_by_key(|entry| !entry.required);
    let label = |entry: &InactiveEntry| {
        if entry.required {
            format!("{} (required)", entry.id)
        } else {
            entry.id.clone()
        }
    };
    if !failed.is_empty() {
        lines.push(String::new());
        lines.push(format!("Failed plugins ({}):", failed.len()));
        for entry in failed {
            lines.push(format!("  {}", label(entry)));
            lines.push(format!("    Package: {}", entry.module));
            if let InactiveOutcome::Failed { phase, detail } = &entry.outcome {
                let detail = match phase {
                    Some(phase) => format!("{phase}: {detail}"),
                    None => detail.clone(),
                };
                for line in detail.split('\n') {
                    lines.push(format!("    {line}"));
                }
            }
        }
    }
    if !pending.is_empty() {
        let width = pending
            .iter()
            .map(|entry| label(entry).chars().count())
            .max()
            .unwrap_or("Plugin".len())
            .max("Plugin".len())
            + 2;
        lines.push(String::new());
        lines.push(format!("Plugins waiting for services ({}):", pending.len()));
        lines.push(format!(
            "  {:<width$}Missing services",
            "Plugin",
            width = width
        ));
        for entry in pending {
            let missing = match &entry.outcome {
                InactiveOutcome::Pending { missing } if !missing.is_empty() => missing.join(", "),
                _ => "unknown".to_owned(),
            };
            lines.push(format!(
                "  {:<width$}{missing}",
                label(entry),
                width = width
            ));
        }
    }
    lines.join("\n")
}

/// The audit's verdict — upstream `auditStartupEntries` returning or throwing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupVerdict {
    /// Present when a required entry (or the bootstrap include) did not activate.
    pub error: Option<String>,
    /// Present when only optional entries did not activate.
    pub warning: Option<String>,
}

/// Judge a settled loader tree — upstream `auditStartupEntries`.
///
/// A required entry — the bootstrap include, which must activate so an unreadable root config is
/// fatal, or one of [`REQUIRED_STARTUP_ENTRY_IDS`] — that did not activate fails startup, and the
/// message carries every inactive entry so the optional ones are visible next to the fatal one.
/// When no required entry failed, the same list becomes one warning and startup continues.
/// `failures` is expected in the loader's own order, which is what upstream renders.
#[must_use]
pub fn audit_startup_entries(bin_name: &str, failures: &[InactiveEntry]) -> StartupVerdict {
    let required = failures.iter().any(|entry| entry.required);
    if required {
        StartupVerdict {
            error: Some(startup_diagnostic(bin_name, failures)),
            warning: None,
        }
    } else if failures.is_empty() {
        StartupVerdict {
            error: None,
            warning: None,
        }
    } else {
        StartupVerdict {
            error: None,
            warning: Some(activation_diagnostic(bin_name, failures)),
        }
    }
}

/// The bound upstream's composition snapshot keeps — `captureStartupComposition`'s row limits.
pub const COMPOSITION_LIMIT: usize = 256;

/// A bounded pre-teardown snapshot — upstream `StartupCompositionDiagnostic`'s plugin half.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Composition {
    /// At most [`COMPOSITION_LIMIT`] rows, inactive ones first.
    pub plugins: Vec<InactiveEntry>,
    /// How many rows were dropped by the bound.
    pub omitted_plugins: usize,
}

/// Bound and order a composition snapshot — upstream `captureStartupComposition`.
///
/// Inactive rows precede healthy ones (`Number(a.state === 'active') - Number(b.state === 'active')`),
/// so a bounded report always retains the failure context; the sort is stable, so the loader's own
/// order survives inside each half. `healthy` are the entries that did activate and carry no
/// failure; they are kept in the report only as trailing context.
#[must_use]
pub fn bound_composition(failures: Vec<InactiveEntry>, healthy: Vec<InactiveEntry>) -> Composition {
    let total = failures.len() + healthy.len();
    let mut rows = failures;
    rows.extend(healthy);
    rows.truncate(COMPOSITION_LIMIT);
    Composition {
        omitted_plugins: total.saturating_sub(rows.len()),
        plugins: rows,
    }
}

// ============================================================================================
// `config-editor` — the edit plan of a profile patch document
// ============================================================================================

/// The ids of the rows a caller may address — upstream `ConfigEditor.entries`.
///
/// A row is addressable when its `id` appears exactly once among the profile's entries: two rows
/// sharing an id make a patch's target ambiguous, and the loader's id index would resolve the last
/// one, so the editor refuses to offer either. Rows are returned by position, in document order.
#[must_use]
pub fn addressable_rows(rows: &[Value]) -> Vec<usize> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for row in rows {
        if let Some(id) = row.get("id").and_then(Value::as_str) {
            *counts.entry(id).or_default() += 1;
        }
    }
    rows.iter()
        .enumerate()
        .filter(|(_, row)| {
            row.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| counts.get(id) == Some(&1))
        })
        .map(|(index, _)| index)
        .collect()
}

/// The inherited and explicit values of one addressable row — upstream
/// `configuration()`'s per-entry result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigurationLayers {
    /// The position of the row in the document.
    pub index: usize,
    /// The row's id.
    pub id: String,
    /// What the composed lower layers supply.
    pub inherited: Value,
    /// What the profile's own patch says.
    pub r#override: Value,
}

/// Inherited and explicit values for each addressable row — upstream `configuration()`.
///
/// `inherited` is what the layers produce with the profile's own `config` keys removed — upstream's
/// `inherited()` rebuilds exactly that by deleting `config` from each patch row and recomposing. The
/// distinction matters because a profile row that carries its own `config` *replaces* the inherited
/// one rather than merging with it, so "what would this row be without my edit" is not the composed
/// value minus my keys. The composed value in effect is the live loader entry's own config, which
/// the caller already holds, so it is not repeated here.
///
/// A patch row with `insert` present is an insertion and never an override — that is the guard on
/// the *inherited* side. The explicit override value is read by upstream's
/// `findLast(row => row.id === id && row.config !== undefined)`, which has no `insert` guard, so a
/// row that both inserts and carries a `config` reports that `config` here. Keeping the asymmetry is
/// deliberate: it is what the editor shows, and a reader that "fixed" it would show a different
/// value from the one upstream shows.
#[must_use]
pub fn configuration_layers(
    rows: &[Value],
    inherited: &BTreeMap<String, Value>,
    patches: &[Value],
) -> Vec<ConfigurationLayers> {
    let empty = || Value::Object(Map::new());
    addressable_rows(rows)
        .into_iter()
        .filter_map(|index| {
            let id = rows[index].get("id")?.as_str()?.to_owned();
            let r#override = patches
                .iter()
                .rev()
                .find(|patch| {
                    patch.get("id").and_then(Value::as_str) == Some(id.as_str())
                        && patch.get("config").is_some()
                })
                .and_then(|patch| patch.get("config").cloned())
                .unwrap_or_else(empty);
            Some(ConfigurationLayers {
                index,
                id: id.clone(),
                inherited: inherited.get(&id).cloned().unwrap_or_else(empty),
                r#override,
            })
        })
        .collect()
}

/// The document change one edit performs.
#[derive(Clone, Debug, PartialEq)]
pub struct EditPlan {
    /// The whole patch document after the edit.
    pub rows: Vec<Value>,
    /// Whether anything changed; `false` means the document is already in the requested state.
    pub changed: bool,
    /// Value positions the caller must write back as `!!js` scalars, as `/`-joined key paths — the
    /// `{__jsExpr: "…"}` maps upstream's `visit` turns back into tags before writing.
    pub js_paths: Vec<String>,
}

/// Plan one profile config edit — upstream `ConfigEditor.edit`'s document change.
///
/// Three outcomes, and the distinction between the first two is the subtle one:
///
/// - **`next` equals `inherited`.** The user asked for the value the lower layers already supply, so
///   the explicit override is removed: every row with this `id` that is not an insertion loses its
///   `config` key, and a row left with nothing but `id` (and `name`, when it had one) is deleted
///   entirely. Writing the inherited value back instead would pin it, and a later bundle change
///   would no longer reach the user.
/// - **No matching row.** The override is appended as `{id, name, config}`, which is how a profile
///   expresses a value the composition does not have yet.
/// - **A matching row.** Its `config` is replaced wholesale — upstream replaces, never deep-merges,
///   so a key the user dropped does not survive from the inherited layer.
///
/// The matching row is the *last* row with this `id` that has no `insert` and, when the caller
/// supplied a `name`, either no `name` or the same one: a patch may assert a name, and an edit must
/// land on the row the profile addresses.
///
/// `js_paths` reports the `{__jsExpr: "…"}` nodes the caller must re-tag as `!!js` scalars when it
/// writes YAML; this function never renders YAML.
#[must_use]
pub fn plan_config_edit(
    rows: &[Value],
    id: &str,
    name: &str,
    next: &Value,
    inherited: &Value,
) -> EditPlan {
    let mut rows = rows.to_vec();
    let mut changed = false;
    let last_index = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.get("id").and_then(Value::as_str) == Some(id)
                && row.get("insert").is_none()
                && match row.get("name").and_then(Value::as_str) {
                    None => true,
                    Some(existing) => existing == name,
                }
        })
        .map(|(index, _)| index)
        .next_back();
    if next == inherited {
        // The requested value is the one the lower layers already supply, so the explicit override
        // is removed instead of being pinned: a later bundle change must still reach this row.
        let mut drop = Vec::new();
        for (index, row) in rows.iter_mut().enumerate() {
            if row.get("id").and_then(Value::as_str) != Some(id) || row.get("insert").is_some() {
                continue;
            }
            let Some(object) = row.as_object_mut() else {
                continue;
            };
            changed |= object.remove("config").is_some();
            // A row left with nothing but its identity says nothing at all, so it goes too.
            if object.keys().all(|key| key == "id" || key == "name") {
                drop.push(index);
                changed = true;
            }
        }
        for index in drop.into_iter().rev() {
            rows.remove(index);
        }
    } else {
        match last_index {
            None => {
                // The profile expresses a value the composition does not have yet.
                let mut row = Map::new();
                row.insert("id".to_owned(), Value::String(id.to_owned()));
                if !name.is_empty() {
                    row.insert("name".to_owned(), Value::String(name.to_owned()));
                }
                row.insert("config".to_owned(), next.clone());
                rows.push(Value::Object(row));
                changed = true;
            }
            Some(index) => {
                // A patch replaces the whole config; it is never a deep merge, so a key the user
                // dropped does not survive from the inherited layer.
                if let Some(object) = rows[index].as_object_mut() {
                    let replaced = object.insert("config".to_owned(), next.clone());
                    changed = replaced.as_ref() != Some(next);
                }
            }
        }
    }
    let js_paths = if changed {
        let mut paths = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            collect_js_paths(row, &index.to_string(), &mut paths);
        }
        paths
    } else {
        Vec::new()
    };
    EditPlan {
        rows,
        changed,
        js_paths,
    }
}

/// Collect the `{__jsExpr: "…"}` value positions under `value`, as `/`-joined key paths.
fn collect_js_paths(value: &Value, path: &str, paths: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if object.len() == 1 && object.get("__jsExpr").is_some_and(Value::is_string) {
                paths.push(path.to_owned());
                return;
            }
            for (key, child) in object {
                collect_js_paths(child, &format!("{path}/{key}"), paths);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_js_paths(child, &format!("{path}/{index}"), paths);
            }
        }
        _ => {}
    }
}
