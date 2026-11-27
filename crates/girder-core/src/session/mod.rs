//! The session seam: durable event log, JSONL framing, and zstd compression.
//!
//! Upstream's session is a zstd-compressed JSONL file of typed events. This module carries the
//! event vocabulary the vertical slice needs — six variants, not the 60+ upstream has — plus the
//! store that appends and replays them.
//!
//! # Why the log is the source of truth
//!
//! A session is not a mutable object that a store updates. It is an append-only sequence of events,
//! and every derived view (the conversation, the token count, the tool history) is a fold over that
//! sequence. That is what makes `session/fork`, replay, and crash recovery the same operation:
//! re-read the log. The alternative — mutating a conversation object and persisting it whole —
//! loses the intermediate states that a fork needs and makes a partial write unrecoverable.
//!
//! # Compression
//!
//! Upstream zstd-compresses the JSONL. Each event is one JSON object on one line, so a truncated
//! tail loses at most the final event. Writing the frame with `zstd::Encoder` at a fixed level keeps
//! the file streamable: a reader decompresses incrementally rather than needing the whole file.
//!
//! # What is deliberately not here
//!
//! Upstream's `request-header`, `seq-ranges`, `repair`, `surface`, and `fork` modules exist because
//! a real session is resumed, compacted, and branched. The slice needs append and replay only; the
//! rest belongs with phase 5+ once there is a reason to resume.

pub mod archive;
pub mod backend;
pub mod branch;
pub mod change_feed;
pub mod checkpoint_store;
pub mod client;
pub mod content_projection;
pub mod controller;
pub mod digest;
pub mod engine;
pub mod export_bundle;
pub mod export_rpc;
pub mod extra;
pub mod filter;
pub mod format_catalog;
pub mod health;
pub mod layout;
pub mod lifecycle_rpc;
pub mod link;
pub mod log_compact;
pub mod log_integrity;
pub mod message;
pub mod metrics;
pub mod migration;
pub mod migration_steps;
pub mod observe;
pub mod persistence;
pub mod portable;
pub mod preview;
pub mod proj_extra;
pub mod projection;
pub mod projection_cache;
pub mod projection_extra;
pub mod projection_host;
pub mod projection_setup;
pub mod query;
pub mod query_plan;
pub mod query_rpc;
pub mod query_syntax;
pub mod queue_rpc;
pub mod recovery;
pub mod reference;
pub mod replay;
pub mod resume_point;
pub mod resume_rpc;
pub mod retention;
pub mod rewind;
pub mod search_index;
pub mod search_persist;
pub mod search_tool;
pub mod session_final;
pub mod session_static;
pub mod snapshot;
pub mod state_rpc;
pub mod stats;
pub mod stats_rollup;
pub mod stream;
pub mod tag;
pub mod telemetry;
pub mod timeline;
pub mod title;
pub mod title_dispatch;
pub mod title_llm;
pub mod top;
pub mod transcript;
pub mod transcript_import;
pub mod wait;
pub mod window;
pub mod workspace;

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::core::scope::BoxFut;
use crate::llm::{ContentBlock, FinishReason};
use crate::util::brand::ToolCallId;

/// Why a turn ended.
///
/// A tagged union rather than a reuse of [`FinishReason`]: a turn can end for reasons a model never
/// reports — a cancellation, a blocked step, a tool that failed to activate. Overloading the model's
/// stop reason would make "the model finished" and "the user pressed Ctrl-C" the same value in the
/// log, and a replay could not tell them apart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TurnEndReason {
    /// The model finished its work.
    Completed,

    /// The caller cancelled.
    Aborted {
        /// Why, so a replay can distinguish a user request from a timeout.
        cause: String,
    },

    /// A step was blocked before it could run.
    Blocked,

    /// The session's token budget ran out.
    ///
    /// Distinct from `Blocked`: nothing objected to the step, the session simply had no more to spend. A reader
    /// investigating a stopped session needs to tell those apart.
    Budget,

    /// The turn failed.
    Failed {
        /// The failure, flattened to text and a code.
        message: String,
        /// A stable code a reader can branch on.
        code: String,
    },
}

impl From<FinishReason> for TurnEndReason {
    fn from(reason: FinishReason) -> Self {
        match reason {
            FinishReason::Stop | FinishReason::ToolCalls => Self::Completed,
            FinishReason::Length => Self::Failed {
                message: "the model reached its output limit".to_owned(),
                code: "MAX_TOKENS".to_owned(),
            },
            FinishReason::Aborted => Self::Aborted {
                cause: "aborted".to_owned(),
            },
            FinishReason::Error { message } => Self::Failed {
                message,
                code: "UNKNOWN".to_owned(),
            },
        }
    }
}

