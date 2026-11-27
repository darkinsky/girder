//! Working directories and message quotes: the three upstream `packages/session` modules that had no
//! counterpart here.
//!
//! # What upstream has
//!
//! | upstream package | lines | kind | what it answers |
//! |---|---|---|---|
//! | `working-directory` (`index.ts`, `error.ts`) | 290 | **service** over a pure fold plus path rules | which directory a session is in, what a `cd` resolves to, and what the model is told when it moves |
//! | `tool-working-directory` | 43 | tool definition | one `working_directory` call: read the directory, or change it |
//! | `message-quotes` | 198, all `src/client/` | browser UI | rendering a Markdown blockquote in a user message as a quoted excerpt |
//!
//! # What this crate had
//!
//! Nothing, on all three counts: `workingDirectory` and `working_directory` did not appear anywhere in
//! `crates/`, and a user message's blockquotes were rendered as ordinary paragraphs. The pieces that
//! *were* here and are reused rather than rebuilt are [`Projection`] (the fold
//! contract), [`ToolDefinition`] (the tool registry's shape),
//! `girder_core::normalize_cache_path`'s lexical folding discipline (see [`lexical`], which re-derives
//! it rather than trimming the input) and `ChatMessage`/`MessageSource`/`ContextForm` for the
//! transition notice.
//!
//! # What is portable, and what is not
//!
//! The service is three things that only look like one:
//!
//! 1. **The rule set.** Empty path, relative path, trailing slash, `..`, a path that is a file rather
//!    than a directory — each has exactly one answer, and none of them needs a disk. [`resolve`],
//!    [`lexical`], [`is_absolute`], [`validate_change`] and [`classify`] are that rule set.
//! 2. **The fold.** `workingDirectory` is a projection over one event type: `header.cwd` at
//!    [`Seq`] zero, then the last committed directory. [`WorkingDirectoryProjection`] implements the
//!    crate's own [`Projection`] trait, and [`fold`] is the same function without the trait.
//! 3. **The I/O.** `fs.stat` on a candidate, `fs.resolve` against the current directory, and the
//!    serialisation of concurrent updates per session. [`validate_change`]/[`restore_target`] keep
//!    the *decisions* outside the I/O: an `async` caller stats the path and hands the answer in, so
//!    every branch that produces an error is testable without a filesystem. The `tokio::fs` calls in
//!    [`change`] and [`ensure`] are the only disk access in this module.
//!
//! What is **not** portable, and is deliberately absent: the Cordis `Service`/`Waterfall` wiring (the
//! `working-directory/access` event, `agent.ctx.fiber.assertActive()`, `agent.inject`), which belongs
//! to the bus, and the whole of `message-quotes`' host entry — its `apply()` is empty and the other
//! 190 lines are React components and slot registrations. One part of it is not UI at all, though:
//! [`quote_ranges`] and [`quote_text`] are the Markdown blockquote grammar the browser plugin uses to
//! decide what a quote *is*, and they are pure string functions. They are ported here, differentially
//! against the TypeScript.
//!
//! # Why the notice is a decision, not a side effect
//!
//! Upstream appends `working-directory/change` and then calls `agent.inject(notice)`, and treats the
//! second as best-effort: a failure is logged and the committed directory stands. That ordering is a
//! real contract — the state has already moved when the notice is attempted, so a caller must not
//! roll the directory back on a notice failure — so [`commit`] returns the notice as a value and the
//! caller decides where it goes. Nothing here queues a message or appends an event.
//!
//! # Why `working_directory` is not `bash`'s `cd`
//!
//! Existing shells and running processes keep their own directory. Changing the session's directory
//! changes what `read`, `write`, `glob` and the next `bash` call *start* at; it cannot move a process
//! that is already inside one. The tool description says so, because a model that assumes otherwise
//! will `cd` and then be surprised by a shell that did not move.
//!
//! # Divergences that are deliberate
//!
//! - `working-directory/change` is not a variant of [`SessionEvent`]. This crate's enum is closed and
//!   the gap module may not edit it, so the event is carried as JSON in the envelope upstream writes
//!   (`{"type":"working-directory/change","data":{"cwd":…}}`, with the flat form also accepted) and
//!   read back by [`decode_change`]. The `UserMessage` that carries the notice is still a first-class
//!   [`SessionEvent`], and that is what a projection can see today.
//! - Upstream's `isAbsolute` is `node:path`'s, i.e. platform-dependent. [`is_absolute`] accepts both
//!   POSIX absolute paths and Windows drive/UNC paths, so a log written on one platform is not
//!   rejected by a reader on another.
//! - Upstream serialises concurrent updates per session with a promise chain. The tool here owns one
//!   [`WorkingDirectory`] behind a `tokio::sync::Mutex`, which is the same guarantee for the same
//!   single-owner reason.

#![allow(dead_code)]

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};

use girder_core::{
    ChatMessage, ChatRole, ContentBlock, ContextForm, MessageId, MessageSource, Projection,
    ProjectionSeq as Seq, SessionEvent, ToolDefinition, ToolError, ToolResult, ToolRunContext,
    ToolValue,
};

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// Upstream `WORKING_DIRECTORY_ERROR_CODES`, in declaration order.
///
/// The order is part of the contract only in the sense that it is stable: a caller that logs the
/// index of a code keeps meaning the same failure across releases.
pub const WORKING_DIRECTORY_ERROR_CODES: [&str; 3] = [
    "WORKING_DIRECTORY_INVALID",
    "WORKING_DIRECTORY_NOT_FOUND",
    "WORKING_DIRECTORY_UNAVAILABLE",
];

/// One machine-routable working-directory failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkingDirectoryErrorCode {
    /// The requested directory was empty or otherwise malformed. The caller's request is at fault.
    Invalid,
    /// The requested directory does not exist, or is not a directory. The caller picks another path.
    NotFound,
    /// Neither the current directory nor the original project exists, or the owner was disposed.
    Unavailable,
}

impl WorkingDirectoryErrorCode {
    /// The stable code string a log or an RPC response carries.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => WORKING_DIRECTORY_ERROR_CODES[0],
            Self::NotFound => WORKING_DIRECTORY_ERROR_CODES[1],
            Self::Unavailable => WORKING_DIRECTORY_ERROR_CODES[2],
        }
    }
}

impl std::str::FromStr for WorkingDirectoryErrorCode {
    type Err = ();

