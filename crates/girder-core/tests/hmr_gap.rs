//! Tests for `hmr_gap`: **what changed → which modules must be invalidated**.
//!
//! `packages/boot/hmr` is 1,740 lines across five files, and the split that matters is not size but
//! *what the code touches*. Two of the five are thin: `error.ts` (41) classifies an esbuild failure,
//! `watch-config.ts` (109) opens a chokidar watcher. The other three hold every decision, and all of
//! those decisions are set and graph algebra over values a caller supplies — which is what makes
//! them portable, and what this file asserts.
//!
//! The properties below are ordered by how quietly they fail:
//!
//! - **Accepted walks forward, evicted walks backward.** `analyzeChanges` follows `imports`; the
//!   eviction set follows `imported-by`. Swapping them still produces plausible sets, and produces a
//!   reload that leaves the old namespace reachable — the exact bug HMR exists to avoid.
//! - **Eviction is transitive.** A module two hops from the changed file still holds a binding into
//!   it. A non-transitive walk evicts the direct importer and keeps the stale grandparent.
//! - **A host module is never replaced in place.** The CLI's own static graph is not a plugin, so a
//!   change inside it must be reported as pending for a restart, not evicted under the process.
//! - **An undecided cycle is declined, not accepted.** Re-evaluating a module whose dependencies did
//!   not change is an infinite reload loop.
//! - **Excluded modules are cut from both walks.** `node:` has no file and `node_modules` is not the
//!   profile's code; either one accepted would reload dependencies on every keystroke.
//! - **A coarse timestamp is a change.** FAT's two-second granularity means an unrecorded file
//!   written within the window before an observation cannot be proven older than it.
//! - **Invalidation is sticky.** HMR never removes an expired package directory, so one that changed
//!   once keeps being read from disk.
//! - **A configured watch path is skipped before every other rule.** A patch file that is also a
//!   cached module is refreshed by the config watch and not reloaded as code.
//!
//! Nothing here starts a watcher, touches a file, or reads a clock: `stat`, `realpath` and the
//! manifest read are parameters, so every assertion is arithmetic. The module is compiled by path,
//! which is why it spells the crate as `girder_core::` even in its own source.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/hmr_gap.rs"]
mod hmr_gap;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;

use hmr_gap::{
    ChangeAction, ChangeAnalysis, ConfigRegistrations, ExpiredDirectories, FileIdentity, Graph,
    ImportContext, InvalidPackageConfig, ModuleFiles, NearestManifest, PackageConfig, PackageType,
    PendingReason, PlanInput, PluginEntry, PluginModule, RefreshQueue, TIMESTAMP_RESOLUTION_MS,
    WatchRoot, WatchRootError, WatchRootState, WatchState, affected_modules, analyze_changes,
    basename, canonical_path, classify_change, dirname, dropped_modules, file_url_to_path,
    find_watch_root, identity_path, is_excluded, is_within, join, load_dependencies, mapping_roots,
    namespaced_path, nearest_package_config, normalize, package_manifests, path_to_file_url,
    plan_replacement, plugin_namespaces, read_build_failure, read_package_config, refresh_reaches,
    resolve_plugin_module, select_reloads, watcher_reaches, with_trailing_sep,
};

// -------------------------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------------------------

/// The same, for call sites that build owned strings (`url`, `with_trailing_sep`).
fn set_of<I, S>(items: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    items.into_iter().map(Into::into).collect()
}

fn url(path: &str) -> String {
    path_to_file_url(path)
}

/// `module -> imports`: the direction `analyze_changes` walks.
fn graph(edges: &[(&str, &str)]) -> Graph {
    let mut graph = Graph::new();
    for (from, to) in edges {
        graph.insert(url(from), url(to));
    }
    graph
}

/// The graph inverted: `imported-by` from `imports`.
fn invert(imports: &Graph) -> Graph {
    let mut inverted = Graph::new();
    for from in imports.sources() {
        let from = from.to_owned();
        for to in imports.edges(&from) {
            inverted.insert(to.to_owned(), from.clone());
        }
    }
    inverted
}

fn not_on_disk_path(_: &str) -> Result<String, ()> {
    Err(())
}

fn no_manifests(_: &str) -> bool {
    false
}

fn all_manifests(_: &str) -> bool {
    true
}

fn not_on_disk(_: &str) -> Option<String> {
    None
}

fn always_dir(_: &str) -> WatchRootState {
    WatchRootState::Directory
}

// -------------------------------------------------------------------------------------------
// Paths: the arithmetic every containment test rests on
// -------------------------------------------------------------------------------------------

#[test]
fn normalization_collapses_dots_and_repeated_separators() {
    assert_eq!(normalize("/w/./a//b/../c"), "/w/a/c");
    assert_eq!(normalize("/w/a/b/../../../.."), "/");
    assert_eq!(normalize("a/./b"), "a/b");
}

#[test]
fn normalization_keeps_leading_parent_segments_of_a_relative_path() {
    // `path.resolve` on a relative path climbs from the cwd; a normaliser has no cwd, so the only
    // answer that stays truthful is to keep the segments it cannot resolve.
    assert_eq!(normalize("a/../.."), "..");
    assert_eq!(normalize("../a/../b"), "../b");
    assert_eq!(normalize("."), ".");
}

#[test]
fn dirname_and_basename_match_node_path() {
    assert_eq!(dirname("/w/a/b.js"), "/w/a");
    assert_eq!(dirname("/w"), "/");
    assert_eq!(dirname("/"), "/");
    assert_eq!(dirname("rel.js"), ".");
    assert_eq!(dirname("/w/a/b/"), "/w/a");
    assert_eq!(basename("/w/a/b.js"), "b.js");
    assert_eq!(basename("/"), "");
}

#[test]
fn join_does_not_normalize() {
    // Normalising here would hide a caller's unnormalised base; `resolve` is where it happens.
    assert_eq!(join("/w/a", "b.js"), "/w/a/b.js");
    assert_eq!(join("/w/a/", "b.js"), "/w/a/b.js");
}

#[test]
fn resolve_prefers_an_absolute_second_argument() {
    assert_eq!(hmr_gap::resolve("/base", "/abs/x.js"), "/abs/x.js");
    assert_eq!(hmr_gap::resolve("/base", "./rel/x.js"), "/base/rel/x.js");
    assert_eq!(hmr_gap::resolve("/base/", "../x.js"), "/x.js");
}

#[test]
fn a_trailing_separator_keeps_a_prefix_sibling_out() {
    assert_eq!(with_trailing_sep("/w/pkg"), "/w/pkg/");
    assert!(is_within("/w/pkg/a.js", "/w/pkg/"));
    assert!(is_within("/w/pkg", "/w/pkg/"));
    assert!(!is_within("/w/pkg-extra/a.js", "/w/pkg/"));
    assert!(!is_within("/w/other", "/w/pkg/"));
}

#[test]
fn refresh_reaches_accepts_the_target_and_its_ancestors_only() {
    assert!(refresh_reaches("/w", "/w/package.json"));
    assert!(refresh_reaches("/w/conf", "/w/conf/dsh.patch.yml"));
    assert!(refresh_reaches(
        "/w/conf/dsh.patch.yml",
        "/w/conf/dsh.patch.yml"
    ));
    assert!(!refresh_reaches(
        "/w/conf/other.yml",
        "/w/conf/dsh.patch.yml"
    ));
    assert!(!refresh_reaches("/w", "/w2/package.json"));
}

#[test]
fn canonical_path_falls_back_to_the_parent_spelling() {
    // A patch file that does not exist yet still has to be addressable.
    let answer = canonical_path("/w/missing/patch.yml", &|path| match path {
        "/w/missing" => Ok("/real/missing".to_owned()),
        _ => Err(()),
    })
    .unwrap();
    assert_eq!(answer, "/real/missing/patch.yml");
}

#[test]
fn canonical_path_gives_up_at_the_root() {
    assert!(canonical_path("/", &not_on_disk_path).is_err());
}

#[test]
fn namespaced_path_is_the_identity_here() {
    // Upstream changes this only on Windows, where 8.3 short names need a namespace prefix.
    assert_eq!(namespaced_path("/w/a.js"), "/w/a.js");
}

