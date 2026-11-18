//! Hot module replacement's decision core: **what changed → which modules must be invalidated**.
//!
//! # What upstream `packages/boot/hmr` contains, measured
//!
//! Five sources, 1,740 lines:
//!
//! | File | Lines | What it decides | Needs |
//! | --- | --- | --- | --- |
//! | `index.ts` | 1,136 | the `Hmr` service: watch roots, change dispatch, dependency analysis, replacement planning, profile reconciliation, cache eviction | Cordis fibers, the Node ESM/CJS caches, chokidar |
//! | `package-manifest.ts` | 330 | expire Node's cached `package.json` reads by hooking five internal methods | `--expose-internals`, native addon |
//! | `module-files.ts` | 124 | record each loaded module file's inode/size/mtime and decide which loaded module is stale | `statSync`, a temp-dir clock |
//! | `watch-config.ts` | 109 | watch one exact patch path and its missing-parent chain | chokidar, `realpath` |
//! | `error.ts` | 41 | classify an esbuild failure and print a code frame | `@babel/code-frame` |
//!
//! # Capability classification
//!
//! The package is one service in five capability layers, and they sort by how much of them survives
//! without a Node module cache:
//!
//! | Capability | Upstream | Portable? | Why |
//! | --- | --- | --- | --- |
//! | **Module invalidation** — dependency analysis, reverse-importer planning, plugin/fiber selection, per-manifest expiry | `index.ts` `analyzeChanges`, `planReplacement`, `stopPlanned`, `evictDropped`, `packageManifests` | **yes** | pure graph and set algebra; the only I/O is `existsSync` for manifests |
//! | **Module staleness** — identity of a loaded file, compare, observe, refresh | `module-files.ts` | **yes** | pure once `stat` is a value: `(ino, size, mtimeMs)` in, stale-set out |
//! | **Change dispatch** — config-path skip, `package.json` expiry, external → full reload, cached → stash, include → refresh, else emit | `index.ts` `dispatch` | **yes** | an ordered decision over five predicate sets |
//! | **Package manifest** — read/validate/`type` normalisation, nearest-manifest walk, expired-directory containment | `package-manifest.ts` | **yes** | a parser plus a directory walk; only the five monkey-patches are Node-specific |
//! | **Watch** — exact-path root discovery over missing parents, ignore-as-not-an-ancestor, per-context registration, serialised refresh | `watch-config.ts`, `index.ts` `watchConfig` | **decision yes, facility no** | [`find_watch_root`], [`refresh_reaches`], [`ConfigRegistrations`], [`RefreshQueue`] are pure; `chokidar` and `realpath` are not, and `crate::file_watch` already owns polling |
//! | **Apply** — `loader.import`, `registry.plugin`, fiber restart, rollback, cache eviction | `index.ts` `partialReload`, `evict`, `reload` | **no** | it *is* Cordis' fiber registry plus Node's `loadCache`; this build's [`crate::fiber`] answers the same question in its own vocabulary |
//! | **Code frames** | `error.ts` | **partly** | failure classification is pure ([`read_build_failure`]); the frame layout is `@babel/code-frame`'s, not HMR's |
//!
//! # What this build already covers
//!
//! - `crate::file_watch` — polling detection of added/changed/removed files under a workspace root.
//!   It answers *"which paths changed"*, which is HMR's input, not its question. Its
//!   [`is_skipped_dir`](crate::file_watch::is_skipped_dir) is a **different** ignore policy from
//!   HMR's: HMR's watcher ignores `**/node_modules`, `**/.*`, `cache` and `data` as picomatch
//!   globs, and its exact-path config watcher ignores everything that is *not an ancestor of the
//!   target*. Both exist here ([`DEFAULT_IGNORED`], [`refresh_reaches`]) because a port that
//!   reused the workspace policy would watch the wrong trees.
//! - `crate::boot_app_gap::file_url` — percent-encoding a path into a `file:` URL, the key HMR
//!   compares against its three caches.
//! - `crate::fiber`, `crate::registry`, `crate::plugin_lifecycle` — the Cordis-shaped half that
//!   [`crate::hmr_gap`] deliberately does not duplicate.
//! - Symbol search across the workspace finds **no** local counterpart for `analyzeChanges`,
//!   `planReplacement`, `packageManifests`, `findNearestPackageConfig`, `isPathInInvalidatedDirectory`,
//!   `ModuleFiles.compare`/`observe`, `canonicalPath`, or `loadDependencies`: zero hits. That is
//!   this module's gap, and the reason it exists.
//!
//! # The three graph questions, which are the whole point
//!
//! HMR answers three different reachability questions and getting them confused silently reloads
//! the wrong thing:
//!
//! 1. **Accepted / declined** ([`analyze_changes`]) walks *forward* along `imports` edges from the
//!    changed module: a module is accepted when it transitively imports a changed module, because
//!    that module's evaluation captured the old code. A module whose every path forward is declined
//!    or excluded is declined. This is the hot-reload gate: only accepted modules are re-evaluated.
//! 2. **Eviction** ([`affected_modules`]) walks *backward* along `imported-by` edges from the changed
//!    module: every module that imports it must be evicted from the cache or it keeps the binding to
//!    the old namespace. Note the direction reversal — accepted walks `imports`, evicted walks
//!    `imported-by`, and upstream keeps the two in different maps (`getLinked` vs `moduleGraph`).
//! 3. **Host reachability** ([`plan_replacement`]) is question 2 plus an ownership test: if any
//!    evicted module is in the CLI entry point's *static* graph, or if an affected plugin runs
//!    outside a Loader entry, the module cannot be replaced in place and is reported
//!    [`PendingReason::HostModule`] / [`PendingReason::UnmanagedPlugin`] instead — the process must
//!    restart. Without that test HMR would evict a host module and the next import would re-evaluate
//!    the CLI's own code under itself.
//!
//! Excluded modules (`node:` builtins and anything under `node_modules`) are cut from every walk.
//!
//! # Platform spelling
//!
//! Upstream threads `path.sep` and `path.join`/`dirname` through all of this, and
//! `toNamespacedPath`/`realpath` make two spellings of one directory compare equal on Windows. The
//! portable half is the algebra, not the spelling, so this module takes `/` as *the* separator:
//! every function expects normalized `/`-separated paths, exactly what
//! [`normalize_path`](crate::normalize_path) already produces and what a `file:` URL decodes to.
//! A Windows caller normalizes first ([`namespaced_path`] is the identity function here, and says
//! so). Symlink resolution stays injected ([`ExpiredDirectories::is_path_expired`]) because only the
//! caller knows whether it has a filesystem.
//!
//! # What is deliberately not here
//!
//! - **No watcher.** [`find_watch_root`] plans a watch; opening one is `crate::file_watch`'s
//!   polling loop or a platform facility. The two are composable: the plan says *which* root, at
//!   *what* depth; the watcher reports paths.
//! - **No `stat`.** Every identity is a parameter. That is what makes the staleness rules testable
//!   without touching a clock or a file, and it is also the honest boundary: HMR's own
//!   `now()` reads the *file* clock by creating a marker file precisely because the system clock and
//!   the filesystem's timestamps disagree, so a port that called `SystemTime::now()` here would
//!   inherit the bug upstream works around.
//! - **No cache eviction.** [`crate::fiber`] already models fibers; a Rust cache is a map `remove`,
//!   not a decision.
//!
//! # Wiring notes for the owner of `lib.rs`
//!
//! 1. Add `pub mod hmr_gap;`. This file imports nothing from `crate::` except
//!    [`crate::boot_app_gap::file_url`] and the path helpers, so it compiles under
//!    `#[path = "../src/hmr_gap.rs"]` in tests unchanged.
//! 2. [`classify_change`] is the one entry point a watcher calls; [`plan_replacement`] and
//!    [`analyze_changes`] are the two a replacement loop calls. Nothing needs a live Cordis
//!    `Context`.
//! 3. [`WatchState::canonicalize`] is a function pointer, not a trait, so a caller that has no
//!    `realpath` passes [`identity_path`] and every containment test still holds on the lexical
//!    spelling.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use girder_core::boot_app_gap::file_url;

