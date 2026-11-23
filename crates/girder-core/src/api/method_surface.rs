//! The Remote method surface this build declares, as data rather than as a registry.
//!
//! `registered_method_names` answers the same question by *building* a registry and asking what
//! landed in it. That works only for the registrations that need nothing a library cannot
//! fabricate: a catalog over an empty directory, a fresh job registry, a fresh projection registry.
//! The rest of the surface -- terminals, the host opener, workspace files, extensions, schedules,
//! settings, the session runtime -- is registered by functions that take a live component, so a
//! caller with no server cannot reach them and the registry it builds is short by exactly those.
//!
//! This module records the surface as compile-time constants instead. A method name is a fact about
//! the source, not about a running process, and a declaration that had to start a process to say
//! what it contains could not be used by a schema command, a client generator, or a doctor check
//! that runs before anything is wired.
//!
//! # Where the table comes from
//!
//! Every entry names the `register_*` function that adds it, and the table was read off those
//! functions rather than copied from a running server. `serve` mounts 69 methods across the
//! functions listed in [`DECLARED`]; each entry's `registration` field says which one.
//!
//! # What is deliberately absent
//!
//! Methods the registry only *declares* as unimplemented -- `terminal.rename`,
//! `terminal.retain`, `settings.describe`, `settings.openSettingsDocument` -- are not listed. They
//! have no handler, so they are not part of the surface a client can call; [`reconcile`] compares
//! against registered methods and would report them as noise forever.
//!
//! The workspace-preferences surface (`register_workspace_extra_methods`) **is** listed now that
//! `serve` calls it. Before that it was deliberately absent, for the right reason: a declaration a
//! client trusts over the wire must describe a server that answers, and no running server answered
//! those seven. The same rule applies to every optional surface -- declare it when `serve` calls it,
//! and not before.

/// One declared method, and what registering it needs.
///
/// Three fields rather than three parallel arrays: a method, the function that registers it, and the
/// capability that function requires are one fact, and splitting them would let the arrays fall out
/// of step with each other -- which is the failure mode this whole module is about.
struct Declared {
    /// The method, as `namespace.method`.
    method: &'static str,
    /// The `register_*` function that adds it, for traceability back to the source.
    registration: &'static str,
    /// The host capability registering it needs, or `None` when a caller can register it with
    /// nothing but an empty catalog, job registry, and projection registry.
    ///
    /// `Some` is not the same as "takes an argument": `session.list` takes a catalog, and a catalog
    /// over an empty directory is something a listing can build for itself. The line is drawn at
    /// capabilities a listing cannot honestly fabricate -- a filesystem, an opener, a runtime, a
    /// queue source, a store.
    capability: Option<&'static str>,
}