#[test]
fn file_urls_round_trip_through_a_percent_encoded_path() {
    let path = "/w/a file #1%.mjs";
    let encoded = path_to_file_url(path);
    assert_eq!(encoded, "file:///w/a%20file%20%231%25.mjs");
    assert_eq!(file_url_to_path(&encoded).unwrap(), path);
}

#[test]
fn a_file_url_with_a_host_still_decodes_its_path() {
    assert_eq!(file_url_to_path("file://host/w/a.js").unwrap(), "/w/a.js");
    assert_eq!(file_url_to_path("file:///w/a.js").unwrap(), "/w/a.js");
    assert_eq!(file_url_to_path("https://example.com/a.js"), None);
    assert_eq!(file_url_to_path("file:///w/%zz.js"), None);
}

// -------------------------------------------------------------------------------------------
// Change dispatch: upstream's five-way decision
// -------------------------------------------------------------------------------------------

/// A dispatcher over an empty watch base.
struct Fixture {
    config_paths: BTreeSet<String>,
    externals: BTreeSet<String>,
    cached: BTreeSet<String>,
    includes: Vec<String>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            config_paths: BTreeSet::new(),
            externals: BTreeSet::new(),
            cached: BTreeSet::new(),
            includes: Vec::new(),
        }
    }

    fn classify(&self, event: &str) -> hmr_gap::ClassifiedChange {
        let state = WatchState {
            base_dir: "/w",
            configured_base: "/w",
            config_paths: &self.config_paths,
            externals: &self.externals,
            cached: &self.cached,
            includes: &self.includes,
            canonicalize: identity_path,
        };
        classify_change(&state, event)
    }
}

#[test]
fn an_unowned_change_is_emitted() {
    let change = Fixture::new().classify("src/readme.md");
    assert_eq!(change.action, ChangeAction::Emit);
    assert_eq!(change.filename, "/w/src/readme.md");
    assert_eq!(change.url, "file:///w/src/readme.md");
    assert_eq!(change.manifest, None);
}

#[test]
fn a_configured_path_is_skipped_before_every_other_rule() {
    let mut fixture = Fixture::new();
    // The same file is a configured watch path *and* an external and a cached module.
    fixture.config_paths.insert("/w/dsh.patch.yml".to_owned());
    fixture.externals.insert(url("/w/dsh.patch.yml"));
    fixture.cached.insert(url("/w/dsh.patch.yml"));
    assert_eq!(
        fixture.classify("dsh.patch.yml").action,
        ChangeAction::Configured
    );
}

#[test]
fn a_configured_path_matches_either_spelling() {
    let mut fixture = Fixture::new();
    fixture
        .config_paths
        .insert("/elsewhere/dsh.patch.yml".to_owned());
    let state = WatchState {
        base_dir: "/w",
        configured_base: "/elsewhere",
        config_paths: &fixture.config_paths,
        externals: &fixture.externals,
        cached: &fixture.cached,
        includes: &fixture.includes,
        canonicalize: identity_path,
    };
    let change = classify_change(&state, "dsh.patch.yml");
    assert_eq!(change.action, ChangeAction::Configured);
    assert_eq!(change.configured, "/elsewhere/dsh.patch.yml");
}

#[test]
fn an_external_wins_over_the_module_cache() {
    let mut fixture = Fixture::new();
    fixture.externals.insert(url("/w/cli/main.js"));
    fixture.cached.insert(url("/w/cli/main.js"));
    assert_eq!(fixture.classify("cli/main.js").action, ChangeAction::Reload);
}

#[test]
fn a_cached_module_is_stashed() {
    let mut fixture = Fixture::new();
    fixture.cached.insert(url("/w/plugins/a/index.js"));
    let change = fixture.classify("plugins/a/index.js");
    assert_eq!(change.action, ChangeAction::Stash);
    assert_eq!(change.manifest, None);
}

#[test]
fn a_package_manifest_expires_even_when_it_is_also_cached() {
    let mut fixture = Fixture::new();
    fixture.cached.insert(url("/w/plugins/a/package.json"));
    let change = fixture.classify("plugins/a/package.json");
    // Stash wins for the module cache, but the manifest is still recorded for expiry.
    assert_eq!(change.action, ChangeAction::Stash);
    assert_eq!(
        change.manifest.as_deref(),
        Some("/w/plugins/a/package.json")
    );
}

#[test]
fn a_manifest_with_no_cached_module_is_only_a_manifest_change() {
    let change = Fixture::new().classify("plugins/a/package.json");
    assert_eq!(change.action, ChangeAction::Manifest);
    assert_eq!(
        change.manifest.as_deref(),
        Some("/w/plugins/a/package.json")
    );
}

#[test]
fn a_manifest_under_node_modules_is_not_expired() {
    // Node owns dependency manifests; expiring them would fight the package manager.
    let change = Fixture::new().classify("node_modules/a/package.json");
    assert_eq!(change.action, ChangeAction::Emit);
    assert_eq!(change.manifest, None);
}

#[test]
fn an_include_is_refreshed_by_either_spelling() {
    let mut fixture = Fixture::new();
    fixture.includes.push("/w/plugins/a/cordis.yml".to_owned());
    assert_eq!(
        fixture.classify("plugins/a/cordis.yml").action,
        ChangeAction::Include
    );

    let mut configured = Fixture::new();
    configured
        .includes
        .push("/elsewhere/plugins/a/cordis.yml".to_owned());
    let state = WatchState {
        base_dir: "/w",
        configured_base: "/elsewhere",
        config_paths: &configured.config_paths,
        externals: &configured.externals,
        cached: &configured.cached,
        includes: &configured.includes,
        canonicalize: identity_path,
    };
    assert_eq!(
        classify_change(&state, "plugins/a/cordis.yml").action,
        ChangeAction::Include
    );
}

#[test]
fn a_cached_module_beats_include_membership() {
    // An include's subtree file that is also a cached module is code, and code reloads.
    let mut fixture = Fixture::new();
    fixture.includes.push("/w/plugins/a/index.js".to_owned());
    fixture.cached.insert(url("/w/plugins/a/index.js"));
    assert_eq!(
        fixture.classify("plugins/a/index.js").action,
        ChangeAction::Stash
    );
}

#[test]
fn the_canonical_spelling_decides_containment() {
    // The watcher reports the link spelling; the module caches hold the real one.
    let fixture = Fixture::new();
    let state = WatchState {
        base_dir: "/w",
        configured_base: "/w",
        config_paths: &fixture.config_paths,
        externals: &fixture.externals,
        cached: &fixture.cached,
        includes: &fixture.includes,
        canonicalize: |path| Ok(path.replace("/w/link", "/real/pkg")),
    };
    let change = classify_change(&state, "link/package.json");
    assert_eq!(change.filename, "/real/pkg/package.json");
    assert_eq!(change.manifest.as_deref(), Some("/real/pkg/package.json"));
}

// -------------------------------------------------------------------------------------------
// The hot-reload gate: accepted walks `imports`
// -------------------------------------------------------------------------------------------

#[test]
fn a_dependency_of_a_changed_module_is_accepted() {
    // `a` imports `b`; `b` changed. `b` is seeded; `a` is *not* reachable forward, so `a` stays put.
    let imports = graph(&[("/w/a.js", "/w/b.js")]);
    let analysis = analyze_changes(&imports, &set_of([url("/w/b.js")]), &BTreeSet::new());
    assert!(analysis.accepted.contains(&url("/w/b.js")));
    assert!(!analysis.accepted.contains(&url("/w/a.js")));
}

#[test]
fn the_dependencies_of_a_changed_module_are_classified() {
    // Upstream seeds the worklist from the changed module's own `linked` jobs, so it directly
    // imports `changed`, `cycleA`, `leaf` and a builtin; only `parent` (which imports `changed`)
    // therefore ends up accepted, and the cycle stays declined.
    let mut imports = graph(&[
        ("/w/changed.mjs", "/w/parent.mjs"),
        ("/w/changed.mjs", "/w/cycleA.mjs"),
        ("/w/changed.mjs", "/w/leaf.mjs"),
        ("/w/parent.mjs", "/w/changed.mjs"),
        ("/w/cycleA.mjs", "/w/cycleB.mjs"),
        ("/w/cycleB.mjs", "/w/cycleA.mjs"),
    ]);
    // A builtin is not a `file:` URL, so it enters the graph verbatim and the exclusion applies.
    imports.insert(url("/w/changed.mjs"), "node:fs");
    let analysis = analyze_changes(&imports, &set_of([url("/w/changed.mjs")]), &BTreeSet::new());
    assert_eq!(
        analysis.accepted,
        set_of([url("/w/changed.mjs"), url("/w/parent.mjs")])
    );
    assert_eq!(
        analysis.declined,
        set_of([
            url("/w/leaf.mjs"),
            url("/w/cycleA.mjs"),
            url("/w/cycleB.mjs")
        ])
    );
}