// ============================================================================================
// Constants — the literals upstream states in code or in its schema
// ============================================================================================

/// The event HMR emits for a changed file that no include and no module cache owns.
pub const HMR_CHANGE_EVENT: &str = "hmr/change";

/// The event HMR emits once replacements have finished loading.
pub const HMR_RELOAD_EVENT: &str = "hmr/reload";

/// Cordis' default `debounce` for combining module changes, in milliseconds.
pub const DEFAULT_DEBOUNCE_MS: u64 = 100;

/// Cordis' default module-watch ignore globs, in declaration order.
pub const DEFAULT_IGNORED: [&str; 4] = ["**/node_modules", "**/.*", "cache", "data"];

/// Node's error code for a `package.json` its reader refuses.
pub const INVALID_PACKAGE_CONFIG: &str = "ERR_INVALID_PACKAGE_CONFIG";

/// Coarsest supported filesystem timestamp granularity, in milliseconds.
///
/// FAT stores modification times in two-second units, so a file written within this window before
/// an observation cannot be proven older than it. Upstream's `TIMESTAMP_RESOLUTION_MS`.
pub const TIMESTAMP_RESOLUTION_MS: u64 = 2000;

/// The separator every path in this module uses. See the module docs on platform spelling.
pub const SEP: char = '/';

// ============================================================================================
// Paths — the arithmetic upstream gets from `node:path`
// ============================================================================================

/// Whether `path` is absolute in this module's spelling.
pub fn is_absolute(path: &str) -> bool {
    path.starts_with(SEP)
}

/// Collapse `.`/`..` and repeated separators lexically, never touching the filesystem.
///
/// `..` at an absolute root stays at the root (as `path.resolve` does), and a relative path that
/// walks above its start keeps the leading `..` segments rather than inventing a root.
pub fn normalize(path: &str) -> String {
    let absolute = is_absolute(path);
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split(SEP) {
        match part {
            "" | "." => {}
            ".." => match parts.last() {
                Some(&last) if last != ".." => {
                    parts.pop();
                }
                Some(_) => {}
                None => {
                    if !absolute {
                        parts.push("..");
                    }
                }
            },
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("{SEP}{joined}")
    } else if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

/// `path.join` for two components, without normalising: the caller decides when to [`normalize`].
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with(SEP) {
        format!("{dir}{name}")
    } else {
        format!("{dir}{SEP}{name}")
    }
}

/// The directory containing `path`, with the edge cases `node:path.dirname` has.
pub fn dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches(SEP);
    if trimmed.is_empty() {
        // Either the root itself or a run of separators naming nothing else.
        return SEP.to_string();
    }
    match trimmed.rfind(SEP) {
        None => ".".to_owned(),
        Some(0) => SEP.to_string(),
        Some(index) => trimmed[..index].to_owned(),
    }
}

/// The last component of `path`, or `""` when the path names only a root.
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(SEP);
    match trimmed.rfind(SEP) {
        None => trimmed,
        Some(index) => &trimmed[index + 1..],
    }
}

/// `path.resolve(base, path)`: an absolute `path` wins, anything else is joined to `base` and
/// normalised.
pub fn resolve(base: &str, path: &str) -> String {
    if is_absolute(path) {
        normalize(path)
    } else {
        normalize(&join(base, path))
    }
}

/// `dir + sep` — the form every containment test in this module compares against.
///
/// A root is stored with its trailing separator so `starts_with` cannot match a sibling that merely
/// shares a prefix: `/w/pkg-extra` is not inside `/w/pkg`.
pub fn with_trailing_sep(dir: &str) -> String {
    let mut out = dir.trim_end_matches(SEP).to_owned();
    out.push(SEP);
    out
}

/// Whether `dir` is `root` itself or below it. `root` must carry its trailing separator.
pub fn is_within(dir: &str, root: &str) -> bool {
    with_trailing_sep(dir).starts_with(root)
}

/// Whether `ancestor` is `target` or an ancestor of it — the exact test upstream expresses as
/// `relative(resolve(path), target.filename)` not starting with `..`.
pub fn refresh_reaches(ancestor: &str, target: &str) -> bool {
    target == ancestor || target.starts_with(&with_trailing_sep(ancestor))
}

/// Upstream's `toNamespacedPath`, which only Windows changes: the identity here, stated rather than
/// hidden, so the comparison sites that use it read the same on both platforms.
pub fn namespaced_path(path: &str) -> String {
    path.to_owned()
}

/// The canonical-path fallback: a path that cannot be resolved keeps its parent's canonical
/// spelling plus its own final component, so a file that will be created is still addressable.
///
/// `canonicalize` is the injected `realpath`; `Ok` answers the resolved path and `Err` means the
/// path does not exist, which is the only error upstream retries through.
#[allow(
    clippy::result_unit_err,
    reason = "the error is the *absence* of a realpath and carries no detail; an Option would express the same thing but the injected `realpath` seam answers a Result, so keeping it a Result means the caller's closure and this function share one shape"
)]
pub fn canonical_path(
    path: &str,
    canonicalize: &dyn Fn(&str) -> Result<String, ()>,
) -> Result<String, ()> {
    let Ok(real) = canonicalize(path) else {
        let parent = dirname(path);
        if parent == path {
            return Err(());
        }
        return Ok(join(
            &canonical_path(&parent, canonicalize)?,
            basename(path),
        ));
    };
    Ok(real)
}

/// Decode a `file:` URL back to a path, the inverse of [`crate::boot_app_gap::file_url`].
pub fn file_url_to_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    // `file:///a` has an empty authority; `file://host/a` has a host component.
    let path = if rest.starts_with(SEP) {
        rest
    } else {
        match rest.find(SEP) {
            Some(index) => &rest[index..],
            None => rest,
        }
    };
    percent_decode(path)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = input.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A `file:` URL for a path — the key HMR compares against its external, cache and record sets.
pub fn path_to_file_url(path: &str) -> String {
    file_url(path)
}

// ============================================================================================
// Change dispatch — `index.ts`'s debounced loop, minus the debounce
// ============================================================================================

/// What the dispatcher decided one changed path is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeAction {
    /// A configured watch path: the configuration watch owns it, so module HMR skips it.
    Configured,
    /// An external module — the CLI entry point's own dependency tree — so the process reloads.
    Reload,
    /// A cached module: stash it for a partial reload.
    Stash,
    /// A `package.json` with no cached module: expiring its manifest is the whole effect.
    Manifest,
    /// A profile patch file owned by a Loader include: refresh that include.
    Include,
    /// Nothing owns it: emit [`HMR_CHANGE_EVENT`].
    Emit,
}

/// One dispatched change, with every spelling the decision needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifiedChange {
    /// The event path as reported by the watcher.
    pub path: String,
    /// The canonical, resolved spelling of the changed file.
    pub filename: String,
    /// The configured spelling — `base + path` without canonicalisation. A Loader entry may name
    /// either spelling, so both are matched.
    pub configured: String,
    /// `file:` URL of [`filename`].
    pub url: String,
    /// The `package.json` this change expires, if it is one and it is not under `node_modules`.
    pub manifest: Option<String>,
    /// What to do.
    pub action: ChangeAction,
}

