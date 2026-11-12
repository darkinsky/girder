//! Checking that a deployment is usable before it accepts a connection.
//!
//! A server that starts with a broken piece of its configuration serves errors to every client, one at a time, instead
//! of saying once that it cannot work. The checks here run at startup and answer in one place, so a misconfiguration is
//! one loud message rather than a hundred quiet ones.
//!
//! # Why a check is a value rather than a panic
//!
//! A deployment may want to start with a missing optional piece -- no provider means the offline path, no desktop means
//! no path handoff -- and refusing to start would take away a usable server. So each finding carries a severity, and
//! only an [`Severity::Fatal`] one refuses to start. The caller decides; this module only reports.
//!
//! # Why the checks take values rather than reading the world
//!
//! A check that called `std::env::var` could not be tested and would answer differently at a different moment. These take
//! the fact as an argument, so a test states the deployment it is describing.

use std::fmt::Write as _;
use std::path::Path;

/// How much a finding matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// The server works, but something is worth saying.
    Note,
    /// A feature will refuse; the rest of the server works.
    Degraded,
    /// The server cannot work.
    Fatal,
}

/// One finding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// What is wrong.
    pub subject: String,
    /// How much it matters.
    pub severity: Severity,
    /// What to do about it.
    pub remedy: String,
}

impl Finding {
    /// A finding that will not stop the server.
    #[must_use]
    pub fn degraded(subject: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            severity: Severity::Degraded,
            remedy: remedy.into(),
        }
    }

    /// A finding that stops the server.
    #[must_use]
    pub fn fatal(subject: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            severity: Severity::Fatal,
            remedy: remedy.into(),
        }
    }

    /// A finding worth mentioning that stops nothing.
    #[must_use]
    pub fn note(subject: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            severity: Severity::Note,
            remedy: remedy.into(),
        }
    }
}

/// What a deployment looks like, as far as the checks care.
///
/// The booleans are each a distinct fact about a deployment rather than a set of flags to be combined, which is why
/// they stay named fields instead of being folded into a bitfield or a nested struct that would repeat the names.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Default)]
pub struct Deployment {
    /// Whether a model provider is configured.
    pub provider: bool,
    /// The sessions directory, when one is configured.
    pub sessions_root: Option<std::path::PathBuf>,
    /// The workspace a session may run in.
    pub workspace: Option<std::path::PathBuf>,
    /// Whether a terminal launcher is available.
    pub terminal: bool,
    /// Whether the desktop can open a path.
    pub desktop: bool,
    /// Whether a shell was found for the tools that need one.
    pub shell: bool,
}

/// Runs every check against a deployment.
///
/// The order is fatal first, then degraded, then notes: a caller reading the report stops at the first line that tells
/// it the server cannot run, rather than scrolling past three warnings to find it.
#[must_use]
pub fn check(deployment: &Deployment) -> Vec<Finding> {
    let mut findings = Vec::new();

    // No provider is not fatal: the offline path is a supported way to drive the real tools without a model, and a
    // deployment that uses it should not have to set a dummy key to start.
    if !deployment.provider {
        findings.push(Finding::degraded(
            "no model provider is configured",
            "set DEEPSEEK_API_KEY (or ANTHROPIC_API_KEY) to use a live model, or leave it unset for the offline path",
        ));
    }

    if let Some(root) = &deployment.workspace {
        if !root.is_dir() {
            // Fatal: every tool that touches a file would fail, one call at a time, with a message about a path rather
            // than about the deployment.
            findings.push(Finding::fatal(
                format!("the workspace `{}` is not a directory", root.display()),
                "create it, or point the workspace at one that exists",
            ));
        }
    }

    if let Some(root) = &deployment.sessions_root {
        if root.exists() && !root.is_dir() {
            findings.push(Finding::fatal(
                format!("the sessions root `{}` is not a directory", root.display()),
                "remove the file, or choose a different directory for the session logs",
            ));
        } else if !root.exists() {
            // Not fatal: the store creates the directory on first write, so a fresh deployment is normal.
            findings.push(Finding::note(
                format!("the sessions root `{}` does not exist yet", root.display()),
                "it will be created on the first session",
            ));
        }
    }

    if !deployment.shell {
        findings.push(Finding::degraded(
            "no shell was found on PATH",
            "the command tools will refuse; install a POSIX shell or set SHELL",
        ));
    }

    if !deployment.terminal {
        findings.push(Finding::degraded(
            "no terminal launcher is available",
            "terminal methods will refuse; a deployment that does not need terminals may ignore this",
        ));
    }

    if !deployment.desktop {
        findings.push(Finding::note(
            "no desktop is available to open paths",
            "host.openPath and session.openWorkspacePath will refuse; this is normal on a server",
        ));
    }

    // Sorted so the report is stable between runs: a caller diffing two reports should see what changed, not how the
    // checks happen to have been ordered.
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.subject.cmp(&b.subject))
    });
    findings
}

/// Whether the findings include one that stops the server.
#[must_use]
pub fn is_fatal(findings: &[Finding]) -> bool {
    findings
        .iter()
        .any(|finding| finding.severity == Severity::Fatal)
}

/// Renders the findings as lines a person reads at startup.
#[must_use]
pub fn report(findings: &[Finding]) -> String {
    let mut out = String::new();
    for finding in findings {
        let label = match finding.severity {
            Severity::Fatal => "fatal",
            Severity::Degraded => "degraded",
            Severity::Note => "note",
        };
        let _ = writeln!(out, "{label}: {}", finding.subject);
        let _ = writeln!(out, "  {}", finding.remedy);
    }
    out
}

/// Checks whether a path looks like a directory without touching it first.
///
/// A missing path answers `true`: it may be about to be created, and a check that treated absent as broken would report
/// every fresh deployment as fatal.
#[must_use]
pub fn may_be_a_directory(path: &Path) -> bool {
    !path.exists() || path.is_dir()
}