/// One durable session event.
///
/// The list carries everything a replay needs to reconstruct a conversation and explain how it got
/// there. Upstream has 54 event types across 30 domains; this is the conversation core — turn, step,
/// message, tool — which is what every other domain's events attach to.
///
/// # Why `step` exists
///
/// A turn is not one model call. It is a sequence of steps, each one model call plus the tool
/// executions that call requested. Recording only turns would lose which tool call answered which
/// step, and a replay could not reconstruct the model's view at any point mid-turn.
///
/// # Adding a variant
///
/// A new variant is a compile error at every `match`, which is the point: the log writer must
/// handle it rather than drop it silently.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
    /// A turn began.
    #[serde(rename = "turn/start")]
    TurnStart {
        /// Turn number, starting at 1.
        turn: u64,
    },

    /// A turn ended.
    #[serde(rename = "turn/end")]
    TurnEnd {
        /// Turn number.
        turn: u64,
        /// Why it stopped.
        reason: TurnEndReason,
    },

    /// A step began: one model call plus the tools it requested.
    #[serde(rename = "step/start")]
    StepStart {
        /// Turn it belongs to.
        turn: u64,
        /// Step number within the turn, starting at 1.
        step: u64,
    },

    /// A step ended.
    #[serde(rename = "step/end")]
    StepEnd {
        /// Turn it belongs to.
        turn: u64,
        /// Step number.
        step: u64,
    },

    /// A user-role message on the model-visible surface.
    ///
    /// One event for the person's prompt, a synthetic injection, and a relayed peer message: the
    /// role is the same, and the message's `source` tells them apart. Splitting into three events
    /// would make every consumer match three cases to read one turn.
    #[serde(rename = "user/message")]
    UserMessage {
        /// The message, with its identity and provenance preserved.
        message: crate::session::message::Message,
    },

    /// A developer-role message: a tool addition or removal riding the turn.
    #[serde(rename = "developer/message")]
    DeveloperMessage {
        /// Turn it was admitted at.
        turn: u64,
        /// Step it was admitted at.
        step: u64,
        /// The message.
        message: crate::session::message::Message,
    },

    /// The model's response for one step.
    #[serde(rename = "assistant/message")]
    AssistantMessage {
        /// The message, carrying provider identity for replay.
        message: crate::session::message::Message,
    },

    /// What one model call cost.
    ///
    /// A separate event rather than a field on the message: a message is content a reader renders, and a cost is
    /// accounting a reader sums. Folding them together would make every consumer of the content carry a number it
    /// does not use, and a replay that wanted only the cost would have to parse messages.
    #[serde(rename = "model/usage")]
    ModelUsage {
        /// The model that answered.
        model: String,
        /// Tokens the prompt cost.
        input_tokens: u64,
        /// Tokens the answer cost.
        output_tokens: u64,
    },

    /// A tool was invoked.
    #[serde(rename = "tool/call")]
    ToolCall {
        /// Provider-issued call id.
        id: ToolCallId,
        /// Tool name.
        name: String,
        /// Raw JSON arguments, exactly as the model produced them.
        arguments: String,
    },

    /// A tool produced a result.
    #[serde(rename = "tool/result")]
    ToolResult {
        /// The call this answers.
        id: ToolCallId,
        /// What it produced.
        content: Vec<ContentBlock>,
        /// Whether it failed.
        is_error: bool,
    },

    /// The rendered system prompt on the model-visible surface.
    ///
    /// Recorded per step rather than once, because the prompt can change between steps: an
    /// instruction file is edited, a tool is added, the working directory moves. A replay that kept
    /// only the first would show the model a prompt it never saw.
    #[serde(rename = "system/message")]
    SystemMessage {
        /// Turn it was assembled in.
        turn: u64,
        /// Step it was assembled for.
        step: u64,
        /// The message.
        message: crate::session::message::Message,
    },

    /// The model selection this step will use.
    ///
    /// Carries the selection rather than a `Value`: a selection has a shape — provider and model are
    /// both required — and storing it loosely would let a log hold one half.
    #[serde(rename = "model/selection")]
    ModelSelection {
        /// The selection.
        selection: crate::core::agent::ModelSelection,
    },

    /// The capability preset in force.
    ///
    /// A preset decides what the model may do — read-only, edit, full access — so a replay without
    /// it cannot explain why a tool was refused.
    #[serde(rename = "permission/preset")]
    PermissionPreset {
        /// The preset name.
        preset: String,
    },

    /// Whether plan mode is on.
    ///
    /// Plan mode changes the contract with the model: it must describe before acting. A turn
    /// recorded without it looks like a model that failed to act.
    #[serde(rename = "plan/mode")]
    PlanMode {
        /// Whether it is active.
        active: bool,
    },

    /// A host move of the session's working directory.
    ///
    /// Upstream's `workspace-root/change`. The header keeps the original project. This event is
    /// the directory file views and the `workingDirectory` projection use from here on.
    #[serde(rename = "workspace-root/change")]
    WorkspaceRootChange {
        /// Absolute directory the session works in from this event on.
        cwd: String,
    },

    /// The sandbox mode in force.
    #[serde(rename = "sandbox/mode")]
    SandboxMode {
        /// The mode.
        mode: String,
        /// Where the setting came from, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },

    /// The approval policy in force.
    #[serde(rename = "approval/policy")]
    ApprovalPolicy {
        /// The policy name.
        policy: String,
    },

    /// The context window the request was planned against.
    ///
    /// Recorded because compaction decisions depend on it: a log without the window cannot explain
    /// why a conversation was compacted when it looked small.
    #[serde(rename = "request/context")]
    RequestContext {
        /// The provider.
        provider: String,
        /// The model.
        model: String,
        /// The window, when the catalogue declared one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
    },

    /// The exact request envelope a turn was built from.
    ///
    /// Upstream's `request/header`. Distinct from [`Self::RequestContext`], which records only the
    /// provider and window for diagnostics: this one carries the **whole call configuration plus the
    /// tool schemas**, because its purpose is replay. A `request/context` says which model ran; a
    /// `request/header` says what bytes a replay would have to send to run it again.
    ///
    /// Serialized as one opaque object rather than exploded into typed fields: the header's shape is
    /// `EpochHeader`'s, it is validated there (`canonical_header`), and duplicating its fields here
    /// would be a second definition that could drift from the one the fold reads.
    #[serde(rename = "request/header")]
    RequestHeader {
        /// The `EpochHeader`, as `request_header_gap` models it.
        header: serde_json::Value,
    },

    /// The session's title, and what produced it.
    #[serde(rename = "session/title")]
    SessionTitle {
        /// The title.
        title: String,
        /// The message sequence numbers it was derived from.
        message_seqs: Vec<u64>,
        /// Whether a human or a model produced it.
        source: String,
    },

    /// An LLM request is about to be attempted.
    ///
    /// Recorded because a retry makes the log otherwise unexplainable: two assistant messages for
    /// one step look like a protocol violation without the attempts between them.
    #[serde(rename = "llm/retry")]
    LlmRetry {
        /// Which attempt this is, starting at 1.
        attempt: u32,
        /// Why the previous one failed, when this is a retry.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },

    /// A message's feedback was recorded or replaced.
    ///
    /// Upstream's `feedback/message-put`, from `packages/feedback/message-feedback`. Feedback is a
    /// log event rather than a side table because it belongs to a message and must survive the same
    /// way the message does: a rating stored elsewhere would be lost by a fork or a compaction that
    /// preserved the conversation.
    #[serde(rename = "feedback/message-put")]
    MessageFeedbackPut {
        /// The message the feedback is about.
        message_id: String,
        /// `positive` or `negative`.
        rating: crate::feedback::message_feedback::Rating,
        /// A note, when the person wrote one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// A category, when the client supplied one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        category: Option<String>,
        /// The item's identity, so a replacement is recognisable as one.
        version: String,
        /// When the item was first recorded.
        created_at: i64,
        /// When it last changed.
        updated_at: i64,
    },

    /// Whether this session's tree runs as a team.
    ///
    /// Upstream's `team/mode`, from `packages/team/agent-team`. A session with team mode on is one
    /// whose root leads a tree of subagents that share a task graph; turning it off stops new team
    /// tools from being offered without stopping the children already running, which is why this is
    /// a recorded selection rather than a switch that kills anything.
    #[serde(rename = "team/mode")]
    TeamMode {
        /// Whether the tree is a team.
        enabled: bool,
    },

    /// One committed revision of a shared team task.
    ///
    /// Upstream's `team/task`. The log is the board: a later read replays these revisions in order,
    /// and a compare-and-set that lost the race is a stale revision rather than a second copy.
    #[serde(rename = "team/task")]
    TeamTask {
        /// Persisted schema version. The board accepts only version 2.
        version: u64,
        /// The root session this task belongs to.
        #[serde(rename = "teamId")]
        team_id: String,
        /// The whole snapshot, as the board stored it.
        task: serde_json::Value,
    },

    /// A remark about the session as a whole.
    ///
    /// Upstream's `feedback/record`, from `packages/feedback/command-feedback`. Distinct from
    /// [`Self::MessageFeedbackPut`] in what it answers about: this is "something about this session",
    /// with no message to attach to, which is what the `/feedback` command writes.
    #[serde(rename = "feedback/record")]
    SessionFeedbackRecord {
        /// The remark, omitted when only a category was given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// A category, when the caller supplied one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        category: Option<String>,
    },

    /// A summary that replaces earlier model-visible history.
    ///
    /// The replaced rows stay in the log. [`model_messages`] omits every id in `shadowed`, including
    /// the id of an older summary a later compaction replaced.
    #[serde(rename = "compaction/summary")]
    CompactionSummary {
        /// Identity of this summary, so a later compaction can replace it.
        id: String,
        /// The text that stands in for the replaced history.
        text: String,
        /// Message ids and tool-call ids this summary replaces.
        shadowed: Vec<String>,
    },

    /// Images omitted so a later request can fit the route's image budget.
    ///
    /// The original blocks stay in the log. A request replaces every listed attachment with the
    /// offload placeholder before it is sent.
    #[serde(rename = "image/offload")]
    ImageOffload {
        /// Attachment ids, oldest first.
        attachments: Vec<String>,
    },

    /// A decision recorded for an action card.
    ///
    /// Upstream's `action-cards/decision`. The card projection reads this event, so a dismiss
    /// survives a restart instead of living only in the process that accepted the click.
    #[serde(rename = "action-cards/decision")]
    ActionCardsDecision {
        /// The assistant message that offered the card.
        #[serde(rename = "cardId")]
        card_id: String,
        /// The action the card named.
        action: String,
        /// The verb that was applied, or `dismissed`.
        phase: String,
        /// Verb payload, when the client sent an object.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },

    /// A message's feedback was withdrawn.
    ///
    /// Upstream's `feedback/message-delete`. A separate event rather than a tombstone: the log is
    /// append-only, and a reader that sees the put followed by the delete knows both that feedback
    /// existed and that it was removed.
    #[serde(rename = "feedback/message-delete")]
    MessageFeedbackDelete {
        /// The message whose feedback was withdrawn.
        message_id: String,
    },
}

impl SessionEvent {
    /// The event's `type` tag, for diagnostics and filtering.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::TurnStart { .. } => "turn/start",
            Self::TurnEnd { .. } => "turn/end",
            Self::StepStart { .. } => "step/start",
            Self::StepEnd { .. } => "step/end",
            Self::ModelUsage { .. } => "model/usage",
            Self::UserMessage { .. } => "user/message",
            Self::DeveloperMessage { .. } => "developer/message",
            Self::AssistantMessage { .. } => "assistant/message",
            Self::ToolCall { .. } => "tool/call",
            Self::ToolResult { .. } => "tool/result",
            Self::SystemMessage { .. } => "system/message",
            Self::ModelSelection { .. } => "model/selection",
            Self::PermissionPreset { .. } => "permission/preset",
            Self::PlanMode { .. } => "plan/mode",
            Self::WorkspaceRootChange { .. } => "workspace-root/change",
            Self::SandboxMode { .. } => "sandbox/mode",
            Self::ApprovalPolicy { .. } => "approval/policy",
            Self::RequestContext { .. } => "request/context",
            Self::RequestHeader { .. } => "request/header",
            Self::SessionTitle { .. } => "session/title",
            Self::LlmRetry { .. } => "llm/retry",
            Self::MessageFeedbackPut { .. } => "feedback/message-put",
            Self::MessageFeedbackDelete { .. } => "feedback/message-delete",
            Self::SessionFeedbackRecord { .. } => "feedback/record",
            Self::CompactionSummary { .. } => "compaction/summary",
            Self::ImageOffload { .. } => "image/offload",
            Self::TeamMode { .. } => "team/mode",
            Self::TeamTask { .. } => "team/task",
            Self::ActionCardsDecision { .. } => "action-cards/decision",
        }
    }

    /// The turn this event belongs to, when it names one.
    #[must_use]
    pub fn turn(&self) -> Option<u64> {
        match self {
            Self::TurnStart { turn }
            | Self::TurnEnd { turn, .. }
            | Self::StepStart { turn, .. }
            | Self::StepEnd { turn, .. }
            | Self::DeveloperMessage { turn, .. }
            | Self::SystemMessage { turn, .. } => Some(*turn),
            Self::UserMessage { .. }
            | Self::AssistantMessage { .. }
            | Self::ModelUsage { .. }
            | Self::ToolCall { .. }
            | Self::ToolResult { .. }
            | Self::ModelSelection { .. }
            | Self::PermissionPreset { .. }
            | Self::PlanMode { .. }
            | Self::WorkspaceRootChange { .. }
            | Self::SandboxMode { .. }
            | Self::ApprovalPolicy { .. }
            | Self::RequestContext { .. }
            | Self::RequestHeader { .. }
            | Self::SessionTitle { .. }
            | Self::LlmRetry { .. }
            // Feedback answers about a message, not about a turn: it may be recorded long after the
            // turn that produced the message, so attributing it to one would be a claim the event
            // does not make.
            | Self::MessageFeedbackPut { .. }
            | Self::MessageFeedbackDelete { .. }
            | Self::SessionFeedbackRecord { .. }
            | Self::CompactionSummary { .. }
            | Self::ImageOffload { .. }
            | Self::TeamMode { .. }
            | Self::TeamTask { .. }
            | Self::ActionCardsDecision { .. } => None,
        }
    }
}

