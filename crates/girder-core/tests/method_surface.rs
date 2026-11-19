//! Tests for the declared Remote method surface.
//!
//! The declaration in `method_surface` exists for one reason: to be compared against a registry. A
//! list that is checked against nothing is a second source of truth, and a second source of truth is
//! how the surface went stale before. The central test here therefore builds the same registry
//! `serve` builds -- every `register_*` call, with a stand-in for each collaborator `serve` brings --
//! and requires the comparison to come back empty.

use std::sync::Arc;

use girder_core::api_config::WorkspaceRegistry;
use girder_core::{
    ArchiveStore, BoxFut, ChunkStream, ExtensionRuntime, FileSystem, JobRegistry, LlmError,
    LlmProvider, LlmRequest, LocalFileSystem, LocalLauncher, MemoryArchiveStore,
    MemoryScheduleStore, MemorySettingsStore, Opener, ProjectionRegistry, RouteRegistry,
    ScheduleStore, SessionCatalog, SettingsStore, TerminalLauncher, TerminalRegistry, ToolRegistry,
};
use girder_core::{SurfaceDiscrepancy, declared_surface, reconcile_surface};
use tokio_util::sync::CancellationToken;

// ---- stand-ins for the collaborators `serve` supplies -------------------------------------------

/// A provider that refuses every turn.
///
/// The runtime is only needed so `session.resume` and `session.state` can be registered; nothing in
/// these tests drives a turn.
struct Silent;

impl LlmProvider for Silent {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>> {
        Box::pin(async { Err(LlmError::Cancelled) })
    }
}

/// An opener that refuses every path.
///
/// `serve` probes the real desktop and falls back to this; a test must not depend on whether the
/// machine running it has a file manager.
struct NoOpener;

impl Opener for NoOpener {
    fn open(&self, _path: &std::path::Path) -> Result<(), String> {
        Err("no desktop in a test".to_owned())
    }

    fn reveal(&self, _path: &std::path::Path) -> Result<(), String> {
        Err("no desktop in a test".to_owned())
    }
}

/// A scratch directory that removes itself.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "girder-method-surface-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&path).expect("created the scratch directory");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Builds the registry `serve` builds, with a stand-in for every collaborator it constructs.
///
/// Kept in step with `serve` by hand, which is the point: if `serve` gains a registration and this
/// function does not, the surface test reports the method as `missing` -- the declaration says the
/// build serves it and this registry does not. That is a weaker guarantee than reading `serve`'s own
/// list, and it is the strongest one available without running the CLI.
fn serve_shaped_registry(scratch: &std::path::Path) -> girder_core::RemoteRegistry {
    let catalog: Arc<dyn SessionCatalog> = Arc::new(girder_core::DirectoryCatalog::new(scratch));
    let runtime = girder_core::AgentRuntime::new(girder_core::RuntimeConfig {
        provider: Arc::new(Silent),
        tools: Arc::new(ToolRegistry::new()),
        bus: Arc::new(girder_core::EventBus::new()),
        model: "test".to_owned(),
        max_turns: 1,
        token_budget: girder_core::Ceiling::Unlimited,
    });
    let mut routes = RouteRegistry::new();
    let _ = routes.register(girder_core::deepseek_route("deepseek", "test"));
    let routes = Arc::new(routes);

    // What `serve` starts with: the session core plus both model methods, which is the one path
    // that gives the route table its picker.
    let mut registry = girder_core::registry_with_routes(&catalog, &runtime, &routes);

    register_runtime_surfaces(&mut registry);
    register_terminal_surface(&mut registry);
    register_workspace_file_surface(&mut registry, scratch);
    register_host_surface(&mut registry, scratch);
    register_lifecycle_surface(&mut registry, &catalog, &runtime, scratch);
    register_settings_surface(&mut registry);
    // The three namespaces added after this table was written. They take collaborators the scratch build creates itself,
    // which is exactly what the declaration's `capability` column says each one needs.
    register_goal_surface(&mut registry);
    register_workflow_surface(&mut registry);
    register_question_surface(&mut registry);

    registry
}

