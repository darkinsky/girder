//! Core primitives for the Girder harness.
//!
//! Girder is a Rust rewrite of `DeepSeek Harness`. Upstream is built on Cordis, where "everything
//! is a plugin": each capability is contributed by a plugin, and plugins reach each other by
//! resolving *services* from a shared context rather than by importing implementations.
//!
//! [`ServiceKey`] and [`Registry`] replace Cordis' dynamic context lookup with a typed key over a
//! type-erased table. [`Scope`] records every contribution and disposes them in reverse
//! registration order. The rest of this crate is the harness built on those primitives: the agent
//! loop, sessions, tools, providers, and the Remote methods a client calls.
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

// The derive emits `::girder_core::…`, which cannot resolve inside `girder-core` itself. This alias
// lets generated code compile here as well as downstream; it is the supported way to name a crate
// from within itself.
extern crate self as girder_core;

pub mod api;
pub mod attachment;
pub mod boot;
pub mod compaction;
pub mod context;
pub mod core;
pub mod credentials;
pub mod deliverables;
pub mod document;
pub mod extensions;
pub mod feedback;
pub mod fs;
pub mod goal;
pub mod guard;
pub mod hooks;
pub mod host;
pub mod interaction;
pub mod jobs;
pub mod llm;
pub mod lsp;
pub mod mcp;
pub mod preset;
pub mod ptc;
pub mod sandbox;
pub mod schedule;
pub mod session;
pub mod settings;
pub mod shell;
pub mod skill;
pub mod spill;
pub mod ssh;
pub mod storage;
pub mod subagent;
pub mod team;
pub mod telemetry;
pub mod terminal;
pub mod todo;
pub mod typert;
pub mod util;
pub mod web;
pub mod workflow;
pub mod workspace;

pub use boot::install::{
    InstallFailureKind, ParsedSpec, SpecKind, is_registry_package_name, parse_install_spec,
    pinned_git_origin,
};

pub use credentials::account::{
    CredentialSource, MemoryCredentialSource, register_credential_methods,
};
pub use host::webserver::{
    BindAddress, bind_url_host, is_loopback_host, is_wildcard_host, listen_endpoint,
    normalize_bind_address, parse_bind_address,
};
pub use schedule::reminder::{Admission as ScheduleAdmission, render as render_schedule_reminder};
pub use team::rpc_extra::{
    MAX_TASKS as TEAM_MAX_TASKS, METHOD as TEAM_GRAPH_METHOD, graph_ids,
    register_team_extra_methods as register_team_graph_method,
};