    /// The code a reader gets back from a string, for a client parsing a failure it did not raise.
    ///
    /// A `FromStr` impl rather than an inherent `from_str`, because the inherent name shadows the
    /// trait method and makes the type harder to use generically for no gain.
    fn from_str(code: &str) -> Result<Self, Self::Err> {
        WORKING_DIRECTORY_ERROR_CODES
            .iter()
            .position(|candidate| *candidate == code)
            .map(|index| match index {
                0 => Self::Invalid,
                1 => Self::NotFound,
                _ => Self::Unavailable,
            })
            .ok_or(())
    }
}

impl fmt::Display for WorkingDirectoryErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A working-directory failure carrying a stable [`WorkingDirectoryErrorCode`].
///
/// The message names the directory and the safe next step, because the model reads it: upstream's
/// `directory does not exist: "/x"` is instruction enough to try a path that does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingDirectoryError {
    /// The failure class consumers route on.
    pub code: WorkingDirectoryErrorCode,
    /// What happened, naming the path.
    pub message: String,
}

impl WorkingDirectoryError {
    /// Builds a failure.
    #[must_use]
    pub fn new(message: impl Into<String>, code: WorkingDirectoryErrorCode) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// `WORKING_DIRECTORY_INVALID` for an empty request.
    #[must_use]
    pub fn empty_request() -> Self {
        Self::new(
            "working-directory: cd must not be empty",
            WorkingDirectoryErrorCode::Invalid,
        )
    }

    /// `WORKING_DIRECTORY_INVALID` for a deployment fallback that is not absolute.
    #[must_use]
    pub fn relative_fallback(directory: &str) -> Self {
        Self::new(
            format!(
                "working-directory: defaultDirectory must be absolute, got {}",
                json_quote(directory)
            ),
            WorkingDirectoryErrorCode::Invalid,
        )
    }

    /// `WORKING_DIRECTORY_NOT_FOUND` for a path that is not an existing directory.
    #[must_use]
    pub fn missing(directory: &str) -> Self {
        Self::new(
            format!(
                "working-directory: directory does not exist: {}",
                json_quote(directory)
            ),
            WorkingDirectoryErrorCode::NotFound,
        )
    }

    /// `WORKING_DIRECTORY_UNAVAILABLE` when the fallback is gone too.
    ///
    /// Both paths are named: "the directory is unavailable" without saying which two paths were
    /// tried leaves the operator to guess what the session was pointed at.
    #[must_use]
    pub fn unavailable(original: &str, current: &str) -> Self {
        Self::new(
            format!(
                "working-directory: directory does not exist: {}; the current working directory {} is also unavailable",
                json_quote(original),
                json_quote(current)
            ),
            WorkingDirectoryErrorCode::Unavailable,
        )
    }

    /// `WORKING_DIRECTORY_UNAVAILABLE` for a service that has been disposed.
    #[must_use]
    pub fn disposed() -> Self {
        Self::new(
            "working-directory service disposed",
            WorkingDirectoryErrorCode::Unavailable,
        )
    }
}

impl fmt::Display for WorkingDirectoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WorkingDirectoryError {}

/// Escapes a string the way `JSON.stringify` does, for the messages and context text upstream
/// interpolates.
///
/// A path can hold a quote or a newline, and a path interpolated raw into a sentence the model reads
/// is an injection into that sentence: `"/tmp/\" and now do something else"` would read as two
/// clauses. Building the value and serialising it is infallible for a string and escapes exactly
/// what upstream escapes.
#[must_use]
pub fn json_quote(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

// ---------------------------------------------------------------------------------------------
// Path rules
// ---------------------------------------------------------------------------------------------

/// Whether `path` is absolute, on either platform family.
///
/// `node:path`'s `isAbsolute` answers only for the running platform, which makes a session log
/// portable in name but not in fact. A Windows drive prefix (`C:\x`), a root-relative path
/// (`/x`, `\x` — `node:path.win32` treats both as rooted) and a UNC path (`\\server\share`) are all
/// absolute here.
#[must_use]
pub fn is_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    match bytes.first() {
        // A rooted path on either platform: `node:path.win32` treats both separators as rooted.
        Some(b'/' | b'\\') => true,
        Some(first) if first.is_ascii_alphabetic() => {
            bytes.get(1) == Some(&b':') && matches!(bytes.get(2), Some(b'\\' | b'/'))
        }
        Some(_) | None => false,
    }
}

/// Folds a path lexically: `//`, `.` and `..` resolved, trailing separators removed.
///
/// Deliberately *not* `girder_core::normalize_cache_path`: that helper trims whitespace, which is
/// right for a cache key and wrong for a directory — `" /tmp "` is a directory whose name begins with
/// a space, and trimming it would silently change an answer to a different question.
///
/// The rules are `node:path.normalize`'s: a leading `/` is kept and collapses to `/`;
/// `..` at the root cannot ascend and stays `/`; a leading `..` of a relative path is kept; an empty
/// result is `.` (for a relative path) or `/` (for an absolute one).
#[must_use]
pub fn lexical(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // `..` pops a real segment when there is one. At the root there is nothing to pop and
                // the root is the answer, so an absolute path discards it; a relative path keeps the
                // `..` it cannot resolve, because dropping it would name a different directory.
                match segments.last() {
                    Some(&last) if last != ".." => {
                        segments.pop();
                    }
                    Some(_) => segments.push(".."),
                    None if !absolute => segments.push(".."),
                    None => {}
                }
            }
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

/// Resolves `request` against `cwd` without touching a disk.
///
/// This is `path.resolve`'s shape and only that shape: an absolute request is folded on its own, a
/// relative one is appended to the current directory and folded. It does **not** resolve symlinks —
/// upstream's `fs.resolve` is the one that does, and a caller that needs the real path asks the
/// filesystem for it (see [`change`], which canonicalises only after the path exists).
#[must_use]
pub fn resolve(cwd: &str, request: &str) -> String {
    if is_absolute(request) {
        return lexical(request);
    }
    let joined = if cwd.ends_with('/') {
        format!("{cwd}{request}")
    } else {
        format!("{cwd}/{request}")
    };
    lexical(&joined)
}

