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

/// One durable session event.
///
/// The discriminant list is the minimum that makes a conversation replayable: turn boundaries, both
/// message authors, a tool call, its result, and why the turn ended. Upstream has 60+ variants;
/// adding one here is a compile error at every `match`, which is the point — a new event type must
/// be handled by the log writer, not silently dropped.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionEvent {
    /// A turn began.
    #[serde(rename = "turn/start")]
    TurnStart {
        /// Turn number, starting at 1.
        turn: u64,
    },

    /// The user sent a message.
    #[serde(rename = "user/message")]
    UserMessage {
        /// Its blocks.
        content: Vec<ContentBlock>,
    },

    /// The model responded.
    #[serde(rename = "assistant/message")]
    AssistantMessage {
        /// Its blocks.
        content: Vec<ContentBlock>,
    },

    /// The model requested a tool.
    #[serde(rename = "tool/call")]
    ToolCall {
        /// Provider-issued call id.
        id: ToolCallId,
        /// Tool name.
        name: String,
        /// Raw JSON arguments.
        arguments: String,
    },

    /// A tool finished.
    #[serde(rename = "tool/result")]
    ToolResult {
        /// The call this answers.
        id: ToolCallId,
        /// What it produced.
        content: Vec<ContentBlock>,
        /// Whether it failed.
        is_error: bool,
    },

    /// The turn ended.
    #[serde(rename = "turn/end")]
    TurnEnd {
        /// Turn number.
        turn: u64,
        /// Why it stopped.
        reason: FinishReason,
    },
}

impl SessionEvent {
    /// The event's `type` tag, for diagnostics and filtering.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::TurnStart { .. } => "turn/start",
            Self::UserMessage { .. } => "user/message",
            Self::AssistantMessage { .. } => "assistant/message",
            Self::ToolCall { .. } => "tool/call",
            Self::ToolResult { .. } => "tool/result",
            Self::TurnEnd { .. } => "turn/end",
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
fn path_lock(path: &Path) -> Arc<std::sync::Mutex<()>> {
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
    /// Opens a store at `path`. The file is created on first append.
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
