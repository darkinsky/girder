//! Terminal sessions: a shell that survives between calls.
//!
//! A one-shot command is a fresh process with no state: no `cd`, no exported variable, no history. A terminal
//! is the opposite — the working directory and environment persist, so a second command sees what the first
//! did.
//!
//! # Why a terminal is not a job
//!
//! A job runs one command and reports its output. A terminal holds a shell whose *input* is a stream of
//! commands, and whose output is interleaved with prompts and escape sequences a person's terminal would
//! interpret. Treating one as the other would either lose the shell's state or hand a model raw escape codes.
//!
//! # Why the size is part of the session
//!
//! A program that draws a table asks the terminal how wide it is. A session with no declared size gets answers
//! from whatever the host defaults to, which changes between systems and makes output that a test cannot
//! reproduce.
//!
//! # Why the transcript is bounded
//!
//! A terminal that has been open for an hour holds everything ever printed. A caller that read all of it would
//! fill a context window with a scrollback nobody wrote for it, so the retained transcript is bounded and the
//! bound is the caller's.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use crate::retention::Omitted;
use crate::scope::BoxFut;

/// The dimensions of a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    /// Columns.
    pub columns: u16,
    /// Rows.
    pub rows: u16,
}

impl Size {
    /// The size a headless session uses.
    ///
    /// 80x24 rather than the host's real size: a session that inherited whatever the launching process had
    /// would produce output a test cannot reproduce, and a program that draws a table would be as wide as
    /// somebody's window happened to be.
    #[must_use]
    pub fn headless() -> Self {
        Self {
            columns: 80,
            rows: 24,
        }
    }

    /// Builds a size.
    ///
    /// # Errors
    ///
    /// Returns `None` for a zero dimension: a terminal with no columns has nothing to print into, and a program
    /// that divided by it would fail in its own way rather than in one place.
    #[must_use]
    pub fn new(columns: u16, rows: u16) -> Option<Self> {
        (columns > 0 && rows > 0).then_some(Self { columns, rows })
    }
}

impl Default for Size {
    fn default() -> Self {
        Self::headless()
    }
}

/// Where a terminal is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not started.
    Stopped,
    /// Running.
    Running,
    /// Exited, with the code the shell reported.
    Exited {
        /// The exit code, or `None` when it was signalled.
        code: Option<i32>,
    },
    /// Failed to start.
    Failed {
        /// Why.
        reason: String,
    },
}

impl Status {
    /// Whether the terminal can accept input.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    /// The wire name.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::Exited { .. } => "exited",
            Self::Failed { .. } => "failed",
        }
    }
}

/// The retained transcript.
///
/// Bounded because a terminal open for an hour holds everything ever printed, and a caller that read all of it
/// would fill a context window with scrollback nobody wrote for it.
#[derive(Debug)]
pub struct Transcript {
    chunks: VecDeque<String>,
    /// Total bytes ever appended, which is also the offset of the next one.
    total: u64,
    /// Bytes dropped from the front.
    dropped: u64,
    max_bytes: usize,
    retained: usize,
}

impl Transcript {
    /// Creates a transcript bounded at `max_bytes`.
    ///
    /// A zero bound is treated as one byte rather than unbounded: a transcript that grew without limit is a
    /// memory leak wearing a configuration option.
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            total: 0,
            dropped: 0,
            max_bytes: max_bytes.max(1),
            retained: 0,
        }
    }

    /// Appends one chunk, dropping the head when the bound is exceeded.
    pub fn push(&mut self, chunk: impl Into<String>) {
        let chunk = chunk.into();
        let length = chunk.len();
        self.total = self.total.saturating_add(length as u64);
        self.retained += length;
        self.chunks.push_back(chunk);
        while self.retained > self.max_bytes && self.chunks.len() > 1 {
            if let Some(removed) = self.chunks.pop_front() {
                self.retained -= removed.len();
                self.dropped = self.dropped.saturating_add(removed.len() as u64);
            }
        }
    }

    /// The retained text, in order.
    #[must_use]
    pub fn text(&self) -> String {
        self.chunks.iter().map(String::as_str).collect()
    }

    /// The offset of the next byte appended.
    #[must_use]
    pub fn end_offset(&self) -> u64 {
        self.total
    }

    /// What was dropped.
    #[must_use]
    pub fn omitted(&self) -> Omitted {
        if self.dropped == 0 {
            Omitted::None
        } else {
            Omitted::Exact {
                count: self.dropped,
            }
        }
    }

    /// How many bytes are retained.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.retained
    }
}

/// Why a terminal operation failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TerminalError {
    /// No such session.
    #[error("no terminal `{id}`")]
    Unknown {
        /// The id.
        id: String,
    },

    /// The session is not running.
    #[error("terminal `{id}` is {status}")]
    NotRunning {
        /// The id.
        id: String,
        /// Its state.
        status: &'static str,
    },

    /// The shell could not be started.
    #[error("cannot start terminal `{id}`: {reason}")]
    Spawn {
        /// The id.
        id: String,
        /// Why.
        reason: String,
    },
}

/// How to start a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    /// The id a caller uses.
    pub id: String,
    /// The shell to run, or the host default when absent.
    pub shell: Option<String>,
    /// Where to start it.
    pub cwd: Option<PathBuf>,
    /// Its dimensions.
    pub size: Size,
    /// How much transcript to retain.
    pub max_bytes: usize,
}