/// What a filesystem inspection found at a candidate directory.
///
/// A value type rather than a `std::io::Error` so the decision below is testable without a disk: the
/// `async` wrappers in this module translate `tokio::fs::metadata` into one of these and hand it over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inspection {
    /// The path exists and is a directory: the safe answer.
    Directory,
    /// The path exists and is something else — a file, a socket, a device.
    NotADirectory,
    /// The path does not exist, or cannot be inspected (a permission failure is the same answer to
    /// the caller's question: this is not a directory it may enter).
    Missing,
}

impl Inspection {
    /// Classifies real metadata.
    #[must_use]
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        if metadata.is_dir() {
            Self::Directory
        } else {
            Self::NotADirectory
        }
    }

    /// The inspection an `io::Result` implies.
    #[must_use]
    pub fn from_result(result: std::io::Result<std::fs::Metadata>) -> Self {
        match result {
            Ok(metadata) => Self::of(&metadata),
            Err(_) => Self::Missing,
        }
    }
}

/// The pure decision behind `set`: what a request means, and what to reject before any I/O.
///
/// Returns the resolved directory that should be inspected. Upstream splits this exactly here —
/// `path.length === 0` is rejected before `fs.resolve` — because an empty request is a caller bug,
/// not a filesystem answer, and reporting it as `NOT_FOUND` would send the model looking for a path
/// it never named.
///
/// # Errors
///
/// [`WorkingDirectoryErrorCode::Invalid`] for an empty request.
pub fn validate_change(cwd: &str, request: &str) -> Result<String, WorkingDirectoryError> {
    if request.is_empty() {
        return Err(WorkingDirectoryError::empty_request());
    }
    // **A relative request is allowed, and a blank one is not.** The two checks are upstream's, and
    // they are in different places: `set` passes the path to `ctx.fs.resolve(path, { cwd })`, so a
    // relative path resolves against the session's directory -- but the *projection* schema is
    // `zod.string().min(1).refine(isAbsolute)`, so what gets recorded is always absolute.
    //
    // A whitespace-only request is the case that needs both: `min(1)` admits `"  "`, and resolving it
    // against the base would produce `/base/dir/  ` -- an absolute path nobody asked for. Refused
    // here rather than at the spawn, because a failure at the spawn names the directory rather than
    // the request.
    if request.trim().is_empty() {
        return Err(WorkingDirectoryError::empty_request());
    }
    let resolved = resolve(cwd, request);
    if !is_absolute(&resolved) {
        // `INVALID`, not `NOT_FOUND`: the request named something that is not a directory *path*,
        // which is a different fix from "that directory does not exist".
        return Err(WorkingDirectoryError::new(
            format!(
                "working-directory: change must be an absolute path: {}",
                json_quote(&resolved)
            ),
            WorkingDirectoryErrorCode::Invalid,
        ));
    }
    Ok(resolved)
}