/// Every Remote method this build declares, in namespace order.
///
/// Kept as one table rather than one per namespace: a reader asking "what does this build serve"
/// should not have to walk nine constants, and `reconcile` needs the whole set anyway.
const DECLARED: &[Declared] = &[
    // ---- session ---------------------------------------------------------------------------
    // The names below are registered by `register_session_core`'s helpers; the helper is named in
    // `registration` because that is where a reader would change the method.
    Declared {
        method: "session.list",
        registration: "register_session_core",
        capability: None,
    },
    Declared {
        method: "session.create",
        registration: "register_create",
        capability: None,
    },
    Declared {
        method: "session.page",
        registration: "register_history",
        capability: None,
    },
    // `page` is deprecated upstream but still called, so both names are served from one read.
    Declared {
        method: "session.events",
        registration: "register_history",
        capability: None,
    },
    Declared {
        method: "session.follow",
        registration: "register_follow",
        capability: None,
    },
    Declared {
        method: "sessionStatus.follow",
        registration: "register_session_status_methods",
        capability: None,
    },
    Declared {
        method: "sessionStatus.mark",
        registration: "register_session_status_methods",
        capability: None,
    },
    Declared {
        method: "session.stream",
        registration: "register_stream",
        capability: Some("live session events"),
    },
    Declared {
        method: "session.version",
        registration: "register_session_core",
        capability: None,
    },
    Declared {
        method: "session.export",
        registration: "register_session_export_method",
        capability: None,
    },
    Declared {
        method: "session.rename",
        registration: "register_rename",
        capability: Some("session catalog with a title sidecar"),
    },
    Declared {
        method: "session.delete",
        registration: "register_delete",
        capability: Some("session catalog that can remove"),
    },
    Declared {
        method: "session.fork",
        registration: "register_fork",
        capability: Some("session catalog that can copy a prefix"),
    },
    Declared {
        method: "session.attachment",
        registration: "register_attachment_method",
        capability: Some("attachment store"),
    },
    // No capability: `register_prompt` registers the same name without a runtime, and the handler
    // then acknowledges without running a turn. `serve` supplies a runtime, which is a better
    // build, not a different declaration.
    Declared {
        method: "session.prompt",
        registration: "register_prompt_with_runtime",
        capability: None,
    },
    // Likewise: without a runtime the name is declared unimplemented rather than omitted, so it is
    // in the surface either way.
    Declared {
        method: "session.cancel",
        registration: "register_cancel",
        capability: Some("live AgentRuntime"),
    },
    Declared {
        method: "session.projections",
        registration: "register_projection_methods",
        capability: None,
    },
    // Both model methods read a route table. The table's *contents* are a deployment decision, but
    // the table itself is a collaborator: `registry_with_runtime` -- the constructor for a caller
    // with no routes -- declares both names unimplemented rather than serving them, so a registry
    // built from a catalog alone does not have them.
    Declared {
        method: "session.modelCatalog",
        registration: "register_model_catalog",
        capability: Some("RouteRegistry"),
    },
    Declared {
        method: "session.selectModel",
        registration: "register_select_model",
        capability: Some("RouteRegistry"),
    },
    Declared {
        method: "session.search",
        registration: "register_session_query_methods",
        capability: Some("session log root"),
    },
    Declared {
        method: "session.openWorkspacePath",
        registration: "register_open_workspace_path",
        capability: Some("Opener"),
    },
    Declared {
        method: "session.workspaceDesktop",
        registration: "register_workspace_desktop_methods",
        capability: Some("Opener + desktop probe"),
    },
    Declared {
        method: "session.canOpenWorkspacePath",
        registration: "register_workspace_desktop_methods",
        capability: Some("Opener + desktop probe"),
    },
    Declared {
        method: "session.workspacePathApplications",
        registration: "register_workspace_desktop_methods",
        capability: Some("Opener + desktop probe"),
    },
    Declared {
        method: "session.resume",
        registration: "register_session_resume_method",
        capability: Some("AgentRuntime"),
    },
    Declared {
        method: "session.state",
        registration: "register_session_state_method_with",
        capability: Some("AgentRuntime (live turns)"),
    },
    Declared {
        method: "session.control",
        registration: "register_session_state_method_with",
        capability: Some("SessionCatalog + ProjectionRegistry"),
    },
    Declared {
        // One observation per call; `girder_rpc::stream` polls it. Upstream holds one connection
        // open and pushes frames, which is the same series with the pacing on the other side.
        method: "session.observeEvent",
        registration: "register_session_state_method_with",
        capability: Some("AgentRuntime (live turns)"),
    },
    Declared {
        method: "session.observeProjections",
        registration: "register_session_state_method_with",
        capability: Some("ProjectionRegistry"),
    },
    Declared {
        method: "session.observeState",
        registration: "register_session_state_method_with",
        capability: Some("AgentRuntime (live turns)"),
    },
    Declared {
        // Feedback is a log event, so `list` replays and the two writes need a store.
        method: "messageFeedback.list",
        registration: "register_message_feedback_methods",
        capability: Some("SessionCatalog"),
    },
    Declared {
        method: "schedule.history",
        registration: "register_schedule_methods",
        capability: Some("ScheduleStore"),
    },
    Declared {
        // Delivering input to a running child, over the two inbox queues the agent loop reads.
        method: "subagents.prompt",
        registration: "register_subagent_methods",
        capability: Some("RunningChildren"),
    },
    Declared {
        method: "subagents.interruptByUser",
        registration: "register_subagent_methods",
        capability: Some("RunningChildren"),
    },
    Declared {
        // The MCP subsystem was already here and had no endpoint.
        method: "mcpManager.probe",
        registration: "register_mcp_manager_methods",
        capability: Some("ServerConfig"),
    },
    Declared {
        method: "mcpManager.restart",
        registration: "register_mcp_manager_methods",
        capability: Some("ServerConfig"),
    },
    Declared {
        method: "mcpManager.watch",
        registration: "register_mcp_manager_methods",
        capability: Some("MCP registry"),
    },
    Declared {
        // Served from a file the endpoint survey missed: it scanned `src/index.ts` only.
        method: "skills.list",
        registration: "register_skill_methods",
        capability: Some("Host"),
    },
    Declared {
        // Served from a file the endpoint survey missed: it scanned `src/index.ts` only.
        method: "fileReferences.list",
        registration: "register_file_reference_methods",
        capability: Some("PathBuf"),
    },
    Declared {
        method: "credentials.describe",
        registration: "register_credential_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "credentials.set",
        registration: "register_credential_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "credentials.unset",
        registration: "register_credential_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        // Served from a file the endpoint survey missed: it scanned `src/index.ts` only.
        method: "directoryPicker.list",
        registration: "register_directory_picker",
        capability: Some("DirectoryPickerBackend"),
    },
    Declared {
        // Served from a file the endpoint survey missed: it scanned `src/index.ts` only.
        method: "directoryPicker.pick",
        registration: "register_directory_picker",
        capability: Some("DirectoryPickerBackend"),
    },
    Declared {
        // Served from a file the endpoint survey missed: it scanned `src/index.ts` only.
        method: "directoryPicker.createDirectory",
        registration: "register_directory_picker",
        capability: Some("DirectoryPickerBackend"),
    },
    Declared {
        method: "pluginInventory.list",
        registration: "register_plugin_inventory_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.listPlugins",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.listBundles",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.listVersions",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.readBundleDetail",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.inspect",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.registries",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.listVersionExemptions",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.setPluginEnabled",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.setBundleEnabled",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.installBundle",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.updateBundle",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.removeBundle",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.cancelInstall",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.setVersionExemption",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "pluginManager.waitForInstall",
        registration: "register_plugin_manager_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "officeKit.generation",
        registration: "register_office_kit_methods",
        capability: Some("officeKit"),
    },
    Declared {
        method: "officeKit.render",
        registration: "register_office_kit_methods",
        capability: Some("officeKit"),
    },
    Declared {
        method: "account.getState",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.getProfile",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.getBalance",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.getNotifications",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.getNotificationContent",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.reportNotificationEvent",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.startSignIn",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.cancelSignIn",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.signOut",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.hasRunningAccountTasks",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.watchState",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.watch",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "account.watchExpiry",
        registration: "register_account_methods",
        capability: Some("CredentialSource"),
    },
    Declared {
        method: "actionCards.act",
        registration: "register_action_card_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.getClientCode",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.inventory",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.invoke",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.reportClientGuardFailure",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.reportRenderFailure",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.resolveInspectQuery",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.resolveRequestRun",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.runHostHalf",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.settleUserRun",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.stopFromPanel",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.syncInspectManifest",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        method: "dynamicCordisRunner.undefineFromPanel",
        registration: "register_dynamic_runner_methods",
        capability: None,
    },
    Declared {
        // Whether a session's tree runs as a team. The root is the caller's assertion, checked
        // rather than trusted, because this build holds no subagent tree to derive it.
        method: "agentTeams.setEnabled",
        registration: "register_agent_team_methods",
        capability: Some("SessionCatalog"),
    },
    Declared {
        // The capability sets a session may start from. This build's presets name tools and a
        // model rather than a Cordis plugin list.
        method: "agentPresets.list",
        registration: "register_agent_preset_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "agentPresets.read",
        registration: "register_agent_preset_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "agentPresets.select",
        registration: "register_agent_preset_methods",
        capability: Some("Registry"),
    },
    Declared {
        method: "terminal.rename",
        registration: "register_terminal_rename_method",
        capability: Some("TerminalRegistry"),
    },
    Declared {
        method: "terminal.retain",
        registration: "register_terminal_retain_method",
        capability: Some("TerminalPersistStore"),
    },
    Declared {
        method: "workspace.rename",
        registration: "register_workspace_extra_methods",
        capability: Some("WorkspaceRegistry + WorkspacePreferences"),
    },
    Declared {
        method: "workspace.insertBefore",
        registration: "register_workspace_extra_methods",
        capability: Some("WorkspaceRegistry + WorkspacePreferences"),
    },
    Declared {
        method: "workspace.insertSessionBefore",
        registration: "register_workspace_extra_methods",
        capability: Some("WorkspaceRegistry + WorkspacePreferences"),
    },
    Declared {
        method: "workspace.archiveSession",
        registration: "register_workspace_extra_methods",
        capability: Some("WorkspaceRegistry + WorkspacePreferences"),
    },
    Declared {
        method: "workspace.unarchiveSession",
        registration: "register_workspace_extra_methods",
        capability: Some("WorkspaceRegistry + WorkspacePreferences"),
    },
    Declared {
        method: "workspace.pinSession",
        registration: "register_workspace_extra_methods",
        capability: Some("WorkspaceRegistry + WorkspacePreferences"),
    },
    Declared {
        method: "workspace.unpinSession",
        registration: "register_workspace_extra_methods",
        capability: Some("WorkspaceRegistry + WorkspacePreferences"),
    },
    Declared {
        method: "workspace.follow",
        registration: "register_workspace_follow",
        capability: Some("WorkspaceFeed"),
    },
    Declared {
        // Creates the first workspace, and only when there is none: a client calls it on every
        // start, so a version that always created would add a workspace per launch.
        method: "workspace.initializeDefault",
        registration: "register_workspace_initialize_default",
        capability: Some("WorkspaceRegistry"),
    },
    Declared {
        method: "settings.describe",
        registration: "register_settings_methods",
        capability: Some("SettingsStore"),
    },
    Declared {
        method: "settings.openSettingsDocument",
        registration: "register_settings_methods",
        capability: Some("SettingsStore"),
    },
    Declared {
        // The sessions a `@`-mention may point at, ranked so the caller's own directory comes first.
        method: "sessionReferenceResolver.candidates",
        registration: "register_session_reference_methods",
        capability: Some("SessionCatalog"),
    },
    Declared {
        // Session-level feedback: a remark about the conversation rather than one message.
        method: "sessionFeedback.record",
        registration: "register_session_feedback_methods",
        capability: Some("SessionCatalog"),
    },
    Declared {
        method: "messageFeedback.put",
        registration: "register_message_feedback_methods",
        capability: Some("SessionCatalog"),
    },
    Declared {
        method: "messageFeedback.delete",
        registration: "register_message_feedback_methods",
        capability: Some("SessionCatalog"),
    },
    Declared {
        // The search and fetch endpoints this deployment registered. Two lists because the two
        // capabilities are separate: a deployment may have fetch and no search index.
        method: "web.listProviders",
        registration: "register_web_methods",
        capability: Some("WebProviders"),
    },
    Declared {
        // Every selectable preset, in contribution order. Three lists because a selector needs to
        // distinguish "may pick now" from "offered as a default".
        method: "permissionPresets.catalog",
        registration: "register_permission_preset_methods",
        capability: Some("Catalogue"),
    },
    Declared {
        method: "permissionPresets.set",
        registration: "register_permission_preset_methods",
        capability: Some("Catalogue"),
    },
    Declared {
        method: "pluginRecommendations.describe",
        registration: "register_plugin_recommendation_methods",
        capability: None,
    },
    Declared {
        method: "remoteAccess.follow",
        registration: "register_remote_access_follow",
        capability: None,
    },
    Declared {
        // Admits a run, then reports the delivery it cannot make. Both facts travel: a client that
        // saw only "admitted" would believe a message was sent.
        method: "schedule.run",
        registration: "register_schedule_methods",
        capability: Some("ScheduleStore"),
    },
    Declared {
        // Update one field or delete the task: upstream's single edit endpoint, because a client
        // edits a task in place and a delete is the edit whose patch is everything.
        method: "schedule.edit",
        registration: "register_schedule_methods",
        capability: Some("ScheduleStore"),
    },
    Declared {
        // Upstream's bulk removal of inactive tasks, and the two reads a picker needs before it
        // schedules anything.
        method: "schedule.catalog",
        registration: "register_schedule_methods",
        capability: Some("ScheduleStore"),
    },
    Declared {
        method: "schedule.deleteInactive",
        registration: "register_schedule_methods",
        capability: Some("ScheduleStore"),
    },
    Declared {
        method: "schedule.deliveryLimits",
        registration: "register_schedule_methods",
        capability: Some("ScheduleStore"),
    },
    Declared {
        // The three endpoints upstream marks `@Remote` on `LlmRuntime`. The adapter answers from
        // this deployment's route table.
        method: "llm.listProviders",
        registration: "register_llm_remote",
        capability: Some("RouteRegistry"),
    },
    Declared {
        method: "llm.listConfigurableProviders",
        registration: "register_llm_remote",
        capability: Some("RouteRegistry"),
    },
    Declared {
        method: "llm.discoverModels",
        registration: "register_llm_remote",
        capability: Some("RouteRegistry"),
    },
    Declared {
        // The market index this build parses, reachable from a client. `setIndexes` takes the
        // already-parsed document: a network read is a deployment decision, so the I/O stays above.
        method: "pluginMarket.listIndexes",
        registration: "register_market_methods",
        capability: Some("MarketCatalog"),
    },
    Declared {
        method: "pluginMarket.setIndexes",
        registration: "register_market_methods",
        capability: Some("MarketCatalog"),
    },
    Declared {
        method: "pluginMarket.search",
        registration: "register_market_methods",
        capability: Some("MarketCatalog"),
    },
    Declared {
        method: "pluginMarket.readEntryDetail",
        registration: "register_market_methods",
        capability: Some("MarketCatalog"),
    },
    Declared {
        // Chooses a default model when the deployment has none. Idempotent against a configured
        // deployment, and an error rather than a no-op when nothing can serve a model.
        method: "session.initializeDefaultModel",
        registration: "register_session_core",
        capability: Some("RouteRegistry"),
    },
    Declared {
        // One observation plus the stopping decision. Upstream's `wait` loops an observation until
        // the decision is terminal; here a client calls again, which keeps a long-lived request off
        // this transport.
        method: "session.wait",
        registration: "register_session_state_method_with",
        capability: Some("AgentRuntime (live turns)"),
    },
    Declared {
        method: "session.duplicate",
        registration: "register_session_lifecycle_methods_with_archive",
        capability: Some("ArchiveStore"),
    },
    Declared {
        method: "session.archive",
        registration: "register_session_lifecycle_methods_with_archive",
        capability: Some("ArchiveStore"),
    },
    Declared {
        method: "session.updateQueue",
        registration: "register_session_queue_methods",
        capability: Some("QueueAgentSource"),
    },
    // ---- command ---------------------------------------------------------------------------
    // Registered by `register_session_core`, because a slash command is available wherever a
    // session is; listed here rather than under `session` because the wire name says `command`.
    Declared {
        method: "commands.list",
        registration: "register_command_methods",
        capability: None,
    },
    Declared {
        method: "commands.execute",
        registration: "register_command_methods",
        capability: None,
    },
    // ---- job -------------------------------------------------------------------------------
    Declared {
        method: "job.list",
        registration: "register_job_methods",
        capability: None,
    },
    Declared {
        method: "job.kill",
        registration: "register_job_methods",
        capability: None,
    },
    Declared {
        method: "job.follow",
        registration: "register_job_follow",
        // Registered by its own function, which takes a `JobRegistry` beside the session catalog: `register_job_methods`
        // covers the list and kill surface, and follow needs the registry the CLI builds for background jobs.
        capability: Some("JobRegistry for follow"),
    },
    // ---- extension -------------------------------------------------------------------------
    Declared {
        method: "extension.list",
        registration: "register_extension_methods",
        capability: Some("ExtensionRegistry"),
    },
    Declared {
        method: "extension.approve",
        registration: "register_extension_methods",
        capability: Some("ExtensionRegistry"),
    },
    // Running an approved extension needs a way to run it. The shell runtime is one choice a host
    // makes, so the registration takes the choice rather than assuming it.
    Declared {
        method: "extension.activate",
        registration: "register_extension_run_methods",
        capability: Some("ExtensionRuntime"),
    },
    Declared {
        method: "extension.deactivate",
        registration: "register_extension_run_methods",
        capability: Some("ExtensionRuntime"),
    },
    Declared {
        method: "extension.status",
        registration: "register_extension_run_methods",
        capability: Some("ExtensionRuntime"),
    },
    // ---- host ------------------------------------------------------------------------------
    Declared {
        method: "host.openPath",
        registration: "register_host_methods",
        capability: Some("Opener"),
    },
    Declared {
        method: "host.revealInFileManager",
        registration: "register_host_methods",
        capability: Some("Opener"),
    },
    // ---- terminal --------------------------------------------------------------------------
    // The two environment probes read nothing: one reports deployment constants, the other probes
    // `PATH`, so `register_terminal_extra_methods` takes no registry and no launcher.
    Declared {
        method: "terminal.environment",
        registration: "register_terminal_extra_methods",
        capability: Some("TerminalLauncher"),
    },
    Declared {
        method: "terminal.shells",
        registration: "register_terminal_extra_methods",
        capability: Some("TerminalLauncher discovery"),
    },
    Declared {
        method: "terminal.list",
        registration: "register_terminal_methods",
        capability: Some("TerminalRegistry + TerminalLauncher"),
    },
    Declared {
        method: "terminal.create",
        registration: "register_terminal_methods",
        capability: Some("TerminalRegistry + TerminalLauncher"),
    },
    Declared {
        method: "terminal.send",
        registration: "register_terminal_methods",
        capability: Some("TerminalRegistry + TerminalLauncher"),
    },
    Declared {
        method: "terminal.read",
        registration: "register_terminal_methods",
        capability: Some("TerminalRegistry + TerminalLauncher"),
    },
    Declared {
        method: "terminal.close",
        registration: "register_terminal_methods",
        capability: Some("TerminalRegistry + TerminalLauncher"),
    },
    Declared {
        method: "terminal.attach",
        registration: "register_terminal_methods",
        capability: Some("TerminalRegistry + TerminalLauncher"),
    },
    Declared {
        method: "terminal.watch",
        registration: "register_terminal_methods",
        capability: Some("TerminalRegistry + TerminalLauncher"),
    },
    // ---- workspace -------------------------------------------------------------------------
    Declared {
        method: "workspaceFiles.list",
        registration: "register_workspace_file_extras",
        capability: Some("FileSystem + WorkspaceRegistry"),
    },
    Declared {
        method: "workspaceFiles.changes",
        registration: "register_workspace_file_methods",
        capability: Some("FileSystem + WorkspaceRegistry"),
    },
    Declared {
        method: "workspaceFiles.readBytes",
        registration: "register_workspace_file_methods",
        capability: Some("FileSystem + WorkspaceRegistry"),
    },
    Declared {
        method: "workspaceFiles.stat",
        registration: "register_workspace_file_extras",
        capability: Some("FileSystem + WorkspaceRegistry"),
    },
    Declared {
        method: "workspaceFiles.read",
        registration: "register_workspace_file_extras",
        capability: Some("FileSystem + WorkspaceRegistry"),
    },
    Declared {
        method: "workspaceFiles.search",
        registration: "register_workspace_file_extras",
        capability: Some("FileSystem + WorkspaceRegistry"),
    },
    Declared {
        method: "workspaceFiles.watch",
        registration: "register_workspace_file_methods",
        capability: Some("FileSystem + WorkspaceRegistry"),
    },
    // The three registry-backed workspace methods take a `WorkspaceRegistry`, but the declaration of
    // their *names* also comes from the `api_config` catalogue, which is why a listing with no
    // workspace table still reports them. `list` is registered twice -- once here and once, with
    // archive filtering, by the optional preferences surface -- and is declared once.
    Declared {
        method: "workspace.list",
        registration: "register_workspace_methods",
        capability: Some("FileSystem"),
    },
    Declared {
        method: "workspace.create",
        registration: "register_workspace_methods",
        capability: Some("workspace registry"),
    },
    Declared {
        method: "workspace.delete",
        registration: "register_workspace_methods",
        capability: Some("workspace registry"),
    },
    // ---- schedule --------------------------------------------------------------------------
    Declared {
        method: "schedule.create",
        registration: "register_schedule_methods",
        capability: Some("ScheduleStore"),
    },
    Declared {
        method: "schedule.list",
        registration: "register_schedule_methods",
        capability: Some("ScheduleStore"),
    },
    Declared {
        method: "schedule.view",
        registration: "register_schedule_methods",
        capability: Some("ScheduleStore"),
    },
    // ---- settings --------------------------------------------------------------------------
    Declared {
        method: "settings.get",
        registration: "register_settings_methods",
        capability: Some("SettingsStore"),
    },
    Declared {
        method: "settings.set",
        registration: "register_settings_methods",
        capability: Some("SettingsStore"),
    },
    Declared {
        method: "settings.list",
        registration: "register_settings_methods",
        capability: Some("SettingsStore"),
    },
    Declared {
        method: "settings.delete",
        registration: "register_settings_methods",
        capability: Some("SettingsStore"),
    },
    Declared {
        method: "settings.update",
        registration: "register_settings_methods",
        capability: Some("SettingsStore"),
    },
    Declared {
        method: "settings.replace",
        registration: "register_settings_methods",
        capability: Some("SettingsStore"),
    },
    Declared {
        method: "settings.mutate",
        registration: "register_settings_methods",
        capability: Some("SettingsStore"),
    },
    // ---- goal ------------------------------------------------------------------
    Declared {
        method: "goals.get",
        registration: "register_goal_methods",
        capability: Some("GoalStore"),
    },
    Declared {
        method: "goals.list",
        registration: "register_goal_methods",
        capability: Some("GoalStore"),
    },
    Declared {
        method: "goals.create",
        registration: "register_goal_methods",
        capability: Some("GoalStore"),
    },
    Declared {
        method: "goals.edit",
        registration: "register_goal_methods",
        capability: Some("GoalStore"),
    },
    Declared {
        method: "goals.pause",
        registration: "register_goal_methods",
        capability: Some("GoalStore"),
    },
    Declared {
        method: "goals.resume",
        registration: "register_goal_methods",
        capability: Some("GoalStore"),
    },
    Declared {
        method: "goals.complete",
        registration: "register_goal_methods",
        capability: Some("GoalStore"),
    },
    Declared {
        method: "goals.clear",
        registration: "register_goal_methods",
        capability: Some("GoalStore"),
    },
    // ---- workflow ------------------------------------------------------------------
    Declared {
        method: "workflow.list",
        registration: "register_workflow_methods",
        capability: Some("WorkflowRegistry"),
    },
    Declared {
        method: "workflow.get",
        registration: "register_workflow_methods",
        capability: Some("WorkflowRegistry"),
    },
    Declared {
        method: "workflow.start",
        registration: "register_workflow_methods",
        capability: Some("WorkflowRegistry"),
    },
    Declared {
        method: "workflow.cancel",
        registration: "register_workflow_methods",
        capability: Some("WorkflowRegistry"),
    },
    // ---- userQuestions ------------------------------------------------------------------
    Declared {
        method: "userQuestions.list",
        registration: "register_question_methods",
        capability: Some("QuestionQueue"),
    },
    Declared {
        method: "userQuestions.answer",
        registration: "register_question_methods",
        capability: Some("QuestionQueue"),
    },
    Declared {
        method: "userQuestions.attachWait",
        registration: "register_question_methods",
        capability: Some("QuestionQueue"),
    },
    Declared {
        method: "userQuestions.cancel",
        registration: "register_question_methods",
        capability: Some("QuestionQueue"),
    },
];