/// The default transcript bound.
///
/// 256 KiB: far more than a session's useful scrollback and far below a context window.
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;

impl Launch {
    /// Builds a launch request.
    ///
    /// # Errors
    ///
    /// Returns `None` for a blank id: an unnamed session cannot be addressed.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Option<Self> {
        let id = id.into();
        if id.trim().is_empty() {
            return None;
        }
        Some(Self {
            id,
            shell: None,
            cwd: None,
            size: Size::headless(),
            max_bytes: DEFAULT_MAX_BYTES,
        })
    }

    /// Runs a specific shell.
    #[must_use]
    pub fn with_shell(mut self, shell: impl Into<String>) -> Self {
        self.shell = Some(shell.into());
        self
    }

    /// Starts in a directory.
    #[must_use]
    pub fn in_directory(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the dimensions.
    #[must_use]
    pub fn with_size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }

    /// Sets the transcript bound.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }
}

/// A running terminal.
///
/// The transport behind it writes to a process and reads what it printed. Separated so the session bookkeeping
/// — status, transcript, identification — is the same over a local process and a remote one.
pub trait Shell: Send + Sync {
    /// Writes a command or keystrokes.
    fn write(&self, input: &str) -> BoxFut<Result<(), TerminalError>>;

    /// Reads what is available, waiting no longer than `wait`.
    fn read(&self, wait: std::time::Duration) -> BoxFut<Result<Option<String>, TerminalError>>;

    /// Ends the session.
    fn close(&self) -> BoxFut<Result<(), TerminalError>>;
}

/// One managed terminal.
pub struct Session {
    id: String,
    shell: Arc<dyn Shell>,
    status: std::sync::Mutex<Status>,
    transcript: std::sync::Mutex<Transcript>,
    size: Size,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("status", &self.status())
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Creates a session around a shell.
    #[must_use]
    pub fn new(id: impl Into<String>, shell: Arc<dyn Shell>, size: Size, max_bytes: usize) -> Self {
        Self {
            id: id.into(),
            shell,
            status: std::sync::Mutex::new(Status::Running),
            transcript: std::sync::Mutex::new(Transcript::new(max_bytes)),
            size,
        }
    }

    /// Its id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Its dimensions.
    #[must_use]
    pub fn size(&self) -> Size {
        self.size
    }

    /// Its state.
    #[must_use]
    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Sends input.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError::NotRunning`] when the shell has exited.
    pub async fn send(&self, input: &str) -> Result<(), TerminalError> {
        let status = self.status();
        if !status.is_running() {
            return Err(TerminalError::NotRunning {
                id: self.id.clone(),
                status: status.as_str(),
            });
        }
        self.shell.write(input).await
    }

    /// Reads what the shell printed, recording it in the transcript.
    ///
    /// Returns `None` when nothing arrived before the wait expired, which is the normal answer for a shell
    /// sitting at a prompt.
    ///
    /// # Errors
    ///
    /// Returns whatever the shell reports.
    pub async fn read(&self, wait: std::time::Duration) -> Result<Option<String>, TerminalError> {
        let chunk = self.shell.read(wait).await?;
        if let Some(chunk) = &chunk {
            self.transcript
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(chunk.clone());
        }
        Ok(chunk)
    }

    /// The retained transcript.
    #[must_use]
    pub fn transcript(&self) -> String {
        self.transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .text()
    }

    /// What the transcript dropped.
    #[must_use]
    pub fn omitted(&self) -> Omitted {
        self.transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .omitted()
    }

    /// Ends the session.
    ///
    /// # Errors
    ///
    /// Returns whatever the shell reports.
    pub async fn close(&self) -> Result<(), TerminalError> {
        let result = self.shell.close().await;
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = if result.is_ok() {
            Status::Stopped
        } else {
            Status::Failed {
                reason: result
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
            }
        };
        result
    }
}

/// The terminals this deployment holds.
#[derive(Default)]
pub struct Registry {
    sessions: std::sync::Mutex<std::collections::BTreeMap<String, Arc<Session>>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("sessions", &self.ids())
            .finish()
    }
}

impl Registry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a running session.
    ///
    /// # Errors
    ///
    /// Returns the id when it is taken: two sessions under one id would make a read go to the wrong shell.
    pub fn add(&self, session: Arc<Session>) -> Result<(), String> {
        let id = session.id().to_owned();
        let mut sessions = self.lock();
        if sessions.contains_key(&id) {
            return Err(id);
        }
        sessions.insert(id, session);
        Ok(())
    }

    /// One session.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.lock().get(id).cloned()
    }

    /// Every id, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// The sessions still running.
    #[must_use]
    pub fn running(&self) -> Vec<String> {
        self.lock()
            .values()
            .filter(|session| session.status().is_running())
            .map(|session| session.id().to_owned())
            .collect()
    }

    /// Removes a session, closing it first.
    ///
    /// A session removed without being closed would leave a process with nothing left that could stop it.
    ///
    /// # Errors
    ///
    /// Returns [`TerminalError::Unknown`] for an unknown id, and whatever the shell reports on close.
    pub async fn remove(&self, id: &str) -> Result<(), TerminalError> {
        let session = self
            .lock()
            .remove(id)
            .ok_or_else(|| TerminalError::Unknown { id: id.to_owned() })?;
        session.close().await
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<String, Arc<Session>>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