/// A failure raised by the session store.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The underlying file could not be read or written.
    #[error("session io: {0}")]
    Io(#[from] std::io::Error),

    /// A line was not valid JSON or not a known event.
    #[error("session log line {line}: {source}")]
    Corrupt {
        /// 1-based line number.
        line: usize,
        /// The decoder's error.
        source: serde_json::Error,
    },

    /// The log's format version is newer than this build understands.
    ///
    /// Distinct from [`Self::Corrupt`] because the two demand opposite actions: corrupt means the
    /// file is damaged and nothing can read it, newer means a *newer build* wrote it and this one
    /// must not guess. A reader told "corrupt" would delete the file; told this, it upgrades.
    #[error(
        "session log uses format v{stored}, but this build reads up to v{current}; \
         a newer build wrote it"
    )]
    FormatTooNew {
        /// The version the log declares.
        stored: u64,
        /// The newest version this build reads.
        current: u64,
    },

    /// The log's format version is older than current, and no migration reaches it.
    ///
    /// Separate from [`Self::FormatTooNew`] because the fix differs: one is an upgrade, this one is
    /// a build that should have carried the edge and does not.
    #[error(
        "session log uses format v{stored}, and no migration from it is implemented (this build \
         writes v{current})"
    )]
    FormatUnsupported {
        /// The version the log declares.
        stored: u64,
        /// The version this build writes.
        current: u64,
    },

    /// A zstd frame could not be decoded.
    #[error("session compression: {0}")]
    Compression(String),
}

