//! Core primitives for the Girder harness.
//!
//! Girder is a Rust rewrite of `DeepSeek Harness`. Upstream is built on Cordis, where "everything
//! is a plugin": each capability is contributed by a plugin, and plugins reach each other by
//! resolving *services* from a shared context rather than by importing implementations.
//!
//! This crate carries the two primitives that model rests on, and nothing else yet:
//!
//! - [`ServiceKey`] and [`Registry`] — typed capability lookup over a type-erased table. This
//!   replaces Cordis' `ctx.anything` proxy, which Rust cannot express and which the migration
//!   plan abandons rather than emulates (§1.2, third layer).
//! - [`Scope`] — effects and reverse-order disposal. Every contribution a plugin makes is
//!   registered here, so teardown belongs to the registry instead of the plugin.
//!
//! Deliberately absent: the fiber state machine, the event bus, and the config layer. Those are
//! phases 1 through 3 of `docs/migration-plan.md`; building them before these primitives settle
//! would fix their interfaces too early.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//!
//! use girder_core::{Registry, ServiceKey};
//!
//! trait Greeter: Send + Sync {
//!     fn greet(&self) -> String;
//! }
//!
//! struct GreeterKey;
//!
//! impl ServiceKey for GreeterKey {
//!     type Service = dyn Greeter;
//!     const ID: &'static str = "greeter";
//! }
//!
//! struct English;
//!
//! impl Greeter for English {
//!     fn greet(&self) -> String {
//!         "hello".to_owned()
//!     }
//! }
//!
//! let registry = Registry::new();
//! let ctx = registry.context("greeter-plugin");
//! ctx.register::<GreeterKey>(Arc::new(English)).unwrap();
//!
//! let greeter = registry.get::<GreeterKey>().unwrap();
//! assert_eq!(greeter.greet(), "hello");
//! ```

mod access;
mod agent;
mod agent_loop;
// The derive emits `::girder_core::…`, which cannot resolve inside `girder-core` itself. This alias
// lets generated code compile here as well as downstream; it is the supported way to name a crate
// from within itself.
extern crate self as girder_core;

mod api_catalog;
pub mod api_config;
pub mod api_job;
mod api_key;
pub mod api_session;
mod approval;
mod assistant_output;
mod atomic_write;
mod attachment;
mod attachment_limits;
mod attachment_rpc;
mod attribution;
mod block_assembler;
mod boot;
mod boot_check;
mod boot_stages;
mod bounded_queue;
mod brand;
mod builtin_commands;
mod call_config;
mod chunked_list;
mod code_language;
mod commands;
mod compaction;
mod compaction_hook;
mod config;
mod config_edit;
mod context_budget;
mod credentials;
mod deliverable;
mod events;
mod exec;
mod extension_run_rpc;
mod extension_runtime;
mod extensions;
mod fiber;
mod file_tools;
mod file_watch;
mod fs;
mod fs_rpc;
mod fs_tools;
mod goal;
mod goal_tool;
mod home_paths;
mod host_rpc;
mod inbox;
mod instructions;
mod instructions_tool;
mod job_follow_rpc;
mod job_tools;
mod jobs;
mod json;
mod launch_environment;
mod limits;
mod llm;
mod llm_deepseek;
mod llm_stats;
mod lsp;
mod lsp_tool;
mod mcp;
mod mcp_tools;
mod message;
mod model_catalog_extra;
mod observation;
mod office_document;
mod openapi;
mod output_budget;
mod patch;
mod paths;
mod permissions;
pub mod persistent_shell;
mod plugin_lifecycle;
mod present_tool;
mod process_group;
mod projection;
mod projection_extra;
mod projection_host;
mod providers;
mod ptc_policy;
mod questions;
mod registry;
mod remote;
mod retention;
mod retry;
mod retrying_provider;
mod rpc_limits;
mod runtime;
mod schedule;
mod schedule_rpc;
mod schedule_runner;
mod scope;
mod search_index;
mod search_tools;
mod service;
mod session;
mod session_archive;
mod session_export_rpc;
mod session_filter;
mod session_layout;
mod session_lifecycle_rpc;
mod session_metrics;
mod session_migration;
mod session_portable;
mod session_query;
mod session_query_rpc;
mod session_queue_rpc;
mod session_resume_rpc;
mod session_rewind;
mod session_search_tool;
mod session_state_rpc;
mod session_stats;
mod session_timeline;
mod session_title;
mod settings_rpc;
mod shell;
mod skill;
mod skill_catalog;
mod skill_tool;
mod spill;
mod spill_policy;
mod ssh;
mod storage;
mod str_replace_editor;
mod stream_blocks;
mod stream_method;
mod subagent;
mod subagent_depth;
mod subagent_interrupt;
mod subagent_pool;
mod subagent_providers;
mod subagent_tools;
mod subprocess;
mod system_prompt;
mod terminal;
mod terminal_local;
mod terminal_rpc;
mod terminal_rpc_extra;
mod terminal_tool;
mod time;
mod todo_tool;
mod token_meter;
mod tool_batching;
mod tool_contracts;
mod tool_result_pruning;
mod tool_retry;
mod tool_schema_export;
mod tool_usage;
mod tools;
mod typert;
mod typert_json;
mod web;
mod web_tools;
mod workflow;
mod workflow_tool;
mod workspace_desktop_rpc;
mod workspace_diff;
mod workspace_files_extra;
mod workspace_files_rpc;
mod workspace_open_rpc;
mod workspace_path;
mod workspace_rpc_extra;

