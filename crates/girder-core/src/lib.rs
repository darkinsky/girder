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

mod agent_loop;
mod brand;
mod config;
mod events;
mod exec;
mod fiber;
mod fs;
mod instructions;
mod json;
mod llm;
mod llm_deepseek;
mod message;
mod paths;
mod registry;
mod remote;
mod retry;
mod runtime;
mod scope;
mod service;
mod session;
mod storage;
mod subagent;
mod subprocess;
mod tools;

pub use agent_loop::{LoopContext, LoopError, LoopStep, dispatch, run};
pub use brand::{
    AgentId, AgentIdBrand, Brand, Id, IdError, MAX_ID_LEN, MessageId, MessageIdBrand, SessionId,
    SessionIdBrand, ToolCallId, ToolCallIdBrand, WorkspaceId, WorkspaceIdBrand,
};
pub use config::{
    ConfigError, ConfigSchema, ConfigValue, Entry, EntryList, Environment, FieldError, FieldRule,
    FieldSchema, Owner, Patch, PatchWarning, Template, When, parse_entries, parse_patches,
};
pub use events::{
    Bail, Chain, DispatchError, Event, EventBus, Layer, ListenerId, ListenerScope, Next,
    ObservedEvent, Order, Parallel, ScopeKey, Serial, Terminal, Waterfall, child_scope,
};
pub use exec::{
    AdvisorySandbox, Enforcement, ExecError, ExecOptions, ExecOutput, PathPolicy, Sandbox,
    bash_tool, has_program, landlock_abi, read_file, run_bash, run_shell,
};
pub use fiber::{Disposer, Fiber, FiberError, FiberState, PluginBody, PluginSpec};
pub use fs::{
    FileSystem, FsDirEntry, FsError, FsInfo, FsTarget, FsTargetKey, FsType, FsVersion,
    LocalFileSystem, read_file as read_file_via_fs, target_at,
};
pub use instructions::{
    DEFAULT_MAX_BYTES_PER_FILE, DEFAULT_MAX_TOTAL_BYTES, HARNESS_GLOBAL_DIRECTORY,
    INSTRUCTION_FILE_NAME, InstructionFile, InstructionLimits, InstructionState, LoadedInstruction,
    RenderedInstructions, TruncatedInstruction, USER_GLOBAL_DIRECTORY, digest,
    discover as discover_instructions, load as load_instructions, render as render_instructions,
};
pub use json::{
    JsonHazard, MAX_SAFE_INTEGER, MIN_SAFE_INTEGER, hazards, is_lossless, normalize,
    number_from_f64, number_from_u64,
};
pub use llm::{
    AssembleError, AssistantMessage, ChunkStream, ContentBlock, FinishReason, LlmError, LlmKey,
    LlmProvider, LlmRequest, Message, Role, StreamChunk, TokenUsage, ToolSchema,
};
pub use llm_deepseek::{
    DeepSeekConfig, DeepSeekProvider, SseEvent, SseParser, Translator, provider_from_env,
};
pub use message::{
    CONTEXT_SUMMARY_MAX_CHARS, ContextForm, ContextSnapshotSection, Message as ChatMessage,
    MessageError, MessageSource, Role as ChatRole, bound_context_summary, check_history,
};
pub use paths::{
    AGENTS_HOME_DISPLAY, AGENTS_HOME_ENV, HARNESS_HOME_DISPLAY, HARNESS_HOME_ENV, Root,
    RootEnvironment, RootError, abbreviate_home, agents_home, expand_home, harness_home, home_dir,
    join_within,
};
pub use registry::{Label, Registry, RegistryContext, RegistryError};
pub use remote::{
    DirectoryCatalog, RemoteCall, RemoteError, RemoteMethod, RemoteRegistry, SessionAddress,
    SessionCatalog, SessionCreateRequest, SessionCreateValue, SessionEventsPage,
    SessionHistoryRecord, SessionListValue, SessionPageRequest, SessionPromptRequest,
    SessionPromptValue, SessionRecord, SessionSummary, default_registry, register_session_methods,
    registry_with_runtime,
};
pub use retry::{Backoff, FailureCode, RetryError, RetryMode, RetryPolicy, run_with_retry};
pub use runtime::{AgentEvent, AgentRuntime, AgentTurn, RuntimeConfig, RuntimeError};
pub use scope::{BoxDisposer, BoxFut, Scope};
pub use service::ServiceKey;
pub use session::{
    MemorySession, Session, SessionError, SessionEvent, SessionKey, SessionStore, TurnEndReason,
    read_session_file,
};
pub use storage::{
    FileBackend, KvBackend, Layout, Records, Snapshot, StorageError, Unit, UnitDescriptor,
    is_valid_record_key, is_valid_unit_name,
};
pub use subagent::{
    DelegationResult, Depth, DepthLimitedTools, ScriptedDelegator, SubagentError, delegate,
    subagent_tool,
};
pub use subprocess::{
    ProcessError, ProcessOutcome, SpawnOptions, has_program as has_program_in_path,
    run as run_process, spawn as spawn_process,
};
pub use tools::{
    PostExecuteArgs, PostToolDecision, PreToolDecision, ToolBody, ToolDefinition, ToolError,
    ToolExecute, ToolExecution, ToolPostExecute, ToolPreExecute, ToolRegistry, ToolRegistryView,
    ToolResult, ToolRunContext, ToolValue, ToolsKey,
};
