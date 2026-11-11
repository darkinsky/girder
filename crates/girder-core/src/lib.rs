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

pub mod api_config;
pub mod api_job;
pub mod api_session;
mod approval;
mod atomic_write;
mod attachment;
mod attachment_rpc;
mod boot;
mod brand;
mod chunked_list;
mod code_language;
mod commands;
mod compaction;
mod compaction_hook;
mod config;
mod credentials;
mod deliverable;
mod events;
mod exec;
mod extension_runtime;
mod extensions;
mod fiber;
mod file_tools;
mod fs;
mod fs_rpc;
mod goal;
mod goal_tool;
mod home_paths;
mod host_rpc;
mod inbox;
mod instructions;
mod instructions_tool;
mod job_tools;
mod jobs;
mod json;
mod launch_environment;
mod limits;
mod llm;
mod llm_deepseek;
mod lsp;
mod lsp_tool;
mod mcp;
mod message;
mod observation;
mod patch;
mod paths;
mod permissions;
mod present_tool;
mod projection;
mod providers;
mod questions;
mod registry;
mod remote;
mod retention;
mod retry;
mod runtime;
mod schedule;
mod scope;
mod search_tools;
mod service;
mod session;
mod session_query;
mod session_query_rpc;
mod session_queue_rpc;
mod session_search_tool;
mod shell;
mod skill;
mod skill_tool;
mod spill;
mod spill_policy;
mod ssh;
mod storage;
mod subagent;
mod subagent_providers;
mod subagent_tools;
mod subprocess;
mod terminal;
mod terminal_rpc;
mod terminal_tool;
mod time;
mod todo_tool;
mod tools;
mod typert;
mod web;
mod web_tools;
mod workflow;
mod workflow_tool;
mod workspace_files_rpc;
mod workspace_path;