/// The state a dispatch needs: five predicate sets and the two spellings of the watch base.
///
/// Every field is borrowed, so a long-lived dispatcher refreshes them in place. That matters for
/// [`Self::cached`] and [`Self::externals`]: upstream keeps both live and mutates them across a
/// replacement, and a snapshot taken once would classify later changes against a stale cache.
pub struct WatchState<'a> {
    /// The real (canonical) watch base directory.
    pub base_dir: &'a str,
    /// The configured base directory, which may be a link.
    pub configured_base: &'a str,
    /// Paths registered as configuration watches, in both spellings.
    pub config_paths: &'a BTreeSet<String>,
    /// `file:` URLs of the process entry point's static dependency tree.
    pub externals: &'a BTreeSet<String>,
    /// `file:` URLs present in the ES module cache.
    pub cached: &'a BTreeSet<String>,
    /// Subtree filenames of the Loader's includes.
    pub includes: &'a [String],
    /// Injected `realpath`: `Ok` resolves, `Err` means the path does not exist.
    ///
    /// A function pointer rather than a closure so the struct needs no lifetime parameter;
    /// [`identity_path`] is the implementation for a caller with no filesystem.
    pub canonicalize: fn(&str) -> Result<String, ()>,
}

/// Classify one watcher event.
///
/// The order is upstream's and it is load-bearing:
///
/// 1. a configured path is skipped **before** anything else, so a patch file that also happens to
///    be a cached module is refreshed by the config watch and not reloaded as code;
/// 2. a `package.json` outside `node_modules` expires its manifest **even when** it is also an
///    external or a cached module — the manifest is read by the next resolution either way;
/// 3. external wins over cached, because replacing a host module in place is never valid;
/// 4. include membership is checked against *both* spellings, and only for a module the cache does
///    not hold.
pub fn classify_change(state: &WatchState<'_>, event_path: &str) -> ClassifiedChange {
    let filename = resolve(state.base_dir, event_path);
    let filename = (state.canonicalize)(&filename).unwrap_or(filename);
    let configured = resolve(state.configured_base, event_path);
    let url = path_to_file_url(&filename);

    if state.config_paths.contains(&filename) || state.config_paths.contains(&configured) {
        return ClassifiedChange {
            path: event_path.to_owned(),
            filename,
            configured,
            url,
            manifest: None,
            action: ChangeAction::Configured,
        };
    }

    let is_manifest = basename(&filename) == "package.json" && !filename.contains("/node_modules/");
    let manifest = is_manifest.then(|| filename.clone());

    let action = if state.externals.contains(&url) {
        ChangeAction::Reload
    } else if state.cached.contains(&url) {
        ChangeAction::Stash
    } else if is_manifest {
        ChangeAction::Manifest
    } else if state
        .includes
        .iter()
        .any(|candidate| candidate == &filename || candidate == &configured)
    {
        ChangeAction::Include
    } else {
        ChangeAction::Emit
    };

    ClassifiedChange {
        path: event_path.to_owned(),
        filename,
        configured,
        url,
        manifest,
        action,
    }
}

/// The identity canonicaliser: what a caller without a `realpath` passes to
/// [`WatchState::canonicalize`].
#[allow(
    clippy::unnecessary_wraps,
    reason = "the signature is the injectable `realpath` slot, which reports absence as `Err`; this implementation never does"
)]
#[allow(
    clippy::result_unit_err,
    reason = "same shape as `canonical_path`, whose injected realpath answers a Result with no detail"
)]
pub fn identity_path(path: &str) -> Result<String, ()> {
    Ok(path.to_owned())
}

// ============================================================================================
// Module graph — `index.ts`'s two walks in one type
// ============================================================================================

/// A directed module graph. The *meaning* of an edge is the caller's: HMR builds it once as
/// `module → imports` (for [`analyze_changes`]) and once as `module → imported-by` (for
/// [`affected_modules`]).
///
/// A missing node and a node with no edges are indistinguishable on purpose: a module with no
/// importers is not a module with an unknown set of importers, and both walks treat it as a leaf.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Graph {
    edges: BTreeMap<String, BTreeSet<String>>,
}

impl Graph {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `edge` from `from` to `to`.
    pub fn insert(&mut self, from: impl Into<String>, to: impl Into<String>) {
        self.edges.entry(from.into()).or_default().insert(to.into());
    }

    /// The targets of `from`, in sorted order.
    pub fn edges<'a>(&'a self, from: &str) -> impl Iterator<Item = &'a str> + 'a {
        self.edges
            .get(from)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    /// Whether `from` has any recorded edge.
    pub fn has_edges(&self, from: &str) -> bool {
        self.edges.get(from).is_some_and(|set| !set.is_empty())
    }

    /// Every node that has at least one edge.
    pub fn sources(&self) -> impl Iterator<Item = &str> {
        self.edges.keys().map(String::as_str)
    }
}

/// Whether a module is cut from every walk.
///
/// Node builtins and anything inside `node_modules` are excluded because replacing them in place
/// is either impossible (`node:` is not a file) or wrong (a dependency's code is not the profile's
/// code). Upstream's single `isExcluded` predicate.
pub fn is_excluded(url: &str) -> bool {
    url.starts_with("node:") || url.contains("/node_modules/")
}

/// The result of the hot-reload gate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeAnalysis {
    /// Modules to re-evaluate: they transitively import a changed module.
    pub accepted: BTreeSet<String>,
    /// Modules that keep running: every path forward is declined or excluded.
    pub declined: BTreeSet<String>,
}

/// Upstream's `analyzeChanges`: the accepted/declined split over `module → imports` edges.
///
/// The algorithm is a worklist fixpoint, not a plain BFS, and the distinction is visible on cycles:
/// a module is accepted as soon as *one* of its imports is accepted, declined only when *all* of
/// them are declined or excluded, and anything still undecided when the worklist stops making
/// progress is declined. That last rule is what keeps a cycle that never reaches a changed module
/// from being reported as accepted — the failure mode would be re-evaluating a module whose
/// dependencies did not change, forever.
pub fn analyze_changes(
    imports: &Graph,
    stashed: &BTreeSet<String>,
    externals: &BTreeSet<String>,
) -> ChangeAnalysis {
    let mut accepted: BTreeSet<String> = stashed.clone();
    let mut declined: BTreeSet<String> = externals.clone();
    let mut pending: Vec<String> = Vec::new();

    for url in stashed {
        for child in imports.edges(url) {
            if accepted.contains(child) || declined.contains(child) || is_excluded(child) {
                continue;
            }
            pending.push(child.to_owned());
        }
    }

    while !pending.is_empty() {
        let mut index = 0;
        let mut updated = false;
        while index < pending.len() {
            let Some(url) = pending.get(index).cloned() else {
                break;
            };
            let mut is_declined = true;
            let mut is_accepted = false;
            for child in imports.edges(&url) {
                // `child` borrows the graph; clone only when it enters the worklist.
                if declined.contains(child) || is_excluded(child) {
                    continue;
                }
                if accepted.contains(child) {
                    is_accepted = true;
                    break;
                }
                is_declined = false;
                if !pending.iter().any(|queued| queued == child) {
                    updated = true;
                    pending.push(child.to_owned());
                }
            }
            if is_accepted || is_declined {
                updated = true;
                pending.remove(index);
                if is_accepted {
                    accepted.insert(url);
                } else {
                    declined.insert(url);
                }
            } else {
                index += 1;
            }
        }
        if !updated {
            break;
        }
    }

    // Undecided after convergence: no path forward reaches a changed module.
    for url in pending {
        declined.insert(url);
    }

    ChangeAnalysis { accepted, declined }
}