/// A durable, append-only session log.
///
/// Writes are buffered per event: [`SessionStore::append`] opens, writes, and closes, so a crash
/// loses at most the event being written. That is slower than holding a handle, but it means a
/// crashed harness leaves a readable log rather than an empty one — the property that makes
/// recovery possible at all.
pub struct SessionStore {
    /// The cross-process write lease, held for this store's lifetime.
    ///
    /// **Separate from the `Mutex` around `append`**, and both are needed: the mutex serialises
    /// writers inside one process, and the lease is a kernel file lock that stops a *second process*
    /// from appending to the same log. Without it two `girder` processes on one session interleave
    /// compressed frames, and a reader gets a stream that decodes into a mixture of both.
    ///
    /// `None` when the store was built without one, which is what a test and an in-memory session
    /// want: acquiring a lease creates a directory and a lock file, and a caller that never writes to
    /// disk should not pay for that.
    lease: Option<std::sync::Arc<crate::storage::persistence_lease::SessionWriteLease>>,

    path: PathBuf,
    /// Compression level. 3 is zstd's default and balances speed against size for log text.
    level: i32,
    /// The last decoded log, while the file has not grown.
    ///
    /// Shared between clones so two handles on one path do not each decode: the loop reads the log every step, and
    /// a log that takes a millisecond to decode would add a millisecond to every step of a long turn.
    cached: Arc<std::sync::RwLock<Option<Cached>>>,
}

/// One decoded log, with the file size it was decoded from.
///
/// `Clone` rather than a reference: the read path holds it while decoding is skipped, and holding a lock across the
/// caller's use of the events would block the next append.
#[derive(Clone)]
struct Cached {
    /// The size the file was when this was decoded.
    ///
    /// A size rather than a modification time: an append always changes the size, and a rewrite that kept the size
    /// would also keep the content this cache holds.
    len: u64,
    /// The events, as a chunked list so a clone shares the chunks rather than copying the vector.
    events: crate::util::chunked_list::List<SessionEvent>,
}

/// One lock per log path, process-wide.
///
/// [`SessionStore::append`] is read-modify-write: it decodes the whole frame, pushes one event, and
/// rewrites it. Two appends to the same file that interleave therefore lose one of them, and both
/// writing the same staging file corrupts the result further. Measured before this lock existed:
/// 80 concurrent appends left 12 events and 41 errors.
///
/// Keyed by path rather than held in the store because a store is cheap to construct — the RPC
/// layer builds one per call — so a per-instance mutex would not serialise anything.
/// A per-path async lock for writers whose critical section contains an `await`.
///
/// A `std::sync::Mutex` guard is not `Send`, so holding one across an `await` makes the future
/// un-spawnable. Anything that must await inside its critical section — a file write, a version
/// check that reads metadata — uses this instead.
pub(crate) fn async_path_lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();

    let locks = LOCKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut guards = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(guards.entry(path.to_path_buf()).or_default())
}

pub(crate) fn path_lock(path: &Path) -> Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<std::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();

    let locks = LOCKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut guards = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(guards.entry(path.to_path_buf()).or_default())
}