/// The pure decision behind `set` and `ensure`, once the filesystem has answered.
///
/// # Errors
///
/// [`WorkingDirectoryErrorCode::NotFound`] when the candidate is not a directory, carrying `candidate`
/// rather than the request: a relative request that resolved to a missing directory is only
/// actionable if the answer says where it looked.
pub fn classify(candidate: &str, inspection: Inspection) -> Result<String, WorkingDirectoryError> {
    match inspection {
        Inspection::Directory => Ok(candidate.to_owned()),
        Inspection::NotADirectory | Inspection::Missing => {
            Err(WorkingDirectoryError::missing(candidate))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The per-session directory
// ---------------------------------------------------------------------------------------------

/// One session's effective directory and the facts needed to recover it.
///
/// Three strings, not one, because they answer three different questions and collapsing them loses
/// the recovery target:
///
/// - `original` — the directory the session started in, held by its immutable header. Recovery goes
///   here, *not* to the current directory: the point of recovery is a directory that is not the
///   broken one.
/// - `default_directory` — the deployment fallback, for a session with no original project.
/// - `recorded` — the committed directory, which is what `get()` answers.
///
/// `current` is therefore `recorded.or(original).unwrap_or(default)`, which is upstream's
/// `stateOf(session) ?? defaultDirectory` with the header initialising the state. A value type with
/// no clock and no handle: [`change`] and [`ensure`] are what touch a disk, this is what they change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingDirectory {
    default_directory: String,
    original: Option<String>,
    recorded: Option<String>,
}

impl WorkingDirectory {
    /// Builds a directory owner for a session with no recorded project.
    ///
    /// # Errors
    ///
    /// [`WorkingDirectoryErrorCode::Invalid`] when the fallback is not absolute: a relative fallback
    /// would resolve against whatever the process happened to start in, so two deployments with the
    /// same config would agree only by accident.
    pub fn new(default_directory: impl Into<String>) -> Result<Self, WorkingDirectoryError> {
        let default_directory = default_directory.into();
        if !is_absolute(&default_directory) {
            return Err(WorkingDirectoryError::relative_fallback(&default_directory));
        }
        Ok(Self {
            default_directory,
            original: None,
            recorded: None,
        })
    }

    /// Records the session header's project directory, which is also the recovery target.
    ///
    /// # Errors
    ///
    /// [`WorkingDirectoryErrorCode::Invalid`] when `original` is not absolute. The projection schema
    /// upstream declares for this value is `string.min(1).refine(isAbsolute).nullable()`, so a
    /// relative header is corrupt state rather than a directory.
    pub fn with_original(
        mut self,
        original: Option<String>,
    ) -> Result<Self, WorkingDirectoryError> {
        if let Some(original) = original.as_deref() {
            if !is_absolute(original) {
                return Err(WorkingDirectoryError::relative_fallback(original));
            }
        }
        self.original = original;
        Ok(self)
    }

    /// Seeds the committed directory, for a session resumed from a log.
    ///
    /// # Errors
    ///
    /// [`WorkingDirectoryErrorCode::Invalid`] when the seed is not absolute, for the reason the fold
    /// rejects a relative event: the projection's state may only ever be null or absolute.
    pub fn with_recorded(
        mut self,
        recorded: Option<String>,
    ) -> Result<Self, WorkingDirectoryError> {
        if let Some(recorded) = recorded.as_deref() {
            if !is_absolute(recorded) {
                return Err(WorkingDirectoryError::relative_fallback(recorded));
            }
        }
        self.recorded = recorded;
        Ok(self)
    }

    /// The deployment fallback, which is also what an unstarted session reports.
    #[must_use]
    pub fn default_directory(&self) -> &str {
        &self.default_directory
    }

    /// The committed directory, without any filesystem I/O.
    ///
    /// This is upstream's `get()`: the recorded value if one is committed, otherwise the original
    /// project, otherwise the deployment fallback.
    #[must_use]
    pub fn current(&self) -> &str {
        match (self.recorded.as_deref(), self.original.as_deref()) {
            (Some(recorded), _) => recorded,
            (None, Some(original)) => original,
            (None, None) => &self.default_directory,
        }
    }

    /// Whether a directory has been committed by an event rather than inferred from the header.
    #[must_use]
    pub fn is_recorded(&self) -> bool {
        self.recorded.is_some()
    }

    /// Where recovery goes: the original project, or the deployment fallback when there is none.
    #[must_use]
    pub fn restore_target(&self) -> &str {
        self.original.as_deref().unwrap_or(&self.default_directory)
    }

    /// Commits a directory and returns the notice that describes the move.
    ///
    /// Renaming the directory it is already in is not a transition: upstream's `commit` returns early
    /// when `previous === directory`, and a notice saying "the working directory changed from X to X"
    /// would be noise the model pays tokens for.
    ///
    /// The notice is the *return value* rather than something queued here, so a caller can decide
    /// what a delivery failure means — upstream warns and keeps the committed state, which is the
    /// behaviour a caller reproduces by ignoring the value.
    pub fn commit(&mut self, directory: impl Into<String>, recovered: bool) -> Option<Notice> {
        let directory = directory.into();
        let previous = self.current().to_owned();
        if previous == directory {
            self.recorded = Some(directory);
            return None;
        }
        self.recorded = Some(directory.clone());
        Some(Notice {
            previous,
            directory,
            recovered,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Notices: the model-visible half of a change
// ---------------------------------------------------------------------------------------------

/// One committed directory transition, and the sentence the model is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// The directory the session was in.
    pub previous: String,
    /// The directory it is in now.
    pub directory: String,
    /// Whether this was a recovery from a directory that disappeared, rather than a requested `cd`.
    pub recovered: bool,
}

impl Notice {
    /// The sentence the model reads.
    ///
    /// A recovery is worded differently on purpose: "the working directory changed from X to Y" reads
    /// as something the session *chose*, and a model that believes it chose will not go looking for
    /// the files it was working on. "X is unavailable" says what actually happened.
    #[must_use]
    pub fn text(&self) -> String {
        let previous = json_quote(&self.previous);
        let directory = json_quote(&self.directory);
        if self.recovered {
            format!(
                "The working directory {previous} is unavailable. The working directory is now {directory}."
            )
        } else {
            format!("The working directory changed from {previous} to {directory}.")
        }
    }

    /// The same sentence as a user-role message carrying [`ContextForm::Notice`].
    ///
    /// A notice, not a snapshot: it describes something that just happened and supersedes nothing. The
    /// source is the person's — upstream declares `source: { kind: 'working-directory' }` as a
    /// *source map* entry rather than a new role, because the model must see the sentence in the same
    /// place it would see a person's, and this crate's [`MessageSource`] has no such kind. The
    /// distinction is carried by [`ContextForm::Notice`] plus [`NOTICE_MARKER`] in the text, which
    /// keeps the message reconstructible without reopening the closed enum.
    #[must_use]
    pub fn message(&self, id: MessageId) -> ChatMessage {
        ChatMessage::new(
            id,
            ChatRole::User,
            MessageSource::User {
                form: Some(ContextForm::Notice),
            },
            vec![ContentBlock::Text {
                text: format!("[{}] {}", NOTICE_MARKER, self.text()),
            }],
        )
    }
}

/// The attribution marker a [`Notice`] message carries in its text.
///
/// Upstream's `MessageSourceMap` entry `'working-directory'` is what a renderer keys on; this crate's
/// [`MessageSource`] is a closed enum that this module may not extend, so the kind is written into
/// the text where a reader can find it and a session export can be re-imported without losing the
/// distinction between a person's prompt and a directory notice.
pub const NOTICE_MARKER: &str = "working-directory";

/// The system-prompt contribution name upstream registers for the current directory.
pub const CONTEXT_NAME: &str = "working-directory:current";

/// The system-prompt text: the directory, quoted, so a path with a space reads as one value.
///
/// `interpolate: false` upstream is a property of the *contribution*, not of this string; the quoting
/// is here because the model is told a literal path and must not mistake a `$HOME` in a directory
/// name for a variable.
#[must_use]
pub fn context_text(cwd: &str) -> String {
    format!("Current working directory: {}.", json_quote(cwd))
}

// ---------------------------------------------------------------------------------------------
// The projection
// ---------------------------------------------------------------------------------------------

/// The event `type` this module folds, matching upstream's `SessionEventMap` key.
pub const CHANGE_EVENT_TYPE: &str = "working-directory/change";

/// The projection key, which is also its field name in a session view.
pub const PROJECTION_KEY: &str = "workingDirectory";

/// The state version upstream declares for this projection.
pub const PROJECTION_STATE_VERSION: u64 = 1;

/// One committed `working-directory/change`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeEvent {
    /// The absolute directory that subsequent directory-based operations use.
    pub cwd: String,
}

/// Why a `working-directory/change` payload could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChangeDecodeError {
    /// The payload carried no `cwd`, or the field was not a string.
    #[error("working-directory/change carries no string `cwd`")]
    Missing,
    /// `cwd` was the empty string, which is not a directory.
    #[error("working-directory/change carries an empty `cwd`")]
    Empty,
    /// `cwd` was relative. The projection's schema is `absolute | null`, so a relative value is
    /// corrupt state: accepting it would put a directory in the fold that depends on where the
    /// *reader* is standing.
    #[error("working-directory/change carries a relative `cwd`: {requested}")]
    Relative {
        /// The value that was rejected.
        requested: String,
    },
}

/// Reads one event envelope as a `working-directory/change`, if it is one.
///
/// Both envelopes are accepted: the nested one upstream writes
/// (`{"type":…,"data":{"cwd":…}}`) and the flat one this crate's own event encoding would produce
/// (`{"type":…,"cwd":…}`). Accepting only one of them would make the fold depend on which writer
/// produced the log, which is exactly the kind of drift a replay cannot detect.
///
/// `Ok(None)` means "not this event", which is the common case in a log where each projection reads
/// a handful of types.
///
/// # Errors
///
/// [`ChangeDecodeError`] when the type matches but the payload is unusable.
pub fn decode_change(root: &Value) -> Result<Option<ChangeEvent>, ChangeDecodeError> {
    let Some(object) = root.as_object() else {
        return Ok(None);
    };
    if object.get("type").and_then(Value::as_str) != Some(CHANGE_EVENT_TYPE) {
        return Ok(None);
    }
    let payload = object.get("data").and_then(Value::as_object);
    let cwd = payload
        .and_then(|data| data.get("cwd"))
        .or_else(|| object.get("cwd"))
        .and_then(Value::as_str)
        .ok_or(ChangeDecodeError::Missing)?;
    if cwd.is_empty() {
        return Err(ChangeDecodeError::Empty);
    }
    if !is_absolute(cwd) {
        return Err(ChangeDecodeError::Relative {
            requested: cwd.to_owned(),
        });
    }
    Ok(Some(ChangeEvent {
        cwd: cwd.to_owned(),
    }))
}

/// Writes one `working-directory/change` in the flat form [`decode_change`] reads back.
///
/// # Errors
///
/// [`ChangeDecodeError`] when `cwd` is empty or relative: refusing to write what the fold would refuse
/// to read keeps a log from containing an event its own reader rejects.
pub fn encode_change(cwd: &str) -> Result<Value, ChangeDecodeError> {
    if cwd.is_empty() {
        return Err(ChangeDecodeError::Empty);
    }
    if !is_absolute(cwd) {
        return Err(ChangeDecodeError::Relative {
            requested: cwd.to_owned(),
        });
    }
    Ok(json!({ "type": CHANGE_EVENT_TYPE, "cwd": cwd }))
}

/// The committed directory a session is in, from its header and its log.
///
/// `None` means "no directory recorded", which is the projection's `null`: a session that never moved
/// has a header and no event, so the answer comes from the header, and a session with neither has no
/// answer at all rather than a guessed one.
#[must_use]
pub fn fold(header_cwd: Option<&str>, changes: &[ChangeEvent]) -> Option<String> {
    changes
        .last()
        .map(|change| change.cwd.clone())
        .or_else(|| header_cwd.map(str::to_owned))
}

/// [`fold`] over raw envelopes, skipping anything that is not a valid change.
///
/// Used where the log is already JSON and a decode failure is not actionable — a projection replaying
/// a log it did not write. A caller that needs to know about corrupt state uses [`decode_change`]
/// directly.
#[must_use]
pub fn fold_envelopes(header_cwd: Option<&str>, events: &[Value]) -> Option<String> {
    let mut state = header_cwd.map(str::to_owned);
    for event in events {
        if let Ok(Some(change)) = decode_change(event) {
            state = Some(change.cwd);
        }
    }
    state
}

/// The `workingDirectory` projection, as the crate's own [`Projection`].
///
/// `init()` is `null` because the trait's `init` has no header to consult; upstream's `init` is
/// `header => header.cwd ?? null`. [`Self::init_from_header`] is that function, and a host that has the
/// header calls it instead of `init`.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkingDirectoryProjection;

impl WorkingDirectoryProjection {
    /// Upstream's `init`: the header's directory, or `null`.
    #[must_use]
    pub fn init_from_header(header_cwd: Option<&str>) -> Value {
        header_cwd.map_or(Value::Null, |cwd| Value::String(cwd.to_owned()))
    }

    /// Upstream's `apply`, as a value transform: a change event replaces the state, anything else
    /// leaves it alone.
    ///
    /// The state is passed through unchanged for an unrelated event, including one that fails to
    /// decode: a corrupt event this projection does not own is not a reason to lose the state it has.
    #[must_use]
    pub fn apply_state(state: Value, event: &SessionEvent) -> Value {
        if let SessionEvent::WorkspaceRootChange { cwd } = event {
            if is_absolute(cwd) {
                return Value::String(cwd.clone());
            }
        }
        match change_of(event) {
            Some(cwd) => Value::String(cwd),
            None => state,
        }
    }
}

impl Projection for WorkingDirectoryProjection {
    fn key(&self) -> &'static str {
        PROJECTION_KEY
    }

    fn init(&self) -> Value {
        Value::Null
    }

    fn apply(&self, state: Value, event: &SessionEvent, _seq: Seq) -> Value {
        Self::apply_state(state, event)
    }

    fn in_summary(&self) -> bool {
        // A path is bytes, not a transcript: it belongs in a summary, unlike the message projections
        // that opt out because their state is the whole log.
        true
    }
}

/// The directory a [`SessionEvent`] notice commits, when it carries the legacy form.
///
/// Durable `workspace-root/change` events are handled directly by
/// [`WorkingDirectoryProjection::apply_state`]. This parser remains for older logs that only
/// contain the notice text.
#[must_use]
pub fn change_of(event: &SessionEvent) -> Option<String> {
    let SessionEvent::UserMessage { message } = event else {
        return None;
    };
    if !matches!(
        message.source,
        MessageSource::User {
            form: Some(ContextForm::Notice)
        }
    ) {
        return None;
    }
    let text = message.text();
    let rest = text.strip_prefix(&format!("[{NOTICE_MARKER}] "))?;
    parse_notice_directory(rest)
}

/// Extracts the directory a [`Notice::text`] sentence names.
///
/// Exists so the round trip is closed: a notice written by [`Notice::text`] can be read back by a
/// projection or a migration without re-deriving the sentence.
fn parse_notice_directory(text: &str) -> Option<String> {
    // Both wordings end with the destination quoted last — `… changed from "A" to "B".` and
    // `… "A" is unavailable. The working directory is now "B".` — so the last JSON string literal in
    // the sentence is the directory. Parsing the literal rather than splitting on `"` keeps a path
    // that itself contains a quote correct: `json_quote` escapes it, and `serde_json` unescapes it.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<usize> = None;
    let mut escaped = false;
    for (index, c) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if open.is_some() => escaped = true,
            '"' => match open.take() {
                Some(start) => spans.push((start, index + 1)),
                None => open = Some(index),
            },
            _ => {}
        }
    }
    let (start, end) = spans.last().copied()?;
    serde_json::from_str(&text[start..end]).ok()
}