#[test]
fn acceptance_is_transitive_back_to_the_changed_module() {
    // changed -> ... is not the direction; a module that imports an *accepted* module joins it.
    let imports = graph(&[
        ("/w/changed.mjs", "/w/mid.mjs"),
        ("/w/mid.mjs", "/w/top.mjs"),
        ("/w/top.mjs", "/w/changed.mjs"),
    ]);
    let analysis = analyze_changes(&imports, &set_of([url("/w/changed.mjs")]), &BTreeSet::new());
    assert!(analysis.accepted.contains(&url("/w/changed.mjs")));
    assert!(
        analysis.accepted.contains(&url("/w/mid.mjs"))
            || analysis.accepted.contains(&url("/w/top.mjs"))
    );
}

#[test]
fn a_module_whose_imports_are_all_declined_is_declined() {
    let imports = graph(&[("/w/changed.mjs", "/w/a.js"), ("/w/a.js", "/w/b.js")]);
    let mut externals = BTreeSet::new();
    externals.insert(url("/w/b.js"));
    let analysis = analyze_changes(&imports, &set_of([url("/w/changed.mjs")]), &externals);
    assert!(analysis.declined.contains(&url("/w/a.js")));
}

#[test]
fn one_accepted_import_is_enough() {
    // `changed` imports both `a` and `c`; `c` closes over `changed`, so it is accepted, and `a`
    // (which imports `c`) is accepted with it rather than declined.
    let imports = graph(&[
        ("/w/changed.mjs", "/w/a.js"),
        ("/w/changed.mjs", "/w/c.js"),
        ("/w/a.js", "/w/c.js"),
        ("/w/c.js", "/w/changed.mjs"),
    ]);
    let analysis = analyze_changes(&imports, &set_of([url("/w/changed.mjs")]), &BTreeSet::new());
    assert!(analysis.accepted.contains(&url("/w/c.js")));
    assert!(analysis.accepted.contains(&url("/w/a.js")));
    assert!(!analysis.declined.contains(&url("/w/a.js")));
}

#[test]
fn excluded_modules_are_cut_from_classification() {
    // `changed` imports only a dependency: the dependency is excluded, nothing is pending, and no
    // module is accepted beyond the changed one.
    let imports = graph(&[("/w/changed.mjs", "file:///w/node_modules/dep/index.js")]);
    let analysis = analyze_changes(&imports, &set_of([url("/w/changed.mjs")]), &BTreeSet::new());
    assert_eq!(analysis.accepted, set_of([url("/w/changed.mjs")]));
    assert!(analysis.declined.is_empty());
}

#[test]
fn a_cycle_that_reaches_no_change_is_declined() {
    let imports = graph(&[
        ("/w/changed.mjs", "/w/cycleA.mjs"),
        ("/w/cycleA.mjs", "/w/cycleB.mjs"),
        ("/w/cycleB.mjs", "/w/cycleA.mjs"),
    ]);
    let analysis = analyze_changes(&imports, &set_of([url("/w/changed.mjs")]), &BTreeSet::new());
    assert!(!analysis.accepted.contains(&url("/w/cycleA.mjs")));
    assert!(!analysis.accepted.contains(&url("/w/cycleB.mjs")));
    assert!(analysis.declined.contains(&url("/w/cycleA.mjs")));
    assert!(analysis.declined.contains(&url("/w/cycleB.mjs")));
}

#[test]
fn a_cycle_that_reaches_a_change_is_accepted() {
    let imports = graph(&[
        ("/w/changed.mjs", "/w/a.js"),
        ("/w/a.js", "/w/b.js"),
        ("/w/b.js", "/w/a.js"),
        ("/w/b.js", "/w/changed.mjs"),
    ]);
    let analysis = analyze_changes(&imports, &set_of([url("/w/changed.mjs")]), &BTreeSet::new());
    assert!(analysis.accepted.contains(&url("/w/a.js")));
    assert!(analysis.accepted.contains(&url("/w/b.js")));
}

#[test]
fn a_changed_module_is_accepted_even_when_it_has_no_edges() {
    let analysis = analyze_changes(&Graph::new(), &set_of([url("/w/a.js")]), &BTreeSet::new());
    assert_eq!(
        analysis,
        ChangeAnalysis {
            accepted: set_of([url("/w/a.js")]),
            declined: BTreeSet::new(),
        }
    );
}

#[test]
fn an_external_is_declined_even_when_it_is_reachable() {
    let imports = graph(&[("/w/changed.mjs", "/w/a.js"), ("/w/a.js", "/w/main.js")]);
    let mut externals = BTreeSet::new();
    externals.insert(url("/w/main.js"));
    let analysis = analyze_changes(&imports, &set_of([url("/w/changed.mjs")]), &externals);
    // Declined is seeded with externals and acceptance never removes it: a host module is not
    // re-evaluated no matter what imports it.
    assert!(analysis.declined.contains(&url("/w/main.js")));
}

// -------------------------------------------------------------------------------------------
// Eviction: backward over `imported-by`, and transitive
// -------------------------------------------------------------------------------------------

#[test]
fn eviction_includes_the_changed_module_itself() {
    let affected = affected_modules(&Graph::new(), &set_of([url("/w/a.js")]));
    assert_eq!(affected, set_of([url("/w/a.js")]));
}

#[test]
fn eviction_is_transitive_over_importers() {
    let imports = graph(&[("/w/a.js", "/w/b.js"), ("/w/b.js", "/w/c.js")]);
    let imported_by = invert(&imports);
    let affected = affected_modules(&imported_by, &set_of([url("/w/c.js")]));
    for module in ["/w/a.js", "/w/b.js", "/w/c.js"] {
        assert!(affected.contains(&url(module)), "{module}");
    }
}

#[test]
fn eviction_follows_importers_not_imports() {
    // `a` imports `b`. Changing `b` evicts `a`; changing `a` must NOT evict `b`.
    let imports = graph(&[("/w/a.js", "/w/b.js")]);
    let imported_by = invert(&imports);
    let affected = affected_modules(&imported_by, &set_of([url("/w/a.js")]));
    assert_eq!(affected, set_of([url("/w/a.js")]));
    assert!(!affected.contains(&url("/w/b.js")));
}

#[test]
fn eviction_of_a_cycle_terminates() {
    let imports = graph(&[("/w/a.js", "/w/b.js"), ("/w/b.js", "/w/a.js")]);
    let imported_by = invert(&imports);
    let affected = affected_modules(&imported_by, &set_of([url("/w/a.js")]));
    assert_eq!(affected, set_of([url("/w/a.js"), url("/w/b.js")]));
}

#[test]
fn a_diamond_evicts_both_branches_once() {
    let imports = graph(&[
        ("/w/top.js", "/w/left.js"),
        ("/w/top.js", "/w/right.js"),
        ("/w/left.js", "/w/leaf.js"),
        ("/w/right.js", "/w/leaf.js"),
    ]);
    let imported_by = invert(&imports);
    let affected = affected_modules(&imported_by, &set_of([url("/w/leaf.js")]));
    assert_eq!(
        affected,
        set_of([
            url("/w/leaf.js"),
            url("/w/left.js"),
            url("/w/right.js"),
            url("/w/top.js"),
        ])
    );
}

// -------------------------------------------------------------------------------------------
// Replacement planning: eviction plus ownership
// -------------------------------------------------------------------------------------------

/// One fiber: its id and the Loader entry that owns it.
type FiberSpec<'a> = (&'a str, Option<&'a str>);