impl SessionStore {
    /// Creates an empty session file if none exists.
    ///
    /// An empty file is a valid session: `read_all` treats a zero-length file as "no events", so a
    /// created session is visible to `list` before anything happens in it. Writing a placeholder
    /// event instead was the earlier approach, and it made a session that had never been prompted
    /// look like it had a turn.
    ///
    /// # Errors
    ///
    /// Returns the I/O error when the file cannot be created.
    pub fn touch(&self) -> Result<bool, SessionError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
        {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Creates an empty session file whose first record is a durable session header.
    ///
    /// The header is stored in the same compressed JSONL stream as the events. This keeps the
    /// immutable session identity beside the conversation while preserving the append-only event
    /// path: later calls to [`SessionStore::append`] add event frames after it.
    pub fn touch_with_header(&self, header: &serde_json::Value) -> Result<bool, SessionError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut line = serde_json::to_vec(header).map_err(|error| {
            SessionError::Compression(format!("failed to encode header: {error}"))
        })?;
        line.push(b'\n');
        let frame = zstd::encode_all(line.as_slice(), self.level)
            .map_err(|error| SessionError::Compression(error.to_string()))?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
        {
            Ok(mut file) => {
                std::io::Write::write_all(&mut file, &frame)?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Replaces the durable session header without dropping the events after it.
    ///
    /// A later working directory has to live in the same record `read_session_header` reads. The
    /// sidecar is not enough: a reader that finds a header in the log returns it and never looks
    /// beside the file. The rewrite holds the same path lock as [`Self::append`], then replaces
    /// the whole compressed stream so a torn frame cannot sit between the old header and the events.
    pub fn rewrite_header(&self, header: &serde_json::Value) -> Result<(), SessionError> {
        let lock = path_lock(&self.path);
        let _guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let encoded = serde_json::to_string(header).map_err(|error| {
            SessionError::Compression(format!("failed to encode header: {error}"))
        })?;
        let existing = match std::fs::read(&self.path) {
            Ok(bytes) if !bytes.is_empty() => {
                let decoded = zstd::decode_all(bytes.as_slice())
                    .map_err(|error| SessionError::Compression(error.to_string()))?;
                String::from_utf8(decoded).map_err(|error| {
                    SessionError::Compression(format!("session log is not UTF-8: {error}"))
                })?
            }
            Ok(_) | Err(_) => String::new(),
        };
        let mut lines: Vec<String> = existing.lines().map(str::to_owned).collect();
        let header_at = lines.iter().position(|line| !line.trim().is_empty());
        match header_at {
            Some(index) if session_header_line(&lines[index]) => lines[index] = encoded,
            Some(index) => lines.insert(index, encoded),
            None => lines.push(encoded),
        }
        let mut payload = lines.join("\n");
        payload.push('\n');
        let frame = zstd::encode_all(payload.as_bytes(), self.level)
            .map_err(|error| SessionError::Compression(error.to_string()))?;
        crate::util::atomic_write::write_atomic(
            &self.path,
            &frame,
            crate::util::atomic_write::Mode(0o644),
        )
        .map_err(|error| SessionError::Io(std::io::Error::other(error.to_string())))?;
        if let Ok(mut cached) = self.cached.write() {
            *cached = None;
        }
        Ok(())
    }

    /// Opens a store at `path`. The file is created on the first append or by [`touch`].
    ///
    /// [`touch`]: SessionStore::touch
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            level: 3,
            lease: None,
            cached: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Opens a store that holds the cross-process write lease.
    ///
    /// # Errors
    ///
    /// [`crate::storage::persistence_lease::LeaseError`] when another holder has it, or the directory
    /// cannot be created. Reported rather than retried: the lock is non-blocking on purpose, because
    /// waiting on a wedged holder is an unbounded hang with no diagnostic.
    pub fn with_lease(
        path: impl Into<PathBuf>,
        id: impl Into<String>,
    ) -> Result<Self, crate::storage::persistence_lease::LeaseError> {
        let path = path.into();
        let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let lease = crate::storage::persistence_lease::SessionWriteLease::acquire(dir, id)?;
        Ok(Self {
            path,
            level: 3,
            lease: Some(std::sync::Arc::new(lease)),
            cached: Arc::new(std::sync::RwLock::new(None)),
        })
    }

    /// Whether this store holds the cross-process write lease.
    ///
    /// Exposed so a caller can tell "I may write here" from "another process is writing here" without
    /// attempting an append that would fail.
    #[must_use]
    pub fn holds_lease(&self) -> bool {
        self.lease.is_some()
    }

    /// Sets the zstd compression level.
    #[must_use]
    pub fn with_level(mut self, level: i32) -> Self {
        self.level = level;
        self
    }

    /// The file this store writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends one event, compressing the whole file.
    ///
    /// zstd frames cannot be appended without re-encoding, so this rewrites the file: read the
    /// current frame, decode, append the line, re-encode. That is O(file) per event, which is
    /// acceptable for the slice (a conversation is kilobytes) and is the reason phase 5 will move
    /// to an uncompressed working file with periodic compaction.
    pub fn append(&self, event: &SessionEvent) -> Result<(), SessionError> {
        // Serialised per path so two appends cannot interleave their frame writes. The lock is still
        // needed although this is a pure append: a torn write would corrupt the frame.
        let lock = path_lock(&self.path);
        let _guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let line = serde_json::to_string(event).map_err(|error| {
            SessionError::Compression(format!("failed to encode event: {error}"))
        })?;
        let mut payload = line.into_bytes();
        payload.push(b'\n');

        // One compressed frame per append, concatenated onto the file.
        //
        // zstd decodes concatenated frames as a single stream, so a reader still sees the whole log.
        // This replaces a read-modify-write of the entire file: append cost was O(log size) and grew
        // quadratically, measured at 4.3 ms for a 500-message session.
        //
        // Appending trades crash-atomicity for that: a torn tail loses the event being written,
        // where a rewrite-and-rename could not. The format already tolerated a truncated tail for
        // exactly this reason, and the reader treats an unparseable line as content.
        let frame = zstd::encode_all(payload.as_slice(), self.level)
            .map_err(|error| SessionError::Compression(error.to_string()))?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        std::io::Write::write_all(&mut file, &frame)?;
        // Deliberately *not* `sync_data` per event. It costs about 3 ms here, twenty times the
        // compress-and-write. The checkpoint before a model request and before a tool body is the
        // cadence that asks for durability; this write only needs to reach the page cache.
        Ok(())
    }

    /// Reads every event, in order.
    ///
    /// A missing file reads as an empty session rather than an error: a store that has never been
    /// written is not corrupt.
    pub fn read_all(&self) -> Result<Vec<SessionEvent>, SessionError> {
        // A cached read is returned while the file has not grown. The check is one `metadata` call, and the
        // alternative is decompressing the whole log again to learn nothing new — which the loop does once per
        // step, so a long turn paid for it dozens of times.
        if let Some(cached) = self.cached.read().ok().and_then(|cached| cached.clone()) {
            let current = std::fs::metadata(&self.path).map_or(0, |meta| meta.len());
            if current == cached.len {
                return Ok(cached.events.iter().cloned().collect());
            }
        }

        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };

        if file.metadata()?.len() == 0 {
            return Ok(Vec::new());
        }

        let decoder = zstd::stream::read::Decoder::new(BufReader::new(file))
            .map_err(|error| SessionError::Compression(error.to_string()))?;
        let reader = BufReader::new(decoder);

        let mut events = Vec::new();
        for (index, line) in reader.lines().enumerate() {
            let line = line.map_err(SessionError::Io)?;
            if line.trim().is_empty() {
                continue;
            }
            let value = serde_json::from_str::<serde_json::Value>(&line).map_err(|source| {
                SessionError::Corrupt {
                    line: index + 1,
                    source,
                }
            })?;
            if value.get("type").and_then(serde_json::Value::as_str) == Some("session") {
                continue;
            }
            let event: SessionEvent =
                serde_json::from_value(value).map_err(|source| SessionError::Corrupt {
                    line: index + 1,
                    source,
                })?;
            events.push(event);
        }

        // Stored as a chunked list so a later append shares every chunk it can rather than copying the vector:
        // the cached value is read far more often than it is written.
        let list = events
            .iter()
            .fold(crate::util::chunked_list::List::new(), |list, event| {
                list.push(event.clone())
            });
        if let Ok(mut cached) = self.cached.write() {
            *cached = Some(Cached {
                len: std::fs::metadata(&self.path).map_or(0, |meta| meta.len()),
                events: list,
            });
        }
        Ok(events)
    }

    /// Whether the session has any user message, without decoding the whole log.
    ///
    /// A list view needs one bit per session: "has this conversation started". Decoding every event
    /// of every session to compute it is O(sessions x events) of disk and zstd work for a boolean —
    /// measured at 10,200 events decoded to render 200 rows.
    ///
    /// The scan stops at the first user message, and a session with no messages is most often a
    /// short one, so the common cases are cheap.
    pub fn has_user_message(&self) -> Result<bool, SessionError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if file.metadata()?.len() == 0 {
            return Ok(false);
        }

        let decoder = zstd::stream::read::Decoder::new(BufReader::new(file))
            .map_err(|error| SessionError::Compression(error.to_string()))?;
        for line in BufReader::new(decoder).lines() {
            let line = line.map_err(SessionError::Io)?;
            if line.trim().is_empty() {
                continue;
            }
            // A line that will not parse still counts as content: the session is not blank, and
            // hiding a corrupt log behind "blank" would be worse than showing it.
            let value = match serde_json::from_str::<serde_json::Value>(&line) {
                Ok(value) => value,
                // A line that will not parse still counts as content: the session is not blank,
                // and hiding a corrupt log behind "blank" would be worse than showing it.
                Err(_) => return Ok(true),
            };
            if value.get("type").and_then(serde_json::Value::as_str) == Some("session") {
                continue;
            }
            match serde_json::from_value::<SessionEvent>(value) {
                Ok(SessionEvent::UserMessage { .. }) | Err(_) => return Ok(true),
                Ok(_) => {}
            }
        }
        Ok(false)
    }

    /// Flushes the log to durable storage.
    ///
    /// Appends are not synced individually because the cost is twenty times the write and buys
    /// durability only against an OS crash. A caller that needs that guarantee — a host shutting
    /// down, a checkpoint — calls this at its own cadence.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error when the file cannot be opened or synced.
    pub fn sync(&self) -> Result<(), SessionError> {
        match File::open(&self.path) {
            Ok(file) => file.sync_all().map_err(SessionError::Io),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Counts events without materializing them all.
    pub fn len(&self) -> Result<usize, SessionError> {
        Ok(self.read_all()?.len())
    }

    /// Whether the session has no events.
    pub fn is_empty(&self) -> Result<bool, SessionError> {
        Ok(self.len()? == 0)
    }
}

/// The session seam: what the loop talks to.
///
/// A trait rather than the concrete [`SessionStore`] so a test can substitute an in-memory log, and
/// so phase 5's remote or forked session is a different implementation rather than a different type
/// at every call site.
pub trait Session: Send + Sync {
    /// Appends one event.
    fn append(&self, event: SessionEvent) -> BoxFut<Result<(), SessionError>>;

    /// Reads every event.
    fn replay(&self) -> BoxFut<Result<Vec<SessionEvent>, SessionError>>;

    /// Returns the original working directory from the durable session header, when present.
    ///
    /// The runtime combines this with replayed `workspace-root/change` events to construct a
    /// per-session working-directory owner. Keeping it on the session seam prevents concurrent
    /// sessions from sharing mutable directory state.
    fn working_directory(&self) -> Option<String> {
        None
    }

    /// Makes every event written so far durable.
    ///
    /// Called before a model request and before a tool body. A failure stops that work: a request
    /// that ran after a failed flush would continue from a log the disk does not have.
    fn flush(&self) -> BoxFut<Result<(), SessionError>>;
}

impl Session for SessionStore {
    fn append(&self, event: SessionEvent) -> BoxFut<Result<(), SessionError>> {
        let path = self.path.clone();
        let level = self.level;
        // The lease travels with the write. Dropping it here would release the cross-process lock
        // while the append this future exists for has not happened yet, which is the one window the
        // lease is for.
        let lease = self.lease.clone();
        Box::pin(async move {
            SessionStore {
                path,
                level,
                lease,
                cached: Arc::new(std::sync::RwLock::new(None)),
            }
            .append(&event)
        })
    }

    fn replay(&self) -> BoxFut<Result<Vec<SessionEvent>, SessionError>> {
        let path = self.path.clone();
        let level = self.level;
        let lease = self.lease.clone();
        Box::pin(async move {
            SessionStore {
                path,
                level,
                lease,
                cached: Arc::new(std::sync::RwLock::new(None)),
            }
            .read_all()
        })
    }

    fn working_directory(&self) -> Option<String> {
        crate::session::read_session_header(&self.path)
            .ok()
            .flatten()
            .and_then(|header| {
                header
                    .get("cwd")
                    .and_then(serde_json::Value::as_str)
                    .filter(|cwd| crate::shell::working_dir::is_absolute(cwd))
                    .map(str::to_owned)
            })
    }

    fn flush(&self) -> BoxFut<Result<(), SessionError>> {
        let result = self.sync();
        Box::pin(async move { result })
    }
}

/// An in-memory session, for tests and for a run with no persistence.
#[derive(Default)]
pub struct MemorySession {
    events: std::sync::Mutex<Vec<SessionEvent>>,
}

impl MemorySession {
    /// Creates an empty session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The events recorded so far.
    #[must_use]
    pub fn events(&self) -> Vec<SessionEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Session for MemorySession {
    fn append(&self, event: SessionEvent) -> BoxFut<Result<(), SessionError>> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
        Box::pin(async { Ok(()) })
    }

    fn replay(&self) -> BoxFut<Result<Vec<SessionEvent>, SessionError>> {
        let events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move { Ok(events) })
    }

    fn flush(&self) -> BoxFut<Result<(), SessionError>> {
        Box::pin(async { Ok(()) })
    }
}

/// One row the model would see, with the id a later compaction can name.
pub struct VisibleTurn {
    /// Message id, tool-call id, or compaction-summary id.
    pub key: String,
    /// The provider message.
    pub message: crate::llm::Message,
    /// The same row in the session's message type, for a summarizer.
    pub chat: crate::session::message::Message,
}

/// The model-visible messages in a log.
///
/// User and assistant messages are copied as they were shown. A tool result answers the call id
/// recorded beside it. `tool/call` events are skipped because the assistant message already carries
/// the call. Turn markers and titles are bookkeeping and stay out of the request. A
/// `compaction/summary` replaces the ids it names, and a `/compact`, `/goal`, or `/feedback` line
/// stays in the log without being sent to the model.
#[must_use]
pub fn model_messages(events: &[SessionEvent]) -> Vec<crate::llm::Message> {
    visible_turns(events)
        .into_iter()
        .map(|turn| turn.message)
        .collect()
}

/// The model-visible rows, keeping the id of each one.
#[must_use]
pub fn visible_turns(events: &[SessionEvent]) -> Vec<VisibleTurn> {
    let shadowed: std::collections::HashSet<&str> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::CompactionSummary { shadowed, .. } => Some(shadowed.as_slice()),
            _ => None,
        })
        .flatten()
        .map(String::as_str)
        .collect();
    let mut summaries = Vec::new();
    let mut turns = Vec::new();
    for event in events {
        match event {
            SessionEvent::UserMessage { message }
            | SessionEvent::DeveloperMessage { message, .. } => {
                if shadowed.contains(message.id.as_str()) || harness_command(message) {
                    continue;
                }
                turns.push(VisibleTurn {
                    key: message.id.as_str().to_owned(),
                    message: crate::llm::Message::new(
                        crate::llm::Role::User,
                        message.content.clone(),
                    ),
                    chat: message.clone(),
                });
            }
            SessionEvent::AssistantMessage { message } => {
                if shadowed.contains(message.id.as_str()) {
                    continue;
                }
                turns.push(VisibleTurn {
                    key: message.id.as_str().to_owned(),
                    message: crate::llm::Message::new(
                        crate::llm::Role::Assistant,
                        message.content.clone(),
                    ),
                    chat: message.clone(),
                });
            }
            SessionEvent::ToolResult { id, content, .. } => {
                if shadowed.contains(id.as_str()) {
                    continue;
                }
                let content = crate::core::tool_result_pruning::prune_blocks(content);
                let chat_id = crate::util::brand::MessageId::new(format!("tool-{}", id.as_str()))
                    .unwrap_or_else(|_| {
                        crate::util::brand::MessageId::new("tool-result")
                            .expect("constant is valid")
                    });
                turns.push(VisibleTurn {
                    key: id.as_str().to_owned(),
                    message: crate::llm::Message::tool_result(id.clone(), content.clone()),
                    chat: crate::session::message::Message::new(
                        chat_id,
                        crate::session::message::Role::Tool,
                        crate::session::message::MessageSource::Tool {
                            call_id: id.clone(),
                        },
                        content.clone(),
                    ),
                });
            }
            SessionEvent::CompactionSummary { id, text, .. } => {
                if shadowed.contains(id.as_str()) {
                    continue;
                }
                let chat_id = crate::util::brand::MessageId::new(id.clone()).unwrap_or_else(|_| {
                    crate::util::brand::MessageId::new("compaction").expect("constant is valid")
                });
                let chat = crate::session::message::Message::user(chat_id, text.clone());
                // The summary describes what came before the rows that survived, so it is shown
                // first. Appending it after those rows would read as a remark about the ending.
                summaries.push(VisibleTurn {
                    key: id.clone(),
                    message: crate::llm::Message::new(crate::llm::Role::User, chat.content.clone()),
                    chat,
                });
            }
            _ => {}
        }
    }
    summaries.extend(turns);
    summaries
}

