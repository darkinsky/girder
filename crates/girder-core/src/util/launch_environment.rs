//! What a launched process inherits, and what it must not.
//!
//! A child process inherits its parent's environment, which is convenient and wrong: the harness's own environment
//! contains whatever credentials it was started with, and a command a model asked to run must not receive them.
//!
//! # Why the allowlist is by prefix as well as by name
//!
//! Some variables are the harness's own — `GIRDER_*` — and a child should see none of them. Naming each one would
//! miss the next one added; a prefix rule covers the family.
//!
//! # Why the ambient set is explicit
//!
//! A variable that happens to be set when a test runs would make the test pass on one machine and fail on another.
//! [`Resolved`] is the answer a caller passes to a spawn, and it is a value rather than a closure over the real
//! environment so a test can inspect it.
//!
//! # Why a removed variable is recorded
//!
//! A command that failed because it could not find a token produces a message about the command. Recording that the
//! variable was withheld lets a reader connect the two.

use std::collections::BTreeMap;

/// Variables a child always receives when the parent has them.
///
/// Short by design: `PATH` so a command finds its tools, `HOME` so a tool that writes a config finds its directory,
/// the locale so output encoding matches, and `TMPDIR` so a temporary file lands somewhere writable.
pub const AMBIENT: &[&str] = &[
    "PATH", "HOME", "LANG", "LC_ALL", "TERM", "TMPDIR", "USER", "SHELL",
];

/// Variable prefixes a child never receives.
///
/// The harness's own namespace, plus the prefixes every cloud provider uses for credentials. A prefix rather than a
/// name because naming each one misses the next one added.
pub const NEVER: &[&str] = &[
    "GIRDER_",
    "AWS_",
    "AZURE_",
    "GCP_",
    "GOOGLE_",
    "DEEPSEEK_",
    "OPENAI_",
    "ANTHROPIC_",
    "SSH_AUTH_SOCK",
    "GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "NPM_TOKEN",
];

/// What a child receives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    /// The variables to set.
    pub values: BTreeMap<String, String>,
    /// Variables withheld because they were in a [`NEVER`] prefix.
    ///
    /// Recorded so a reader can connect a command's authentication failure to the variable it was denied.
    pub withheld: Vec<String>,
}

impl Resolved {
    /// Whether a variable is present.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    /// One value.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }
}

/// Whether a variable must never reach a child.
#[must_use]
pub fn is_forbidden(name: &str) -> bool {
    NEVER.iter().any(|prefix| {
        // An exact match covers names without a separator, like `SSH_AUTH_SOCK`, but those are in the same list and a
        // prefix test would also withhold `SSH_AUTH_SOCKET`, which is the right call.
        name == *prefix || name.starts_with(prefix)
    })
}

/// Whether a variable is passed through from the parent.
#[must_use]
pub fn is_ambient(name: &str) -> bool {
    AMBIENT.contains(&name)
}

/// Resolves an environment.
///
/// `parent` is the environment the child would inherit; `extra` is what a caller wants to add. An `extra` entry that
/// names a forbidden variable is dropped rather than honoured: a caller asking for a credential to pass through is
/// making the mistake this module exists to prevent.
#[must_use]
pub fn resolve(parent: &BTreeMap<String, String>, extra: &BTreeMap<String, String>) -> Resolved {
    let mut resolved = Resolved::default();

    for (name, value) in parent {
        if is_forbidden(name) {
            resolved.withheld.push(name.clone());
            continue;
        }
        if is_ambient(name) {
            resolved.values.insert(name.clone(), value.clone());
        }
    }

    for (name, value) in extra {
        if is_forbidden(name) {
            resolved.withheld.push(name.clone());
            continue;
        }
        resolved.values.insert(name.clone(), value.clone());
    }

    resolved.withheld.sort();
    resolved.withheld.dedup();
    resolved
}