/// One group of methods, as the build declares it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Surface {
    /// Every declared method, as `namespace.method`.
    pub methods: Vec<String>,
    /// The subset of [`Surface::methods`] a registry cannot contain without a host-supplied
    /// collaborator.
    ///
    /// These are the names a caller sees as missing when it compares the declaration against a
    /// registry assembled from nothing but an empty catalog, a fresh job registry, and a fresh
    /// projection registry -- a terminal needs a launcher, a workspace file read needs a
    /// filesystem, a resumed session needs a runtime, and none of those can be fabricated.
    pub needs_collaborator: Vec<String>,
}

/// The methods this build declares.
///
/// Read from [`DECLARED`] rather than built by registering handlers: a registry needs collaborators,
/// and which methods exist is a fact about the source that does not. A caller that wants to know
/// what a server *would* serve can ask before anything is wired -- which is what a schema command,
/// a client generator, and a boot check all need.
///
/// The result is in declaration order, grouped by namespace, and contains no duplicates; see the
/// tests for the guarantees, and [`reconcile`] for comparing it against a real registry.
#[must_use]
pub fn declared() -> Surface {
    Surface {
        methods: DECLARED
            .iter()
            .map(|entry| entry.method.to_owned())
            .collect(),
        needs_collaborator: DECLARED
            .iter()
            .filter(|entry| entry.capability.is_some())
            .map(|entry| entry.method.to_owned())
            .collect(),
    }
}