/// Projections, jobs, and extensions -- the group that needs something the server owns.
fn register_goal_surface(registry: &mut girder_core::RemoteRegistry) {
    let store: std::sync::Arc<dyn girder_core::GoalStore> =
        std::sync::Arc::new(girder_core::MemoryGoalStore::new());
    girder_core::register_goal_methods(registry, &store);
}

fn register_workflow_surface(registry: &mut girder_core::RemoteRegistry) {
    let workflows = std::sync::Arc::new(girder_core::WorkflowRegistry::new());
    girder_core::register_workflow_methods(registry, workflows);
}

fn register_question_surface(registry: &mut girder_core::RemoteRegistry) {
    let store: std::sync::Arc<dyn girder_core::QuestionStore> =
        std::sync::Arc::new(girder_core::MemoryQuestionStore::new());
    let queue = std::sync::Arc::new(girder_core::QuestionQueue::new(store));
    girder_core::register_question_methods(registry, queue);
}

fn register_runtime_surfaces(registry: &mut girder_core::RemoteRegistry) {
    let projections = Arc::new(ProjectionRegistry::new());
    let jobs = Arc::new(JobRegistry::new(4096));
    let extensions = Arc::new(girder_core::ExtensionRegistry::new());

    girder_core::register_projection_methods(registry, &projections);
    girder_core::register_job_methods(registry, &jobs);
    girder_core::register_job_follow(registry, &jobs);
    girder_core::register_extension_methods(registry, &extensions);

    let runtime: Arc<dyn ExtensionRuntime> = Arc::new(girder_core::ExtensionShellRuntime::new());
    girder_core::register_extension_run_methods(registry, &extensions, &runtime);
    // Two surfaces `serve` registers that this helper had not: the LLM runtime's endpoints and the
    // market index's. Both are shapes `serve` builds, so leaving them out made the test's "serve
    // shaped" claim slightly untrue.
    if let Ok(llm) = girder_core::llm_remote::runtime_from_routes(&{
        let mut routes = girder_core::RouteRegistry::new();
        let _ = routes.register(girder_core::deepseek_route("deepseek", "test"));
        Arc::new(routes)
    }) {
        let _ = girder_core::register_llm_remote(registry, &llm);
    }
    let market = Arc::new(girder_core::plugin_market_rpc::MarketCatalog::new());
    girder_core::plugin_market_rpc::register_market_methods(registry, &market);
    // A catalog over a path that need not exist: these tests check that a method is *routable*,
    // not that a session can be read from it.
    let feedback_catalog: Arc<dyn SessionCatalog> = Arc::new(girder_core::DirectoryCatalog::new(
        std::path::Path::new("/nonexistent-for-routing"),
    ));
    girder_core::message_feedback::register_message_feedback_methods(registry, &feedback_catalog);
    girder_core::message_feedback::register_session_feedback_methods(registry, &feedback_catalog);
    girder_core::session_reference::register_session_reference_methods(registry, &feedback_catalog);
    girder_core::agent_presets::register_agent_preset_methods(
        registry,
        &girder_core::agent_presets::default_registry(),
    );
    girder_core::web_rpc::register_web_methods(
        registry,
        &Arc::new(girder_core::web_rpc::WebProviders::new()),
    );
    girder_core::permission_presets_rpc::register_permission_preset_methods(
        registry,
        &girder_core::permission_presets_rpc::default_catalogue(),
        girder_core::permission_presets_rpc::DEFAULT_PRESET,
    );
}

/// The terminals: two probes that need nothing, then the session methods over one registry.
fn register_terminal_surface(registry: &mut girder_core::RemoteRegistry) {
    girder_core::register_terminal_extra_methods(registry);
    let terminals = Arc::new(TerminalRegistry::new());
    let launcher: Arc<dyn TerminalLauncher> = Arc::new(LocalLauncher::new());
    girder_core::register_terminal_rpc_methods(registry, &terminals, &launcher);
    girder_core::register_terminal_rename_method(registry, &terminals);
    girder_core::register_terminal_retain_method(
        registry,
        Arc::new(girder_core::TerminalPersistStore::new(
            std::path::Path::new("/nonexistent-for-routing"),
        )),
    );
}

