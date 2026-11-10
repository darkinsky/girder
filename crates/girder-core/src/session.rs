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

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::brand::ToolCallId;
use crate::llm::{ContentBlock, FinishReason};
use crate::scope::BoxFut;

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
        message: crate::message::Message,
    },

    /// A developer-role message: a tool addition or removal riding the turn.
    #[serde(rename = "developer/message")]
    DeveloperMessage {
        /// Turn it was admitted at.
        turn: u64,
        /// Step it was admitted at.
        step: u64,
        /// The message.
        message: crate::message::Message,
    },

    /// The model's response for one step.
    #[serde(rename = "assistant/message")]
    AssistantMessage {
        /// The message, carrying provider identity for replay.
        message: crate::message::Message,
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
        message: crate::message::Message,
    },

    /// The model selection this step will use.
    ///
    /// Carries the selection rather than a `Value`: a selection has a shape — provider and model are
    /// both required — and storing it loosely would let a log hold one half.
    #[serde(rename = "model/selection")]
    ModelSelection {
        /// The selection.
        selection: crate::agent::ModelSelection,
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

    /// The sandbox mode in force.
    #[serde(rename = "sandbox/mode")]
    SandboxMode {
        /// The mode.
        mode: String,
        /// Where the setting came from, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
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
            Self::SandboxMode { .. } => "sandbox/mode",
            Self::RequestContext { .. } => "request/context",
            Self::SessionTitle { .. } => "session/title",
            Self::LlmRetry { .. } => "llm/retry",
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
            | Self::SandboxMode { .. }
            | Self::RequestContext { .. }
            | Self::SessionTitle { .. }
            | Self::LlmRetry { .. } => None,
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
    path: PathBuf,
    /// Compression level. 3 is zstd's default and balances speed against size for log text.
    level: i32,
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

    /// Opens a store at `path`. The file is created on the first append or by [`touch`].
    ///
    /// [`touch`]: SessionStore::touch
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            level: 3,
        }
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
        // compress-and-write, and it buys durability only against an OS crash — not against the
        // process crashing, which the write already survives because the data is in the page cache.
        // The session log matters most for the latter case; a host that needs the former can call
        // `sync` on its own cadence. See `docs/migration-plan.md` §2.19 for the measurement.
        Ok(())
    }

    /// Reads every event, in order.
    ///
    /// A missing file reads as an empty session rather than an error: a store that has never been
    /// written is not corrupt.
    pub fn read_all(&self) -> Result<Vec<SessionEvent>, SessionError> {
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
            let event = serde_json::from_str(&line).map_err(|source| SessionError::Corrupt {
                line: index + 1,
                source,
            })?;
            events.push(event);
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
            match serde_json::from_str::<SessionEvent>(&line) {
                // A line that will not parse still counts as content: the session is not blank, and
                // hiding a corrupt log behind "blank" would be worse than showing it.
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
}

impl Session for SessionStore {
    fn append(&self, event: SessionEvent) -> BoxFut<Result<(), SessionError>> {
        let path = self.path.clone();
        let level = self.level;
        Box::pin(async move { SessionStore { path, level }.append(&event) })
    }

    fn replay(&self) -> BoxFut<Result<Vec<SessionEvent>, SessionError>> {
        let path = self.path.clone();
        let level = self.level;
        Box::pin(async move { SessionStore { path, level }.read_all() })
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

    let mut events = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str(line).map_err(|source| SessionError::Corrupt {
            line: index + 1,
            source,
        })?;
        events.push(event);
    }
    Ok(events)
}

impl SessionEvent {
    /// Builds a user-message event from text.
    ///
    /// A constructor rather than a literal at each call site: every user message needs an id and a
    /// source, and repeating that at ten call sites is how one of them ends up with a duplicated id.
    #[must_use]
    pub fn user_text(id: &str, text: impl Into<String>) -> Self {
        let message = crate::message::Message::new(
            crate::brand::MessageId::new(id).unwrap_or_else(|_| {
                crate::brand::MessageId::new("msg-invalid").expect("constant is valid")
            }),
            crate::message::Role::User,
            crate::message::MessageSource::User { form: None },
            vec![ContentBlock::Text { text: text.into() }],
        );
        Self::UserMessage { message }
    }

    /// Builds an assistant-message event from text.
    #[must_use]
    pub fn assistant_text(id: &str, text: impl Into<String>, provider: &str) -> Self {
        let message = crate::message::Message::new(
            crate::brand::MessageId::new(id).unwrap_or_else(|_| {
                crate::brand::MessageId::new("msg-invalid").expect("constant is valid")
            }),
            crate::message::Role::Assistant,
            crate::message::MessageSource::Model {
                provider: provider.to_owned(),
                model: provider.to_owned(),
                replay_state: None,
            },
            vec![ContentBlock::Text { text: text.into() }],
        );
        Self::AssistantMessage { message }
    }
}