/// The `register_*` function that adds `method`, if this build declares it.
///
/// Exposed because the declaration's only claim to correctness is traceability: a reader who doubts
/// an entry can name the function to open instead of searching the tree. A missing name returns
/// `None` rather than an empty string, so a caller cannot mistake "not declared" for "declared by
/// nobody".
///
/// A function rather than a field on [`Surface`], because the mapping is a debugging and review aid;
/// putting one `&'static str` beside every name in the public surface would make the common case --
/// "what does this build serve" -- carry detail it does not need.
#[must_use]
pub fn registration_of(method: &str) -> Option<&'static str> {
    DECLARED
        .iter()
        .find(|entry| entry.method == method)
        .map(|entry| entry.registration)
}

/// What a comparison between the declaration and a registry disagreed about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discrepancy {
    /// Declared but not registered: the declaration promises a method the build does not serve, so a
    /// client that trusts it sends a call that answers `UNKNOWN_METHOD`.
    pub missing: Vec<String>,
    /// Registered but not declared: the build serves a method the declaration does not mention, so
    /// anything generated from the declaration -- a document, a typed client, a doctor check --
    /// silently omits it.
    ///
    /// Also where a method an optional surface registers lands while `serve` does not call it. An
    /// empty `extra` means every method a catalogue names is also declared and served.
    pub extra: Vec<String>,
}

/// Compares the declaration against the methods a registry actually holds.
///
/// `registered` is the real registry's names, qualified the same way. The comparison is set-based:
/// duplicates in either side collapse, and both output lists are sorted, so two callers that
/// assembled their input differently still get the same answer. An empty [`Discrepancy`] means the
/// declaration and the build agree.
#[must_use]
pub fn reconcile(registered: &[String]) -> Discrepancy {
    use std::collections::BTreeSet;

    let declared: BTreeSet<&str> = DECLARED.iter().map(|entry| entry.method).collect();
    let actual: BTreeSet<&str> = registered.iter().map(String::as_str).collect();

    Discrepancy {
        missing: declared
            .difference(&actual)
            .map(|name| (*name).to_owned())
            .collect(),
        extra: actual
            .difference(&declared)
            .map(|name| (*name).to_owned())
            .collect(),
    }
}