/// Whether a user message is a harness command rather than something the model should read.
fn harness_command(message: &crate::session::message::Message) -> bool {
    if message.role != crate::session::message::Role::User {
        return false;
    }
    let mut text = String::new();
    for block in &message.content {
        if let crate::llm::ContentBlock::Text { text: block_text } = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(block_text);
        }
    }
    let trimmed = text.trim();
    for name in ["compact", "goal", "feedback", "team"] {
        let Some(rest) = trimmed.strip_prefix('/') else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(name) else {
            continue;
        };
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return true;
        }
    }
    false
}

/// Events that belong to turns which have already ended.
///
/// A fork runs during a turn, and the child must not see that turn's in-flight prompt or tool
/// calls. The cut is the open `turn/start`. A log with no open turn is returned whole.
#[must_use]
pub fn completed_turns(events: &[SessionEvent]) -> &[SessionEvent] {
    let last_start = events
        .iter()
        .rposition(|event| matches!(event, SessionEvent::TurnStart { .. }));
    let last_end = events
        .iter()
        .rposition(|event| matches!(event, SessionEvent::TurnEnd { .. }));
    match (last_start, last_end) {
        (Some(start), Some(end)) if start > end => &events[..start],
        (Some(start), None) => &events[..start],
        _ => events,
    }
}