/// The workspace browser: one filesystem, one workspace table.
fn register_workspace_file_surface(
    registry: &mut girder_core::RemoteRegistry,
    scratch: &std::path::Path,
) {
    let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem);
    let workspaces = Arc::new(WorkspaceRegistry::new());
    girder_core::register_workspace_file_methods(registry, &fs, &workspaces);
    girder_core::register_workspace_file_extras(registry, &fs, &workspaces);
    girder_core::register_workspace_methods(registry, &workspaces);
    // `serve` calls these too, so a "serve-shaped" registry must as well: otherwise the declaration
    // and this helper disagree about a surface that is in fact served.
    girder_core::register_workspace_initialize_default(registry, &workspaces, None, 0);
    let preferences: Arc<dyn girder_core::WorkspacePreferences> =
        Arc::new(girder_core::InMemoryPreferences::new());
    let workspace_catalog: Arc<dyn girder_core::SessionCatalog> =
        Arc::new(girder_core::DirectoryCatalog::new(scratch));
    girder_core::register_workspace_extra_methods(
        registry,
        &workspaces,
        &preferences,
        &workspace_catalog,
    );
    girder_core::register_session_query_methods(registry, scratch);
}

/// The desktop handoff: one opener serving the host and workspace-desktop methods.
fn register_host_surface(registry: &mut girder_core::RemoteRegistry, workspace: &std::path::Path) {
    let opener: Arc<dyn Opener> = Arc::new(NoOpener);
    girder_core::register_host_methods_with(registry, workspace, Arc::clone(&opener));
    girder_core::register_open_workspace_path(registry, workspace, &opener);
    girder_core::register_workspace_desktop_methods(registry, workspace, &opener);
}

/// The session lifecycle: resume, state, schedules, duplicate and archive.
fn register_lifecycle_surface(
    registry: &mut girder_core::RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    runtime: &Arc<girder_core::AgentRuntime>,
    scratch: &std::path::Path,
) {
    girder_core::register_session_resume_method(registry, catalog, runtime);

    let projections = Arc::new(ProjectionRegistry::new());
    let turns: Arc<dyn girder_core::LiveTurns> = Arc::new(|_session_id: &str| false);
    girder_core::register_session_state_method_with(registry, catalog, &projections, &turns);

    let schedules: Arc<dyn ScheduleStore> = Arc::new(MemoryScheduleStore::new());
    girder_core::register_schedule_methods(registry, &schedules);

    let archive: Arc<dyn ArchiveStore> = Arc::new(MemoryArchiveStore::new());
    girder_core::register_session_lifecycle_methods_with_archive(registry, catalog, &archive);

    // The queue editor's source. `serve` creates it empty and every call answers `NoLiveAgent`,
    // which is still a registered method.
    let queues = Arc::new(girder_core::AgentQueueMap::new());
    let source: Arc<dyn girder_core::QueueAgentSource> = queues;
    girder_core::register_session_queue_methods(registry, catalog, &source);
    let _ = scratch;
}

/// The settings store, which `serve` registers through the workspace-file surface.
fn register_settings_surface(registry: &mut girder_core::RemoteRegistry) {
    let settings: Arc<dyn SettingsStore> = Arc::new(MemorySettingsStore::new());
    girder_core::register_settings_methods(registry, &settings);
}

// ---- the declaration itself ---------------------------------------------------------------------

#[test]
fn the_declaration_is_not_empty() {
    // A declaration that listed nothing would reconcile cleanly against an empty registry, which is
    // every bit as misleading as a stale list.
    let surface = declared_surface();
    assert!(
        !surface.methods.is_empty(),
        "a build with no methods has nothing to declare"
    );
    assert!(
        surface.methods.len() >= 40,
        "this build serves far more than {} methods; a short list is a missing table entry",
        surface.methods.len()
    );
}