/// `(module, plugin, fibers)`.
type ExportSpec<'a> = (&'a str, &'a str, &'a [FiberSpec<'a>]);

fn exports_of(entries: &[ExportSpec<'_>]) -> BTreeMap<String, Vec<hmr_gap::ExportedPlugin>> {
    let mut map: BTreeMap<String, Vec<hmr_gap::ExportedPlugin>> = BTreeMap::new();
    for (module, plugin, fibers) in entries {
        map.entry(url(module))
            .or_default()
            .push(hmr_gap::ExportedPlugin {
                plugin: (*plugin).to_owned(),
                fibers: fibers
                    .iter()
                    .map(|(id, owner)| hmr_gap::FiberRef {
                        id: (*id).to_owned(),
                        owner: owner.map(str::to_owned),
                    })
                    .collect(),
            });
    }
    map
}

#[test]
fn a_managed_plugin_is_evicted_with_its_manifests_and_fibers() {
    let imports = graph(&[("/w/plugins/a/index.js", "/w/plugins/a/lib.js")]);
    let imported_by = invert(&imports);
    let exports = exports_of(&[(
        "/w/plugins/a/lib.js",
        "plugin-a",
        &[("f1", Some("entry-a"))],
    )]);
    let host = BTreeSet::new();
    let managed = set_of(["entry-a"]);
    let roots = [with_trailing_sep("/w/")];
    let input = PlanInput {
        imported_by: &imported_by,
        host: &host,
        exports: &exports,
        managed: &managed,
        roots: &roots,
        manifests: &all_manifests,
    };
    let plan = plan_replacement(&[url("/w/plugins/a/lib.js")], &input);
    assert!(plan.pending.is_empty());
    assert!(plan.evicted.contains(&url("/w/plugins/a/index.js")));
    assert!(plan.evicted.contains(&url("/w/plugins/a/lib.js")));
    assert_eq!(plan.plugins, set_of(["plugin-a"]));
    assert_eq!(plan.fibers, set_of(["f1"]));
    // The walk climbs from the module's directory to the root *inclusive*, so the root's own
    // manifest expires too.
    assert_eq!(
        plan.manifests,
        set_of([
            "/w/package.json",
            "/w/plugins/package.json",
            "/w/plugins/a/package.json",
        ])
    );
}

#[test]
fn a_host_module_is_pending_rather_than_evicted() {
    let imports = graph(&[("/w/cli/main.js", "/w/plugins/a/lib.js")]);
    let imported_by = invert(&imports);
    let exports = exports_of(&[]);
    // The *importer* is in the host graph: evicting it is as fatal as evicting the changed file.
    let host = set_of([url("/w/cli/main.js")]);
    let managed = BTreeSet::new();
    let roots = [with_trailing_sep("/w/")];
    let input = PlanInput {
        imported_by: &imported_by,
        host: &host,
        exports: &exports,
        managed: &managed,
        roots: &roots,
        manifests: &all_manifests,
    };
    let plan = plan_replacement(&[url("/w/plugins/a/lib.js")], &input);
    assert_eq!(
        plan.pending,
        vec![hmr_gap::PendingModule {
            url: url("/w/plugins/a/lib.js"),
            reason: PendingReason::HostModule,
        }]
    );
    assert!(plan.evicted.is_empty());
    assert!(plan.manifests.is_empty());
    assert_eq!(PendingReason::HostModule.as_str(), "host-module");
}

#[test]
fn a_plugin_with_no_managed_owner_is_pending() {
    let imports = graph(&[("/w/plugins/a/index.js", "/w/plugins/a/lib.js")]);
    let imported_by = invert(&imports);
    let exports = exports_of(&[("/w/plugins/a/index.js", "plugin-a", &[("f1", None)])]);
    let host = BTreeSet::new();
    let managed = set_of(["entry-a"]);
    let roots = [with_trailing_sep("/w/")];
    let input = PlanInput {
        imported_by: &imported_by,
        host: &host,
        exports: &exports,
        managed: &managed,
        roots: &roots,
        manifests: &all_manifests,
    };
    let plan = plan_replacement(&[url("/w/plugins/a/lib.js")], &input);
    assert_eq!(plan.pending.len(), 1);
    assert_eq!(plan.pending[0].reason, PendingReason::UnmanagedPlugin);
    assert_eq!(PendingReason::UnmanagedPlugin.as_str(), "unmanaged-plugin");
    assert!(plan.evicted.is_empty());
}

#[test]
fn a_partially_owned_plugin_set_is_still_unmanaged() {
    // One fiber with an entry and one without: the un-owned one cannot be restarted.
    let imports = graph(&[("/w/plugins/a/index.js", "/w/plugins/a/lib.js")]);
    let imported_by = invert(&imports);
    let exports = exports_of(&[(
        "/w/plugins/a/index.js",
        "plugin-a",
        &[("f1", Some("entry-a")), ("f2", None)],
    )]);
    let host = BTreeSet::new();
    let managed = set_of(["entry-a"]);
    let roots = [with_trailing_sep("/w/")];
    let input = PlanInput {
        imported_by: &imported_by,
        host: &host,
        exports: &exports,
        managed: &managed,
        roots: &roots,
        manifests: &all_manifests,
    };
    let plan = plan_replacement(&[url("/w/plugins/a/lib.js")], &input);
    assert_eq!(plan.pending[0].reason, PendingReason::UnmanagedPlugin);
}

#[test]
fn a_plugin_exported_by_two_affected_modules_is_stopped_once() {
    let imports = graph(&[("/w/plugins/a/index.js", "/w/plugins/a/lib.js")]);
    let imported_by = invert(&imports);
    let exports = exports_of(&[
        (
            "/w/plugins/a/index.js",
            "plugin-a",
            &[("f1", Some("entry-a"))],
        ),
        (
            "/w/plugins/a/lib.js",
            "plugin-a",
            &[("f1", Some("entry-a"))],
        ),
    ]);
    let host = BTreeSet::new();
    let managed = set_of(["entry-a"]);
    let roots = [with_trailing_sep("/w/")];
    let input = PlanInput {
        imported_by: &imported_by,
        host: &host,
        exports: &exports,
        managed: &managed,
        roots: &roots,
        manifests: &all_manifests,
    };
    let plan = plan_replacement(&[url("/w/plugins/a/lib.js")], &input);
    assert_eq!(plan.plugins, set_of(["plugin-a"]));
    assert_eq!(plan.fibers, set_of(["f1"]));
}

#[test]
fn a_manifest_is_expired_only_below_a_root() {
    // `/etc/package.json` is above the profile: it belongs to the installation, not the profile.
    let manifests = package_manifests(
        &url("/w/plugins/a/lib.js"),
        &[with_trailing_sep("/w/plugins/")],
        &all_manifests,
    );
    // `/w/plugins` is a root, so its own manifest is included; `/w` is above it and is not.
    assert_eq!(
        manifests,
        vec!["/w/plugins/a/package.json", "/w/plugins/package.json"]
    );
    assert!(
        package_manifests(
            &url("/etc/x.js"),
            &[with_trailing_sep("/w/")],
            &all_manifests
        )
        .is_empty()
    );
}

#[test]
fn a_nested_package_expires_every_manifest_above_it() {
    let manifests = package_manifests(
        &url("/w/plugins/a/node_modules/b/lib.js"),
        &[with_trailing_sep("/w/")],
        &all_manifests,
    );
    assert_eq!(
        manifests,
        vec![
            "/w/plugins/a/node_modules/b/package.json",
            "/w/plugins/a/node_modules/package.json",
            "/w/plugins/a/package.json",
            "/w/plugins/package.json",
            "/w/package.json",
        ]
    );
}

#[test]
fn a_missing_manifest_is_not_expired() {
    assert!(
        package_manifests(&url("/w/a.js"), &[with_trailing_sep("/w/")], &no_manifests).is_empty()
    );
}

// -------------------------------------------------------------------------------------------
// Dropped mappings: what a disabled package unloads
// -------------------------------------------------------------------------------------------

#[test]
fn a_dropped_mapping_evicts_its_modules_but_not_the_hosts() {
    let dropped = set_of([with_trailing_sep("/w/node_modules/old")]);
    let kept = set_of([with_trailing_sep("/w/node_modules/new")]);
    let host = set_of([url("/w/node_modules/old/host.js")]);
    let urls = [
        url("/w/node_modules/old/a.js"),
        url("/w/node_modules/old/host.js"),
        url("/w/node_modules/new/b.js"),
    ];
    let evicted = dropped_modules(urls.iter().map(String::as_str), &dropped, &kept, &host);
    assert_eq!(evicted, vec![url("/w/node_modules/old/a.js")]);
}

#[test]
fn a_moved_package_still_mapped_stays_loaded() {
    // Dropped *and* kept (a retargeted link): evicting would re-evaluate unchanged code.
    let dropped = set_of([with_trailing_sep("/w/node_modules/a")]);
    let kept = set_of([with_trailing_sep("/w/node_modules/a")]);
    let urls = [url("/w/node_modules/a/index.js")];
    assert!(
        dropped_modules(
            urls.iter().map(String::as_str),
            &dropped,
            &kept,
            &BTreeSet::new()
        )
        .is_empty()
    );
}

#[test]
fn mapping_roots_are_deduplicated_and_trailing() {
    let roots = mapping_roots(vec!["/w/link".to_owned(), "/w/plain".to_owned()], &|path| {
        if path == "/w/link" {
            vec!["/real/a".to_owned(), "/real/b".to_owned()]
        } else {
            vec![path.to_owned()]
        }
    });
    assert_eq!(roots, set_of(["/real/a/", "/real/b/", "/w/plain/"]));
}

// -------------------------------------------------------------------------------------------
// Package manifests: validation, normalisation, and the nearest-manifest walk
// -------------------------------------------------------------------------------------------

#[test]
fn a_missing_manifest_is_absent_configuration_not_an_error() {
    assert_eq!(read_package_config("/w/package.json", None, None), Ok(None));
}

#[test]
fn a_manifest_is_normalised_into_the_fields_node_consumes() {
    let source = br##"{"name":"a","main":"index.js","type":"module","exports":{".":"./i.js"},"imports":"#x"}"##;
    let config = read_package_config("/w/package.json", Some(source), None)
        .unwrap()
        .unwrap();
    assert_eq!(config.name.as_deref(), Some("a"));
    assert_eq!(config.main.as_deref(), Some("index.js"));
    assert_eq!(config.kind, PackageType::Module);
    assert_eq!(config.kind.as_str(), "module");
    assert_eq!(config.exports, Some(json!({".":"./i.js"})));
    assert_eq!(config.imports, Some(json!("#x")));
}

#[test]
fn an_unknown_type_normalises_to_none() {
    let config = read_package_config("/w/package.json", Some(br#"{"type":"weird"}"#), None)
        .unwrap()
        .unwrap();
    assert_eq!(config.kind, PackageType::None);
    assert_eq!(config.kind.as_str(), "none");
}

#[test]
fn a_non_string_main_is_dropped_not_an_error() {
    let config = read_package_config("/w/package.json", Some(br#"{"main":7}"#), None)
        .unwrap()
        .unwrap();
    assert_eq!(config.main, None);
}

#[test]
fn a_non_string_or_non_object_exports_is_dropped() {
    let config = read_package_config(
        "/w/package.json",
        Some(br#"{"exports":[],"imports":3}"#),
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(config.exports, None);
    assert_eq!(config.imports, None);
}

#[test]
fn a_bom_prefixed_manifest_parses() {
    let mut source = vec![0xef, 0xbb, 0xbf];
    source.extend_from_slice(br#"{"name":"a"}"#);
    let config = read_package_config("/w/package.json", Some(&source), None)
        .unwrap()
        .unwrap();
    assert_eq!(config.name.as_deref(), Some("a"));
}

#[test]
fn invalid_utf8_is_refused() {
    let error = read_package_config("/w/package.json", Some(&[0xff, 0xfe]), None).unwrap_err();
    assert_eq!(error.code(), InvalidPackageConfig::CODE);
    assert_eq!(error.message, "Invalid package config /w/package.json.");
}

#[test]
fn unparsable_or_non_object_manifests_are_refused() {
    for source in [
        &b"{not json"[..],
        &b"[]"[..],
        &b"null"[..],
        &b"\"text\""[..],
    ] {
        let error = read_package_config("/w/package.json", Some(source), None).unwrap_err();
        assert!(
            matches!(
                error,
                InvalidPackageConfig {
                    ref path,
                    ..
                } if path == "/w/package.json"
            ),
            "{source:?}"
        );
    }
}

#[test]
fn a_non_string_name_or_type_is_refused() {
    for source in [&b"{\"name\":1}"[..], &b"{\"type\":true}"[..]] {
        assert!(read_package_config("/w/package.json", Some(source), None).is_err());
    }
}

#[test]
fn a_lone_surrogate_never_reaches_the_reader() {
    // Upstream errors on a string field holding a lone surrogate. Rust cannot represent one, and
    // `serde_json` rejects the escape, so the same input is refused for the same reason one layer
    // earlier — asserted here so the guarantee is pinned rather than assumed.
    let error =
        read_package_config("/w/package.json", Some(br#"{"name":"\ud800"}"#), None).unwrap_err();
    assert_eq!(error.code(), InvalidPackageConfig::CODE);
}

#[test]
fn the_error_text_names_the_importer() {
    let context = ImportContext {
        base: "file:///w/a.js".to_owned(),
        specifier: "./b.js".to_owned(),
    };
    let error = read_package_config("/w/pkg/package.json", Some(b"{"), Some(&context)).unwrap_err();
    assert_eq!(
        error.message,
        "Invalid package config /w/pkg/package.json while importing \"./b.js\" from /w/a.js."
    );
}

#[test]
fn the_binding_tuple_is_in_upstream_field_order() {
    let config = read_package_config(
        "/w/package.json",
        Some(br#"{"name":"a","main":"m.js","type":"commonjs","exports":"./e.js"}"#),
        None,
    )
    .unwrap()
    .unwrap();
    let tuple = config.to_tuple("/w/package.json");
    assert_eq!(
        tuple,
        hmr_gap::PackageTuple {
            name: Some("a".to_owned()),
            main: Some("m.js".to_owned()),
            kind: PackageType::CommonJs,
            imports: None,
            exports: Some("./e.js".to_owned()),
            path: "/w/package.json".to_owned(),
        }
    );
}

fn config(name: &str) -> PackageConfig {
    PackageConfig {
        name: Some(name.to_owned()),
        main: None,
        kind: PackageType::None,
        exports: None,
        imports: None,
    }
}

#[test]
fn an_unexpired_manifest_is_node_s_own_answer() {
    let found = nearest_package_config("/w/a/lib.js", true, &|_| false, &|_| None);
    assert_eq!(
        found,
        NearestManifest::NodeAnswers {
            path: "/w/a/package.json".to_owned()
        }
    );
}

#[test]
fn an_expired_manifest_is_read_from_disk() {
    let found = nearest_package_config(
        "/w/a/lib.js",
        true,
        &|path| path == "/w/a/package.json",
        &|path| (path == "/w/a/package.json").then(|| config("a")),
    );
    assert_eq!(
        found,
        NearestManifest::Cached {
            path: "/w/a/package.json".to_owned(),
            config: config("a"),
        }
    );
}

#[test]
fn an_expired_manifest_with_no_config_keeps_walking_up() {
    let found = nearest_package_config(
        "/w/a/lib.js",
        true,
        &|path| path == "/w/a/package.json",
        &|_| None,
    );
    assert_eq!(
        found,
        NearestManifest::NodeAnswers {
            path: "/w/package.json".to_owned()
        }
    );
}

#[test]
fn the_walk_stops_at_node_modules() {
    let found = nearest_package_config("/w/node_modules/a/lib.js", true, &|_| true, &|_| {
        Some(config("a"))
    });
    // The directory itself is `a`, so the first candidate is read; the check is on the directory the
    // walk reaches, which is what makes the boundary a boundary.
    assert!(matches!(found, NearestManifest::Cached { .. }));
    let stopped = nearest_package_config("/w/a/lib.js", true, &|_| true, &|_| Some(config("a")));
    assert!(matches!(stopped, NearestManifest::Cached { .. }));
}

#[test]
fn without_include_root_the_root_manifest_is_never_read() {
    // Reaching `/` stops with "Node answers" *before* reading `/package.json`; that is what keeps
    // the CommonJS reader's absent result instead of inventing a root package.
    let found = nearest_package_config("/a.js", false, &|_| true, &|_| Some(config("root")));
    assert_eq!(
        found,
        NearestManifest::NodeAnswers {
            path: "/package.json".to_owned()
        }
    );
}

// -------------------------------------------------------------------------------------------
// Expired directories: containment, real paths, and stickiness
// -------------------------------------------------------------------------------------------

#[test]
fn invalidation_is_sticky() {
    let mut expired = ExpiredDirectories::new();
    assert!(expired.is_empty());
    expired.invalidate("/w/node_modules/a/package.json");
    expired.invalidate("/w/node_modules/b/package.json");
    // A directory that changed once is expected to change again; nothing removes it.
    assert_eq!(expired.len(), 2);
    assert_eq!(
        expired.directories().collect::<Vec<_>>(),
        vec!["/w/node_modules/a", "/w/node_modules/b"]
    );
}

#[test]
fn a_path_in_an_expired_directory_is_expired() {
    let mut expired = ExpiredDirectories::new();
    expired.invalidate("/w/node_modules/a/package.json");
    assert!(expired.is_path_expired("/w/node_modules/a/lib.js", &not_on_disk));
    assert!(!expired.is_path_expired("/w/node_modules/b/lib.js", &not_on_disk));
    assert!(!expired.is_path_expired("/w/node_modules/a-extra/lib.js", &not_on_disk));
}

#[test]
fn a_linked_package_is_contained_through_its_real_path() {
    let mut expired = ExpiredDirectories::new();
    expired.invalidate("/real/pkg/package.json");
    // The caller names the link; only `realpath` relates the two.
    let lookup = |path: &str| match path {
        "/w/node_modules/a" => Some("/real/pkg".to_owned()),
        _ => None,
    };
    assert!(expired.is_path_expired("/w/node_modules/a/lib.js", &lookup));
    assert!(!expired.is_path_expired("/w/node_modules/b/lib.js", &lookup));
}

#[test]
fn an_unresolvable_parent_keeps_its_configured_spelling() {
    let mut expired = ExpiredDirectories::new();
    expired.invalidate("/w/node_modules/a/package.json");
    assert!(expired.is_path_expired("/w/node_modules/a/lib.js", &not_on_disk));
}

#[test]
fn invalidating_again_drops_the_real_path_memo() {
    // A link can be retargeted; a memo kept across an invalidation would answer for the old target.
    let mut expired = ExpiredDirectories::new();
    expired.invalidate("/real/pkg/package.json");
    let first = |_: &str| Some("/real/pkg".to_owned());
    assert!(expired.is_path_expired("/w/link/lib.js", &first));
    expired.invalidate("/real/pkg/package.json");
    let second = |_: &str| Some("/elsewhere/pkg".to_owned());
    assert!(!expired.is_path_expired("/w/link/lib.js", &second));
}

// -------------------------------------------------------------------------------------------
// Dependency closure and plugin selection: the bridge from "this module changed" to
// "this plugin restarts"
// -------------------------------------------------------------------------------------------

#[test]
fn a_dependency_closure_includes_its_root_and_every_transitive_import() {
    let imports = graph(&[("/w/a.js", "/w/b.js"), ("/w/b.js", "/w/c.js")]);
    let deps = load_dependencies(&imports, &url("/w/a.js"), &BTreeSet::new());
    assert_eq!(
        deps,
        set_of([url("/w/a.js"), url("/w/b.js"), url("/w/c.js")])
    );
}

#[test]
fn a_declined_module_is_cut_with_its_subtree() {
    // `b` is declined, so neither it nor its dependency `c` is in the closure: a module reached
    // only through a declined edge is not a reason to reload.
    let imports = graph(&[("/w/a.js", "/w/b.js"), ("/w/b.js", "/w/c.js")]);
    let deps = load_dependencies(&imports, &url("/w/a.js"), &set_of([url("/w/b.js")]));
    assert_eq!(deps, set_of([url("/w/a.js")]));
}

#[test]
fn an_excluded_module_is_cut_with_its_subtree() {
    let mut imports = graph(&[("/w/a.js", "/w/b.js")]);
    imports.insert(url("/w/a.js"), url("/w/node_modules/dep/index.js"));
    let deps = load_dependencies(&imports, &url("/w/a.js"), &BTreeSet::new());
    assert_eq!(deps, set_of([url("/w/a.js"), url("/w/b.js")]));
}

#[test]
fn a_cyclic_dependency_closure_terminates() {
    let imports = graph(&[("/w/a.js", "/w/b.js"), ("/w/b.js", "/w/a.js")]);
    let deps = load_dependencies(&imports, &url("/w/a.js"), &BTreeSet::new());
    assert_eq!(deps, set_of([url("/w/a.js"), url("/w/b.js")]));
}

fn entry(module: &str, plugin: &str, runtime: Option<&str>) -> PluginEntry {
    PluginEntry {
        url: url(module),
        plugin: plugin.to_owned(),
        runtime: runtime.map(str::to_owned),
    }
}

#[test]
fn an_entry_whose_closure_reaches_a_change_reloads() {
    let imports = graph(&[("/w/plugin.mjs", "/w/dep.mjs")]);
    let entries = [entry("/w/plugin.mjs", "p", Some("rt"))];
    let mut accepted = set_of([url("/w/dep.mjs")]);
    let groups = select_reloads(&entries, &imports, &BTreeSet::new(), &mut accepted);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].filename, url("/w/plugin.mjs"));
    assert_eq!(groups[0].runtime.as_deref(), Some("rt"));
    // The closure is folded into `accepted`, which is what the cache eviction consumes.
    assert!(accepted.contains(&url("/w/plugin.mjs")));
}

#[test]
fn an_entry_that_does_not_reach_a_change_does_not_reload() {
    let imports = graph(&[("/w/plugin.mjs", "/w/dep.mjs")]);
    let entries = [entry("/w/plugin.mjs", "p", Some("rt"))];
    let mut accepted = set_of([url("/w/elsewhere.mjs")]);
    let groups = select_reloads(&entries, &imports, &BTreeSet::new(), &mut accepted);
    assert!(groups.is_empty());
    assert!(!accepted.contains(&url("/w/plugin.mjs")));
}

#[test]
fn an_entry_sharing_a_replaced_runtime_joins_that_reload() {
    // Two entries, one runtime: one runtime cannot be replaced twice, so the second becomes a
    // module of the first group.
    let imports = graph(&[
        ("/w/first.mjs", "/w/dep.mjs"),
        ("/w/second.mjs", "/w/dep.mjs"),
    ]);
    let entries = [
        entry("/w/first.mjs", "p1", Some("rt")),
        entry("/w/second.mjs", "p2", Some("rt")),
    ];
    let mut accepted = set_of([url("/w/dep.mjs")]);
    let groups = select_reloads(&entries, &imports, &BTreeSet::new(), &mut accepted);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].filename, url("/w/first.mjs"));
    assert_eq!(
        groups[0].modules,
        set_of([url("/w/first.mjs"), url("/w/second.mjs")])
    );
    // The re-export root is accepted in the second pass, so it is evicted too.
    assert!(accepted.contains(&url("/w/second.mjs")));
}

#[test]
fn a_declined_entry_is_never_selected() {
    let imports = graph(&[("/w/plugin.mjs", "/w/dep.mjs")]);
    let entries = [entry("/w/plugin.mjs", "p", Some("rt"))];
    let mut accepted = set_of([url("/w/dep.mjs")]);
    let groups = select_reloads(
        &entries,
        &imports,
        &set_of([url("/w/plugin.mjs")]),
        &mut accepted,
    );
    assert!(groups.is_empty());
}

#[test]
fn a_plugin_selected_twice_does_not_restart_twice() {
    let imports = graph(&[("/w/one.mjs", "/w/dep.mjs"), ("/w/two.mjs", "/w/dep.mjs")]);
    let entries = [
        entry("/w/one.mjs", "same", Some("rt1")),
        entry("/w/two.mjs", "same", Some("rt2")),
    ];
    let mut accepted = set_of([url("/w/dep.mjs")]);
    let groups = select_reloads(&entries, &imports, &BTreeSet::new(), &mut accepted);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].plugin, "same");
}

// -------------------------------------------------------------------------------------------
// Module files: identity, comparison, observe, refresh
// -------------------------------------------------------------------------------------------

#[test]
fn a_missing_file_is_stale_and_reported_as_unseen() {
    let files = ModuleFiles::new(10_000);
    let mut read = BTreeMap::new();
    read.insert(url("/w/a.js"), None);
    let comparison = files.compare(&read);
    assert!(comparison.stale.contains(&url("/w/a.js")));
    assert_eq!(comparison.seen.get(&url("/w/a.js")), Some(&None));
}

#[test]
fn an_identity_is_inode_size_and_mtime() {
    let identity = FileIdentity::new(7, 42, 1_700);
    assert_eq!(identity.key, "7:42:1700");
    assert_eq!(identity.modified_at_ms, 1_700);
}

#[test]
fn a_recorded_module_is_stale_only_when_its_identity_moves() {
    let mut files = ModuleFiles::new(10_000);
    let identity = FileIdentity::new(1, 10, 9_000);
    let mut seen = BTreeMap::new();
    seen.insert(url("/w/a.js"), Some(identity.key.clone()));
    files.observe(&[url("/w/a.js")], &seen, &BTreeSet::new(), 10_000, &|_| {
        None
    });

    let mut same = BTreeMap::new();
    same.insert(url("/w/a.js"), Some(identity));
    assert!(files.compare(&same).stale.is_empty());

    let mut moved = BTreeMap::new();
    moved.insert(url("/w/a.js"), Some(FileIdentity::new(1, 11, 9_500)));
    assert!(files.compare(&moved).stale.contains(&url("/w/a.js")));
}

#[test]
fn an_unrecorded_module_uses_the_coarse_timestamp_window() {
    let files = ModuleFiles::new(10_000);
    let mut inside = BTreeMap::new();
    inside.insert(
        url("/w/a.js"),
        Some(FileIdentity::new(1, 1, 10_000 - TIMESTAMP_RESOLUTION_MS)),
    );
    assert!(files.compare(&inside).stale.contains(&url("/w/a.js")));

    let mut older = BTreeMap::new();
    older.insert(
        url("/w/a.js"),
        Some(FileIdentity::new(
            1,
            1,
            10_000 - TIMESTAMP_RESOLUTION_MS - 1,
        )),
    );
    assert!(files.compare(&older).stale.is_empty());
}

#[test]
fn observing_records_read_seen_retained_and_stale_modules() {
    let mut files = ModuleFiles::new(1_000);
    let read = |path: &str| match path {
        _ if path.ends_with("read.js") => Some(FileIdentity::new(9, 9, 500)),
        _ => None,
    };
    let mut seen = BTreeMap::new();
    seen.insert(url("/w/seen.js"), Some("seen-key".to_owned()));
    files.observe(
        &[url("/w/read.js"), url("/w/seen.js"), url("/w/late.js")],
        &seen,
        &BTreeSet::new(),
        1_000,
        &read,
    );
    assert_eq!(files.record_key("/w/read.js"), None);
    assert_eq!(files.record_key(&url("/w/read.js")), Some("9:9:500"));
    assert_eq!(files.record_key(&url("/w/seen.js")), Some("seen-key"));
    // Read after the observation: nothing proves it older, so it stays known-stale.
    assert!(files.is_recorded(&url("/w/late.js")));
    assert_eq!(files.record_key(&url("/w/late.js")), None);
    assert_eq!(files.len(), 3);
}

#[test]
fn observing_keeps_a_retained_module_s_record() {
    let mut files = ModuleFiles::new(10);
    files.observe(
        &[url("/w/a.js")],
        &BTreeMap::new(),
        &BTreeSet::new(),
        10,
        &|_| Some(FileIdentity::new(1, 1, 1)),
    );
    let mut retained = BTreeSet::new();
    retained.insert(url("/w/a.js"));
    files.observe(&[url("/w/a.js")], &BTreeMap::new(), &retained, 20, &|_| {
        None
    });
    assert_eq!(files.record_key(&url("/w/a.js")), Some("1:1:1"));
    assert_eq!(files.observed_at(), 20);
}

#[test]
fn a_fresh_record_set_is_empty() {
    // A new `ModuleFiles` has observed nothing but carries the comparison clock it was created with;
    // a first `compare` therefore trusts only the coarse-timestamp window.
    let files = ModuleFiles::new(5_000);
    assert!(files.is_empty());
    assert_eq!(files.len(), 0);
    assert_eq!(files.observed_at(), 5_000);
}

#[test]
fn refresh_updates_only_the_named_modules() {
    let mut files = ModuleFiles::new(0);
    files.observe(
        &[url("/w/a.js"), url("/w/b.js")],
        &BTreeMap::new(),
        &BTreeSet::new(),
        10,
        &|_| Some(FileIdentity::new(1, 1, 1)),
    );
    files.refresh(&[url("/w/a.js")], &|_| Some(FileIdentity::new(2, 2, 2)));
    assert_eq!(files.record_key(&url("/w/a.js")), Some("2:2:2"));
    assert_eq!(files.record_key(&url("/w/b.js")), Some("1:1:1"));
}

// -------------------------------------------------------------------------------------------
// Watch root discovery and configuration registrations
// -------------------------------------------------------------------------------------------

#[test]
fn a_watch_root_is_the_nearest_existing_directory() {
    let root = find_watch_root(
        "/w/conf/sub/dsh.patch.yml",
        &|path| {
            if path == "/w/conf" {
                WatchRootState::Directory
            } else {
                WatchRootState::Missing
            }
        },
        &|path| Some(path.to_owned()),
    )
    .unwrap();
    assert_eq!(
        root,
        WatchRoot {
            filename: "/w/conf/sub/dsh.patch.yml".to_owned(),
            root: "/w/conf".to_owned(),
            depth: 1,
        }
    );
}

#[test]
fn a_watch_root_follows_a_link_once_after_the_climb() {
    let root = find_watch_root("/w/link/dsh.patch.yml", &always_dir, &|path| {
        Some(path.replace("/w/link", "/real/target"))
    })
    .unwrap();
    assert_eq!(root.root, "/real/target");
    assert_eq!(root.filename, "/real/target/dsh.patch.yml");
    assert_eq!(root.depth, 0);
}

#[test]
fn a_non_directory_parent_is_refused() {
    let error = find_watch_root(
        "/w/file.txt/patch.yml",
        &|_| WatchRootState::NotADirectory,
        &|path| Some(path.to_owned()),
    )
    .unwrap_err();
    assert!(matches!(error, WatchRootError::NotADirectory { .. }));
    assert!(
        error
            .message()
            .starts_with("config watch parent is not a directory:")
    );
}

#[test]
fn a_watch_with_no_existing_ancestor_is_refused() {
    let error = find_watch_root("/", &|_| WatchRootState::Missing, &|_| None).unwrap_err();
    assert!(matches!(error, WatchRootError::Missing { .. }));
}

#[test]
fn a_patch_path_registers_once_across_both_spellings() {
    let mut registrations = ConfigRegistrations::new();
    let (declared, canonical) = registrations
        .register("/w/link/dsh.patch.yml", "/real/dsh.patch.yml")
        .unwrap();
    assert!(registrations.contains("/w/link/dsh.patch.yml"));
    assert!(registrations.contains("/real/dsh.patch.yml"));
    // The other spelling names the same file.
    assert_eq!(
        registrations.register("/w/link/dsh.patch.yml", "/real/dsh.patch.yml"),
        Err("/w/link/dsh.patch.yml".to_owned())
    );
    assert_eq!(
        registrations.register("/real/dsh.patch.yml", "/real2/dsh.patch.yml"),
        Err("/real/dsh.patch.yml".to_owned())
    );
    registrations.release(&declared, &canonical);
    assert!(registrations.is_empty());
    // Released: registerable again.
    assert!(
        registrations
            .register("/w/link/dsh.patch.yml", "/real/dsh.patch.yml")
            .is_ok()
    );
    assert_eq!(registrations.len(), 2);
}

// -------------------------------------------------------------------------------------------
// The refresh queue: serialisation, coalescing, and the closed idle window
// -------------------------------------------------------------------------------------------

#[test]
fn a_mark_while_idle_starts_the_loop() {
    let mut queue = RefreshQueue::new();
    assert!(!queue.is_running());
    assert!(queue.mark());
    assert!(queue.is_running());
    assert!(queue.is_dirty());
}

#[test]
fn a_mark_during_a_run_does_not_start_a_second_loop() {
    let mut queue = RefreshQueue::new();
    assert!(queue.mark());
    assert!(!queue.mark());
    assert!(queue.is_running());
}

#[test]
fn a_burst_coalesces_to_one_refresh() {
    let mut queue = RefreshQueue::new();
    let mut runs = 0;
    queue.mark();
    queue.mark();
    queue.mark();
    while queue.take() {
        runs += 1;
    }
    queue.idle();
    assert_eq!(runs, 1);
}

#[test]
fn a_change_during_a_refresh_runs_a_second_one() {
    let mut queue = RefreshQueue::new();
    queue.mark();
    let mut runs = 0;
    while queue.take() {
        runs += 1;
        if runs == 1 {
            queue.mark();
        }
    }
    queue.idle();
    assert_eq!(runs, 2);
    assert!(!queue.is_running());
}

#[test]
fn the_idle_window_is_closed() {
    // A mark after the final `take` but before `idle` must not be lost: `idle` clears `running`, and
    // the next `mark` returns `true` because the flag ordering is fixed.
    let mut queue = RefreshQueue::new();
    queue.mark();
    while queue.take() {}
    queue.idle();
    assert!(queue.mark());
}

#[test]
fn drain_counts_one_run_and_leaves_the_queue_idle() {
    let mut queue = RefreshQueue::new();
    queue.mark();
    let mut runs = 0;
    assert_eq!(
        queue.drain(|| {
            runs += 1;
        }),
        1
    );
    assert_eq!(runs, 1);
    assert!(!queue.is_running());
    assert!(!queue.is_dirty());
}

#[test]
fn an_exact_path_watch_ignores_sibling_paths() {
    assert!(watcher_reaches("/w/conf", "/w/conf/dsh.patch.yml"));
    assert!(watcher_reaches(
        "/w/conf/dsh.patch.yml",
        "/w/conf/dsh.patch.yml"
    ));
    assert!(!watcher_reaches(
        "/w/conf/other.yml",
        "/w/conf/dsh.patch.yml"
    ));
    assert!(!watcher_reaches("/w", "/w2/dsh.patch.yml"));
}

// -------------------------------------------------------------------------------------------
// Build failures
// -------------------------------------------------------------------------------------------

#[test]
fn an_esbuild_failure_yields_one_diagnostic_per_error() {
    let failure = json!({
        "errors": [
            {"text": "bad", "location": {"file": "/w/a.ts", "line": 3, "column": 7}},
            {"text": "worse"},
        ]
    });
    let diagnostics = read_build_failure(&failure).unwrap();
    assert_eq!(diagnostics.len(), 2);
    assert_eq!(diagnostics[0].header(), "File: /w/a.ts:3:7\nbad");
    assert_eq!(diagnostics[1].header(), "worse");
}

#[test]
fn a_value_that_is_not_a_build_failure_is_not_half_read() {
    // One malformed error makes the whole value "not a build failure": logging it as-is is the only
    // honest answer, because a partially read failure loses the diagnostic that mattered.
    assert!(read_build_failure(&json!({"message": "x"})).is_none());
    assert!(read_build_failure(&json!({"errors": "nope"})).is_none());
    assert!(read_build_failure(&json!({"errors": [{"text": 1}]})).is_none());
    assert!(read_build_failure(&json!({"errors": [{}]})).is_none());
    assert_eq!(read_build_failure(&json!({"errors": []})), Some(Vec::new()));
}

// -------------------------------------------------------------------------------------------
// Plugin → module resolution
// -------------------------------------------------------------------------------------------

#[test]
fn a_name_without_a_namespace_is_the_loader_s_problem() {
    assert_eq!(
        resolve_plugin_module("a", "file:///w/", None, &BTreeMap::new()),
        Ok(PluginModule::External)
    );
    assert_eq!(
        resolve_plugin_module("cordis:timer", "file:///w/", Some("ns"), &BTreeMap::new()),
        Ok(PluginModule::External)
    );
}

#[test]
fn a_namespace_resolves_to_its_loaded_url() {
    let mut urls = BTreeMap::new();
    urls.insert("ns".to_owned(), url("/w/plugins/a/index.js"));
    assert_eq!(
        resolve_plugin_module("a", "file:///w/", Some("ns"), &urls),
        Ok(PluginModule::Url(url("/w/plugins/a/index.js")))
    );
}

#[test]
fn an_unknown_namespace_fails_rather_than_falling_back_to_the_loader() {
    // Falling back would load a second copy of the module.
    let error =
        resolve_plugin_module("a", "file:///w/", Some("missing"), &BTreeMap::new()).unwrap_err();
    assert_eq!(
        error,
        "HMR cannot locate the loaded module for a from file:///w/"
    );
}

#[test]
fn a_name_with_several_namespaces_yields_each_loaded_one() {
    let mut names: BTreeMap<String, Vec<Option<String>>> = BTreeMap::new();
    names.insert(
        "a".to_owned(),
        vec![Some("ns1".to_owned()), Some("ns2".to_owned())],
    );
    names.insert("b".to_owned(), vec![None]);
    // A `None` namespace is skipped only when the same name also has a loaded one.
    names.get_mut("a").unwrap().push(None);
    assert_eq!(
        plugin_namespaces(&names),
        vec![
            ("a".to_owned(), Some("ns1".to_owned())),
            ("a".to_owned(), Some("ns2".to_owned())),
            ("b".to_owned(), None),
        ]
    );
}

// -------------------------------------------------------------------------------------------
// The constants upstream states in code or in its schema
// -------------------------------------------------------------------------------------------

#[test]
fn the_event_names_are_upstream_s() {
    // A renamed event is a silent incompatibility: the emitter fires a name no listener subscribed to.
    assert_eq!(hmr_gap::HMR_CHANGE_EVENT, "hmr/change");
    assert_eq!(hmr_gap::HMR_RELOAD_EVENT, "hmr/reload");
}

#[test]
fn the_watcher_defaults_are_upstream_s() {
    // Cordis' schema: 100 ms debounce, and these four ignore globs in this order.
    assert_eq!(hmr_gap::DEFAULT_DEBOUNCE_MS, 100);
    assert_eq!(
        hmr_gap::DEFAULT_IGNORED,
        ["**/node_modules", "**/.*", "cache", "data"]
    );
}

#[test]
fn an_empty_expired_directory_set_expires_nothing() {
    let mut expired = ExpiredDirectories::new();
    assert!(expired.is_empty());
    assert!(!expired.is_path_expired("/w/a/lib.js", &not_on_disk));
}

// -------------------------------------------------------------------------------------------
// The graph type itself
// -------------------------------------------------------------------------------------------

#[test]
fn a_missing_node_and_a_node_without_edges_are_the_same_leaf() {
    let graph = Graph::new();
    assert_eq!(graph.edges("/w/a.js").count(), 0);
    assert!(!graph.has_edges("/w/a.js"));
}

#[test]
fn edges_are_sorted_and_deduplicated() {
    let mut graph = Graph::new();
    graph.insert("a", "c");
    graph.insert("a", "b");
    graph.insert("a", "b");
    assert_eq!(graph.edges("a").collect::<Vec<_>>(), vec!["b", "c"]);
    assert_eq!(graph.sources().collect::<Vec<_>>(), vec!["a"]);
}

#[test]
fn excluded_modules_cover_builtins_and_dependencies() {
    assert!(is_excluded("node:fs"));
    assert!(is_excluded("file:///w/node_modules/a/index.js"));
    assert!(!is_excluded("file:///w/src/node_modules_like/a.js"));
}

#[test]
fn every_pending_reason_has_a_wire_spelling() {
    assert_eq!(PendingReason::HostModule.as_str(), "host-module");
    assert_eq!(PendingReason::UnmanagedPlugin.as_str(), "unmanaged-plugin");
    assert_eq!(
        PendingReason::ModuleComparison.as_str(),
        "module-comparison"
    );
}