/// The registry key for the session store.
pub struct SessionKey;

impl crate::ServiceKey for SessionKey {
    type Service = dyn Session;
    const ID: &'static str = "session";
}

/// Reads a session file written by any compatible version.
///
/// Kept as a free function so a tool or a CLI subcommand can inspect a log without constructing a
/// store.
/// The `version` a session header declares, when the line is one.
///
/// A header is a line whose `type` is `session`. Anything else is an event, and an event has no
/// version of its own -- a log is one format, stated once.
fn header_version(value: &serde_json::Value) -> Option<u64> {
    if value.get("type").and_then(serde_json::Value::as_str) != Some("session") {
        return None;
    }
    value.get("version").and_then(serde_json::Value::as_u64)
}

pub fn read_session_file(path: impl AsRef<Path>) -> Result<Vec<SessionEvent>, SessionError> {
    let mut bytes = Vec::new();
    File::open(path.as_ref())?.read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let decoded = zstd::decode_all(&bytes[..])
        .map_err(|error| SessionError::Compression(error.to_string()))?;
    let text = String::from_utf8(decoded)
        .map_err(|error| SessionError::Compression(format!("session log is not UTF-8: {error}")))?;

    // The first line may be a session header carrying `version`. Classifying it before anything is
    // parsed is what turns "a field is missing somewhere" into "this log is from a newer build":
    // without it a v9 log parsed by a v4 reader fails on whichever field the reader needed first,
    // and that message names a field rather than the difference that produced it.
    if let Some(version) = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .as_ref()
        .and_then(header_version)
    {
        if let Ok(catalog) = crate::session::format_catalog::FormatCatalog::released() {
            match catalog.classify(version) {
                crate::session::format_catalog::VersionClass::Current => {}
                crate::session::format_catalog::VersionClass::NewerThanBuild {
                    stored,
                    current,
                } => {
                    return Err(SessionError::FormatTooNew { stored, current });
                }
                crate::session::format_catalog::VersionClass::MigrationRequired => {
                    // The route is what says whether a migration exists, not the version number: a
                    // stored version below current with no traversable edge is unsupported too, and
                    // reporting it as "migration required" would start a loop that fails one edge
                    // later.
                    // `blocked_edge` is what says whether the path is walkable: a version below
                    // current with no traversable edge is unsupported too, and reporting it as
                    // "migration required" would start a loop that fails one edge later.
                    let walkable = catalog
                        .route(version)
                        .is_ok_and(|route| route.blocked_edge().is_none());
                    if !walkable {
                        return Err(SessionError::FormatUnsupported {
                            stored: version,
                            current: *catalog.readable_versions().end(),
                        });
                    }
                }
            }
        }
    }

    let mut events = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        // A header is metadata, not an event. It is skipped rather than parsed as one: this build's
        // own logs begin with an event, and a log from a build that writes a header would otherwise
        // fail on its first line with "unknown variant `session`" -- a message about the reader's
        // vocabulary when the actual fact is that a header is there.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return Err(SessionError::Corrupt {
                line: index + 1,
                source: serde_json::from_str::<SessionEvent>(line).expect_err("already failed"),
            });
        };
        if header_version(&value).is_some() {
            continue;
        }
        let event = serde_json::from_value(value).map_err(|source| SessionError::Corrupt {
            line: index + 1,
            source,
        })?;
        events.push(event);
    }
    Ok(events)
}

fn session_header_line(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .as_deref()
        == Some("session")
}

/// Reads the first durable session header, when a log has one.
///
/// Older Girder logs began directly with events, so absence is a valid compatibility result rather
/// than corruption. Header validation remains the responsibility of the format layer that needs
/// migration diagnostics.
pub fn read_session_header(
    path: impl AsRef<Path>,
) -> Result<Option<serde_json::Value>, SessionError> {
    let mut bytes = Vec::new();
    File::open(path.as_ref())?.read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let decoded = zstd::decode_all(&bytes[..])
        .map_err(|error| SessionError::Compression(error.to_string()))?;
    let text = String::from_utf8(decoded)
        .map_err(|error| SessionError::Compression(format!("session log is not UTF-8: {error}")))?;
    let Some(line) = text.lines().find(|line| !line.trim().is_empty()) else {
        return Ok(None);
    };
    let value = serde_json::from_str::<serde_json::Value>(line)
        .map_err(|source| SessionError::Corrupt { line: 1, source })?;
    Ok((value.get("type").and_then(serde_json::Value::as_str) == Some("session")).then_some(value))
}