#[test]
fn every_declared_name_is_namespace_dot_method() {
    // `reconcile` splits on the single dot to compare against the registry's `(namespace, method)`
    // pairs, and a name without exactly one would be compared against the wrong thing -- or, worse,
    // silently never match.
    for name in declared_surface().methods {
        let mut parts = name.split('.');
        let namespace = parts.next().unwrap_or_default();
        let method = parts.next().unwrap_or_default();

        assert!(
            !namespace.is_empty(),
            "`{name}` has an empty namespace: the name must be `namespace.method`"
        );
        assert!(
            !method.is_empty(),
            "`{name}` has an empty method: the name must be `namespace.method`"
        );
        assert!(
            parts.next().is_none(),
            "`{name}` has more than one dot: the name must be `namespace.method`"
        );
        assert!(
            namespace
                .chars()
                .all(|character| character.is_ascii_alphanumeric()),
            "`{name}` has a namespace that is not plain ASCII alphanumeric"
        );
        assert!(
            method
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_lowercase()),
            "`{name}` has a method that does not start lower-case, which no convention in this \
             codebase uses"
        );
    }
}

#[test]
fn the_declaration_lists_each_method_once() {
    // A duplicate is not merely untidy: `reconcile` would report a set difference, so the copy would
    // be invisible while making the count wrong for anyone who trusted `methods.len()`.
    let methods = declared_surface().methods;
    let mut unique = methods.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        methods.len(),
        "the declaration lists a method twice; sorted: {unique:?}"
    );
}

#[test]
fn needs_collaborator_is_not_empty_and_is_a_subset() {
    // The distinction is the reason the field exists: a caller comparing against a registry built
    // from nothing but an empty catalog can only expect the names that need nothing.
    let surface = declared_surface();
    assert!(
        !surface.needs_collaborator.is_empty(),
        "terminals, workspace files, and the host opener all need a collaborator, so an empty list \
         is a claim that no method does"
    );

    for name in &surface.needs_collaborator {
        assert!(
            surface.methods.contains(name),
            "`{name}` needs a collaborator but is not declared as a method"
        );
    }
    assert!(
        surface.needs_collaborator.len() < surface.methods.len(),
        "every method declared as needing a collaborator would make the distinction useless"
    );
}

// ---- reconcile: the drift detector ---------------------------------------------------------------

#[test]
fn reconcile_is_empty_when_the_registry_matches() {
    let methods = declared_surface().methods;
    assert_eq!(reconcile_surface(&methods), SurfaceDiscrepancy::default());
}

#[test]
fn reconcile_reports_a_registered_method_the_declaration_omits() {
    // The failure this catches: something registers a method and the declaration is never told, so
    // every document and typed client generated from it is missing a method the server serves.
    let mut methods = declared_surface().methods;
    methods.push("session.invented".to_owned());

    let discrepancy = reconcile_surface(&methods);
    assert_eq!(discrepancy.extra, vec!["session.invented".to_owned()]);
    assert!(
        discrepancy.missing.is_empty(),
        "nothing was removed: {:?}",
        discrepancy.missing
    );
}

#[test]
fn reconcile_reports_a_declared_method_no_registry_holds() {
    // The opposite failure, and the one a client sees: a declaration that promises a method answers
    // `UNKNOWN_METHOD`, which reads as a client bug rather than a build gap.
    let mut methods = declared_surface().methods;
    let dropped = methods.remove(0);

    let discrepancy = reconcile_surface(&methods);
    assert_eq!(discrepancy.missing, vec![dropped]);
    assert!(
        discrepancy.extra.is_empty(),
        "nothing was added: {:?}",
        discrepancy.extra
    );
}

#[test]
fn reconcile_ignores_order_and_duplicates() {
    // A registry hands back names in hash order and a declaration is in table order; comparing the
    // two as sequences would report every method as drift.
    let mut methods = declared_surface().methods;
    methods.reverse();
    methods.extend(declared_surface().methods.iter().take(3).cloned());

    assert_eq!(reconcile_surface(&methods), SurfaceDiscrepancy::default());
}