// ---------------------------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------------------------

/// The tool's registered name, as the model calls it.
pub const TOOL_NAME: &str = "working_directory";

/// The tool's description, which is also the contract it promises.
pub const TOOL_DESCRIPTION: &str = "Read the current working directory, or change it with cd. Relative paths use the current directory. Existing shells and running processes keep their own directories.";

/// The schema the model sees: one optional string, and no `required`.
///
/// `cd` is optional because omitting it is how the directory is *read*: upstream's `output` schema has
/// a required `cwd`, and its `parameters` has nothing required at all.
#[must_use]
pub fn tool_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "cd": {
                "type": "string",
                "description": "Existing directory to enter. Omit to read the current directory."
            }
        }
    })
}

/// The output schema: a single absolute path, and nothing else.
///
/// `additionalProperties: false` is what makes the result checkable — a tool that added a field would
/// fail validation rather than silently widen a contract a client renders.
#[must_use]
pub fn tool_output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "cwd": {
                "type": "string",
                "required": true,
                "description": "Current absolute working directory."
            }
        }
    })
}

/// Parsed `working_directory` arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolArgs {
    /// The directory to enter, or `None` to read the current one.
    pub cd: Option<String>,
}

impl ToolArgs {
    /// Parses the model's arguments.
    ///
    /// An explicit `null` is the same as an omitted `cd`: some providers serialise an absent optional
    /// field that way, and treating it as "enter the directory `null`" would fail a call that meant
    /// "read".
    ///
    /// # Errors
    ///
    /// [`WorkingDirectoryErrorCode::Invalid`] for an empty `cd` — upstream raises `ToolArgsError`
    /// with the suggestion "omit it to read the current directory", because an empty string is a model
    /// mistake it can correct from the message alone — or for a `cd` that is not a string.
    pub fn parse(value: &Value) -> Result<Self, WorkingDirectoryError> {
        let Some(cd) = value.get("cd") else {
            return Ok(Self { cd: None });
        };
        if cd.is_null() {
            return Ok(Self { cd: None });
        }
        let Some(cd) = cd.as_str() else {
            return Err(WorkingDirectoryError::new(
                "working_directory: cd must be a string",
                WorkingDirectoryErrorCode::Invalid,
            ));
        };
        if cd.is_empty() {
            return Err(WorkingDirectoryError::new(
                "cd must not be empty; omit it to read the current directory",
                WorkingDirectoryErrorCode::Invalid,
            ));
        }
        Ok(Self {
            cd: Some(cd.to_owned()),
        })
    }
}