/// The transitive dependencies of one module, skipping declined and excluded modules —
/// upstream's `loadDependencies(job, ignored)`.
///
/// The root is included: a plugin entry file is itself a dependency of the plugin, and
/// `partialReload` relies on that when it re-exports the entry's URL into `accepted`. An excluded
/// module is skipped *with its whole subtree*, because a dependency's own dependencies are equally
/// not the profile's code.
pub fn load_dependencies(
    imports: &Graph,
    root: &str,
    ignored: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut dependencies = BTreeSet::new();
    let mut stack = vec![root.to_owned()];
    while let Some(url) = stack.pop() {
        if ignored.contains(&url) || dependencies.contains(&url) || is_excluded(&url) {
            continue;
        }
        dependencies.insert(url.clone());
        for child in imports.edges(&url) {
            if !dependencies.contains(child) {
                stack.push(child.to_owned());
            }
        }
    }
    dependencies
}

/// One Loader entry's plugin, as `partialReload` sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginEntry {
    /// `file:` URL the entry's name resolved to.
    pub url: String,
    /// The plugin the module exports.
    pub plugin: String,
    /// The running runtime's identity, when the plugin is registered.
    pub runtime: Option<String>,
}

/// One plugin reload: the entry that leads it and every entry module that shares its runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReloadGroup {
    /// The plugin being replaced.
    pub plugin: String,
    /// The entry file the reload re-imports first.
    pub filename: String,
    /// The runtime, when the plugin has one.
    pub runtime: Option<String>,
    /// Entry modules that must be re-imported with it.
    pub modules: BTreeSet<String>,
}

/// Which plugin entries a change forces to re-import — the step between
/// [`analyze_changes`] and any actual replacement.
///
/// This is the other half of the accepted/declined split, and it is where "the module changed" turns
/// into "this plugin restarts". Upstream's `partialReload` does it in three passes, all of them here:
///
/// 1. **A declined entry is skipped, and a resolved entry is marked declined** so a second entry
///    pointing at the same URL is not added twice;
/// 2. **an entry reloads only if its dependency closure reaches an accepted module** — the closure
///    is [`load_dependencies`] with `declined` as the cut, so a build-time-only dependency never
///    causes a reload — and its closure is then folded into `accepted`, which is what makes a
///    second entry that shares a dependency reload with it rather than separately;
/// 3. **a module joins the reload of its runtime** when another entry already claims that runtime,
///    because one runtime cannot be replaced twice; and every such re-export root is added to
///    `accepted` in a second pass, so a sibling entry that merely re-exports it is evicted too.
///
/// `accepted` is taken by `&mut` because passes 2 and 3 grow it; the caller keeps the grown set for
/// its cache eviction.
pub fn select_reloads(
    entries: &[PluginEntry],
    imports: &Graph,
    declined: &BTreeSet<String>,
    accepted: &mut BTreeSet<String>,
) -> Vec<ReloadGroup> {
    let mut declined = declined.clone();
    let mut pending: Vec<&PluginEntry> = Vec::new();
    for entry in entries {
        if declined.contains(&entry.url) {
            continue;
        }
        pending.push(entry);
        declined.insert(entry.url.clone());
    }

    let mut groups: Vec<ReloadGroup> = Vec::new();
    for entry in &pending {
        declined.remove(&entry.url);
        let dependencies = load_dependencies(imports, &entry.url, &declined);
        declined.insert(entry.url.clone());
        if dependencies.is_empty() || !dependencies.iter().any(|dep| accepted.contains(dep)) {
            continue;
        }
        for dependency in &dependencies {
            accepted.insert(dependency.clone());
        }
        if let Some(group) = groups.iter_mut().find(|group| {
            group.plugin == entry.plugin
                || (entry.runtime.is_some() && group.runtime == entry.runtime)
        }) {
            group.modules.insert(entry.url.clone());
            continue;
        }
        groups.push(ReloadGroup {
            plugin: entry.plugin.clone(),
            filename: entry.url.clone(),
            runtime: entry.runtime.clone(),
            modules: std::iter::once(entry.url.clone()).collect(),
        });
    }

    // Pass 3's second half: a re-export root of an already-replaced runtime is itself accepted.
    for entry in &pending {
        if groups.iter().any(|group| {
            group.plugin == entry.plugin
                || (entry.runtime.is_some() && group.runtime == entry.runtime)
        }) {
            accepted.insert(entry.url.clone());
        }
    }

    groups
}

/// Every module that transitively imports one of `stale`, walking `imported_by` edges.
///
/// This is the eviction set: a changed module's importers hold bindings into its old namespace, so
/// leaving them cached keeps the old code reachable. The walk includes `stale` itself.
pub fn affected_modules(imported_by: &Graph, stale: &BTreeSet<String>) -> BTreeSet<String> {
    let mut affected: BTreeSet<String> = BTreeSet::new();
    for url in stale {
        let mut frontier = vec![url.clone()];
        while let Some(module) = frontier.pop() {
            if !affected.insert(module.clone()) {
                continue;
            }
            for importer in imported_by.edges(&module) {
                if !affected.contains(importer) {
                    frontier.push(importer.to_owned());
                }
            }
        }
    }
    affected
}

/// Why a changed module cannot be replaced in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingReason {
    /// The process entry point's static graph imports it, so replacing it would re-evaluate the
    /// CLI under itself.
    HostModule,
    /// It is imported by a plugin that runs outside a traversable Loader entry, so nothing can
    /// restart that plugin.
    UnmanagedPlugin,
    /// Loaded modules could not be compared with disk at all.
    ModuleComparison,
}

impl PendingReason {
    /// The wire spelling upstream reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HostModule => "host-module",
            Self::UnmanagedPlugin => "unmanaged-plugin",
            Self::ModuleComparison => "module-comparison",
        }
    }
}

/// A changed module that keeps running its previous code until a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingModule {
    /// `file:` URL of the changed module.
    pub url: String,
    /// Why it cannot be replaced.
    pub reason: PendingReason,
}

/// A fiber of a plugin instance that a stale module reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FiberRef {
    /// Opaque fiber identity.
    pub id: String,
    /// The Loader entry whose own fiber is this fiber's ancestor, when there is one.
    ///
    /// Upstream finds it by walking the fiber's ancestor chain and asking whether the entry's own
    /// fiber is in the affected set; only such an entry can start a replacement. `None` is a fiber
    /// no entry owns — a plugin registered outside the Loader — and it is what makes a change
    /// unmanaged.
    pub owner: Option<String>,
}

/// A plugin instance a loaded module exports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportedPlugin {
    /// Opaque plugin identity.
    pub plugin: String,
    /// Its running fibers.
    pub fibers: Vec<FiberRef>,
}

/// Everything [`plan_replacement`] reads.
pub struct PlanInput<'a> {
    /// `module → imported-by` edges.
    pub imported_by: &'a Graph,
    /// The process entry point's static module set.
    pub host: &'a BTreeSet<String>,
    /// Plugins each loaded module exports, keyed by module URL.
    pub exports: &'a BTreeMap<String, Vec<ExportedPlugin>>,
    /// Loader entry ids the process manages.
    pub managed: &'a BTreeSet<String>,
    /// Roots (with trailing separators) that bound manifest expiry.
    pub roots: &'a [String],
    /// Whether a manifest exists — injected so the plan is testable without a filesystem.
    pub manifests: &'a dyn Fn(&str) -> bool,
}

/// The modules a replacement evicts, the manifests it expires, and the plugins it stops.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplacementPlan {
    /// Changed modules that wait for a restart, in the order [`plan_replacement`] was given.
    pub pending: Vec<PendingModule>,
    /// Modules to evict from the cache.
    pub evicted: BTreeSet<String>,
    /// Manifests to expire.
    pub manifests: BTreeSet<String>,
    /// Plugin ids to stop.
    pub plugins: BTreeSet<String>,
    /// Fibers to await.
    pub fibers: BTreeSet<String>,
}

