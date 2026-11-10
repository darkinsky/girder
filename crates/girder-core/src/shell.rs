//! The shell capability: running a command, and describing what the run was.
//!
//! `subprocess` owns the mechanics of a process. This owns the *policy vocabulary* a shell tool needs:
//! which program, what environment, how long, whether it may write, and how its output is bounded.
//!
//! # Why the specification is separate from the request
//!
//! A request is what a caller asks for; a specification is what the runtime resolved it to. The two are
//! different the moment a default applies — a caller that omitted a timeout and a caller that asked for
//! the default timeout produce the same specification but are not the same request. Keeping them apart
//! means a log can record what actually ran rather than what was asked.
//!
//! # Why the environment is built rather than inherited
//!
//! A child inherits its parent's environment, which includes whatever secrets the parent was started
//! with. [`Environment`] therefore starts from an explicit allowlist and adds named values, so a
//! credential in the harness's own environment does not silently reach a command a model asked to run.
//!
//! # Why the output bound is part of the specification
//!
//! A command's output is unbounded. The bound is decided before the run — a caller that discovered the
//! size afterwards would already have buffered it — so it belongs here rather than at the read.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::retention::TextStrategy;

/// The prefix this harness sets on its own environment variables.
///
/// A prefix rather than a list so a command can recognise the harness's own values — a test that must
/// not inherit a real API key checks for names starting with this.
pub const ENV_PREFIX: &str = "GIRDER_";

/// What a caller asks to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The command line, as written.
    pub command: String,
    /// The working directory, when it differs from the session's.
    pub cwd: Option<PathBuf>,
    /// A hard limit on the run.
    pub timeout: Option<Duration>,
}

impl Request {
    /// Builds a request for one command line.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
            timeout: None,
        }
    }

    /// Runs it in `cwd`.
    #[must_use]
    pub fn in_directory(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Bounds the run.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// The program and arguments a command line resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Program {
    /// A POSIX shell, with the command line as its `-c` argument.
    ///
    /// The default: a model writes shell, and a shell is what understands pipes, redirections, and
    /// globbing. A direct spawn would need the caller to have already split the words.
    Posix {
        /// The shell to use.
        shell: String,
    },
    /// A direct program with arguments, with no shell in between.
    ///
    /// Safer for a command whose words are already known: nothing in the arguments is interpreted.
    Direct {
        /// The program.
        program: String,
        /// Its arguments.
        args: Vec<String>,
    },
}

impl Program {
    /// The POSIX shell this build prefers.
    #[must_use]
    pub fn default_shell() -> String {
        // `sh` rather than `bash`: it exists on every POSIX system, and a tool that needed bash-isms
        // would fail on a minimal image in a way that looks like the harness is broken.
        "sh".to_owned()
    }

    /// A POSIX shell program.
    #[must_use]
    pub fn posix() -> Self {
        Self::Posix {
            shell: Self::default_shell(),
        }
    }

    /// A direct program.
    #[must_use]
    pub fn direct(program: impl Into<String>, args: Vec<String>) -> Self {
        Self::Direct {
            program: program.into(),
            args,
        }
    }
}

/// The environment a child receives.
///
/// An allowlist plus additions rather than an inherited copy: a child inherits its parent's
/// environment, which includes whatever secrets the parent was started with. A credential in the
/// harness's own environment must not silently reach a command a model asked to run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Environment {
    /// Values passed through from the parent, by name.
    passed: Vec<String>,
    /// Values set explicitly.
    set: BTreeMap<String, String>,
}