/// Reads a candidate's kind, without following the question of *why* it failed.
async fn inspect(path: &str) -> Inspection {
    Inspection::from_result(tokio::fs::metadata(path).await)
}

/// Reads the session's directory, restoring the original project when it has disappeared.
///
/// The recovery is the interesting branch: a directory that was deleted under a long session would
/// otherwise make every later tool call fail with a path nobody can fix. Upstream restores the
/// original project and *commits* the move, so the next call starts in a directory that exists rather
/// than repeating the same failed check.
///
/// # Errors
///
/// [`WorkingDirectoryErrorCode::Unavailable`] when the original project is gone too, or
/// [`WorkingDirectoryErrorCode::Invalid`] when the original was never absolute — a configuration
/// failure that recovery cannot fix.
pub async fn ensure(state: &mut WorkingDirectory) -> Result<String, WorkingDirectoryError> {
    let current = state.current().to_owned();
    if inspect(&current).await == Inspection::Directory {
        if !state.is_recorded() {
            // The first successful check records the header's directory, which is what makes the
            // projection's state non-null without inventing a transition: `commit` returns `None`
            // because the directory did not change.
            let _ = state.commit(current.clone(), false);
        }
        return Ok(current);
    }
    let original = state.restore_target().to_owned();
    if !is_absolute(&original) {
        return Err(WorkingDirectoryError::relative_fallback(&original));
    }
    let restored = lexical(&original);
    if inspect(&restored).await != Inspection::Directory {
        return Err(WorkingDirectoryError::unavailable(&restored, &current));
    }
    let _ = state.commit(restored.clone(), true);
    Ok(restored)
}

/// Enters a directory, or fails with the reason it cannot.
///
/// A relative request is resolved lexically against the committed directory and then inspected; the
/// stored value is the canonical path the filesystem reports, so a symlinked directory is recorded as
/// the path a later `read` will actually see. Upstream does the same with `fs.processPath` after
/// `fs.resolve`.
///
/// # Errors
///
/// [`WorkingDirectoryErrorCode::Invalid`] for an empty request,
/// [`WorkingDirectoryErrorCode::NotFound`] when the resolved path is not an existing directory.
pub async fn change(
    state: &mut WorkingDirectory,
    request: &str,
) -> Result<String, WorkingDirectoryError> {
    let candidate = validate_change(state.current(), request)?;
    if inspect(&candidate).await != Inspection::Directory {
        return Err(WorkingDirectoryError::missing(&candidate));
    }
    let canonical = tokio::fs::canonicalize(&candidate)
        .await
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or(candidate);
    let _ = state.commit(canonical.clone(), false);
    Ok(canonical)
}

/// Builds the `working_directory` tool over one shared directory owner.
///
/// The owner is shared and locked rather than cloned per call: two concurrent `working_directory`
/// calls in one step must not each read the old directory, resolve against it, and commit — one of
/// them would lose. Upstream serialises per session with a promise chain for the same reason.
#[must_use]
pub fn working_directory_tool(state: Arc<tokio::sync::Mutex<WorkingDirectory>>) -> ToolDefinition {
    ToolDefinition::new(
        TOOL_NAME,
        TOOL_DESCRIPTION,
        tool_parameters(),
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            #[cfg(test)]
            let state = {
                let _ = ctx;
                Arc::clone(&state)
            };
            #[cfg(not(test))]
            let state = ctx.working_directory.unwrap_or_else(|| Arc::clone(&state));
            Box::pin(async move {
                let value = execute(&args, &state)
                    .await
                    .map_err(|error| ToolError::Failed {
                        tool: TOOL_NAME.to_owned(),
                        message: error.to_string(),
                    })?;
                let cwd = value.get("cwd").and_then(Value::as_str).unwrap_or_default();
                Ok(tool_result(cwd))
            })
        }),
    )
}