/// Plan a replacement for `stale` without changing anything.
///
/// One decision per stale module, independent of the others (upstream loops the same way, so two
/// stale modules sharing an importer evict that importer twice into the same set):
///
/// 1. `affected` = the stale module plus every transitive importer;
/// 2. if any affected module is a host module → pending, `host-module`;
/// 3. else if any affected plugin fiber has no managed owner → pending, `unmanaged-plugin`;
/// 4. else evict every affected module, expire the `package.json` files above the changed module up
///    to its root, and stop the affected plugins' fibers.
///
/// Step 2 is checked for *every* affected module, not just the stale one: an importer inside the
/// host graph is reachable from the changed module, and evicting it is exactly as fatal.
pub fn plan_replacement(stale: &[String], input: &PlanInput<'_>) -> ReplacementPlan {
    let mut plan = ReplacementPlan::default();

    for url in stale {
        let single: BTreeSet<String> = std::iter::once(url.clone()).collect();
        let affected = affected_modules(input.imported_by, &single);

        let mut running: Vec<(&ExportedPlugin, &FiberRef)> = Vec::new();
        let mut plugins: BTreeSet<&str> = BTreeSet::new();
        for module in &affected {
            for plugin in input.exports.get(module).into_iter().flatten() {
                // Upstream dedupes plugin instances before expanding fibers: a plugin exported by
                // two affected modules is still one running plugin.
                if !plugins.insert(plugin.plugin.as_str()) {
                    continue;
                }
                for fiber in &plugin.fibers {
                    running.push((plugin, fiber));
                }
            }
        }

        let reason = if affected.iter().any(|module| input.host.contains(module)) {
            Some(PendingReason::HostModule)
        } else {
            let owned = running
                .iter()
                .filter(|(_, fiber)| {
                    fiber
                        .owner
                        .as_ref()
                        .is_some_and(|entry| input.managed.contains(entry))
                })
                .count();
            (owned < running.len()).then_some(PendingReason::UnmanagedPlugin)
        };

        if let Some(reason) = reason {
            plan.pending.push(PendingModule {
                url: url.clone(),
                reason,
            });
            continue;
        }

        for manifest in package_manifests(url, input.roots, input.manifests) {
            plan.manifests.insert(manifest);
        }
        plan.evicted.extend(affected);
        for (plugin, fiber) in running {
            plan.plugins.insert(plugin.plugin.clone());
            plan.fibers.insert(fiber.id.clone());
        }
    }

    plan
}

/// Every `package.json` from a module's directory up to the profile root or linked target that
/// contains it, deepest first — upstream's `packageManifests`.
///
/// A module can sit under a nested package, and each manifest on the way up is cached separately by
/// Node, so all of them expire. The walk stops at the first directory that is not inside a root: a
/// manifest above the profile belongs to the installation, not to the profile.
pub fn package_manifests(
    url: &str,
    roots: &[String],
    exists: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let Some(path) = file_url_to_path(url) else {
        return Vec::new();
    };
    let contains = |dir: &str| roots.iter().any(|root| is_within(dir, root));
    let mut manifests = Vec::new();
    let mut directory = dirname(&path);
    while contains(&directory) {
        let candidate = join(&directory, "package.json");
        if exists(&candidate) {
            manifests.push(candidate);
        }
        let parent = dirname(&directory);
        if parent == directory {
            break;
        }
        directory = parent;
    }
    manifests
}

// ============================================================================================
// Package manifests — `package-manifest.ts` without the five monkey-patches
// ============================================================================================

/// The `type` Node resolved for a package.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageType {
    /// `"type": "module"`.
    Module,
    /// `"type": "commonjs"`.
    CommonJs,
    /// Absent or unrecognised — Node's `'none'`.
    None,
}

impl PackageType {
    /// The serialized spelling Node's binding returns.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::CommonJs => "commonjs",
            Self::None => "none",
        }
    }
}

/// The package configuration fields Node's resolver consumes.
#[derive(Clone, Debug, PartialEq)]
pub struct PackageConfig {
    /// `name`, when it is a string.
    pub name: Option<String>,
    /// `main`, when it is a string.
    pub main: Option<String>,
    /// Normalised `type`.
    pub kind: PackageType,
    /// `exports`, when it is a string or an object.
    pub exports: Option<Value>,
    /// `imports`, when it is a string or an object.
    pub imports: Option<Value>,
}

/// The tuple Node's native binding hands its callers, in field order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageTuple {
    /// `name`.
    pub name: Option<String>,
    /// `main`.
    pub main: Option<String>,
    /// Normalised `type`.
    pub kind: PackageType,
    /// `imports`, serialized.
    pub imports: Option<String>,
    /// `exports`, serialized.
    pub exports: Option<String>,
    /// Manifest path.
    pub path: String,
}

impl PackageConfig {
    /// Serialize exactly as Node's binding does, in the binding's field order.
    pub fn to_tuple(&self, path: &str) -> PackageTuple {
        PackageTuple {
            name: self.name.clone(),
            main: self.main.clone(),
            kind: self.kind,
            imports: self.imports.as_ref().map(serialize_field),
            exports: self.exports.as_ref().map(serialize_field),
            path: namespaced_path(path),
        }
    }
}

/// A JSON field's serialized form: a string stays itself, anything else is JSON.
fn serialize_field(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Where an import that read a bad manifest came from, for the error text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportContext {
    /// The importing module's URL, which may be a `file:` URL or a bare path.
    pub base: String,
    /// The specifier being resolved.
    pub specifier: String,
}

/// A manifest Node refuses to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidPackageConfig {
    /// The manifest path.
    pub path: String,
    /// The message Node's reader throws.
    pub message: String,
}

impl InvalidPackageConfig {
    /// Node's error code for every instance of this failure.
    pub const CODE: &'static str = INVALID_PACKAGE_CONFIG;

    /// Node's error code, as the `code` property callers read.
    #[allow(
        clippy::unused_self,
        reason = "the accessor mirrors the `code` property error objects carry; callers read it off an instance"
    )]
    pub fn code(&self) -> &'static str {
        Self::CODE
    }
}

/// Read and validate a manifest.
///
/// `bytes` is `None` when the file does not exist, which is not an error: Node reports a missing
/// manifest as absent configuration. Everything else that can go wrong is
/// [`InvalidPackageConfig`], and the order of the checks matters because each one is a different
/// answer to the same bad input:
///
/// - **invalid UTF-8** → error, before any decode;
/// - **a UTF-8 BOM** is stripped, so a BOM-prefixed manifest is valid;
/// - **unparsable JSON** → error;
/// - **not an object** (including `null` and an array) → error;
/// - **`name` or `type` present but not a string** → error;
/// - **`name`, `type`, `exports` or `imports` a string with a lone surrogate** → error. In Rust this
///   is unrepresentable: a `String` cannot hold one, and `serde_json` rejects the unpaired escape
///   rather than passing it through, so the check upstream performs is *satisfied by construction*
///   here — the test below pins that down for the JSON case;
/// - otherwise **normalise**, never reject: `main` is dropped when it is not a string, `type`
///   collapses to `module`/`commonjs`/`none`, and `exports`/`imports` are kept only as a string or
///   an object.
pub fn read_package_config(
    path: &str,
    bytes: Option<&[u8]>,
    context: Option<&ImportContext>,
) -> Result<Option<PackageConfig>, InvalidPackageConfig> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let invalid = || InvalidPackageConfig {
        path: path.to_owned(),
        message: invalid_message(path, context),
    };
    let Ok(source) = std::str::from_utf8(bytes) else {
        return Err(invalid());
    };
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let Ok(parsed) = serde_json::from_str::<Value>(source) else {
        return Err(invalid());
    };
    let Value::Object(fields) = parsed else {
        return Err(invalid());
    };
    for key in ["name", "type"] {
        if fields.get(key).is_some_and(|value| !value.is_string()) {
            return Err(invalid());
        }
    }

    let string_field = |key: &str| -> Option<String> {
        fields.get(key).and_then(Value::as_str).map(str::to_owned)
    };
    let normalize = |key: &str| -> Option<Value> {
        match fields.get(key) {
            Some(Value::String(text)) => Some(Value::String(text.clone())),
            Some(value @ Value::Object(_)) => Some(value.clone()),
            _ => None,
        }
    };
    let kind = match fields.get("type").and_then(Value::as_str) {
        Some("module") => PackageType::Module,
        Some("commonjs") => PackageType::CommonJs,
        _ => PackageType::None,
    };

    Ok(Some(PackageConfig {
        name: string_field("name"),
        main: string_field("main"),
        kind,
        exports: normalize("exports"),
        imports: normalize("imports"),
    }))
}