// ---- the declaration against the real surface ---------------------------------------------------

#[test]
fn the_declaration_matches_the_registry_serve_builds() {
    // The central test: every declared method is really registered by the same functions `serve`
    // calls, and every method those functions register is really declared. Either direction failing
    // means the declaration has drifted from the build, which is the only thing it exists to
    // prevent.
    let scratch = Scratch::new("serve-shaped");
    let registry = serve_shaped_registry(&scratch.0);

    let mut registered: Vec<String> = registry
        .names()
        .into_iter()
        .map(|(namespace, method)| format!("{namespace}.{method}"))
        .collect();
    registered.sort();

    let discrepancy = reconcile_surface(&registered);
    assert_eq!(
        discrepancy,
        SurfaceDiscrepancy::default(),
        "the declaration and the registry disagree: {} declared, {} registered",
        declared_surface().methods.len(),
        registered.len()
    );
}

#[test]
fn every_declared_method_is_reachable_in_the_serve_shaped_registry() {
    // `reconcile` compares names; this asserts the same thing through the lookup a call actually
    // takes, so a name that is declared, registered, and still unroutable cannot hide.
    let scratch = Scratch::new("routable");
    let registry = serve_shaped_registry(&scratch.0);

    for qualified in declared_surface().methods {
        let (namespace, method) = qualified
            .split_once('.')
            .expect("every declared name has exactly one dot");
        assert!(
            registry.get(namespace, method).is_some(),
            "`{qualified}` is declared and registered but not routable"
        );
    }
}

/// Every method missing from the collaborator-free registry has a reason to be missing.
#[test]
fn every_absent_method_has_a_reason_to_be_absent() {
    // `registered_method_names` builds a registry from three registrations -- the session core, the
    // jobs, and the projections -- plus three catalogues. It therefore lacks a method for exactly two
    // reasons, and this test requires each absent name to have one of them:
    //
    //   1. Registering it needs a collaborator the build has none of, which is what
    //      `needs_collaborator` declares; or
    //   2. Its `register_*` function is one this build never calls, which is the actual defect --
    //      `terminal.environment` and `terminal.shells` need nothing, and are still missing.
    //
    // Case 2 is the drift the module documents. Asserting it by name of the registration rather than
    // by name of the method means the test keeps telling the truth after a method is added to one of
    // those functions.
    const CALLED: &[&str] = &[
        "register_session_core",
        "register_job_methods",
        "register_projection_methods",
        "register_workspace_methods",
    ];

    let surface = declared_surface();
    let registered = girder_core::registered_method_names();
    let absent = reconcile_surface(&registered).missing;
    assert!(
        !absent.is_empty(),
        "a build that registered every declared method would make this test vacuous"
    );

    for name in &absent {
        if surface.needs_collaborator.contains(name) {
            continue;
        }
        let registration = girder_core::surface_registration_of(name)
            .unwrap_or_else(|| panic!("`{name}` is declared, so it has a registration"));
        assert!(
            !CALLED.contains(&registration),
            "`{name}` needs no collaborator and is registered by `{registration}`, which this build \
             calls -- so it should not be absent"
        );
    }
}

/// The declaration must name the function that actually registers each method.
#[test]
fn every_declared_method_names_the_function_that_registers_it() {
    // `registration_of` is the declaration's only claim to being traceable to the source. A name that
    // no longer appears beside its entry -- or an entry pointing at a function that does not register
    // it -- would send a reader to the wrong place, and the table would slowly stop describing the
    // code it claims to describe.
    for name in declared_surface().methods {
        let registration = girder_core::surface_registration_of(&name)
            .unwrap_or_else(|| panic!("`{name}` is declared but has no registration"));
        assert!(
            registration.starts_with("register_"),
            "`{name}` names `{registration}`, which is not a `register_*` function"
        );
    }
    assert_eq!(
        girder_core::surface_registration_of("session.invented"),
        None
    );
}