/// Runs one `working_directory` call against an owned directory, without the tool registry.
///
/// Split from [`working_directory_tool`] so the behaviour is testable by awaiting it: a registry wraps
/// the same function, and a test that had to build a registry to check that `cd ..` resolves would be
/// testing the registry.
///
/// # Errors
///
/// Whatever [`ensure`] or [`change`] returns, plus the argument failures [`ToolArgs::parse`] reports.
pub async fn execute(
    args: &Value,
    state: &Arc<tokio::sync::Mutex<WorkingDirectory>>,
) -> Result<Value, WorkingDirectoryError> {
    let args = ToolArgs::parse(args)?;
    let mut state = state.lock().await;
    let cwd = match args.cd {
        Some(request) => change(&mut state, &request).await?,
        None => ensure(&mut state).await?,
    };
    Ok(json!({ "cwd": cwd }))
}

/// The tool's result for a directory: the path as text, and the same path as the canonical value.
///
/// The text is the bare path rather than a JSON object because upstream's `render` is
/// `value.cwd` — a model that reads `{"cwd":"/x"}` has to parse JSON to find one string, and a
/// provider that renders the text block verbatim would show it braces and all.
#[must_use]
pub fn tool_result(cwd: &str) -> ToolResult {
    ToolResult {
        content: vec![ContentBlock::Text {
            text: cwd.to_owned(),
        }],
        value: Some(ToolValue::Json(json!({ "cwd": cwd }))),
        is_error: false,
    }
}

/// Whether `path` names a directory, synchronously, for a caller that is not in an async context.
#[must_use]
pub fn is_directory(path: impl AsRef<Path>) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

// ---------------------------------------------------------------------------------------------
// Message quotes: the blockquote grammar `message-quotes` recognises
// ---------------------------------------------------------------------------------------------

/// One Markdown blockquote span, as byte offsets into the original text.
///
/// Bytes, not the UTF-16 units upstream reports: a JavaScript string index is not a byte offset, and
/// a caller that slices the original with one gets a panic on the first non-ASCII message. The grammar
/// is identical; only the unit of the numbers differs, and every test pins the byte answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuoteRange {
    /// First byte of the span, including its up-to-three leading spaces.
    pub start: usize,
    /// One past the last byte, excluding the line break that ended it.
    pub end: usize,
}

impl QuoteRange {
    /// The span's text in the original message.
    #[must_use]
    pub fn slice<'a>(&self, text: &'a str) -> &'a str {
        &text[self.start..self.end]
    }
}

/// Whether `c` is whitespace by JavaScript's definition, which is what upstream's `.trim()` uses.
///
/// Rust's `char::is_whitespace` follows Unicode `White_Space`, which excludes `U+FEFF` and includes
/// nothing else the two disagree on — so a message that is exactly `"> \u{FEFF}"` is a quote to Rust's
/// `trim` and not to JavaScript's. Upstream returns no span for it, and this function is why the port
/// does the same.
#[must_use]
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Whether `text` is blank in the sense that makes a quote span not worth reporting.
#[must_use]
pub fn is_blank(text: &str) -> bool {
    text.chars().all(is_js_whitespace)
}

/// Normalises line endings: `\r\n` and a lone `\r` both become `\n`, which is upstream's
/// `/\r\n?/gu → '\n'`.
#[must_use]
pub fn normalize_newlines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                let _ = chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// One line terminator in the sense the `m` flag gives `^`: `\n`, `\r`, `U+2028`, `U+2029`.
fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// For every byte index, whether a line may start there.
///
/// The `m`-flag definition, which is JavaScript's: a match may begin at the start of the text or
/// immediately after any line terminator — including `U+2028`/`U+2029`, which Rust's own `(?m)^` does
/// not treat as terminators. That difference is visible: `"x\u{2028}> y"` is a quote upstream and
/// would not be one under Rust's `(?m)`.
fn line_start_flags(text: &str) -> Vec<bool> {
    let mut flags = vec![false; text.len() + 1];
    flags[0] = true;
    for (index, c) in text.char_indices() {
        if is_line_terminator(c) {
            flags[index + c.len_utf8()] = true;
        }
    }
    flags
}

/// The length of the blockquote marker at the start of `bytes`, or zero when there is none.
///
/// `^ {0,3}>[ \t]?`: up to three spaces, a `>`, and one optional space or tab. Four spaces is an
/// indented code block in Markdown, which is why the limit is three and not a convenience.
fn marker_len(bytes: &[u8]) -> usize {
    let mut index = 0;
    while index < bytes.len() && index < 3 && bytes[index] == b' ' {
        index += 1;
    }
    if bytes.get(index) != Some(&b'>') {
        return 0;
    }
    index += 1;
    if matches!(bytes.get(index), Some(b' ' | b'\t')) {
        index += 1;
    }
    index
}

/// The end of the run of quote lines starting at `start`, as upstream's
/// `^ {0,3}>[^\r\n]*(?:\r?\n {0,3}>[^\r\n]*)*` measures it.
///
/// The greedy `[^\r\n]*` excludes only `\r` and `\n`, so it swallows `U+2028` and `U+2029` inside a
/// "line"; the continuation, by contrast, requires a real `\r?\n`. That asymmetry is upstream's, and
/// reproducing it is why a `>` after a lone `\r` starts a *new* span rather than continuing this one.
fn quoted_body_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    loop {
        while end < bytes.len() && bytes[end] != b'\r' && bytes[end] != b'\n' {
            end += 1;
        }
        let break_at = end;
        let mut next = end;
        if bytes.get(next) == Some(&b'\r') && bytes.get(next.saturating_add(1)) == Some(&b'\n') {
            next += 2;
        } else if bytes.get(next) == Some(&b'\n') {
            next += 1;
        } else {
            return break_at;
        }
        let mut spaces = 0;
        while bytes.get(next) == Some(&b' ') && spaces < 3 {
            next += 1;
            spaces += 1;
        }
        if bytes.get(next) != Some(&b'>') {
            return break_at;
        }
        end = next + 1;
        if matches!(bytes.get(end), Some(b' ' | b'\t')) {
            end += 1;
        }
    }
}