/// Node's exact message: `Invalid package config {path}{importing}.`
fn invalid_message(path: &str, context: Option<&ImportContext>) -> String {
    let importing = context.map_or_else(String::new, |context| {
        let base = file_url_to_path(&context.base).unwrap_or_else(|| context.base.clone());
        format!(" while importing \"{}\" from {base}", context.specifier)
    });
    format!(
        "Invalid package config {}{importing}.",
        namespaced_path(path)
    )
}

/// What a nearest-manifest lookup found.
#[derive(Clone, Debug, PartialEq)]
pub enum NearestManifest {
    /// Node's own reader answers: either the first manifest *outside* the invalidated directories,
    /// or the point where the walk must stop without a manifest.
    NodeAnswers {
        /// The manifest or boundary path.
        path: String,
    },
    /// A manifest read from disk because its directory was invalidated.
    Cached {
        /// The manifest path, in namespaced spelling.
        path: String,
        /// The configuration read from it.
        config: PackageConfig,
    },
}

/// Node's nearest-manifest walk, restricted to the directories HMR expired.
///
/// Upstream's `findNearestPackageConfig`. The walk goes *up* from `start` and stops at three
/// places, and the first two are the ones that are easy to get wrong:
///
/// - a directory named `node_modules` stops the walk with "Node answers", because the question was
///   about a package scope and Node's own answer is the boundary;
/// - with `include_root` false, reaching the filesystem root stops with "Node answers" *before*
///   reading the root's own manifest — that is what makes the `CommonJS` reader keep the running
///   Node version's absent result instead of inventing one;
/// - an unexpired manifest stops with "Node answers", because the native reader still has it;
/// - an expired manifest with no readable configuration keeps walking to the next directory up.
///
/// `expired` answers whether a manifest path is inside an invalidated package directory, and `read`
/// is the disk read; both are injected.
pub fn nearest_package_config(
    start: &str,
    include_root: bool,
    expired: &dyn Fn(&str) -> bool,
    read: &dyn Fn(&str) -> Option<PackageConfig>,
) -> NearestManifest {
    let mut directory = dirname(start);
    loop {
        let manifest = join(&directory, "package.json");
        let parent = dirname(&directory);
        if basename(&directory) == "node_modules" || (!include_root && parent == directory) {
            return NearestManifest::NodeAnswers { path: manifest };
        }
        if !expired(&manifest) {
            return NearestManifest::NodeAnswers { path: manifest };
        }
        if let Some(config) = read(&manifest) {
            return NearestManifest::Cached {
                path: namespaced_path(&manifest),
                config,
            };
        }
        if parent == directory {
            return NearestManifest::NodeAnswers { path: manifest };
        }
        directory = parent;
    }
}

/// The package directories HMR has invalidated, and the real-path memo its containment test needs.
///
/// **Invalidation is sticky.** Upstream only ever *adds* to its directory set — an invalidated
/// package directory keeps reading its manifest from disk for the rest of the process's life.
/// That is the intended behaviour (a package that changed once is expected to change again during a
/// session) and not an oversight, so this type has no `clear`; a caller that wants a fresh state
/// drops the value.
#[derive(Clone, Debug, Default)]
pub struct ExpiredDirectories {
    directories: BTreeSet<String>,
    real: BTreeMap<String, String>,
}

impl ExpiredDirectories {
    /// No expired directories.
    pub fn new() -> Self {
        Self::default()
    }

    /// Expire the package directory a manifest lives in, and drop the real-path memo.
    ///
    /// The memo is dropped because a symlinked package can be retargeted; keeping a real path across
    /// an invalidation would let the containment test answer for the *old* target.
    pub fn invalidate(&mut self, manifest: &str) {
        self.directories.insert(dirname(manifest));
        self.real.clear();
    }

    /// Whether a path lives in an expired package directory.
    ///
    /// The path's parent is resolved through `realpath` (memoised per parent) before the comparison,
    /// because a package can be reached through a link: the caller names the link path, while the
    /// manifest that changed was named by its real path. A parent that cannot be resolved keeps its
    /// configured spelling — it has no manifest to read through a link.
    pub fn is_path_expired(
        &mut self,
        path: &str,
        realpath: &dyn Fn(&str) -> Option<String>,
    ) -> bool {
        let parent = dirname(path);
        let real = if let Some(real) = self.real.get(&parent) {
            real.clone()
        } else {
            let resolved = realpath(&parent).unwrap_or_else(|| parent.clone());
            self.real.insert(parent.clone(), resolved.clone());
            resolved
        };
        let target = join(&real, basename(path));
        self.directories
            .iter()
            .any(|directory| is_within(&target, &with_trailing_sep(directory)))
    }

    /// The expired directories, sorted.
    pub fn directories(&self) -> impl Iterator<Item = &str> {
        self.directories.iter().map(String::as_str)
    }

    /// How many distinct directories are expired. Upstream never removes one, so this only grows.
    pub fn len(&self) -> usize {
        self.directories.len()
    }

    /// Whether nothing has been invalidated.
    pub fn is_empty(&self) -> bool {
        self.directories.is_empty()
    }
}

/// Modules below `dropped` that a recomposition disabled, excluding host modules and modules below
/// a directory that is still mapped.
///
/// Upstream's `evictDropped`. The `kept` exclusion is what makes a *moved* package reload: its old
/// directory is dropped and its new one is kept, and evicting a module that is still mapped would
/// re-evaluate a package that did not change.
pub fn dropped_modules<'a>(
    urls: impl IntoIterator<Item = &'a str>,
    dropped: &BTreeSet<String>,
    kept: &BTreeSet<String>,
    host: &BTreeSet<String>,
) -> Vec<String> {
    urls.into_iter()
        .filter(|url| {
            if !url.starts_with("file:") || host.contains(*url) {
                return false;
            }
            let Some(path) = file_url_to_path(url) else {
                return false;
            };
            dropped.iter().any(|dir| is_within(&path, dir))
                && !kept.iter().any(|dir| is_within(&path, dir))
        })
        .map(str::to_owned)
        .collect()
}

/// A directory root in every real spelling, with its trailing separator.
///
/// Upstream collects both the JavaScript and the native `realpath` spelling because Node's module
/// caches use one and the runtime resolution the other, and on Windows they differ for 8.3 short
/// names. `spellings` is the injected resolver; the distinct answers all become roots.
pub fn mapping_roots(
    directories: impl IntoIterator<Item = String>,
    spellings: &dyn Fn(&str) -> Vec<String>,
) -> BTreeSet<String> {
    let mut roots = BTreeSet::new();
    for directory in directories {
        let mut resolved = spellings(&directory);
        if resolved.is_empty() {
            resolved.push(directory);
        }
        for spelling in resolved {
            roots.insert(with_trailing_sep(&spelling));
        }
    }
    roots
}

// ============================================================================================
// Module staleness — `module-files.ts`
// ============================================================================================

/// One loaded module file's disk identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    /// Inode, size and modification time, as one comparable key.
    pub key: String,
    /// Modification time in milliseconds since the epoch.
    pub modified_at_ms: u64,
}