pub use access::{
    Access, AllowList, Decision as AccessDecision, Declaration, DenyAll, Policy, ReadOnly,
    Request as AccessRequest, Resource as AccessResource,
};
pub use agent::{
    Agent, AgentHandle, AgentOptions, AgentSpec, ModelSelection, ModelSelectionState,
    ModelSelectionView, SelectionError, SelectionSource, validate as validate_model_selection,
};
pub use agent_loop::{LoopContext, LoopError, LoopStep, dispatch, run};
pub use api_catalog::{Catalog, CatalogError, EventEntry, MethodEntry, ServiceEntry};
pub use api_key::{KeyCheck, check as check_api_key, normalize as normalize_api_key};
/// The session Remote surface, namespaced because its wire types share names with the
/// service-side request types in `remote`.
pub use api_session as session_api;
pub use approval::{
    ApprovalError, Approver, DEFAULT_TIMEOUT as DEFAULT_APPROVAL_TIMEOUT, DecidedBy, Decision,
    Record as ApprovalRecord, Request as ApprovalRequest, Scripted as ScriptedApprover,
    Unavailable as UnavailableApprover,
};
pub use assistant_output::{FinalOutput, final_output, final_text};
pub use atomic_write::{
    AtomicError, DEFAULT_LOCK_WAIT, Mode, with_lock, write_atomic, write_text_atomic,
};
pub use attachment::{
    ACCEPTED_IMAGE_TYPES, AttachmentError, AttachmentStore,
    DEFAULT_CAPACITY as ATTACHMENT_CAPACITY, DecodedImage, EncodedImage, ImageRef,
    Limits as AttachmentLimits, MemoryStore, admit_batch, admit_image, decode_base64_canonical,
    detect_media_type, encode_base64, storage_key as attachment_storage_key, store_batch,
};
pub use attachment_limits::{
    Policy as AttachmentPolicy, RejectReason, Verdict, check as check_attachment, is_image,
};
pub use attachment_rpc::register_attachment_upload_method;
pub use attribution::Attribution as ContentAttribution;
pub use block_assembler::{Assembler as BlockAssembler, AssemblyError};
pub use boot::{
    BootConfig, BootError, BootReport, Booted, DEFAULT_ENDPOINT, DEFAULT_MODEL,
    Outcome as BootOutcome, boot,
};
pub use boot_check::{
    Deployment, Finding, Severity, check as check_deployment, is_fatal as boot_is_fatal,
    may_be_a_directory, report as boot_report,
};
pub use boot_stages::{BootTrace, Stage as BootStage, StageOutcome as BootStageOutcome};
pub use bounded_queue::{BoundedQueue, Closed, Overflow};
pub use brand::{
    AgentId, AgentIdBrand, Brand, Id, IdError, MAX_ID_LEN, MessageId, MessageIdBrand, SessionId,
    SessionIdBrand, ToolCallId, ToolCallIdBrand, WorkspaceId, WorkspaceIdBrand,
};
pub use builtin_commands::register_builtin_commands;
pub use call_config::{CallConfig, ConfigError as CallConfigError, ConfigTable};
pub use chunked_list::{CHUNK_CAPACITY, List as ChunkedList};
pub use code_language::{for_extension, for_path, read_hint};
pub use commands::{
    Arguments as CommandArguments, CommandError, CommandId, CommandResult,
    Definition as CommandDefinition, DefinitionId as CommandDefinitionId,
    Handler as CommandHandler, Input as CommandInput, Registry as CommandRegistry,
};
pub use compaction::{
    CHECKPOINT_PREAMBLE, COMPACTION_INSTRUCTION, CompactionError, CompactionOutcome,
    CompactionRequest, CompactionTrigger, PairingBalance, SUMMARY_CLOSE_TAG, SUMMARY_OPEN_TAG,
    Selection, SummaryError, SurfaceEntry, balances, checkpoint_message, extract_summary,
    is_checkpoint, needs_compaction, pairing_balanced_at, select as select_compaction,
    summary_request, transcript_for_summary,
};
pub use compaction_hook::{CompactionPolicy, Plan as CompactionPlan, history_cost, message_cost};
pub use config::{
    ConfigError, ConfigSchema, ConfigValue, Entry, EntryList, Environment, FieldError, FieldRule,
    FieldSchema, Owner, Patch, PatchWarning, Template, When, parse_entries, parse_patches,
};
pub use config_edit::{SchemaError, SchemaRegistry, SettingSpec};
pub use context_budget::{
    Budget as ContextBudget, Decision as BudgetDecision, Usage as ContextUsage,
    check as check_context_budget,
};
pub use credentials::{
    CredentialError, CredentialRef, Info as CredentialInfo, Record as CredentialRecord,
    Store as CredentialStore, valid_environment,
};
pub use deliverable::{Deliverable, Kind as DeliverableKind, Set as DeliverableSet};
pub use events::{
    Bail, Chain, DispatchError, Event, EventBus, Layer, ListenerId, ListenerScope, Next,
    ObservedEvent, Order, Parallel, ScopeKey, Serial, Terminal, Waterfall, child_scope,
};
pub use exec::{
    AdvisorySandbox, Enforcement, ExecError, ExecOptions, ExecOutput, PathPolicy, Sandbox,
    bash_tool, has_program, landlock_abi, read_file, run_bash, run_shell,
};
pub use extension_run_rpc::{
    register_extension_run_methods, register_extension_run_methods_with_services,
};
pub use extension_runtime::{
    NoopRuntime as ExtensionNoopRuntime, ShellRuntime as ExtensionShellRuntime, assert_runnable,
};
pub use extensions::{
    Definition as ExtensionDefinition, ExtensionError, ExtensionId, Registry as ExtensionRegistry,
    Run as ExtensionRun, Runtime as ExtensionRuntime, Status as ExtensionStatus,
};
pub use fiber::{Disposer, Fiber, FiberError, FiberState, PluginBody, PluginSpec};
pub use file_tools::{MAX_READ_BYTES, ask_tool, edit_tool, patch_tool, read_tool, write_tool};
pub use file_watch::{
    Change as FileChange, ChangeKind as FileChangeKind, PollingWatcher, snapshot as watch_snapshot,
    stamp as watch_stamp,
};
pub use fs::{
    FileSystem, FsDirEntry, FsError, FsInfo, FsTarget, FsTargetKey, FsType, FsVersion,
    LocalFileSystem, read_file as read_file_via_fs, target_at,
};
pub use fs_rpc::{DEFAULT_LIST_LIMIT as FS_LIST_LIMIT, register_fs_methods};
pub use fs_tools::{copy_tool, delete_tool, make_dir_tool, move_tool, register_fs_tools};
pub use girder_macros::{Typert, remote};
pub use goal::{
    Activation, BlockReason, DEFAULT_MAX_GOAL_ROUNDS, Goal, GoalError, GoalId, GoalRef, GoalState,
    Phase, check_reference as check_goal_reference,
};
pub use goal_tool::goal_tool;
pub use home_paths::Paths as HomePaths;
pub use host_rpc::{
    Opener, SystemOpener, UnavailableOpener, register_host_methods, register_host_methods_with,
};
pub use inbox::{Inbox, InboxError, Splice, Target as InboxTarget, replay as replay_inbox};
pub use instructions::{
    DEFAULT_MAX_BYTES_PER_FILE, DEFAULT_MAX_TOTAL_BYTES, HARNESS_GLOBAL_DIRECTORY,
    INSTRUCTION_FILE_NAME, InstructionFile, InstructionLimits, InstructionState, LoadedInstruction,
    RenderedInstructions, TruncatedInstruction, USER_GLOBAL_DIRECTORY, digest,
    discover as discover_instructions, load as load_instructions, render as render_instructions,
};
pub use instructions_tool::{
    MAX_OUTPUT_BYTES as INSTRUCTIONS_MAX_OUTPUT_BYTES, TOOL_NAME as INSTRUCTIONS_TOOL_NAME,
    instructions_tool,
};
pub use job_follow_rpc::register_job_follow;
pub use job_tools::{
    job_kill_tool, job_list_tool, job_output_tool, job_start_tool, register_job_tools,
};
pub use jobs::{
    Channel as JobChannel, Job, JobChunk, JobError, JobId, JobProducer, JobRegistry,
    JobRegistryHandle, Outcome as JobOutcome, OutputRing, Status as JobStatus, spawn_job,
};
pub use json::{
    JsonHazard, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER, hazards, is_lossless, normalize,
    number_from_f64, number_from_u64,
};
pub use launch_environment::{
    AMBIENT as AMBIENT_VARIABLES, NEVER as NEVER_VARIABLES, Resolved as LaunchEnvironment,
    is_ambient, is_forbidden, resolve as resolve_launch_environment,
};
pub use limits::{Budget, Ceiling, Session as SessionBudget};
pub use llm::{
    AssembleError, AssistantMessage, ChunkStream, ContentBlock, FinishReason, LlmError, LlmKey,
    LlmProvider, LlmRequest, Message, Role, StreamChunk, TokenUsage, ToolSchema,
};
pub use llm_deepseek::{
    DeepSeekConfig, DeepSeekProvider, SseEvent, SseParser, Translator, provider_from_env,
};
pub use llm_stats::Stats as LlmStats;
pub use lsp::{
    Answer as LspAnswer, Location as LspLocation, LspError, Operation as LspOperation,
    Position as LspPosition, Provider as LspProvider, Range as LspRange, Registry as LspRegistry,
    Request as LspRequest, Static as StaticLsp, utf16_column_to_byte,
};
pub use lsp_tool::{character_to_utf16_column, lsp_tool, utf16_column_to_character};
pub use mcp::{
    CallResult as McpCallResult, Catalogue as McpCatalogue, McpError, Registry as McpRegistry,
    Status as McpStatus, Tool as McpTool, Transport as McpTransport,
};
pub use mcp::{Resource as McpResource, ResourceContent as McpResourceContent};
pub use mcp_tools::{MAX_BRIDGED_TOOLS, register_mcp_tools};
pub use message::{
    CONTEXT_SUMMARY_MAX_CHARS, ContextForm, ContextSnapshotSection, Message as ChatMessage,
    MessageError, MessageSource, Role as ChatRole, bound_context_summary, check_history,
};
pub use model_catalog_extra::{
    Capabilities as ModelCapabilities, ModelEntry, filter_by as filter_models,
    to_catalog_json as models_to_catalog_json,
};
pub use observation::{
    Conflict as ObservationConflict, EditError, Observation, Session as ObservationSession,
    Version as FileVersion, replace_exactly_once, replace_lines, version_of,
};
pub use office_document::{
    Format as DocumentFormat, detect as detect_document, explain as explain_document,
    from_extension as document_from_extension, is_document, is_readable as document_is_readable,
};
pub use openapi::{Info as OpenApiInfo, document as openapi_document};
pub use output_budget::{
    Allocation as OutputAllocation, Budget as OutputBudget, allocate as allocate_output,
};
pub use patch::{
    Applied as PatchApplied, Failure as PatchFailure, Hunk as PatchHunk,
    Operation as PatchOperation, ParseError as PatchParseError, Reason as PatchFailureReason,
    apply as apply_patch, parse as parse_patch,
};
pub use paths::{
    AGENTS_HOME_DISPLAY, AGENTS_HOME_ENV, HARNESS_HOME_DISPLAY, HARNESS_HOME_ENV, Root,
    RootEnvironment, RootError, abbreviate_home, agents_home, expand_home, harness_home, home_dir,
    join_within,
};
pub use permissions::{
    Approval, Catalogue as PermissionCatalogue, Entry as PermissionEntry, Knobs, PermissionError,
    Preset, State as PermissionState,
};
pub use plugin_lifecycle::{
    Cause as LifecycleCause, History as LifecycleHistory, State as LifecycleState,
    Transition as LifecycleTransition, TransitionError as LifecycleError,
    allows as transition_allowed,
};
pub use present_tool::present_tool;
pub use process_group::{GroupError, GroupGuard, run_isolated};
pub use projection::{
    ModelSelectionProjection, ProjectedSession, Projection, ProjectionError, ProjectionHandle,
    ProjectionRegistry, Seq as ProjectionSeq, TitleProjection, TurnBoundary,
};
pub use projection_extra::{
    CacheError, Checkpoint, CheckpointRow, ProjectionCache, Snapshot as ProjectionSnapshot,
    Versioned, VersionedProjection,
};
pub use projection_host::{HostError, ProjectionHost, Registry as ProjectionHosts};
pub use providers::{
    Modality, ModelInfo, MountedProvider, ProviderRoute, Providers, Route, RouteError,
    RouteRegistry, deepseek_route, fits as model_fits, window_of,
};
pub use ptc_policy::{
    Decision as PtcDecision, ExecRequest, Limits as PtcLimits, Policy as PtcPolicy,
};
pub use questions::{
    Answer, AnsweredBy, DEFAULT_TIMEOUT as DEFAULT_QUESTION_TIMEOUT, Option_ as QuestionOption,
    Question, QuestionError, Reply as QuestionReply, Responder, Scripted as ScriptedResponder,
    Shape as QuestionShape, Unavailable as UnavailableResponder,
};
pub use registry::{Label, Registry, RegistryContext, RegistryError};
pub use remote::{
    DirectoryCatalog, RemoteCall, RemoteError, RemoteMethod, RemoteRegistry, SessionAddress,
    SessionCatalog, SessionCreateRequest, SessionCreateValue, SessionEventsPage,
    SessionHistoryRecord, SessionListValue, SessionPageRequest, SessionPromptRequest,
    SessionPromptValue, SessionRecord, SessionSummary, declared_gaps, default_registry,
    register_attachment_method, register_command_methods, register_extension_methods,
    register_job_methods, register_model_catalog, register_projection_methods,
    register_select_model, register_session_methods, register_workspace_methods,
    registered_method_names, registry_with_routes, registry_with_runtime,
};
pub use retention::{
    ItemRetainer, ItemStrategy, Omitted, RetainedItems, RetainedText, TextRetainer, TextStrategy,
};
pub use retry::{Backoff, FailureCode, RetryError, RetryMode, RetryPolicy, run_with_retry};
pub use retrying_provider::RetryingProvider;
pub use rpc_limits::{Limiter, Permit, Quota, Refusal as QuotaRefusal};
pub use runtime::{AgentEvent, AgentRuntime, AgentTurn, RuntimeConfig, RuntimeError};
pub use schedule::{
    CreateError, CronExpression, Firing, Instant as ScheduleInstant, MAX_TITLE_CHARS, MIN_INTERVAL,
    Offset, Rule, Schedule, ScheduleError, ScheduleId, Scheduler, Tick, TimeOfDay,
    create as create_schedule, next_fire, now_millis, pause as pause_schedule,
    resume as resume_schedule, run_scheduler,
};
pub use schedule_rpc::{
    MemoryScheduleStore, ScheduleStore, UnavailableScheduleStore, register_schedule_methods,
};
pub use schedule_runner::{
    Deliverer, Delivery, IDLE_WAIT, MAX_WAIT, Pass, Runner, pass as run_schedule_pass,
    wait_for as schedule_wait_for,
};
pub use scope::{BoxDisposer, BoxFut, Scope};
pub use search_index::{Entry as SearchIndexEntry, Index as SearchIndex};
pub use search_tools::{DEFAULT_LIMIT as SEARCH_DEFAULT_LIMIT, glob_tool, grep_tool};
pub use service::ServiceKey;
pub use session::{
    MemorySession, Session, SessionError, SessionEvent, SessionKey, SessionStore, TurnEndReason,
    read_session_file,
};
pub use session_archive::{
    ArchiveOutcome, ArchivePolicy, LogStats, analyze as analyze_log, archive as archive_session,
};
pub use session_export_rpc::register_session_export_method;
pub use session_filter::{
    Filter as SessionFilter, Order as SessionOrder, SessionSummary as FilterSummary,
    apply as apply_session_filter,
};
pub use session_layout::{
    Layout as SessionLayout, LayoutError as SessionLayoutError, SessionFiles,
};
pub use session_lifecycle_rpc::{
    ARCHIVE_METHOD, ArchiveError, ArchiveStore, COPY_SUFFIX,
    DECLARED_ACCESS as SESSION_LIFECYCLE_ACCESS, DUPLICATE_METHOD, MAX_COPY_TITLE_CHARS,
    MemoryArchiveStore, NAMESPACE as SESSION_LIFECYCLE_NAMESPACE, UnavailableArchiveStore,
    register_session_lifecycle_methods, register_session_lifecycle_methods_with_archive,
    retain_visible,
};
pub use session_metrics::{
    Sample, to_aggregate_samples, to_prometheus, to_samples as session_metric_samples,
};
pub use session_migration::{
    FormatVersion, Migration, MigrationError, Migrator, detect as detect_format,
    upgrade as upgrade_event, upgrade_in_session,
};
pub use session_portable::{
    Bundle as SessionBundle, BundleError, VERSION as BUNDLE_VERSION, export as export_bundle,
    import as import_bundle,
};
pub use session_query::{
    Candidate, Catalogue as SearchCatalogue, FileCatalogue, Hit, Kind as SearchKind,
    Query as SearchQuery, Results as SearchResults, excerpt as session_excerpt,
    matches as search_matches, query as search_sessions, run as run_query, search_files,
    searchable_text,
};
pub use session_query_rpc::{
    DEFAULT_LIMIT as SESSION_SEARCH_DEFAULT_LIMIT, MAX_TEXT_CHARS as SESSION_SEARCH_MAX_TEXT_CHARS,
    register_session_query_methods,
};
pub use session_queue_rpc::{
    AgentQueueMap, DECLARED_ACCESS as SESSION_QUEUE_ACCESS, QueueAgentSource, QueueError,
    register_session_queue_methods,
};
pub use session_resume_rpc::register_session_resume_method;
pub use session_rewind::{Checkpoint as RewindPoint, checkpoint_for_turn, checkpoints, rewind_to};
pub use session_search_tool::{DEFAULT_HITS as SESSION_SEARCH_DEFAULT_HITS, session_search_tool};
pub use session_state_rpc::{
    LiveTurns, NoLiveTurns, register_session_state_method, register_session_state_method_with,
};
pub use session_stats::Stats as SessionStats;
pub use session_timeline::{Segment as TimelineSegment, open_turn, outline as session_outline};
pub use session_title::{Source as TitleSource, derive as derive_title};
pub use settings_rpc::{
    MemorySettingsStore, SettingsStore, UnavailableSettingsStore, register_settings_methods,
};
pub use shell::{
    DEFAULT_MAX_BYTES, DEFAULT_TIMEOUT, ENV_PREFIX, Environment as ShellEnvironment, Program,
    Request as ShellRequest, RunResult as ShellRunResult, SandboxMode,
    Specification as ShellSpecification, split_words, which,
};
pub use skill::{
    Definition as SkillDefinition, Provider as SkillProvider, Registry as SkillRegistry,
    SkillError, Static as StaticSkills, Summary as SkillSummary,
};
pub use skill_catalog::{
    SkillMeta, discover as discover_skills, parse_frontmatter, render_list as render_skill_list,
};
pub use skill_tool::skill_tool;
pub use spill::{
    LocalStore as LocalSpillStore, Reference as SpillReference, SaveRequest as SpillSaveRequest,
    SpillError, Store as SpillStore, Unavailable as UnavailableSpillStore, should_spill,
};
pub use spill_policy::{
    Action as SpillAction, Context as SpillContext, Reason as SpillReason, decide as decide_spill,
    omitted as spill_omitted,
};
pub use ssh::{
    Client as SshClient, Command as SshCommand, DEFAULT_PORT as SSH_DEFAULT_PORT,
    DEFAULT_TIMEOUT as SSH_DEFAULT_TIMEOUT, Host as SshHost, Outcome as SshOutcome, SshError,
    Transport as SshTransport, Unavailable as UnavailableSsh,
};
pub use storage::{
    FileBackend, KvBackend, Layout, Records, Snapshot, StorageError, Unit, UnitDescriptor,
    is_valid_record_key, is_valid_unit_name,
};
pub use str_replace_editor::{
    EditFailure as ReplaceFailure, Replacement, apply as apply_replacements,
};
pub use stream_blocks::{StreamObserver, assemble_message};
pub use stream_method::{StreamError, StreamHandle, StreamMethod, Streams};
pub use subagent::{
    DelegationResult, Depth, DepthLimitedTools, ScriptedDelegator, SubagentError, delegate,
    subagent_tool,
};
pub use subagent_depth::{
    Depth as SubagentDepth, DepthError as SubagentDepthError, Limit as SubagentDepthLimit,
};
pub use subagent_interrupt::{
    Armed as InterruptArmed, Cancellers, InterruptOutcome, Interrupter, default_interrupter,
    register_subagent_interrupt, subagent_interrupt_tool,
};
pub use subagent_pool::{Pool as SubagentPool, Slot as SubagentSlot};
pub use subagent_providers::{
    Capabilities as SubagentCapabilities, ChildHandle, ProviderHandle, ProviderRegistry,
    StartError, StartRequest, SubagentProvider,
};
pub use subagent_tools::{
    DEFAULT_CONCURRENCY, StartRefusal, Status as SubagentStatus, SubagentEntry, SubagentRegistry,
    WaitError, register_subagent_tools, subagent_list_tool, subagent_send_tool, subagent_wait_tool,
    traced_subagent_tool, wait_timeout_ms,
};
pub use subprocess::{
    ProcessError, ProcessOutcome, SpawnOptions, has_program as has_program_in_path,
    run as run_process, spawn as spawn_process,
};
pub use system_prompt::{Parts as PromptParts, assemble as assemble_prompt};
pub use terminal::{
    DEFAULT_MAX_BYTES as DEFAULT_TRANSCRIPT_BYTES, Launch as TerminalLaunch,
    Registry as TerminalRegistry, Session as TerminalSession, Shell, Size as TerminalSize,
    Status as TerminalStatus, TerminalError, Transcript,
};
pub use terminal_local::{LocalLauncher, ProcessShell};
pub use terminal_rpc::{
    DEFAULT_READ_WAIT_MS as TERMINAL_DEFAULT_READ_WAIT_MS, MAX_WAIT_MS as TERMINAL_MAX_WAIT_MS,
    register_terminal_methods as register_terminal_rpc_methods,
};
pub use terminal_rpc_extra::{TerminalShell, discover_shells, register_terminal_extra_methods};
pub use terminal_tool::{
    TerminalLauncher, UnavailableLauncher, register_terminal_tools, terminal_read_tool,
    terminal_send_tool, terminal_start_tool,
};
pub use time::{
    Instant as TimeInstant, MAX_FOUR_DIGIT_YEAR, MIN_FOUR_DIGIT_YEAR, MS_PER_DAY, MS_PER_HOUR,
    MS_PER_MINUTE, MS_PER_SECOND, Span, SpanParts, TimeError, WallClock, civil_from_days,
    days_from_civil, days_in_month, format_instant, format_span, is_four_digit_year, is_leap_year,
    parse_instant,
};
pub use todo_tool::{Status as TodoStatus, Todo, read_todos, todo_tool};
pub use token_meter::{
    Price, cost as token_cost, estimate_blocks, estimate_message, estimate_text, price_for,
};
pub use tool_batching::{Strategy as BatchStrategy, ToolCallInfo, choose as choose_batch_strategy};
pub use tool_contracts::{FailureKind, Violation, validate_arguments as validate_tool_arguments};
pub use tool_result_pruning::{Presentation, apply as apply_pruning, decide as decide_pruning};
pub use tool_retry::{
    Failure as ToolFailure, FailureReason as ToolFailureReason, Policy as ToolRetryPolicy,
    corrective_message,
};
pub use tool_schema_export::{same_tool_set, to_mcp, to_openai};
pub use tool_usage::{ToolStats, usage as tool_usage};
pub use tools::{
    PostExecuteArgs, PostToolDecision, PreToolDecision, ToolBody, ToolDefinition, ToolError,
    ToolExecute, ToolExecution, ToolPostExecute, ToolPreExecute, ToolRegistry, ToolRegistryView,
    ToolResult, ToolRunContext, ToolValue, ToolsKey,
};
pub use typert::{
    Catalogue, Field, Method, ObjectSchema, Parameter, Schema, Variant, to_typescript,
};
pub use typert_json::{catalogue_to_json_schema, method_to_json_schema, to_json_schema};
pub use web::{
    EndpointProvider, FetchBody, FetchProvider, FetchRequest, FetchResult, SearchProvider,
    SearchRequest, SearchResult, SearchSource, Web, WebError, build_web_from_env,
};
pub use web_tools::{DEFAULT_SOURCES as WEB_DEFAULT_SOURCES, fetch_tool, search_tool};
pub use workflow::{
    AgentRecord as WorkflowAgentRecord, Engine as WorkflowEngine, Event as WorkflowEvent,
    Phase as WorkflowPhase, Registry as WorkflowRegistry, Run as WorkflowRun, RunId,
    StartRequest as WorkflowStartRequest, Status as WorkflowStatus, WorkflowError,
};
pub use workflow_tool::{register_workflow_tools, workflow_run_tool, workflow_status_tool};
pub use workspace_desktop_rpc::{
    DesktopEnvironment, FileApplication, FileApplicationProber, FileManager, HostPlatform,
    register_workspace_desktop_methods, register_workspace_desktop_methods_with,
};
pub use workspace_diff::{
    DEFAULT_CONTEXT, FileChange as FileDiffChange, LineDiff, TextDiff, diff_text,
};
pub use workspace_files_extra::{
    DEFAULT_READ_BYTES as WORKSPACE_READ_BYTES, DEFAULT_SEARCH_HITS as WORKSPACE_SEARCH_HITS,
    MAX_READ_BYTES as WORKSPACE_MAX_READ_BYTES, register_workspace_file_extras,
};
pub use workspace_files_rpc::register_workspace_file_methods;
pub use workspace_open_rpc::register_open_workspace_path;
pub use workspace_path::{
    contains as workspace_contains, display as workspace_display, has_drive_prefix, is_absolute,
    is_unc, normalize as normalize_path, relative_to, resolve as resolve_in_workspace,
};
pub use workspace_rpc_extra::{
    InMemoryPreferences, UnavailablePreferences, WorkspacePreferences,
    register_workspace_extra_methods, register_workspace_extra_methods_without_preferences,
};

pub use persistent_shell::{
    Config as PersistentShellConfig, Output as PersistentShellOutput, PersistentShell,
    ShellError as PersistentShellError,
};