/// Whether a `>` at `at` begins a quote span, and where that span starts.
///
/// A span starts at the `>` or at the spaces before it, provided those spaces themselves begin a line:
/// `"a > b"` has a `>` that starts nothing, and `"    > a"` has four spaces, which is code.
fn quote_start_at(bytes: &[u8], flags: &[bool], at: usize) -> Option<usize> {
    let mut start = at;
    while start > 0 && bytes[start - 1] == b' ' && at - start < 3 {
        start -= 1;
    }
    if at - start <= 3 && flags.get(start).copied().unwrap_or(false) {
        Some(start)
    } else {
        None
    }
}

/// Every non-blank blockquote span in `text`, in order.
///
/// This is upstream's `messageQuoteRanges`. An inline `>` (`a > b`) is ordinary text: only a `>` that
/// begins a line — possibly after up to three spaces — starts a span.
#[must_use]
pub fn quote_ranges(text: &str) -> Vec<QuoteRange> {
    let bytes = text.as_bytes();
    let flags = line_start_flags(text);
    let mut ranges = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let Some(offset) = bytes[index..].iter().position(|byte| *byte == b'>') else {
            break;
        };
        let at = index + offset;
        index = at + 1;
        let Some(start) = quote_start_at(bytes, &flags, at) else {
            continue;
        };
        let end = quoted_body_end(bytes, start);
        // A `>` inside a span belongs to that span: upstream resumes after the whole match, so any
        // marker on a continuation line was already consumed by the match itself.
        index = end;
        if !is_blank(&quote_text(&text[start..end])) {
            ranges.push(QuoteRange { start, end });
        }
    }
    ranges
}

/// One span's excerpt text: the blockquote markers removed, one per explicitly quoted line.
///
/// This is upstream's `quoteTextOf`. Nesting is preserved rather than flattened — `">> a"` decodes to
/// `"> a"`, one marker removed — because the excerpt is shown as literal text, and a second pass would
/// turn a quotation of a quotation into an attribution nobody made.
#[must_use]
pub fn quote_text(text: &str) -> String {
    let normalized = normalize_newlines(text);
    let bytes = normalized.as_bytes();
    let mut out = String::with_capacity(normalized.len());
    let mut index = 0usize;
    let mut at_line_start = true;
    while index < bytes.len() {
        if at_line_start {
            let marker = marker_len(&bytes[index..]);
            if marker > 0 {
                index += marker;
                at_line_start = false;
                continue;
            }
        }
        let Some(c) = normalized[index..].chars().next() else {
            break;
        };
        out.push(c);
        index += c.len_utf8();
        at_line_start = is_line_terminator(c);
    }
    out
}

/// Every quote span's decoded excerpt, in order: what a renderer shows, without the offsets.
#[must_use]
pub fn quote_excerpts(text: &str) -> Vec<String> {
    quote_ranges(text)
        .into_iter()
        .map(|range| quote_text(range.slice(text)))
        .collect()
}

/// Whether a message is entirely one quotation, which is what a "reply to selection" prompt is.
///
/// Not in upstream — a client-side convenience that the port needs because [`NOTICE_MARKER`] shares the
/// user role with real prompts and a caller has to be able to tell a quoted selection from a typed
/// one.
#[must_use]
pub fn is_single_quote(text: &str) -> bool {
    let trimmed = text.trim_end_matches(['\n', '\r']);
    let ranges = quote_ranges(text);
    ranges.len() == 1 && ranges[0].start == 0 && ranges[0].end >= trimmed.len()
}

/// The chunked-quote view a UI renders: ranges plus their decoded text, computed once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quote {
    /// Where the span is in the original message.
    pub range: QuoteRange,
    /// Its excerpt text.
    pub text: String,
}

/// [`quote_ranges`] and [`quote_text`] together, for a caller that wants both.
#[must_use]
pub fn quotes(text: &str) -> Vec<Quote> {
    quote_ranges(text)
        .into_iter()
        .map(|range| Quote {
            text: quote_text(range.slice(text)),
            range,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Inline tests for the pieces whose contract is an equality
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_rules_match_node_path_normalize() {
        assert_eq!(lexical("/a/b/"), "/a/b");
        assert_eq!(lexical("/a//b"), "/a/b");
        assert_eq!(lexical("/a/./b"), "/a/b");
        assert_eq!(lexical("/a/b/.."), "/a");
        assert_eq!(lexical("/a/b/../c"), "/a/c");
        assert_eq!(lexical("/.."), "/");
        assert_eq!(lexical("/../.."), "/");
        assert_eq!(lexical("a/../.."), "..");
        assert_eq!(lexical(""), ".");
        assert_eq!(lexical("a/"), "a");
        assert_eq!(lexical(" /a"), " /a");
    }

    #[test]
    fn resolve_joins_relative_requests_and_folds_absolute_ones() {
        assert_eq!(resolve("/work", "sub"), "/work/sub");
        assert_eq!(resolve("/work", "../other"), "/other");
        assert_eq!(resolve("/work/", "./sub"), "/work/sub");
        assert_eq!(resolve("/work", "/abs/./x"), "/abs/x");
    }

    #[test]
    fn is_absolute_accepts_both_platform_families() {
        assert!(is_absolute("/x"));
        assert!(is_absolute("\\\\server\\share"));
        assert!(is_absolute("C:\\x"));
        assert!(is_absolute("c:/x"));
        assert!(!is_absolute("x"));
        assert!(!is_absolute("./x"));
        assert!(!is_absolute("C:"));
        assert!(!is_absolute(""));
    }

    #[test]
    fn codes_round_trip() {
        for code in [
            WorkingDirectoryErrorCode::Invalid,
            WorkingDirectoryErrorCode::NotFound,
            WorkingDirectoryErrorCode::Unavailable,
        ] {
            assert_eq!(code.as_str().parse().ok(), Some(code));
        }
        assert_eq!("NOT_A_CODE".parse::<WorkingDirectoryErrorCode>().ok(), None);
    }

    #[test]
    fn workspace_root_change_ignores_relative_paths() {
        let relative = SessionEvent::WorkspaceRootChange {
            cwd: "subdir".to_owned(),
        };
        assert_eq!(
            WorkingDirectoryProjection::apply_state(
                Value::String("/original".to_owned()),
                &relative
            ),
            Value::String("/original".to_owned())
        );

        let absolute = SessionEvent::WorkspaceRootChange {
            cwd: "/moved".to_owned(),
        };
        assert_eq!(
            WorkingDirectoryProjection::apply_state(
                Value::String("/original".to_owned()),
                &absolute
            ),
            Value::String("/moved".to_owned())
        );
    }
}