impl FileIdentity {
    /// Build an identity the way upstream's `identityOf` does.
    pub fn new(inode: u64, size: u64, modified_at_ms: u64) -> Self {
        Self {
            key: format!("{inode}:{size}:{modified_at_ms}"),
            modified_at_ms,
        }
    }
}

/// What a comparison found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    /// Modules whose file disappeared or whose identity moved.
    pub stale: BTreeSet<String>,
    /// The identity read for each compared module, `None` for a missing file.
    pub seen: BTreeMap<String, Option<String>>,
}

/// Records the identity of loaded `file:` modules and decides which are stale.
///
/// Change time is deliberately not recorded: creating a hard link to a file, as pnpm does when it
/// imports from its store, changes `ctime` without changing content, so comparing it would report a
/// reload for an install that changed nothing.
///
/// A module first loaded *after* the last observation has no record. It is stale when its file was
/// modified at or after that observation, less [`TIMESTAMP_RESOLUTION_MS`] — the coarse-timestamp
/// allowance — so a reinstall between loading and the next comparison is detected at the cost of
/// reloading a module that was first loaded after such a write.
#[derive(Clone, Debug)]
pub struct ModuleFiles {
    records: BTreeMap<String, Option<String>>,
    observed_at: u64,
}

impl ModuleFiles {
    /// A record set that has observed nothing yet, with the observation time the caller's file
    /// clock reported.
    pub fn new(observed_at_ms: u64) -> Self {
        Self {
            records: BTreeMap::new(),
            observed_at: observed_at_ms,
        }
    }

    /// The last observation time, in milliseconds on the file clock.
    pub fn observed_at(&self) -> u64 {
        self.observed_at
    }

    /// Whether the module has a record at all — including a record that marks it known-stale.
    pub fn is_recorded(&self, url: &str) -> bool {
        self.records.contains_key(url)
    }

    /// The recorded identity key of one module, `None` when it is known-stale or unrecorded.
    pub fn record_key(&self, url: &str) -> Option<&str> {
        self.records.get(url).and_then(Option::as_deref)
    }

    /// How many modules are recorded.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Compare the given modules — keyed by URL, with the identity just read for each — against the
    /// records.
    ///
    /// A missing file is always stale. A recorded module is stale when its identity differs. An
    /// unrecorded module is stale when it was written at or after the last observation minus the
    /// timestamp resolution.
    pub fn compare(&self, read: &BTreeMap<String, Option<FileIdentity>>) -> Comparison {
        let mut stale = BTreeSet::new();
        let mut seen = BTreeMap::new();
        let floor = self.observed_at.saturating_sub(TIMESTAMP_RESOLUTION_MS);
        for (url, identity) in read {
            match identity {
                None => {
                    seen.insert(url.clone(), None);
                    stale.insert(url.clone());
                }
                Some(identity) => {
                    seen.insert(url.clone(), Some(identity.key.clone()));
                    let changed = self.records.get(url).map_or_else(
                        || identity.modified_at_ms >= floor,
                        |record| record.as_deref() != Some(identity.key.as_str()),
                    );
                    if changed {
                        stale.insert(url.clone());
                    }
                }
            }
        }
        Comparison { stale, seen }
    }

    /// Replace every record with the identities of the given loaded modules.
    ///
    /// `seen` supplies identities read earlier for modules whose cache entry has not changed since,
    /// so a later write stays detectable. `retained` modules keep their record (or keep their
    /// known-stale `None`). Anything else is read through `read` now and stays stale when its file
    /// was modified after `at`.
    ///
    /// A write in the same file-clock tick as `at` counts as earlier — `>` and not `>=` — so files
    /// installed just before the comparison are not reloaded.
    pub fn observe(
        &mut self,
        urls: &[String],
        seen: &BTreeMap<String, Option<String>>,
        retained: &BTreeSet<String>,
        at: u64,
        read: &dyn Fn(&str) -> Option<FileIdentity>,
    ) {
        let mut records = BTreeMap::new();
        for url in urls {
            let record = if retained.contains(url) {
                self.records.get(url).cloned().unwrap_or(None)
            } else if let Some(identity) = seen.get(url) {
                identity.clone()
            } else {
                match read(url) {
                    Some(identity) if identity.modified_at_ms <= at => Some(identity.key),
                    _ => None,
                }
            };
            records.insert(url.clone(), record);
        }
        self.records = records;
        self.observed_at = at;
    }

    /// Record the current identity of modules just evaluated again, leaving every other record
    /// unchanged.
    pub fn refresh(&mut self, urls: &[String], read: &dyn Fn(&str) -> Option<FileIdentity>) {
        for url in urls {
            self.records
                .insert(url.clone(), read(url).map(|identity| identity.key));
        }
    }
}

// ============================================================================================
// Watch — `watch-config.ts` and the `Hmr.watchConfig` wrapper
// ============================================================================================

/// What a `stat` says about one watch root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchRootState {
    /// A directory, watchable.
    Directory,
    /// Something that is not a directory.
    NotADirectory,
    /// The path does not exist yet.
    Missing,
}

/// A watch that can be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchRoot {
    /// The patch file's canonical path: root plus the path relative to it.
    pub filename: String,
    /// The canonical directory to watch.
    pub root: String,
    /// How many directories deep the target sits below the root, for the watcher's depth bound.
    pub depth: u32,
}

/// Why a watch root could not be planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchRootError {
    /// A parent exists and is not a directory.
    NotADirectory {
        /// The offending path.
        path: String,
    },
    /// No ancestor exists, up to the filesystem root.
    Missing {
        /// The path that was reached last.
        path: String,
    },
}

impl WatchRootError {
    /// The message upstream throws.
    pub fn message(&self) -> String {
        match self {
            Self::NotADirectory { path } => {
                format!("config watch parent is not a directory: {path}")
            }
            Self::Missing { path } => format!("config watch path has no existing ancestor: {path}"),
        }
    }
}

/// Find the nearest existing ancestor of a patch file and bound the watch to it.
///
/// A configuration watch must notice a patch file being *created*, so a missing parent is climbed
/// instead of rejected, and the count of climbed levels becomes the watcher's depth: subscribing to
/// the whole tree would report every sibling, and subscribing to none of the missing levels would
/// never fire. The canonical root is resolved *after* the climb, so a link in the chain is followed
/// exactly once.
///
/// `stat` answers [`WatchRootState`]; both it and `canonicalize` are injected.
pub fn find_watch_root(
    filename: &str,
    stat: &dyn Fn(&str) -> WatchRootState,
    canonicalize: &dyn Fn(&str) -> Option<String>,
) -> Result<WatchRoot, WatchRootError> {
    let mut root = dirname(filename);
    let mut depth = 0;
    loop {
        match stat(&root) {
            WatchRootState::Directory => {
                let canonical = canonicalize(&root).unwrap_or_else(|| root.clone());
                let relative = filename
                    .strip_prefix(&with_trailing_sep(&root))
                    .unwrap_or(filename);
                return Ok(WatchRoot {
                    filename: resolve(&canonical, relative),
                    root: canonical,
                    depth,
                });
            }
            WatchRootState::NotADirectory => {
                return Err(WatchRootError::NotADirectory { path: root });
            }
            WatchRootState::Missing => {
                let parent = dirname(&root);
                if parent == root {
                    return Err(WatchRootError::Missing { path: root });
                }
                root = parent;
                depth += 1;
            }
        }
    }
}

/// The set of patch paths a context has registered — upstream's `registrations` `WeakMap`.
///
/// Registration is keyed by the **canonical** path, and the same patch file named by its declared
/// and canonical spellings counts once: both spellings are registered together, so a second
/// registration of either is rejected. A disposer removes exactly the two paths it added.
#[derive(Clone, Debug, Default)]
pub struct ConfigRegistrations {
    paths: BTreeSet<String>,
}

