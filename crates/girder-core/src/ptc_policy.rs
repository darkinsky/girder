//! The policy layer a code-execution request passes through *before* anything runs.
//!
//! Upstream ships a real execution seam (`packages/ptc-runtime`) whose Node backend runs
//! model-written TypeScript in a managed child process under a resolved sandbox policy, with an
//! elapsed deadline, an output budget and a V8 heap limit. Nothing in Girder executes model-written
//! code yet, and this module does not change that. What it adds is the part worth having without a
//! sandbox: the arithmetic that decides whether a request is admissible at all, and the limits an
//! execution layer would have to honour if one existed.
//!
//! # What this is not
//!
//! There is no isolation here, and none is implied. This module never reads a file, spawns a process
//! or touches the host; it reads a request and a policy and answers. [`Limits::allowed_paths`]
//! *declares* the one root an executor would confine a program to — declaring is not enforcing, and
//! an executor that ignored the field would still run. This is the same honesty as
//! [`crate::exec`]'s path policy, which says in its own module documentation that it is advisory
//! rather than kernel-enforced; the point is more acute here, because a policy layer that claimed to
//! sandbox would be worse than no policy layer at all.
//!
//! # Why an oversized request is clamped rather than refused
//!
//! A request for 999,999 MB is not an attack. It is a caller that does not know the ceiling, or does
//! not care to compute it, and the useful question is "how much may this run have", not "was the
//! caller impertinent". Refusing would make every caller responsible for a constant that belongs to
//! the deployment, and the refusal would end in the same place as a clamp, one round trip later. So
//! the policy clamps to its ceiling and reports the result in [`Limits`], which lets a caller compare
//! what it asked for against what it got.
//!
//! The same arithmetic lets a *smaller* request win: a caller that asks for 5 seconds gets 5, because
//! a tighter limit is a caller spending its own budget rather than a claim on the deployment's. The
//! ceiling is therefore both the default and the maximum — [`Policy`] carries one number per dimension
//! because two ("default" and "max") could disagree, and a reader would then have to know which one a
//! missing field meant.
//!
//! Zero is the one value on this axis that is refused rather than clamped. A caller asking for zero
//! seconds is not asking for a lot; it is asking for something that cannot succeed, and reporting
//! `Allow` for a plan that is guaranteed to fail would push a configuration mistake onto the
//! execution layer with less information attached.
//!
//! # Why there is no scan for "dangerous" code
//!
//! Because it would not work, and looking like it works is the harm. A substring check for
//! `os.system` is defeated by `getattr(os, "sys" + "tem")`, by `__import__`, by a name read from a
//! file, or by any of a hundred ways a language spells the same call. Such a scan would either miss
//! the obfuscated spelling while reassuring whoever saw "policy checked", or grow into a dialect no
//! one can reason about. The bounds that do hold are quantitative, and all of them are here: a closed
//! language set, a byte ceiling, and the limits above. A count is checkable; a pattern is not.
//!
//! Anything about *what* a program does belongs to the executor, which can confine it with a real
//! mechanism — a child process with resource limits, a path policy, a sandbox. This module's job is
//! narrower and it can actually do it: make sure the executor is only ever asked for something
//! bounded, in a language it claims to support, against a workspace root it was given.
//!
//! # Why the decision is a pure function
//!
//! Nothing here canonicalizes, stats or opens a path. A decision that depended on the filesystem
//! would make every test depend on one too, and worse, would let the answer change between the check
//! and the launch — the classic reason a policy check is worthless. The workspace is returned exactly
//! as given. Resolving it, and re-checking after resolution because a symlink can move the target,
//! belongs to the executor, which is the only party that can act on the answer.

use std::path::{Path, PathBuf};

/// One request to execute a program the model wrote.
///
/// The fields mirror what a caller already knows before execution: the text, the language it claims
/// to be, and whether it has an opinion about the resource ceilings. `None` means "no opinion", not
/// "unlimited" — the policy supplies its default, and there is no way to ask for more than it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecRequest {
    /// The program text.
    ///
    /// Byte length, not character count, is what the policy bounds: an interpreter reads bytes, and
    /// a page of multi-byte text can be several times larger than its character count suggests.
    pub code: String,
    /// The language the program is written in, e.g. `"python"`, `"javascript"`, `"shell"`.
    ///
    /// Matched against [`Policy::languages`] case-insensitively, because the value is a label the
    /// model chose rather than a security token: `Python` and `python` name the same runtime, and
    /// refusing one teaches the caller nothing while the whitelist stays just as closed.
    pub language: String,
    /// The memory ceiling the caller would like, in megabytes.
    ///
    /// A value above the policy ceiling is clamped down to it; a value below it is honoured.
    pub memory_mb: Option<u64>,
    /// The wall-clock deadline the caller would like, in seconds.
    ///
    /// A value above the policy ceiling is clamped down to it; a value below it is honoured.
    pub timeout_secs: Option<u64>,
}

/// What the policy decided about one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The request may run, with exactly these limits.
    ///
    /// The limits are the effective ones. A caller that asked for more has been clamped; a caller
    /// that asked for less got what it asked for, so the variant's payload is also the answer to
    /// "what did my request turn into".
    Allow(Limits),
    /// The request may not run, for the reason given.
    ///
    /// The string is written to be shown to a model or an operator, so it names the offending field
    /// and, where one exists, the set of values that would have been accepted. It is data, not a
    /// formatting convention: callers pass it through rather than matching on it.
    Refuse(String),
}

impl Decision {
    /// Whether the request may run.
    ///
    /// A caller that only needs the yes/no answer should not have to match the variant and discard
    /// the reason, but it should also not be able to forget that a refusal carries one.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow(_))
    }
}