pub use api::catalog::{Catalog, CatalogError, EventEntry, MethodEntry, ServiceEntry};
pub use api::config::WorkspaceRegistry;
pub use api::inbox::{Inbox, InboxError, Splice, Target as InboxTarget, replay as replay_inbox};
pub use api::key::{KeyCheck, check as check_api_key, normalize as normalize_api_key};
pub use api::method_surface::{
    Discrepancy as SurfaceDiscrepancy, Surface as MethodSurface, declared as declared_surface,
    reconcile as reconcile_surface, registration_of as surface_registration_of,
};
pub use api::openapi::{Info as OpenApiInfo, document as openapi_document};
pub use api::openapi_full::{
    Operation as FullOpenApiOperation, document as full_openapi_document,
    document_from_names as openapi_document_from_names, operations as openapi_operations,
};
pub use api::remote::{
    BinaryRemoteMethod, BinaryRemoteResult, DirectoryCatalog, RemoteAttachment,
    RemoteAttachmentSegment, RemoteCall, RemoteError, RemoteMethod, RemoteRegistry,
    RemoteStreamMethod, SessionAddress, SessionCatalog, SessionCreateRequest, SessionCreateValue,
    SessionEventsPage, SessionHistoryRecord, SessionListValue, SessionPageRequest,
    SessionPromptRequest, SessionPromptValue, SessionRecord, SessionSummary, StreamOpen,
    StreamYield, bind_session_places, declared_gaps, default_registry, register_attachment_method,
    register_command_methods, register_extension_methods, register_job_methods,
    register_model_catalog, register_projection_methods, register_select_model,
    register_session_methods, register_workspace_methods, registered_method_names,
    registry_with_routes, registry_with_runtime,
};
pub use api::remote_access::{
    CallRequest as AccessCallRequest, MethodAccessResolver, NO_ALLOW, RESOURCE_INVALID,
    Refused as AccessRefused, Resolved, ResolvedResource, ResourceIds, all_allowed, declaration,
    kinds_of, reader, resolve_event_resources,
};
pub use api::remote_follow::{
    DirectoryPolicy, RemoteAccessSnapshot, register_remote_access_follow,
    register_remote_access_follow_with, snapshot_for_directory,
};
pub use api::remote_service::{
    Endpoint, Handler as RemoteHandler, RemoteService, endpoint_names, register_remote_service,
};
pub use api::remotes::{
    ABANDONED_INVOCATION, AGENT_FIELD, AllowlistError, Delivery as ForwardDelivery,
    Dispatch as QueueEvent, Dispatch, EventQueue, FORWARDED_EVENTS, ForwardError, ForwardedEvent,
    Frame, GLOBAL_RESOURCE_KIND, Outcome, ProjectedRequest, Push, Resource, SESSION_RESOURCE_KIND,
    SIGNAL_FIELD, SourceEnd, UNEXPECTED_SOURCE_END, WaterfallSlot, WaterfallTicket,
    forward_waterfall, forwarded_event, forwarded_event_names, is_forwardable, project_request,
    resources_of, source_end_reason, validate_allowlist, validate_args,
};
pub use api::result_paging::{
    Cursor as PageCursor, CursorError, Page as ResultPage, Request as PageRequest,
    page as page_result, page_from as page_result_from,
};
pub use api::rpc_limits::{Limiter, Permit, Quota, Refusal as QuotaRefusal};
/// The session Remote surface, namespaced because its wire types share names with the
/// service-side request types in `remote`.
pub use api::session as session_api;
pub use api::share_link::{Share, ShareError, Shares};
pub use attachment::limits::{
    Policy as AttachmentPolicy, RejectReason, Verdict, check as check_attachment, is_image,
};
pub use attachment::rpc::register_attachment_upload_method;
pub use attachment::{
    ACCEPTED_IMAGE_TYPES, AttachmentError, AttachmentStore,
    DEFAULT_CAPACITY as ATTACHMENT_CAPACITY, DecodedImage, EncodedImage,
    FileStore as AttachmentFileStore, ImageRef, ImageShelf, Limits as AttachmentLimits,
    MemoryStore, admit_batch, admit_image, decode_base64_canonical, detect_media_type,
    encode_base64, storage_key as attachment_storage_key, store_batch,
};
pub use boot::check::{
    Deployment, Finding, Severity, check as check_deployment, is_fatal as boot_is_fatal,
    may_be_a_directory, report as boot_report, unmounted_gaps,
};
pub use boot::plugin_lifecycle::{
    Cause as LifecycleCause, History as LifecycleHistory, State as LifecycleState,
    Transition as LifecycleTransition, TransitionError as LifecycleError,
    allows as transition_allowed,
};
pub use boot::stages::{BootTrace, Stage as BootStage, StageOutcome as BootStageOutcome};
pub use boot::{
    BootConfig, BootError, BootReport, Booted, DEFAULT_ENDPOINT, DEFAULT_MODEL,
    Outcome as BootOutcome, boot,
};
pub use compaction::hook::{CompactionPolicy, Plan as CompactionPlan, history_cost, message_cost};
pub use compaction::pipeline::{Outcome as PipelineOutcome, run as run_compaction_pipeline};
pub use compaction::summary::{
    Segment as CompactionSegment, render as render_compaction,
    summarize as summarize_for_compaction,
};
pub use compaction::{
    CHECKPOINT_PREAMBLE, COMPACTION_INSTRUCTION, CompactionError, CompactionOutcome,
    CompactionRequest, CompactionTrigger, PairingBalance, SUMMARY_CLOSE_TAG, SUMMARY_OPEN_TAG,
    Selection, SummaryError, SurfaceEntry, balances, checkpoint_message, extract_summary,
    is_checkpoint, needs_compaction, pairing_balanced_at, select as select_compaction,
    summary_request, transcript_for_summary,
};
pub use context::budget::{
    Budget as ContextBudget, Decision as BudgetDecision, Usage as ContextUsage,
    check as check_context_budget,
};
pub use context::context::{
    ErrorCode as ReferenceErrorCode, Limits as ReferenceLimits, ParsedText, ReferenceError,
    decode_uri, encode_uri, format_mention, normalize_references, parse_text,
    project as project_references, rank_candidates, reference_budget, render_omissions,
    render_reference_prompt, retain as retain_references, stringify_tag_safe_json,
    truncate_with_notice, validate_limits,
};
pub use context::instructions::{
    DEFAULT_MAX_BYTES_PER_FILE, DEFAULT_MAX_TOTAL_BYTES, HARNESS_GLOBAL_DIRECTORY,
    INSTRUCTION_FILE_NAME, InstructionFile, InstructionLimits, InstructionState, LoadedInstruction,
    RenderedInstructions, TruncatedInstruction, USER_GLOBAL_DIRECTORY, digest,
    discover as discover_instructions, load as load_instructions, render as render_instructions,
};
pub use context::instructions_tool::{
    MAX_OUTPUT_BYTES as INSTRUCTIONS_MAX_OUTPUT_BYTES, TOOL_NAME as INSTRUCTIONS_TOOL_NAME,
    instructions_tool,
};
pub use core::agent::{
    Agent, AgentHandle, AgentOptions, AgentSpec, ModelSelection, ModelSelectionState,
    ModelSelectionView, SelectionError, SelectionSource, validate as validate_model_selection,
};
pub use core::agent_loop::{LiveParents, LoopContext, LoopError, LoopStep, dispatch, run};
pub use core::agent_scope::{
    Binding as ScopeBinding, ConsumedWork, EndedTurn, Event as ConsumedWorkEvent, ScopeError,
    ScopeGraph, ScopeKey as ScopeGraphKey, SpliceOutcome, accounts_for_claim, fold_consumed_work,
};
pub use core::argument_infer::{
    Completion as ArgumentCompletion, Provenance as ArgumentProvenance, complete_from_defaults,
    provenance as argument_provenance,
};
pub use core::argument_limits::{
    Kind as ArgumentLimitKind, Limits as ArgumentLimits, Violation as ArgumentViolation,
    check as check_arguments, defaults as default_argument_limits,
};
pub use core::ask::{
    ApprovalOutcome, TOOL_ABORTED, TOOL_ABORTED_BEFORE_DISPATCH, aborted_outcome,
    aborted_outcome_advising, to_tool_result,
};
pub use core::assistant_output::{FinalOutput, final_output, final_text};
pub use core::cache_invalidate::{
    Effect as CacheEffect, apply as apply_cache_effect, effect as cache_effect,
    normalized_path as normalize_cache_path,
};
pub use core::cancel_orchestra::{Cancelled as CancelOutcome, cancel_and_clean, is_quiescent};
pub use core::cancel_scope::{CancelReason, Scope as CancelScope, run_cleanups};
pub use core::event_index::{Index as EventIndex, TurnSpan};
pub use core::events::{
    Bail, Chain, DispatchError, Event, EventBus, Layer, ListenerId, ListenerScope, Next,
    ObservedEvent, Order, Parallel, ScopeKey, Serial, Terminal, Waterfall, child_scope,
};
pub use core::fiber::{Disposer, Fiber, FiberError, FiberState, PluginBody, PluginSpec};
pub use core::observation::{
    Conflict as ObservationConflict, EditError, Observation, Session as ObservationSession,
    Version as FileVersion, replace_exactly_once, replace_lines, version_of,
};
pub use core::output_budget::{
    Allocation as OutputAllocation, Budget as OutputBudget, allocate as allocate_output,
};
pub use core::output_digest::{
    Importance as DigestImportance, Line as DigestLine, classify as classify_output,
    digest as digest_output, has_errors as output_has_errors,
};
pub use core::output_predict::{Prediction as OutputPrediction, predict as predict_output};
pub use core::present_tool::present_tool;
pub use core::prompt_template::{TemplateError, Variables, render as render_template};
pub use core::registry::{Label, Registry, RegistryContext, RegistryError};
pub use core::runtime::{AgentEvent, AgentRuntime, AgentTurn, RuntimeConfig, RuntimeError};
pub use core::scope::{BoxDisposer, BoxFut, Scope};
pub use core::service::ServiceKey;
pub use core::state_consistency::{
    Inconsistency, Severity as InconsistencySeverity, Values as ConsistencyValues,
    check as check_state_consistency,
};
pub use core::surface::{
    EncodedSeq, OpenTurnCloseCause, SeqRangeError, SurfaceEnvelope, SurfaceError, SurfaceFold,
    SurfaceOp, SurfaceReplacement, SyntheticToolResult, TOOL_NOT_STARTED, TOOL_OUTCOME_UNKNOWN,
    ToolCallRecovery, decode_seq_ranges, derive_event_message, encode_seq_ranges, fold_surface,
    interrupted_turn_closers, is_append_surface_event, is_replacement_surface_event,
    is_surface_eligible_type, needs_message_projection, open_turn_closers, tool_result_content,
    validate_surface_metadata,
};
pub use core::system_prompt::{Parts as PromptParts, assemble as assemble_prompt};
pub use core::table_tools::{
    FileRow, MatchRow, file_table, match_table, render_table as render_aligned_table,
};
pub use core::tool_batching::{
    Strategy as BatchStrategy, ToolCallInfo, choose as choose_batch_strategy,
};
pub use core::tool_budget_config::{
    Budget as ToolBudget, DEFAULT_MAX_CALLS, MAX_CALLS_KEY, Warning as BudgetWarning,
    Watcher as ToolBudgetWatcher,
};
pub use core::tool_cache::{
    Cache as ToolCache, Entry as ToolCacheEntry, Key as ToolCacheKey,
    is_cacheable as is_tool_cacheable,
};
pub use core::tool_contracts::{
    FailureKind, Violation, validate_arguments as validate_tool_arguments,
};
pub use core::tool_extract::{
    Extracted as ExtractedOutput, extract as extract_output, glob_paths, grep_paths,
};
pub use core::tool_graph::{
    GraphError, Node as GraphNode, Plan as GraphPlan, happens_before, plan as plan_graph,
};
pub use core::tool_idempotency::{
    Decision as IdempotencyDecision, Executed, Idempotency, decide as decide_idempotent,
    executed as executed_calls, idempotency as tool_idempotency,
};
pub use core::tool_limits::{Limit as ToolLimit, Tracker as ToolTracker, Verdict as ToolVerdict};
pub use core::tool_output_view::{Outcome as OutputOutcome, View as ToolOutputView, block_cost};
pub use core::tool_result_pruning::{
    Presentation, apply as apply_pruning, decide as decide_pruning,
};
pub use core::tool_retry::{
    Failure as ToolFailure, FailureReason as ToolFailureReason, Policy as ToolRetryPolicy,
    corrective_message,
};
pub use core::tool_retry_plan::{
    Failure as RetryFailure, Kind as RetryKind, Plan as RetryPlan, plan as plan_tool_retry,
};
pub use core::tool_schedule::{
    Call as ScheduleCall, Plan as SchedulePlan, is_read_only, plan as plan_schedule,
};
pub use core::tool_schema_export::{same_tool_set, to_mcp, to_openai};
pub use core::tool_trace::{
    Phase as ToolPhase, Summary as ToolTraceSummary, Trace as ToolTrace,
    summarize as summarize_tool_traces,
};
pub use core::tool_usage::{ToolStats, usage as tool_usage};
pub use core::tool_value::{
    Table as RenderedTable, Value as StructuredValue, infer as infer_tool_value,
    render as render_tool_value,
};
pub use core::tool_watchdog::{Policy as WatchdogPolicy, Verdict as WatchdogVerdict, Watch};
pub use core::tools::{
    PostExecuteArgs, PostToolDecision, PreToolDecision, ToolBody, ToolDefinition, ToolError,
    ToolExecute, ToolExecution, ToolPostExecute, ToolPreExecute, ToolRegistry, ToolRegistryView,
    ToolResult, ToolRunContext, ToolValue, ToolsKey,
};
pub use core::turn_closure::{
    Closure as TurnClosure, evaluate as evaluate_turn_closure, is_interrupted as turns_interrupted,
    open_turn_of as open_turn_number,
};
pub use credentials::credential_ref::{
    CredentialRef as SecretRef, KNOWN_PREFIXES, RefError as SecretRefError, redact,
};
pub use credentials::permissions::{
    Approval, Catalogue as PermissionCatalogue, Entry as PermissionEntry, Knobs, PermissionError,
    Preset, State as PermissionState,
};
pub use credentials::{
    CredentialError, CredentialRef, Info as CredentialInfo, Record as CredentialRecord,
    Store as CredentialStore, valid_environment,
};
pub use deliverables::deliverable::{Deliverable, Kind as DeliverableKind, Set as DeliverableSet};
pub use document::office_document::{
    Format as DocumentFormat, detect as detect_document, explain as explain_document,
    from_extension as document_from_extension, is_document, is_readable as document_is_readable,
};
pub use extensions::extension_run_rpc::{
    register_extension_run_methods, register_extension_run_methods_with_services,
};
pub use extensions::extension_runtime::{
    NoopRuntime as ExtensionNoopRuntime, ShellRuntime as ExtensionShellRuntime, assert_runnable,
};
pub use extensions::{
    Definition as ExtensionDefinition, ExtensionError, ExtensionId, Registry as ExtensionRegistry,
    Run as ExtensionRun, Runtime as ExtensionRuntime, Status as ExtensionStatus,
};
pub use fs::diff_patch::{
    Hunk as DiffHunk, Patch as DiffPatch, PatchError as PatchErrorKind, apply as apply_diff_patch,
    parse as parse_diff_patch,
};
pub use fs::file_lock::{Contention, Lock as FileLock, Locks as FileLocks};
pub use fs::file_tools::{MAX_READ_BYTES, ask_tool, edit_tool, patch_tool, read_tool, write_tool};
pub use fs::file_watch::{
    Change as FileChange, ChangeKind as FileChangeKind, PollingWatcher, snapshot as watch_snapshot,
    stamp as watch_stamp,
};
pub use fs::patch::{
    Applied as PatchApplied, Failure as PatchFailure, Hunk as PatchHunk,
    Operation as PatchOperation, ParseError as PatchParseError, Reason as PatchFailureReason,
    apply as apply_patch, parse as parse_patch,
};
pub use fs::rpc::{DEFAULT_LIST_LIMIT as FS_LIST_LIMIT, register_fs_methods};
pub use fs::search_tools::{DEFAULT_LIMIT as SEARCH_DEFAULT_LIMIT, glob_tool, grep_tool};
pub use fs::search_view::{
    FileMatches, GLOB_MAX_RESULTS, GREP_MAX_LINE_BYTES, GREP_MAX_MATCHES, GlobSample, GrepMatch,
    LineMatch, SEARCH_META_MAX_BYTES, SearchMeta, format_glob_output, format_grep_matches,
    format_grep_output, format_retained_grep, glob_search_meta, grep_search_meta,
    group_matches_by_file, preview_line, render_glob_paths, retain_glob_paths, retain_grep_matches,
    sample_across_top_level, to_workdir_relative,
};
pub use fs::str_replace_editor::{
    EditFailure as ReplaceFailure, Replacement, apply as apply_replacements,
};
pub use fs::tools::{copy_tool, delete_tool, make_dir_tool, move_tool, register_fs_tools};
pub use fs::undo_log::{Change as UndoChange, Log as UndoLog};
pub use fs::{
    FileSystem, FsDirEntry, FsError, FsInfo, FsTarget, FsTargetKey, FsType, FsVersion,
    LocalFileSystem, read_file as read_file_via_fs, target_at,
};
pub use girder_macros::{Typert, remote};
pub use goal::rpc::{
    GoalStore, METHODS as GOAL_METHODS, MemoryGoalStore, STARTS_ROUNDS, UnavailableGoalStore,
    register_goal_methods,
};
pub use goal::tool::goal_tool;
pub use goal::{
    Activation, BlockReason, DEFAULT_MAX_GOAL_ROUNDS, Goal, GoalError, GoalId, GoalRef, GoalState,
    Phase, check_reference as check_goal_reference,
};
pub use guard::access::{
    Access, AllowList, Decision as AccessDecision, Declaration, DenyAll, Policy, ReadOnly,
    Request as AccessRequest, Resource as AccessResource,
};
pub use guard::injection_guard::{
    Pattern as InjectionPattern, Severity as InjectionSeverity, Signal as InjectionSignal,
    annotate as annotate_injection, default_patterns as injection_patterns, scan as scan_injection,
};
pub use guard::pattern_check::{
    Outcome as PatternOutcome, check as check_pattern, check_enum as check_pattern_enum,
    check_length as check_pattern_length,
};
pub use guard::text_safety::{
    Hazard as TextHazard, Kind as HazardKind, is_safe as is_text_safe, sanitize as sanitize_text,
    scan as scan_text,
};
pub use host::rpc::{
    Opener, SystemOpener, UnavailableOpener, register_host_methods, register_host_methods_with,
};
pub use interaction::action_cards_rpc::{
    ActionCards, register_action_card_methods, register_action_card_methods_with,
};
pub use interaction::approval::{
    ApprovalError, Approver, DEFAULT_TIMEOUT as DEFAULT_APPROVAL_TIMEOUT, DecidedBy, Decision,
    Record as ApprovalRecord, Request as ApprovalRequest, Scripted as ScriptedApprover,
    StdinApprover, Unavailable as UnavailableApprover,
};
pub use interaction::approval_rules::{
    Action as ApprovalAction, Decision as ApprovalDecision, Rule as ApprovalRule,
    Rules as ApprovalRules,
};
pub use interaction::builtin_commands::register_builtin_commands;
pub use interaction::commands::{
    Arguments as CommandArguments, CommandError, CommandId, CommandResult,
    Definition as CommandDefinition, DefinitionId as CommandDefinitionId,
    Handler as CommandHandler, Input as CommandInput, Registry as CommandRegistry,
};
pub use interaction::questions::{
    Answer, AnsweredBy, DEFAULT_TIMEOUT as DEFAULT_QUESTION_TIMEOUT, Option_ as QuestionOption,
    Question, QuestionError, Reply as QuestionReply, Responder, Scripted as ScriptedResponder,
    Shape as QuestionShape, Unavailable as UnavailableResponder,
};
pub use interaction::questions_rpc::{
    MemoryQuestionStore, QuestionQueue, QuestionStore,
    UNSERVED_METHODS as UNSERVED_QUESTION_METHODS, UnavailableQuestionStore,
    register_question_methods,
};
pub use jobs::job_follow_rpc::register_job_follow;
pub use jobs::job_tools::{
    job_kill_tool, job_list_tool, job_output_tool, job_start_tool, register_job_tools,
};
pub use jobs::{
    Channel as JobChannel, Job, JobChunk, JobError, JobId, JobProducer, JobRegistry,
    JobRegistryHandle, Outcome as JobOutcome, OutputRing, Status as JobStatus, spawn_job,
};
pub use llm::block_assembler::{Assembler as BlockAssembler, AssemblyError};
pub use llm::cost_estimate::{
    Batch as CostBatch, Estimate as CostEstimate, Price as ModelPrice, Table as PriceTable,
    Usage as CostUsage, estimate as estimate_cost, estimate_batch as estimate_cost_batch,
};
pub use llm::deepseek::{
    DeepSeekConfig, DeepSeekProvider, ImageUploadPlan, SseEvent, SseParser, Translator,
    UploadDecision, provider_from_env,
};
pub use llm::image_block::{
    ImageError, ImagePayload, MAX_BYTES as MAX_IMAGE_BYTES, SUPPORTED_TYPES as IMAGE_TYPES,
    decode as decode_image, encode as encode_image, sniff_type as sniff_image_type,
};
pub use llm::image_tokens::{
    Dimensions as ImageDimensions, ImageRequestPrice, MAX_IMAGE_TOKENS, MIN_PIXELS, PATCH_SIZE,
    image_tokens, price_request_image, request_dimensions,
};
pub use llm::model_catalog_extra::{
    Capabilities as ModelCapabilities, ModelEntry, filter_by as filter_models,
    to_catalog_json as models_to_catalog_json,
};
pub use llm::model_fallback::{
    Candidate as FallbackCandidate, Needs as ModelNeeds, Shortfall as ModelShortfall,
    candidates_from_routes, choose as choose_model,
};
pub use llm::model_memory::{Memory as ModelMemory, Selection as ModelSelectionRecord};
pub use llm::openai::{
    DEFAULT_BASE_URL as OPENAI_DEFAULT_BASE_URL, OpenAiConfig, OpenAiProvider,
    SseParser as OpenAiSseParser, Translator as OpenAiTranslator,
    provider_from_env as provider_from_environment,
};
pub use llm::provider_health::{Health as ProviderHealth, HealthTracker};
pub use llm::providers::{
    Modality, ModelInfo, MountedProvider, ProviderRoute, Providers, Route, RouteError,
    RouteRegistry, deepseek_route, fits as model_fits, window_of,
};
pub use llm::remote::{LlmRuntimeKey, register_llm_remote};
pub use llm::response_cache::{
    Cache as ResponseCache, CachedResponse, CachingProvider, DEFAULT_CAPACITY as CACHE_CAPACITY,
    Fingerprint, Stats as CacheStats, is_cacheable,
};
pub use llm::retry::{Backoff, FailureCode, RetryError, RetryMode, RetryPolicy, run_with_retry};
pub use llm::retrying_provider::RetryingProvider;
pub use llm::stats::Stats as LlmStats;
pub use llm::stream_blocks::{StreamObserver, assemble_message};
pub use llm::stream_flow::{Flow, SendResult, SlowDetector};
pub use llm::stream_method::{StreamError, StreamHandle, StreamMethod, Streams};
pub use llm::stream_stall::{
    Detector as StallDetector, Health as StreamHealth, Policy as StallPolicy,
};
pub use llm::stream_validate::{
    Ending as StreamEnding, Problem as StreamProblem, ProblemKind as StreamProblemKind,
    is_fatal as stream_is_fatal, validate as validate_stream,
    validate_with as validate_stream_ended,
};
pub use llm::token_meter::{
    Price, cost as token_cost, estimate_blocks, estimate_message, estimate_text, price_for,
};
pub use llm::usage_quota::{
    Quota as UsageQuota, Totals as UsageTotals, Tracker as UsageTracker, Usage as UsageRecord,
    Verdict as UsageVerdict,
};
pub use llm::{
    AssembleError, AssistantMessage, ChunkStream, ContentBlock, FinishReason, LlmError, LlmKey,
    LlmProvider, LlmRequest, Message, Role, StreamChunk, TokenUsage, ToolSchema,
};
pub use lsp::tool::{character_to_utf16_column, lsp_tool, utf16_column_to_character};
pub use lsp::{
    Answer as LspAnswer, Location as LspLocation, LspError, Operation as LspOperation,
    Position as LspPosition, Provider as LspProvider, Range as LspRange, Registry as LspRegistry,
    Request as LspRequest, Static as StaticLsp, utf16_column_to_byte,
};
pub use mcp::client::{
    IdAllocator, McpError as McpProtocolError, McpResponse, McpTool as McpProtocolTool,
    NAMESPACE_SEPARATOR, PROTOCOL_VERSION, RpcError, ServerConfig, call_tool_request,
    initialize_request, list_tools_request, parse_response, parse_tools, qualified_name,
    split_qualified_name, validate_server_name,
};
pub use mcp::config::{
    ConfigError as McpConfigError, ServerConfig as McpServerConfig,
    StartedServer as StartedMcpServer, parse_config as parse_mcp_config,
    start_all as start_mcp_servers,
};
pub use mcp::process::{
    ExitReason, Fleet as McpFleet, FleetError as McpFleetError, Lifecycle, State as McpState,
};
pub use mcp::stdio::{FrameBuffer, StdioConfig, decode_frames, encode_frame};
pub use mcp::subprocess::Subprocess as McpSubprocess;
pub use mcp::tools::{
    MAX_BRIDGED_TOOLS, register_mcp_resource_tools, register_mcp_tools, render_resource,
};
pub use mcp::{
    CallResult as McpCallResult, Catalogue as McpCatalogue, McpError, Registry as McpRegistry,
    Status as McpStatus, Tool as McpTool, Transport as McpTransport,
};
pub use mcp::{Resource as McpResource, ResourceContent as McpResourceContent};
pub use ptc::policy::{
    Decision as PtcDecision, ExecRequest, Limits as PtcLimits, Policy as PtcPolicy,
};
pub use sandbox::path::{Sandbox as PathSandbox, SandboxError, Verdict as SandboxVerdict};
pub use sandbox::policy::{
    ConfineError as SandboxConfineError, ConfinedArgv, ESCALATION_TARGETS, EscalationApproval,
    EscalationApprover, EscalationAsk, EscalationDisplayReason, EscalationError,
    EscalationErrorCode, EscalationOutcome, EscalationRequest, ExecutionPolicy as SandboxPolicy,
    LANDLOCK_LAUNCHER_FAILURE_EXIT, LandlockGrants, PolicyRequest, PolicyResolver,
    Runner as SandboxRunner, SANDBOX_MODES, SandboxRootError, SelectedRunner,
    WINDOWS_ACL_RUNNER_FAILURE_EXIT, approve_escalation, assert_temp_root_outside_workspace,
    bwrap_profile_args, canonical_path as sandbox_canonical_path,
    configured_runner_denial_signatures, confine, denial_signatures, escalation_audit_reason,
    escalation_display_reason, host_temp_root, is_execution_world_path, is_strictly_wider,
    landlock_grants, parse_mode, platform_chain, render_policy_context, runner_failure_rules,
    sandbox_permissions_description, seatbelt_profile_args, select_runner, session_override,
    set_sandbox_mode, static_enforcement, temp_root_inside_workspace, unavailable_message,
    validate_escalation_args, validate_runner_config, wider_modes, workspace_writable_roots,
    writable_roots,
};
pub use schedule::rpc::{
    MemoryScheduleStore, ScheduleStore, UnavailableScheduleStore, register_schedule_methods,
};
pub use schedule::runner::{
    Deliverer, Delivery, IDLE_WAIT, MAX_WAIT, Pass, Runner, pass as run_schedule_pass,
    wait_for as schedule_wait_for,
};
pub use schedule::{
    CreateError, CronExpression, Firing, Instant as ScheduleInstant, MAX_TITLE_CHARS, MIN_INTERVAL,
    Offset, Rule, Schedule, ScheduleError, ScheduleId, Scheduler, Tick, TimeOfDay,
    create as create_schedule, next_fire, now_millis, pause as pause_schedule,
    resume as resume_schedule, run_scheduler,
};
pub use session::archive::{
    ArchiveOutcome, ArchivePolicy, LogStats, analyze as analyze_log, archive as archive_session,
};
pub use session::backend::{
    Backend as SessionBackend, Capabilities as BackendCapabilities, Choice as BackendChoice,
    Config as SessionBackendConfig, choose as choose_session_backend,
};
pub use session::branch::{
    BranchError, Forest as SessionForest, Link as SessionLink, forest_from_ids, links_from_ids,
};
pub use session::checkpoint_store::{
    Checkpoint as StoredCheckpoint, Kind as CheckpointKind, Store as CheckpointStore,
    StoreError as CheckpointError,
};
pub use session::digest::{Digest, digest as digest_session, one_line as digest_one_line};
pub use session::engine::{
    Channel as ProjectionChannel, ListenerId as ProjectionListenerId, Notifier,
    ProjectionValueStore, Publish, PublishKind, SeqCursor,
};
pub use session::export_bundle::{
    Bundle as ExportBundle, BundleError as ExportBundleError, Entry as ExportBundleEntry,
    manifest as export_manifest, validate as validate_export_bundle,
};
pub use session::export_rpc::register_session_export_method;
pub use session::filter::{
    Filter as SessionFilter, Order as SessionOrder, SessionSummary as FilterSummary,
    apply as apply_session_filter,
};
pub use session::health::{
    Issue as HealthIssue, Kind as HealthKind, Severity as HealthSeverity,
    diagnose as diagnose_health, is_healthy, score as health_score,
};
pub use session::layout::{
    Layout as SessionLayout, LayoutError as SessionLayoutError, SessionFiles,
};
pub use session::lifecycle_rpc::{
    ARCHIVE_METHOD, ArchiveError, ArchiveStore, COPY_SUFFIX,
    DECLARED_ACCESS as SESSION_LIFECYCLE_ACCESS, DUPLICATE_METHOD, MAX_COPY_TITLE_CHARS,
    MemoryArchiveStore, NAMESPACE as SESSION_LIFECYCLE_NAMESPACE, UnavailableArchiveStore,
    register_session_lifecycle_methods, register_session_lifecycle_methods_with_archive,
    retain_visible,
};
pub use session::link::{
    Graph as SessionLinkGraph, Kind as SessionLinkKind, Link as CrossSessionLink,
    LinkError as SessionLinkError,
};
pub use session::log_compact::{Compacted, compact as compact_log, is_compactable};
pub use session::log_integrity::{Integrity as LogIntegrity, inspect as inspect_log_integrity};
pub use session::message::{
    CONTEXT_SUMMARY_MAX_CHARS, ContextForm, ContextSnapshotSection, Message as ChatMessage,
    MessageError, MessageSource, Role as ChatRole, bound_context_summary, check_history,
};
pub use session::metrics::{
    Sample, to_aggregate_samples, to_prometheus, to_samples as session_metric_samples,
};
pub use session::migration::{
    FormatVersion, Migration, MigrationError, Migrator, detect as detect_format,
    upgrade as upgrade_event, upgrade_in_session,
};
pub use session::persistence::{
    Footprint as PersistenceFootprint, Persistence, Pruned as PrunedData,
};
pub use session::portable::{
    Bundle as SessionBundle, BundleError, VERSION as BUNDLE_VERSION, export as export_bundle,
    import as import_bundle,
};
pub use session::preview::{
    Preview as SessionPreview, preview as preview_session,
    preview_from_tail as preview_session_tail,
};
pub use session::projection::{
    ModelSelectionProjection, ProjectedSession, Projection, ProjectionError, ProjectionHandle,
    ProjectionRegistry, Seq as ProjectionSeq, TitleProjection, TurnBoundary,
};
pub use session::projection_extra::{
    CacheError, Checkpoint, CheckpointRow, ProjectionCache, Snapshot as ProjectionSnapshot,
    Versioned, VersionedProjection,
};
pub use session::projection_host::{HostError, ProjectionHost, Registry as ProjectionHosts};
pub use session::query::{
    Candidate, Catalogue as SearchCatalogue, FileCatalogue, Hit, Kind as SearchKind,
    Query as SearchQuery, Results as SearchResults, excerpt as session_excerpt,
    matches as search_matches, query as search_sessions, run as run_query, search_files,
    searchable_text,
};
pub use session::query_rpc::{
    DEFAULT_LIMIT as SESSION_SEARCH_DEFAULT_LIMIT, MAX_TEXT_CHARS as SESSION_SEARCH_MAX_TEXT_CHARS,
    register_session_query_methods,
};
pub use session::query_syntax::{
    Expr as QueryExpr, QueryError as QuerySyntaxError, matches as query_matches,
    parse as parse_query,
};
pub use session::queue_rpc::{
    AgentQueueMap, DECLARED_ACCESS as SESSION_QUEUE_ACCESS, QueueAgentSource, QueueError,
    register_session_queue_methods,
};
pub use session::recovery::{
    Diagnosis, Recovery, diagnose as diagnose_session, repair as repair_session,
};
pub use session::replay::{
    Diff as ReplayDiff, Snapshot as ReplaySnapshot, diff as diff_events, divergence_turn,
    replay_until, snapshot as snapshot_events,
};
pub use session::resume_point::{
    Point as ResumePoint, Reason as ResumeReason, find as find_resume, is_safe as resume_is_safe,
};
pub use session::resume_rpc::register_session_resume_method;
pub use session::retention::{
    ItemRetainer, ItemStrategy, Omitted, RetainedItems, RetainedText, TextRetainer, TextStrategy,
};
pub use session::rewind::{Checkpoint as RewindPoint, checkpoint_for_turn, checkpoints, rewind_to};
pub use session::search_index::{Entry as SearchIndexEntry, Index as SearchIndex};
pub use session::search_persist::{
    PersistError, Record as SearchRecord, Snapshot as SearchSnapshot,
    VERSION as SEARCH_INDEX_VERSION, load as load_search_index, path_in as search_index_path,
    save as save_search_index,
};
pub use session::search_tool::{DEFAULT_HITS as SESSION_SEARCH_DEFAULT_HITS, session_search_tool};
pub use session::session_static::{
    Page as StaticPage, escape as escape_html, render as render_static_export,
};
pub use session::state_rpc::{
    LiveTurns, NoLiveTurns, NoPendingInputs, PendingInputs, register_session_state_method,
    register_session_state_method_observing, register_session_state_method_with,
};
pub use session::stats::Stats as SessionStats;
pub use session::stream::{EndReason as StreamEndReason, Frame as SessionFrame, SessionStream};
pub use session::tag::{Source as TagSource, Tag, TagError, TagSet, normalize as normalize_tag};
pub use session::telemetry::{
    Attribute as TelemetryAttribute, Channel as TelemetryChannel, Policy as TelemetryPolicy,
    Record as TelemetryRecord, SessionFacts as TelemetrySessionFacts,
    Severity as TelemetrySeverity, SharingStatus, Verdict as TelemetryVerdict,
    agent_error as telemetry_agent_error, apply_policies as apply_telemetry_policies,
    body_of as telemetry_body, body_texts as telemetry_body_texts, capture as capture_telemetry,
    error_detail, escalate as telemetry_escalate, is_exported as telemetry_is_exported,
    plan_export_batches as plan_telemetry_export, redact_literal,
    resume_after as telemetry_resume_after, severity_of as telemetry_severity,
    shutdown as telemetry_shutdown, starts_history as telemetry_starts_history,
};
pub use session::timeline::{Segment as TimelineSegment, open_turn, outline as session_outline};
pub use session::title::{Source as TitleSource, derive as derive_title};
pub use session::transcript::{
    Format as TranscriptFormat, Message as TranscriptMessage, Transcript as SessionTranscript,
    export as export_transcript, to_json as transcript_to_json,
};
pub use session::transcript_import::{
    IMPORTED_PROVIDER, ImportError as TranscriptImportError, Imported as ImportedTranscript,
    detect as detect_transcript_format, import as import_transcript,
};
pub use session::workspace::{
    Association as WorkspaceAssociation, Index as WorkspaceIndex,
    WorkspaceError as WorkspacePathError, WorkspaceId as WorkspaceRoot,
};
pub use session::{
    MemorySession, Session, SessionError, SessionEvent, SessionKey, SessionStore, TurnEndReason,
    read_session_file, read_session_header,
};
pub use settings::call_config::{CallConfig, ConfigError as CallConfigError, ConfigTable};
pub use settings::config::{
    ConfigError, ConfigSchema, ConfigValue, Entry, EntryList, Environment, FieldError, FieldRule,
    FieldSchema, Owner, Patch, PatchWarning, Template, When, parse_entries, parse_patches,
};
pub use settings::config_edit::{SchemaError, SchemaRegistry, SettingSpec};
pub use settings::file::FileSettingsStore;
pub use settings::rpc::{
    MemorySettingsStore, SettingsStore, UnavailableSettingsStore, register_settings_methods,
};
pub use shell::exec::{
    AdvisorySandbox, Enforcement, ExecError, ExecOptions, ExecOutput, PathPolicy, Sandbox,
    bash_tool, has_program, landlock_abi, read_file, run_bash, run_shell,
};
pub use shell::process_group::{GroupError, GroupGuard, run_isolated};
pub use shell::render::{
    Channel as ShellChannel, Chunk as ShellChunk, CollectedOutput, ParsedExit, ParsedExitStatus,
    RunOutcome as ShellRunOutcome, RunnerFailureRule, SandboxEnforcement, ShellSandboxInfo,
    classify_denial, classify_runner_failure, escalation_hint_marker, matches_signature,
    parse_exit_status, render_job_read, render_promoted, render_result, ring_delta,
    runner_failure_notice, sandbox_denial_marker, stream_text,
};
pub use shell::subprocess::{
    ProcessError, ProcessOutcome, SpawnOptions, has_program as has_program_in_path,
    run as run_process, spawn as spawn_process,
};
pub use shell::{
    DEFAULT_MAX_BYTES, DEFAULT_TIMEOUT, ENV_PREFIX, Environment as ShellEnvironment, Program,
    Request as ShellRequest, RunResult as ShellRunResult, SandboxMode,
    Specification as ShellSpecification, split_words, which,
};
pub use skill::catalog::{
    SkillHomes, SkillMeta, SkillRoot, discover as discover_skills, load_skill_registry,
    parse_frontmatter, parse_invocation, render_list as render_skill_list, skill_roots,
};
pub use skill::tool::skill_tool;
pub use skill::{
    Definition as SkillDefinition, Provider as SkillProvider, Registry as SkillRegistry,
    SkillError, Static as StaticSkills, Summary as SkillSummary,
};
pub use spill::policy::{
    Action as SpillAction, Context as SpillContext, Reason as SpillReason, decide as decide_spill,
    omitted as spill_omitted,
};
pub use spill::{
    LocalStore as LocalSpillStore, Reference as SpillReference, SaveRequest as SpillSaveRequest,
    SpillError, Store as SpillStore, Unavailable as UnavailableSpillStore, should_spill,
};
pub use ssh::{
    Client as SshClient, Command as SshCommand, DEFAULT_PORT as SSH_DEFAULT_PORT,
    DEFAULT_TIMEOUT as SSH_DEFAULT_TIMEOUT, Host as SshHost, Outcome as SshOutcome, SshError,
    Transport as SshTransport, Unavailable as UnavailableSsh,
};
pub use storage::per_record::{
    Descriptor as RecordDescriptor, GLOBAL_DOCUMENT, Loaded as LoadedRecords, PerRecordUnit,
    RECORD_SUFFIX, RecordError, STAGED_SUFFIX, format_backup_stamp, global_path,
    is_safe_record_key, is_valid_unit_name as is_safe_unit_name, parse_record, record_path,
    serialize_record, unit_directory,
};
pub use storage::quota::{
    Footprint, Quota as StorageQuota, Usage as StorageUsage, evict as evict_storage, is_oversized,
    usage as storage_usage,
};
pub use storage::{
    FileBackend, KvBackend, Layout, Records, Snapshot, StorageError, Unit, UnitDescriptor,
    is_valid_record_key, is_valid_unit_name,
};
pub use subagent::depth::{
    Depth as SubagentDepth, DepthError as SubagentDepthError, Limit as SubagentDepthLimit,
};
pub use subagent::interrupt::{
    Armed as InterruptArmed, Cancellers, InterruptOutcome, Interrupter, default_interrupter,
    subagent_interrupt_tool,
};
pub use subagent::pool::{Pool as SubagentPool, Slot as SubagentSlot};
pub use subagent::providers::{
    Capabilities as SubagentCapabilities, ChildHandle, ProviderHandle, ProviderRegistry,
    StartError, StartRequest, SubagentProvider,
};
pub use subagent::tools::{
    DEFAULT_CONCURRENCY, StartRefusal, Status as SubagentStatus, SubagentEntry, SubagentRegistry,
    WaitError, register_live_subagent_tools, register_subagent_tools, subagent_fork_tool,
    subagent_list_tool, subagent_send_tool, subagent_wait_tool, traced_subagent_tool,
    wait_timeout_ms,
};
pub use subagent::{
    DelegationResult, Depth, DepthLimitedTools, ScriptedDelegator, SubagentError, delegate,
    delegate_with, subagent_tool,
};
pub use telemetry::attribution::Attribution as ContentAttribution;
pub use telemetry::health_trend::{
    HEALTH_RATE_DROP, SCORE_DROP, Sample as HealthSample, Trend as HealthTrend,
    analyze as analyze_health_trend, sample as sample_health,
};
pub use telemetry::stats_csv::{Table as CsvTable, escape_field, session_table, tool_table};
pub use telemetry::stats_trend::{
    Comparison as TrendComparison, Point as TrendPoint, Trend, analyze as analyze_trend,
    compare as compare_windows, outliers as trend_outliers,
};
pub use telemetry::trace_span::{Span as TraceSpan, Tracer};
pub use terminal::attach::{Attachment as TerminalAttachment, Fanout as TerminalFanout};
pub use terminal::attach_rpc::{AttachResponse, TerminalFrame};
pub use terminal::buffer::{Buffer as TerminalBuffer, Chunk as TerminalChunk};
pub use terminal::env::{
    Environment as TerminalEnvironment, Shell as AvailableShell, detect as detect_terminal_env,
    detect_shells,
};
pub use terminal::local::{LocalLauncher, ProcessShell};
pub use terminal::meta::{
    Meta as TerminalMeta, MetaError as TerminalMetaError, Retention as TerminalRetention,
    Store as TerminalMetaStore,
};
pub use terminal::persist::{PersistError as TerminalPersistError, Store as TerminalPersistStore};
pub use terminal::retain::{
    Request as RetainRequest, RetainError, Retained as RetainedOutput, read as read_retained,
    retain,
};
pub use terminal::retain_rpc::{
    Args as RetainArgs, description as retain_description, execute as execute_retain,
    parse_args as parse_retain_args,
};
pub use terminal::rpc::{
    DEFAULT_READ_WAIT_MS as TERMINAL_DEFAULT_READ_WAIT_MS, MAX_WAIT_MS as TERMINAL_MAX_WAIT_MS,
    register_terminal_methods as register_terminal_rpc_methods, register_terminal_rename_method,
    register_terminal_retain_method,
};
pub use terminal::rpc_extra::{TerminalShell, discover_shells, register_terminal_extra_methods};
pub use terminal::tool::{
    TerminalLauncher, UnavailableLauncher, register_terminal_tools, terminal_read_tool,
    terminal_send_tool, terminal_start_tool,
};
pub use terminal::{
    DEFAULT_MAX_BYTES as DEFAULT_TRANSCRIPT_BYTES, Launch as TerminalLaunch,
    Registry as TerminalRegistry, Session as TerminalSession, Shell, Size as TerminalSize,
    Status as TerminalStatus, TerminalError, Transcript,
};
pub use todo::tool::{Status as TodoStatus, Todo, read_todos, todo_tool};
pub use tokio_util::sync::CancellationToken;
pub use typert::json::{catalogue_to_json_schema, method_to_json_schema, to_json_schema};
pub use typert::{
    Catalogue, Field, Method, ObjectSchema, Parameter, Schema, Variant, to_typescript,
};
pub use util::atomic_write::{
    AtomicError, DEFAULT_LOCK_WAIT, Mode, with_lock, write_atomic, write_text_atomic,
};
pub use util::backoff::{
    Retryable as BackoffRetryable, Strategy as BackoffStrategy, should_retry as should_retry_after,
};
pub use util::bounded_queue::{BoundedQueue, Closed, Overflow};
pub use util::brand::{
    AgentId, AgentIdBrand, Brand, Id, IdError, MAX_ID_LEN, MessageId, MessageIdBrand, SessionId,
    SessionIdBrand, ToolCallId, ToolCallIdBrand, WorkspaceId, WorkspaceIdBrand,
};
pub use util::canonical::{Encoder, canonical_json, key_of};
pub use util::chunked_list::{CHUNK_CAPACITY, List as ChunkedList};
pub use util::code_language::{for_extension, for_path, read_hint};
pub use util::error_recovery::{
    Action as RecoveryAction, Kind as RecoveryKind, Suggestion as RecoverySuggestion,
    suggest as suggest_recovery,
};
pub use util::error_taxonomy::{
    Category as ErrorCategory, categorize as categorize_error, categorize_removal, looks_missing,
    tag as error_tag,
};
pub use util::home_paths::Paths as HomePaths;
pub use util::json::{
    JsonHazard, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER, hazards, is_lossless, normalize,
    number_from_f64, number_from_u64,
};
pub use util::launch_environment::{
    AMBIENT as AMBIENT_VARIABLES, NEVER as NEVER_VARIABLES, Resolved as LaunchEnvironment,
    is_ambient, is_forbidden, resolve as resolve_launch_environment,
};
pub use util::limits::{Budget, Ceiling, Session as SessionBudget};
pub use util::list_render::{
    FileEntry, MatchEntry, parse_glob_output, parse_grep_output, render_files as render_file_list,
    render_matches as render_match_list,
};
pub use util::name_suggest::{
    Candidate as NameCandidate, closest as closest_names, distance as name_distance,
    suggest_tool as suggest_tool_name,
};
pub use util::partial_json::{PartialArguments, deep_equal_json, is_json_value, utf16_len};
pub use util::paths::{
    AGENTS_HOME_DISPLAY, AGENTS_HOME_ENV, HARNESS_HOME_DISPLAY, HARNESS_HOME_ENV, Root,
    RootEnvironment, RootError, abbreviate_home, agents_home, expand_home, harness_home, home_dir,
    join_within,
};
pub use util::time::{
    Instant as TimeInstant, MAX_FOUR_DIGIT_YEAR, MIN_FOUR_DIGIT_YEAR, MS_PER_DAY, MS_PER_HOUR,
    MS_PER_MINUTE, MS_PER_SECOND, Span, SpanParts, TimeError, WallClock, civil_from_days,
    days_from_civil, days_in_month, format_instant, format_span, is_four_digit_year, is_leap_year,
    parse_instant,
};
pub use web::tools::{DEFAULT_SOURCES as WEB_DEFAULT_SOURCES, fetch_tool, search_tool};
pub use web::{
    EndpointProvider, FetchBody, FetchProvider, FetchRequest, FetchResult, SearchProvider,
    SearchRequest, SearchResult, SearchSource, Web, WebError, build_web_from_env,
};
pub use workflow::rpc::register_workflow_methods;
pub use workflow::tool::{register_workflow_tools, workflow_run_tool, workflow_status_tool};
pub use workflow::{
    AgentRecord as WorkflowAgentRecord, Engine as WorkflowEngine, Event as WorkflowEvent,
    Phase as WorkflowPhase, Registry as WorkflowRegistry, Run as WorkflowRun, RunId,
    StartRequest as WorkflowStartRequest, Status as WorkflowStatus, WorkflowError,
};
pub use workspace::controller::{
    AllowAll, ConfiguredDocuments, DEFAULT_FOLLOW_FRAMES, DEFAULT_JOURNAL_CAPACITY,
    DEFAULT_WORKSPACE_DIRECTORY, DirectoryEntry, DirectoryListing, DirectoryPickerBackend,
    DocumentsDirectory, FollowAccess, FollowPage, HARNESS_DIRECTORY, Hidden, LocalDirectoryPicker,
    MAX_DIRECTORY_ENTRIES, MutableAccess, PickerCapability, PickerError, SystemDocuments,
    WorkspaceFeed, default_workspace_path, is_default_workspace_title, register_directory_picker,
    register_initialize_default, register_workspace_follow, register_workspace_follow_allow_all,
    validate_directory_name, validate_documents_directory, validate_documents_directory_for,
    workspace_display_title, workspace_id_for_path,
};
pub use workspace::desktop_rpc::{
    DesktopEnvironment, FileApplication, FileApplicationProber, FileManager, HostPlatform,
    register_workspace_desktop_methods, register_workspace_desktop_methods_with,
};
pub use workspace::diff::{
    DEFAULT_CONTEXT, FileChange as FileDiffChange, LineDiff, TextDiff, diff_text,
};
pub use workspace::files_extra::{
    DEFAULT_READ_BYTES as WORKSPACE_READ_BYTES, DEFAULT_SEARCH_HITS as WORKSPACE_SEARCH_HITS,
    MAX_READ_BYTES as WORKSPACE_MAX_READ_BYTES, register_workspace_file_extras,
    register_workspace_file_extras_with_sessions, workspace_file_root,
};
pub use workspace::files_rpc::{
    register_workspace_file_methods, register_workspace_file_methods_with_sessions,
};
pub use workspace::open_rpc::register_open_workspace_path;
pub use workspace::path::{
    contains as workspace_contains, display as workspace_display, has_drive_prefix, is_absolute,
    is_unc, normalize as normalize_path, relative_to, resolve as resolve_in_workspace,
};
pub use workspace::rpc_extra::{
    FilePreferences, InMemoryPreferences, UnavailablePreferences, WorkspacePreferences,
    register_workspace_extra_methods, register_workspace_extra_methods_without_preferences,
    register_workspace_initialize_default,
};

pub use shell::persistent_shell::{
    Config as PersistentShellConfig, Output as PersistentShellOutput, PersistentShell,
    ShellError as PersistentShellError,
};