impl ConfigRegistrations {
    /// No registrations.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a patch file by its declared and canonical spellings.
    ///
    /// Returns the two paths to release, or the offending declared path when either spelling is
    /// already registered.
    pub fn register(
        &mut self,
        declared: &str,
        canonical: &str,
    ) -> Result<(String, String), String> {
        if self.paths.contains(declared) || self.paths.contains(canonical) {
            return Err(declared.to_owned());
        }
        self.paths.insert(declared.to_owned());
        self.paths.insert(canonical.to_owned());
        Ok((declared.to_owned(), canonical.to_owned()))
    }

    /// Release a pair returned by [`Self::register`].
    pub fn release(&mut self, declared: &str, canonical: &str) {
        self.paths.remove(declared);
        self.paths.remove(canonical);
    }

    /// Whether a path is registered.
    pub fn contains(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    /// How many paths are registered (two per patch file).
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// The serialised refresh loop behind an exact-path watch.
///
/// Upstream's `state.dirty` plus `running` promise. The loop it enables is
///
/// ```text
/// let mut queue = RefreshQueue::new();
/// // on an event:
/// if queue.mark() { loop_it(); }
/// // loop_it:
/// while queue.take() { refresh() }   // refresh may mark again
/// queue.idle();
/// ```
///
/// and the three properties that shape it are each a bug avoided:
///
/// - **A change that arrives during a refresh is not lost.** [`Self::mark`] returns `false` when a
///   run is already active; the flag it sets is what [`Self::take`] sees on the next turn, so the
///   second change runs a second refresh instead of being dropped.
/// - **One refresh at a time.** `refresh` is called from the loop, never concurrently, so two
///   refreshes cannot interleave their reads of the same patch files.
/// - **The idle window is closed.** Upstream clears `running` in a `finally` that runs *after* its
///   `while (state.dirty)` test, so a mark landing between the test and the clear is lost. Here
///   [`Self::idle`] clears `running` only after `take` answered "nothing", which is the same code
///   path with the window removed — a mark either lands before `take` (and the loop runs) or after
///   `idle` (and `mark` returns `true` to start a new loop).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RefreshQueue {
    dirty: bool,
    running: bool,
}

impl RefreshQueue {
    /// An idle queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Note a change. Returns `true` when the caller must start the drain loop; `false` when a run
    /// is already active and will pick the change up.
    pub fn mark(&mut self) -> bool {
        self.dirty = true;
        if self.running {
            return false;
        }
        self.running = true;
        true
    }

    /// Take the next refresh. `true` means "run one", and clears the dirty flag first so a mark
    /// made *during* that run is still pending when the loop comes back around.
    pub fn take(&mut self) -> bool {
        if !self.dirty {
            return false;
        }
        self.dirty = false;
        true
    }

    /// End the loop. After this a [`Self::mark`] starts a new one.
    pub fn idle(&mut self) {
        self.running = false;
    }

    /// Run `refresh` once per pending change, serially, until nothing is left.
    ///
    /// For callers whose refresh cannot itself (re-)mark: a burst of marks coalesces to one run,
    /// and the count returned is what a caller asserts on. A refresh that needs to record another
    /// change uses [`Self::take`]/[`Self::idle`] directly.
    pub fn drain(&mut self, mut refresh: impl FnMut()) -> usize {
        let mut runs = 0;
        while self.take() {
            refresh();
            runs += 1;
        }
        self.idle();
        runs
    }

    /// Whether a drain is active.
    pub fn is_running(self) -> bool {
        self.running
    }

    /// Whether a change is waiting.
    pub fn is_dirty(self) -> bool {
        self.dirty
    }
}

/// Whether a watcher's reported path is worth delivering for an exact-path watch.
///
/// Upstream's `ignored` callback: a reported path is ignored unless it is the target or one of its
/// ancestors. Stated as the positive test so a caller cannot invert it by accident; the chokidar
/// option is its negation.
pub fn watcher_reaches(reported: &str, target: &str) -> bool {
    refresh_reaches(&normalize(reported), &normalize(target))
}

// ============================================================================================
// Build failures — `error.ts`'s classification
// ============================================================================================

/// A source position an esbuild failure points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLocation {
    /// The file the failure is in.
    pub file: String,
    /// One-based line.
    pub line: u64,
    /// Zero-based column, as babel's code frame expects.
    pub column: u64,
}

/// One diagnostic from a failed build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildDiagnostic {
    /// The failure text.
    pub text: String,
    /// Where it is, when the tool could say.
    pub location: Option<SourceLocation>,
}

impl BuildDiagnostic {
    /// The header upstream prefixes to a code frame: `File: {file}:{line}:{column}`.
    pub fn header(&self) -> String {
        match &self.location {
            None => self.text.clone(),
            Some(location) => format!(
                "File: {}:{}:{}\n{}",
                location.file, location.line, location.column, self.text
            ),
        }
    }
}

/// Recognise an esbuild `BuildFailure` and flatten it into diagnostics.
///
/// The shape check is upstream's and it is exact: a non-array `errors`, or one error whose `text`
/// is not a string, means this is *not* a build failure and must be logged as the original value
/// rather than half-read. A failure with no location is still a diagnostic — the text alone.
pub fn read_build_failure(value: &Value) -> Option<Vec<BuildDiagnostic>> {
    let errors = value.get("errors")?.as_array()?;
    let mut diagnostics = Vec::with_capacity(errors.len());
    for error in errors {
        let text = error.get("text")?.as_str()?.to_owned();
        let location = error.get("location").and_then(|location| {
            Some(SourceLocation {
                file: location.get("file")?.as_str()?.to_owned(),
                line: location.get("line")?.as_u64()?,
                column: location.get("column")?.as_u64()?,
            })
        });
        diagnostics.push(BuildDiagnostic { text, location });
    }
    Some(diagnostics)
}

// ============================================================================================
// Plugin → module resolution — `EntryNamespaces.resolve`
// ============================================================================================

/// What resolving a plugin name to its loaded module found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginModule {
    /// The name is not a loaded module of ours: the loader must resolve it.
    External,
    /// The loaded module's URL.
    Url(String),
}

/// The module a plugin name resolves to, across namespaces.
///
/// Upstream's `EntryNamespaces`. Two rules:
///
/// - a name with no namespace, or a `cordis:` pseudo-name, is handed to the loader — HMR cannot
///   locate a module it did not load;
/// - a name with a namespace uses that namespace's loaded URL, and *fails* otherwise, because the
///   namespace exists and its module is simply unknown; falling back to the loader there would
///   silently load a second copy.
///
/// `namespaces` maps name → (namespace id, url), and a name with several namespaces is exactly what
/// the second rule is about.
pub fn resolve_plugin_module(
    name: &str,
    base_url: &str,
    namespace: Option<&str>,
    urls: &BTreeMap<String, String>,
) -> Result<PluginModule, String> {
    if namespace.is_none() || name.starts_with("cordis:") {
        return Ok(PluginModule::External);
    }
    match namespace.and_then(|namespace| urls.get(namespace)) {
        Some(url) => Ok(PluginModule::Url(url.clone())),
        None => Err(format!(
            "HMR cannot locate the loaded module for {name} from {base_url}"
        )),
    }
}

/// The (name, namespace) pairs a plugin set contributes, skipping a `None` namespace only for
/// names that also have a loaded one.
///
/// Upstream's `EntryNamespaces[Symbol.iterator]`: an entry without a namespace is yielded when it is
/// the name's only one, and skipped when the same name has a loaded namespace — the loaded module
/// is the one worth reloading.
pub fn plugin_namespaces(
    names: &BTreeMap<String, Vec<Option<String>>>,
) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for (name, namespaces) in names {
        for namespace in namespaces {
            if namespace.is_none() && namespaces.len() > 1 {
                continue;
            }
            out.push((name.clone(), namespace.clone()));
        }
    }
    out
}