/// The limits an admitted request actually runs under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The memory ceiling to apply, in megabytes.
    ///
    /// Always at most the policy's `default_memory_mb` and always at least 1: a zero ceiling is
    /// refused before a [`Limits`] is ever constructed, so an executor can treat this as runnable
    /// without re-deriving that conclusion.
    pub memory_mb: u64,
    /// The wall-clock deadline to apply, in seconds.
    ///
    /// Same guarantees as [`Limits::memory_mb`]: at most the policy ceiling, never zero.
    pub timeout_secs: u64,
    /// The paths the program may touch.
    ///
    /// The workspace root and nothing else. A [`Vec`] rather than a single path because the shape
    /// has to survive the day a policy admits a cache directory alongside the workspace, and an
    /// executor that iterates is already correct when that happens.
    ///
    /// Declared, not enforced: see the module documentation.
    pub allowed_paths: Vec<PathBuf>,
}

/// One execution policy: which languages are enabled and what every run is bounded by.
///
/// The policy is the deployment's statement about its own capacity, so it is plain data with public
/// fields rather than a builder or a config schema. A caller constructs it once from configuration
/// and passes it to [`Policy::check`] per request.
#[derive(Clone, Debug)]
pub struct Policy {
    /// The languages that may run at all.
    ///
    /// A closed set, and the reason it is closed: an unrecognised language string is a request whose
    /// limits this policy does not know how to apply — a language with its own resource model, or
    /// one whose name a caller invented. Admitting it would mean handing the executor something it
    /// has no rule for. An empty list admits nothing, which is the correct fail-closed state for a
    /// deployment that forgot to configure the field.
    pub languages: Vec<String>,
    /// The memory ceiling in megabytes, and the default when a request expresses no opinion.
    pub default_memory_mb: u64,
    /// The wall-clock ceiling in seconds, and the default when a request expresses no opinion.
    pub default_timeout_secs: u64,
    /// The largest program that may be admitted, in bytes.
    ///
    /// Checked before anything else looks at the text, because a 100 MB program is the attack the
    /// other checks would be spending time on while it is parsed.
    pub max_code_bytes: usize,
}

impl Policy {
    /// Decides whether `request` may run against `workspace`, and under which limits.
    ///
    /// The checks run cheapest-and-most-categorical first: language, then code size, then the
    /// workspace root, then the numeric limits. A request can fail several at once, and the reason
    /// reported is the one worth fixing first — an unknown language means the caller does not need
    /// this seam at all, while an oversized program means it needs to try harder.
    ///
    /// Empty code is admitted. An empty program does nothing; refusing it would be a refusal of
    /// something harmless, which trains a caller to work around the policy instead of with it.
    #[must_use]
    pub fn check(&self, request: &ExecRequest, workspace: &Path) -> Decision {
        let language = request.language.trim();
        if language.is_empty() {
            return Decision::Refuse(
                "the request names no language; a program cannot be admitted without one"
                    .to_owned(),
            );
        }

        if !self.permits(language) {
            return Decision::Refuse(format!(
                "language `{language}` is not enabled by this policy; enabled languages are {}",
                self.enabled_languages()
            ));
        }

        if request.code.len() > self.max_code_bytes {
            return Decision::Refuse(format!(
                "the program is {} bytes, over the {} byte ceiling; a program that large is the \
                 attack, not what it would do once it ran",
                request.code.len(),
                self.max_code_bytes
            ));
        }

        if workspace.as_os_str().is_empty() {
            return Decision::Refuse(
                "the workspace root is empty; an executor could read that as the current directory, \
                 which is not what the policy meant to allow"
                    .to_owned(),
            );
        }

        let memory_mb = match resolve_limit(
            request.memory_mb,
            self.default_memory_mb,
            "memory ceiling in megabytes",
        ) {
            Ok(value) => value,
            Err(reason) => return Decision::Refuse(reason),
        };

        let timeout_secs = match resolve_limit(
            request.timeout_secs,
            self.default_timeout_secs,
            "deadline in seconds",
        ) {
            Ok(value) => value,
            Err(reason) => return Decision::Refuse(reason),
        };

        Decision::Allow(Limits {
            memory_mb,
            timeout_secs,
            allowed_paths: vec![workspace.to_path_buf()],
        })
    }

    /// Whether `language` is one this policy enables.
    ///
    /// Entries are trimmed and compared case-insensitively so that a configuration file with
    /// incidental whitespace still works; the set of admitted names is unchanged either way.
    fn permits(&self, language: &str) -> bool {
        self.languages
            .iter()
            .any(|allowed| allowed.trim().eq_ignore_ascii_case(language))
    }

    /// A renderable list of the enabled languages, for a refusal message.
    ///
    /// An empty policy renders as `(none)` rather than an empty string, because a message ending in
    /// "enabled languages are " reads as a truncation bug rather than as an answer.
    fn enabled_languages(&self) -> String {
        if self.languages.is_empty() {
            "(none)".to_owned()
        } else {
            self.languages.join(", ")
        }
    }
}

/// Resolves one requested limit against the policy ceiling.
///
/// Returns the effective value: the request when it is tighter than the ceiling, the ceiling when it
/// is looser, and the ceiling when the request has no opinion. Only a zero effective value is an
/// error, and the message distinguishes the two ways to arrive at one, because they call for
/// different repairs — one is a caller's bug, the other is a deployment's.
fn resolve_limit(requested: Option<u64>, ceiling: u64, descriptor: &str) -> Result<u64, String> {
    let effective = requested.map_or(ceiling, |value| value.min(ceiling));
    if effective > 0 {
        return Ok(effective);
    }

    Err(if requested == Some(0) {
        format!("the request asks for a zero {descriptor}, which no execution could satisfy")
    } else {
        format!("this policy sets a zero {descriptor}, so no request can be admitted")
    })
}