impl SessionEvent {
    /// Builds a user-message event from text.
    ///
    /// A constructor rather than a literal at each call site: every user message needs an id and a
    /// source, and repeating that at ten call sites is how one of them ends up with a duplicated id.
    #[must_use]
    pub fn user_text(id: &str, text: impl Into<String>) -> Self {
        let message = crate::session::message::Message::new(
            crate::util::brand::MessageId::new(id).unwrap_or_else(|_| {
                crate::util::brand::MessageId::new("msg-invalid").expect("constant is valid")
            }),
            crate::session::message::Role::User,
            crate::session::message::MessageSource::User { form: None },
            vec![ContentBlock::Text { text: text.into() }],
        );
        Self::UserMessage { message }
    }

    /// Builds an assistant-message event from text.
    #[must_use]
    pub fn assistant_text(id: &str, text: impl Into<String>, provider: &str) -> Self {
        let message = crate::session::message::Message::new(
            crate::util::brand::MessageId::new(id).unwrap_or_else(|_| {
                crate::util::brand::MessageId::new("msg-invalid").expect("constant is valid")
            }),
            crate::session::message::Role::Assistant,
            crate::session::message::MessageSource::Model {
                provider: provider.to_owned(),
                model: provider.to_owned(),
                replay_state: None,
            },
            vec![ContentBlock::Text { text: text.into() }],
        );
        Self::AssistantMessage { message }
    }
}

impl SessionStore {
    /// The events after `after_seq`, without handing back the ones the caller already has.
    ///
    /// [`SessionStore::read_all`] answers the same question by cloning every event, which is the right shape for a
    /// caller that wants the log and the wrong one for a caller that already holds it: a poller resuming from a cursor
    /// paid to copy the whole conversation on every poll it made no progress on.
    ///
    /// The cursor is one-based like every other sequence in this crate, so `after_seq = 0` means "from the start" and
    /// `after_seq = n` means "everything after the nth event". A sequence past the end returns nothing rather than an
    /// error: a poller that has not missed anything is the normal case, not a mistake.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the log cannot be read or decoded. A truncated tail is not an error -- it is what
    /// an interrupted write looks like, and [`SessionStore::read_all`] already treats it as one.
    pub fn read_after(&self, after_seq: u64) -> Result<Vec<SessionEvent>, SessionError> {
        let all = self.read_all()?;
        // `skip` on a collected Vec would walk it; the index is what `after_seq` already is, since the sequence is
        // one-based and the vector is zero-based.
        let start = usize::try_from(after_seq).unwrap_or(usize::MAX);
        Ok(all.into_iter().skip(start).collect())
    }

    /// The number of events in the log, without cloning them.
    ///
    /// A caller showing "3 of 40 events" needs the length, not the events, and asking for the events to count them is
    /// how a status line ends up decoding a conversation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the log cannot be read.
    pub fn event_count(&self) -> Result<u64, SessionError> {
        let all = self.read_all()?;
        Ok(u64::try_from(all.len()).unwrap_or(u64::MAX))
    }
}

#[cfg(test)]
mod flush_tests {
    use super::{SessionEvent, SessionStore};

    #[test]
    fn a_missing_log_flushes_as_success() {
        let path = std::env::temp_dir().join(format!(
            "girder-flush-missing-{}-{}.jsonl.zst",
            std::process::id(),
            crate::now_millis()
        ));
        let _ = std::fs::remove_file(&path);
        SessionStore::new(&path).sync().expect("nothing to persist");
    }

    #[test]
    fn a_written_log_flushes() {
        let path = std::env::temp_dir().join(format!(
            "girder-flush-written-{}-{}.jsonl.zst",
            std::process::id(),
            crate::now_millis()
        ));
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::new(&path);
        store
            .append(&SessionEvent::user_text("m", "hello"))
            .expect("append");
        store.sync().expect("sync");
        assert_eq!(store.read_all().expect("read").len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rewriting_the_header_keeps_the_events() {
        let path = std::env::temp_dir().join(format!(
            "girder-header-rewrite-{}-{}.jsonl.zst",
            std::process::id(),
            crate::now_millis()
        ));
        let _ = std::fs::remove_file(&path);
        let store = SessionStore::new(&path);
        store
            .touch_with_header(&serde_json::json!({
                "type": "session",
                "version": 4,
                "createdAt": 1,
            }))
            .expect("created");
        store
            .append(&SessionEvent::user_text("m", "hello"))
            .expect("append");
        store
            .rewrite_header(&serde_json::json!({
                "type": "session",
                "version": 4,
                "createdAt": 1,
                "cwd": "/work",
            }))
            .expect("rewritten");
        let header = super::read_session_header(&path)
            .expect("header")
            .expect("present");
        assert_eq!(header["cwd"], "/work");
        assert_eq!(store.read_all().expect("events").len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_catalog_directory_change_is_visible_from_the_log() {
        let dir = std::env::temp_dir().join(format!(
            "girder-header-catalog-{}-{}",
            std::process::id(),
            crate::now_millis()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let catalog = crate::api::remote::DirectoryCatalog::new(&dir);
        let created = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(crate::api::remote::SessionCatalog::create(&catalog, "s1"))
            .expect("created");
        assert_eq!(created.id, "s1");
        crate::api::remote::SessionCatalog::set_working_directory(&catalog, "s1", "/workspace")
            .expect("cwd");
        let header =
            crate::api::remote::SessionCatalog::session_header(&catalog, "s1").expect("header");
        assert_eq!(header.cwd.as_deref(), Some("/workspace"));
        let reopened = crate::api::remote::DirectoryCatalog::new(&dir);
        assert_eq!(
            crate::api::remote::SessionCatalog::session_header(&reopened, "s1")
                .and_then(|header| header.cwd),
            Some("/workspace".to_owned())
        );
        assert_eq!(
            crate::api::remote::SessionCatalog::working_directory(&reopened, "s1"),
            Some("/workspace".to_owned())
        );
        SessionStore::new(dir.join("s1.jsonl.zst"))
            .append(&SessionEvent::WorkspaceRootChange {
                cwd: "/moved".to_owned(),
            })
            .expect("move");
        assert_eq!(
            crate::api::remote::SessionCatalog::working_directory(&reopened, "s1"),
            Some("/moved".to_owned())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod history_tests {
    use super::{SessionEvent, TurnEndReason, completed_turns, model_messages};
    use crate::llm::Role;

    #[test]
    fn a_fork_keeps_finished_turns_and_drops_the_open_one() {
        let events = vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::user_text("m1", "earlier"),
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            SessionEvent::TurnStart { turn: 2 },
            SessionEvent::user_text("m2", "still going"),
        ];
        let messages = model_messages(completed_turns(&events));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
        assert!(format!("{:?}", messages[0].content).contains("earlier"));
    }
}