impl Environment {
    /// The variables nearly every command needs.
    ///
    /// Deliberately short: `PATH` so a command can find its tools, `HOME` so a tool that writes a config
    /// finds its directory, and the locale so output encoding matches. Everything else is opt-in.
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            passed: vec![
                "PATH".to_owned(),
                "HOME".to_owned(),
                "LANG".to_owned(),
                "LC_ALL".to_owned(),
                "TERM".to_owned(),
                "TMPDIR".to_owned(),
            ],
            set: BTreeMap::new(),
        }
    }

    /// Nothing at all.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Passes a parent variable through.
    #[must_use]
    pub fn pass(mut self, name: impl Into<String>) -> Self {
        self.passed.push(name.into());
        self
    }

    /// Sets a value.
    #[must_use]
    pub fn set(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.set.insert(name.into(), value.into());
        self
    }

    /// Whether a name is passed through from the parent.
    #[must_use]
    pub fn is_passed(&self, name: &str) -> bool {
        self.passed.iter().any(|passed| passed == name)
    }

    /// The names passed through, in order.
    #[must_use]
    pub fn passed(&self) -> &[String] {
        &self.passed
    }

    /// The explicitly set values.
    #[must_use]
    pub fn set_values(&self) -> &BTreeMap<String, String> {
        &self.set
    }

    /// The variables this environment would contribute, resolving passes against `parent`.
    ///
    /// A passed name the parent does not hold contributes nothing: an empty value would be
    /// indistinguishable from a variable the parent did not set, and a command that checked for
    /// presence would get the wrong answer.
    #[must_use]
    pub fn resolve(&self, parent: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let mut resolved = BTreeMap::new();
        for name in &self.passed {
            if let Some(value) = parent.get(name) {
                resolved.insert(name.clone(), value.clone());
            }
        }
        resolved.extend(self.set.clone());
        resolved
    }
}

/// Whether a command may write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxMode {
    /// Writes outside the working directory are refused.
    ReadOnly,
    /// Writes inside the working directory are allowed.
    WorkspaceWrite,
    /// Anything is allowed.
    Full,
}

impl SandboxMode {
    /// The wire name, matching the upstream spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::Full => "danger-full-access",
        }
    }
}

/// What a request resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Specification {
    /// What to run.
    pub program: Program,
    /// Where.
    pub cwd: Option<PathBuf>,
    /// With what environment.
    pub environment: Environment,
    /// For how long.
    pub timeout: Duration,
    /// Whether it may write.
    pub sandbox: SandboxMode,
    /// How its output is bounded.
    pub retention: TextStrategy,
}

/// The default timeout when a request names none.
///
/// Two minutes rather than unlimited: a command that never returns holds a process slot and a tool call
/// open for the session's life, and a caller that genuinely needs longer can say so.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The default output bound.
///
/// 64 KiB: far above what a model can usefully read and far below a context window, so a chatty command
/// is truncated rather than rejected.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;

impl Request {
    /// Resolves to a specification.
    ///
    /// Defaults are applied here rather than at the run, so the resolved form is what a log records: a
    /// caller that omitted a timeout and one that asked for the default produce the same specification,
    /// and a reader that saw only the request could not tell which it was.
    #[must_use]
    pub fn resolve(&self, sandbox: SandboxMode) -> Specification {
        Specification {
            program: Program::posix(),
            cwd: self.cwd.clone(),
            environment: Environment::baseline(),
            timeout: self.timeout.unwrap_or(DEFAULT_TIMEOUT),
            sandbox,
            retention: TextStrategy::HeadTail {
                head_bytes: DEFAULT_MAX_BYTES / 2,
                tail_bytes: DEFAULT_MAX_BYTES / 2,
            },
        }
    }
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunResult {
    /// The exit code, or `None` when the process was signalled.
    pub code: Option<i32>,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Whether the output was bounded.
    pub truncated: bool,
}

impl RunResult {
    /// Whether the command reported success.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Whether a command name could be found on `PATH`.
///
/// A check a caller runs before spawning, so a missing tool is reported as "not installed" rather than
/// as a spawn failure whose message depends on the platform.
#[must_use]
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

/// Splits a command line into words the way a POSIX shell would for a simple command.
///
/// Not a shell parser: no expansions, no redirections, no operators. It exists for the direct-spawn
/// path, where the caller has a line it believes is simple — and the honest handling of a line that is
/// not simple is to fall back to [`Program::posix`] rather than to guess.
#[must_use]
pub fn split_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for character in command.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if quote != Some('\'') => escaped = true,
            '\'' | '"' => match quote {
                Some(open) if open == character => quote = None,
                None => quote = Some(character),
                // A different quote inside a quote is a literal character.
                Some(_) => current.push(character),
            },
            character if character.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}