pub use access::{
    Access, AllowList, Decision as AccessDecision, Declaration, DenyAll, Policy, ReadOnly,
    Request as AccessRequest, Resource as AccessResource,
};
pub use agent::{
    Agent, AgentHandle, AgentOptions, AgentSpec, ModelSelection, ModelSelectionState,
    ModelSelectionView, SelectionError, SelectionSource, validate as validate_model_selection,
};
pub use agent_loop::{LoopContext, LoopError, LoopStep, dispatch, run};
/// The session Remote surface, namespaced because its wire types share names with the
/// service-side request types in `remote`.
pub use api_session as session_api;
pub use approval::{
    ApprovalError, Approver, DEFAULT_TIMEOUT as DEFAULT_APPROVAL_TIMEOUT, DecidedBy, Decision,
    Record as ApprovalRecord, Request as ApprovalRequest, Scripted as ScriptedApprover,
    Unavailable as UnavailableApprover,
};
pub use atomic_write::{
    AtomicError, DEFAULT_LOCK_WAIT, Mode, with_lock, write_atomic, write_text_atomic,
};
pub use attachment::{
    ACCEPTED_IMAGE_TYPES, AttachmentError, AttachmentStore, DecodedImage, EncodedImage, ImageRef,
    Limits as AttachmentLimits, MemoryStore, admit_batch, admit_image, decode_base64_canonical,
    detect_media_type, encode_base64, storage_key as attachment_storage_key, store_batch,
};
pub use attachment_rpc::register_attachment_upload_method;
pub use boot::{
    BootConfig, BootError, BootReport, Booted, DEFAULT_ENDPOINT, DEFAULT_MODEL,
    Outcome as BootOutcome, boot,
};
pub use brand::{
    AgentId, AgentIdBrand, Brand, Id, IdError, MAX_ID_LEN, MessageId, MessageIdBrand, SessionId,
    SessionIdBrand, ToolCallId, ToolCallIdBrand, WorkspaceId, WorkspaceIdBrand,
};
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
pub use extension_runtime::{
    NoopRuntime as ExtensionNoopRuntime, ShellRuntime as ExtensionShellRuntime, assert_runnable,
};
pub use extensions::{
    Definition as ExtensionDefinition, ExtensionError, ExtensionId, Registry as ExtensionRegistry,
    Run as ExtensionRun, Runtime as ExtensionRuntime, Status as ExtensionStatus,
};
pub use fiber::{Disposer, Fiber, FiberError, FiberState, PluginBody, PluginSpec};
pub use file_tools::{MAX_READ_BYTES, ask_tool, edit_tool, patch_tool, read_tool, write_tool};
pub use fs::{
    FileSystem, FsDirEntry, FsError, FsInfo, FsTarget, FsTargetKey, FsType, FsVersion,
    LocalFileSystem, read_file as read_file_via_fs, target_at,
};
pub use fs_rpc::{DEFAULT_LIST_LIMIT as FS_LIST_LIMIT, register_fs_methods};
pub use girder_macros::{Typert, remote};
pub use goal::{
    Activation, BlockReason, DEFAULT_MAX_GOAL_ROUNDS, Goal, GoalError, GoalId, GoalRef, GoalState,
    Phase, check_reference as check_goal_reference,
};
pub use goal_tool::goal_tool;
pub use home_paths::Paths as HomePaths;
pub use host_rpc::{SystemOpener, UnavailableOpener, register_host_methods};
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
pub use message::{
    CONTEXT_SUMMARY_MAX_CHARS, ContextForm, ContextSnapshotSection, Message as ChatMessage,
    MessageError, MessageSource, Role as ChatRole, bound_context_summary, check_history,
};
pub use observation::{
    Conflict as ObservationConflict, EditError, Observation, Session as ObservationSession,
    Version as FileVersion, replace_exactly_once, replace_lines,
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
pub use present_tool::present_tool;
pub use projection::{
    ModelSelectionProjection, ProjectedSession, Projection, ProjectionError, ProjectionHandle,
    ProjectionRegistry, Seq as ProjectionSeq, TitleProjection, TurnBoundary,
};
pub use providers::{
    Modality, ModelInfo, MountedProvider, ProviderRoute, Providers, Route, RouteError,
    RouteRegistry, deepseek_route, fits as model_fits, window_of,
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
pub use runtime::{AgentEvent, AgentRuntime, AgentTurn, RuntimeConfig, RuntimeError};
pub use schedule::{
    CreateError, CronExpression, Firing, Instant as ScheduleInstant, MAX_TITLE_CHARS, MIN_INTERVAL,
    Offset, Rule, Schedule, ScheduleError, ScheduleId, Scheduler, Tick, TimeOfDay,
    create as create_schedule, next_fire, now_millis, pause as pause_schedule,
    resume as resume_schedule, run_scheduler,
};
pub use scope::{BoxDisposer, BoxFut, Scope};
pub use search_tools::{DEFAULT_LIMIT as SEARCH_DEFAULT_LIMIT, glob_tool, grep_tool};
pub use service::ServiceKey;
pub use session::{
    MemorySession, Session, SessionError, SessionEvent, SessionKey, SessionStore, TurnEndReason,
    read_session_file,
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
pub use session_search_tool::{DEFAULT_HITS as SESSION_SEARCH_DEFAULT_HITS, session_search_tool};
pub use shell::{
    DEFAULT_MAX_BYTES, DEFAULT_TIMEOUT, ENV_PREFIX, Environment as ShellEnvironment, Program,
    Request as ShellRequest, RunResult as ShellRunResult, SandboxMode,
    Specification as ShellSpecification, split_words, which,
};
pub use skill::{
    Definition as SkillDefinition, Provider as SkillProvider, Registry as SkillRegistry,
    SkillError, Static as StaticSkills, Summary as SkillSummary,
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
pub use subagent::{
    DelegationResult, Depth, DepthLimitedTools, ScriptedDelegator, SubagentError, delegate,
    subagent_tool,
};
pub use subagent_providers::{
    Capabilities as SubagentCapabilities, ChildHandle, ProviderHandle, ProviderRegistry,
    StartError, StartRequest, SubagentProvider,
};
pub use subagent_tools::{
    Status as SubagentStatus, SubagentEntry, SubagentRegistry, WaitError, register_subagent_tools,
    subagent_list_tool, subagent_send_tool, subagent_wait_tool, traced_subagent_tool,
    wait_timeout_ms,
};
pub use subprocess::{
    ProcessError, ProcessOutcome, SpawnOptions, has_program as has_program_in_path,
    run as run_process, spawn as spawn_process,
};
pub use terminal::{
    DEFAULT_MAX_BYTES as DEFAULT_TRANSCRIPT_BYTES, Launch as TerminalLaunch,
    Registry as TerminalRegistry, Session as TerminalSession, Shell, Size as TerminalSize,
    Status as TerminalStatus, TerminalError, Transcript,
};
pub use terminal_rpc::{
    DEFAULT_READ_WAIT_MS as TERMINAL_DEFAULT_READ_WAIT_MS, MAX_WAIT_MS as TERMINAL_MAX_WAIT_MS,
    register_terminal_methods as register_terminal_rpc_methods,
};
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
pub use tools::{
    PostExecuteArgs, PostToolDecision, PreToolDecision, ToolBody, ToolDefinition, ToolError,
    ToolExecute, ToolExecution, ToolPostExecute, ToolPreExecute, ToolRegistry, ToolRegistryView,
    ToolResult, ToolRunContext, ToolValue, ToolsKey,
};
pub use typert::{
    Catalogue, Field, Method, ObjectSchema, Parameter, Schema, Variant, to_typescript,
};
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
pub use workspace_files_rpc::register_workspace_file_methods;
pub use workspace_path::{
    contains as workspace_contains, display as workspace_display, has_drive_prefix, is_absolute,
    is_unc, normalize as normalize_path, relative_to, resolve as resolve_in_workspace,
};
