//! Remote hosts over SSH: connections, addressing, and what a remote command may do.
//!
//! The transport is OpenSSH, which is a protocol with its own authentication, forwarding, and multiplexing. This
//! module owns the *vocabulary* — what a host is, how a remote path is addressed, and how a failure is reported —
//! and leaves the protocol to whoever implements it.
//!
//! # Why a host is not a URL
//!
//! `ssh://user@host:port` is a URL, and parsing one would let a caller smuggle in query parameters, paths, and
//! percent-encoding that mean nothing to a shell. A host is a name, a port, and a user, and each is validated on
//! its own.
//!
//! # Why a remote path is joined rather than resolved
//!
//! The remote filesystem's rules are not the local one's. A path that resolved locally — normalising `..` and
//! collapsing separators — could name a different file remotely, because the remote may be a different operating
//! system with different separator and case rules. Joining is the honest operation: it says what it does.
//!
//! # Why the exit status is separate from the output
//!
//! A remote command that failed still produced output, and a caller that only saw an error would lose the
//! diagnostic the command printed before failing. The status is data, not the transport's condition.

use std::path::PathBuf;
use std::time::Duration;

use crate::scope::BoxFut;

/// The default SSH port.
pub const DEFAULT_PORT: u16 = 22;

/// The default time to wait for one remote command.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// One remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    /// Its name or address.
    pub name: String,
    /// Its port.
    pub port: u16,
    /// The user to connect as, or the client's default when absent.
    pub user: Option<String>,
}

impl Host {
    /// Builds a host on the default port.
    ///
    /// # Errors
    ///
    /// Returns `None` for a blank name, and for a name containing characters a hostname cannot hold. The check
    /// is not exhaustive — a name that resolves is the only proof — but it refuses the shapes that mean something
    /// else to a shell.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Option<Self> {
        let name = name.into();
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return None;
        }
        // A hostname, an IPv4 address, or a bracketed IPv6 address. A space, slash, or at-sign means the caller
        // passed a URL or a command, neither of which is a host.
        if trimmed
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, '/' | '@' | '\\'))
        {
            return None;
        }
        Some(Self {
            name: trimmed.to_owned(),
            port: DEFAULT_PORT,
            user: None,
        })
    }

    /// Sets the port.
    #[must_use]
    pub fn on_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Sets the user.
    ///
    /// # Errors
    ///
    /// Returns `None` for a user containing characters that mean something else to a shell.
    #[must_use]
    pub fn as_user(mut self, user: impl Into<String>) -> Option<Self> {
        let user = user.into();
        if user.is_empty()
            || user
                .chars()
                .any(|character| character.is_whitespace() || matches!(character, '@' | ':' | '/'))
        {
            return None;
        }
        self.user = Some(user);
        Some(self)
    }

    /// The target as the client spells it: `user@host:port`, with defaults omitted.
    ///
    /// For a log or a listing, not for a command line: a caller that interpolated this into a shell would be
    /// building the very command this type exists to avoid.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = String::new();
        if let Some(user) = &self.user {
            out.push_str(user);
            out.push('@');
        }
        out.push_str(&self.name);
        if self.port != DEFAULT_PORT {
            out.push(':');
            out.push_str(&self.port.to_string());
        }
        out
    }
}

/// A command to run on a remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The host.
    pub host: Host,
    /// The command line, as the remote shell will read it.
    ///
    /// A line rather than a program and arguments: the remote side is a shell, and a caller that wanted to avoid
    /// quoting would have to know the remote's quoting rules — which are the ones it is trying to avoid.
    pub line: String,
    /// Where to run it.
    pub cwd: Option<PathBuf>,
    /// How long to wait.
    pub timeout: Duration,
}

impl Command {
    /// Builds a command.
    ///
    /// # Errors
    ///
    /// Returns `None` for a blank line: a command that runs nothing would report success for work that never
    /// happened.
    #[must_use]
    pub fn new(host: Host, line: impl Into<String>) -> Option<Self> {
        let line = line.into();
        if line.trim().is_empty() {
            return None;
        }
        Some(Self {
            host,
            line,
            cwd: None,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Runs it in a directory.
    #[must_use]
    pub fn in_directory(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Bounds the wait.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// What a remote command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Its exit status, or `None` when it was signalled.
    pub code: Option<i32>,
    /// What it printed.
    pub stdout: String,
    /// What it printed to standard error.
    pub stderr: String,
}

impl Outcome {
    /// Whether it reported success.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Why an SSH operation failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SshError {
    /// The host could not be reached.
    #[error("cannot reach `{host}`: {reason}")]
    Unreachable {
        /// The host.
        host: String,
        /// Why.
        reason: String,
    },

    /// Authentication failed.
    ///
    /// Distinct from unreachable: the host answered and refused this client, which a user fixes differently.
    #[error("authentication to `{host}` failed")]
    Unauthenticated {
        /// The host.
        host: String,
    },

    /// The command ran longer than its timeout.
    #[error("`{host}` did not finish within {seconds}s")]
    Timeout {
        /// The host.
        host: String,
        /// The limit.
        seconds: u64,
    },

    /// The connection was lost mid-command.
    ///
    /// Distinct from a command failure: the command may have run, and a caller that retried blindly would run it
    /// twice.
    #[error("the connection to `{host}` was lost")]
    Disconnected {
        /// The host.
        host: String,
    },
}

/// The transport a deployment implements.
pub trait Transport: Send + Sync {
    /// Runs one command.
    fn run(&self, command: Command) -> BoxFut<Result<Outcome, SshError>>;
}

/// The hosts this deployment knows, and the transport that reaches them.
pub struct Client {
    transport: std::sync::Mutex<Option<std::sync::Arc<dyn Transport>>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("has_transport", &self.is_available())
            .finish()
    }
}

impl Client {
    /// Creates a client with no transport.
    ///
    /// A client without one refuses every command: a deployment that mounted the vocabulary without the protocol
    /// should say so rather than appear to work.
    #[must_use]
    pub fn new() -> Self {
        Self {
            transport: std::sync::Mutex::new(None),
        }
    }

    /// Creates a client over a transport.
    #[must_use]
    pub fn with_transport(transport: std::sync::Arc<dyn Transport>) -> Self {
        Self {
            transport: std::sync::Mutex::new(Some(transport)),
        }
    }

    /// Whether a transport is mounted.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Runs a command.
    ///
    /// # Errors
    ///
    /// Returns [`SshError::Unreachable`] when no transport is mounted, and whatever the transport reports.
    pub async fn run(&self, command: Command) -> Result<Outcome, SshError> {
        let transport = self
            .transport
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| SshError::Unreachable {
                host: command.host.display(),
                reason: "no SSH transport is mounted".to_owned(),
            })?;
        transport.run(command).await
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// A transport that refuses everything, for a deployment without SSH.
#[derive(Debug, Default)]
pub struct Unavailable;

impl Transport for Unavailable {
    fn run(&self, command: Command) -> BoxFut<Result<Outcome, SshError>> {
        let host = command.host.display();
        Box::pin(async move {
            Err(SshError::Unreachable {
                host,
                reason: "this deployment has no SSH transport".to_owned(),
            })
        })
    }
}
