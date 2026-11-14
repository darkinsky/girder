//! Recovery advice for a tool call that failed for an *environmental* reason.
//!
//! [`crate::tool_retry`] answers "should this call run again", and [`crate::backoff`] answers "how
//! long to wait before it does". Both are about the same call. Neither answers the question the
//! model actually faces when a file is not there: the tool did what it was asked and the world said
//! no, so running it again — after any delay — produces the same refusal. The only recovery is to
//! change *what* is asked, and the text the model receives (`No such file or directory (os error
//! 2)`) names the symptom without naming a single move that would help.
//!
//! So this module maps the error text a tool already returns onto the moves that are worth making,
//! and tags each with [`Kind`] so a caller can distinguish "go look before you act" from "stop and
//! ask a human". It runs no tool and reads no filesystem: it is a pure function of two strings, so
//! the table it implements can be tested without a tool loop.
//!
//! # Why `Investigate` is the move that matters most
//!
//! The dominant model failure is not failing to retry; it is continuing on a wrong assumption. A
//! model that read `No such file or directory` for `src/session/store.rs` tends to write the same
//! path with different capitalisation, or to create the file — when the file is at
//! `crates/girder-core/src/session.rs` and one `glob` would have said so. [`Kind::Investigate`]
//! exists to put the cheap look before the expensive guess, which is why every `not found` answer
//! starts with it.
//!
//! # Why permission is `Escalate` and not a retry
//!
//! A refused permission is a property of the environment, not of the call: the same `EACCES`
//! comes back on the hundredth attempt, and a model that keeps trying spends the user's context on
//! an outcome that cannot change. [`Kind::Escalate`] is the honest answer, and telling the user is
//! work the model can actually finish.
//!
//! # Why an unknown error suggests nothing
//!
//! A wrong suggestion is worse than none: it sends the model down a path the error does not
//! support, and the model has no way to tell an inferred hint from an observed fact. So anything
//! this table does not recognise returns an empty `actions` list with the error text unchanged,
//! which is the one thing the module knows to be true.
//!
//! # Why the tool name is an input
//!
//! The same text means different things depending on what was called. `No such file` from `read`
//! is a path the model got wrong, and `glob` is the move; `No such file` from `glob` means the
//! *start directory* derived from the pattern is missing, and re-running `glob` is the loop this
//! module exists to prevent. Each branch therefore names a *different* tool from the one that
//! failed.

/// A recovery suggestion for one failed call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    /// What went wrong, in the terms the error supports.
    ///
    /// Written for the model: it names the tool and the shape of the failure, not the errno. When
    /// the text matched no known pattern this is the error text itself, verbatim and uninterpreted.
    pub problem: String,
    /// What can be tried, in the order worth trying it.
    ///
    /// Empty when the failure was not recognised — see the module documentation for why guessing
    /// is worse than saying nothing.
    pub actions: Vec<Action>,
}

/// One move the model can make in response to a failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    /// The move, written as a sentence the model can follow directly.
    pub describe: String,
    /// What sort of move it is.
    pub kind: Kind,
}

/// The kind of a recovery move.
///
/// The kinds exist so a caller can order or filter the moves without parsing
/// [`Action::describe`] — for instance, keeping only [`Self::Investigate`] moves when the model's
/// budget for this turn is one more call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Same tool, different path or parameter.
    ChangeArgument,
    /// A different tool answers the question.
    UseOtherTool,
    /// Find out the fact the failed call assumed, before acting on it again.
    Investigate,
    /// Stop and tell the user, because the obstacle is not the model's to remove.
    Escalate,
}

/// The failure shape this module recognises.
///
/// Deliberately small and closed: each variant is a shape whose recovery is *known*, so adding one
/// means adding a branch that has been thought about rather than a substring that looked common.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    /// The path, program, or resource is not there.
    NotFound,
    /// The caller is not allowed, or the filesystem is mounted read-only.
    PermissionDenied,
    /// A call that needed a file was given a directory.
    IsDirectory,
    /// A path component that must be a directory is a file.
    NotDirectory,
    /// The bytes are not text.
    NotText,
    /// The target is bigger than the tool will accept in one call.
    TooLarge,
    /// The file changed after it was read, so the write would be based on a stale view.
    Stale,
    /// The call omitted something the tool requires.
    MissingArgument,
    /// The program a shell call named is not installed.
    ProgramMissing,
    /// The deadline passed before the call finished.
    TimedOut,
    /// The device has no room left.
    NoSpace,
    /// An edit matched more than one place.
    AmbiguousMatch,
}

/// The substrings that identify each fault.
///
/// Two spellings of the same failure appear here on purpose: the raw `std::io::Error` text
/// (`No such file or directory`, `Permission denied`) that most tools forward verbatim, and this
/// crate's own wording (`does not exist`, `changed since it was read`) from [`crate::fs`]. A table
/// of only one spelling would miss half the errors that actually reach a model.
///
/// Matching is a case-insensitive substring test rather than a parser because the inputs are
/// already-formatted prose: tools wrap, prefix, and translate their errors, and a grammar that
/// insisted on `errno` would reject the wrapped forms while a substring test still finds them.
const NEEDLES: &[(Fault, &[&str])] = &[
    (
        Fault::PermissionDenied,
        &[
            "permission denied",
            "operation not permitted",
            "access denied",
            "not allowed to",
            "eacces",
            "eperm",
            "read-only file system",
            "erofs",
            "approval rejected",
        ],
    ),
    (
        Fault::NoSpace,
        &["no space left", "disk full", "enospc", "quota exceeded"],
    ),
    (
        Fault::ProgramMissing,
        &[
            "command not found",
            "no such command",
            "not recognized as an internal",
            "executable file not found",
            "is not installed",
        ],
    ),
    (
        Fault::MissingArgument,
        &[
            "is required",
            "required argument",
            "must be provided",
            "required field",
        ],
    ),
    (
        Fault::NotFound,
        &[
            "no such file",
            "enoent",
            "does not exist",
            "not found",
            "cannot find",
            "could not find",
            "no matching file",
            "not present",
        ],
    ),
    (
        Fault::IsDirectory,
        &["is a directory", "eisdir", "expected a file"],
    ),
    (
        Fault::NotDirectory,
        &["not a directory", "enotdir", "no such directory"],
    ),
    (
        Fault::NotText,
        &[
            "not valid utf-8",
            "invalid utf-8",
            "binary file",
            "not text",
        ],
    ),
    (
        Fault::TooLarge,
        &[
            "bytes, over the",
            "too large",
            "exceeds the limit",
            "file too large",
            "efbig",
        ],
    ),
    (Fault::Stale, &["changed since it was read", "stale"]),
    (
        Fault::AmbiguousMatch,
        &[
            "matches more than once",
            "more than one match",
            "ambiguous",
            "multiple occurrences",
        ],
    ),
    (
        Fault::TimedOut,
        &[
            "timed out",
            "timeout",
            "deadline exceeded",
            "etimedout",
            "took too long",
        ],
    ),
];

/// Recovers the moves worth making after `tool` failed with `error`.
///
/// The function is total: it never panics and never fails, because it runs on the failure path
/// where losing the call would be worst. An error text this table does not recognise yields no
/// actions rather than a guess.
///
/// # Examples
///
/// ```
/// use girder_core::{RecoveryKind, suggest_recovery};
///
/// let suggestion = suggest_recovery("read", "cannot read `src/nope.rs`: No such file or directory (os error 2)");
/// assert_eq!(suggestion.actions[0].kind, RecoveryKind::Investigate);
///
/// let unknown = suggest_recovery("bash", "segmentation fault");
/// assert!(unknown.actions.is_empty());
/// ```
#[must_use]
pub fn suggest(tool: &str, error: &str) -> Suggestion {
    let fault = classify(&error.to_lowercase());
    let Some(fault) = fault else {
        // Verbatim, not summarised: the caller may still recognise its own error, and paraphrasing
        // text this module did not understand is exactly the fabrication it avoids.
        return Suggestion {
            problem: error.trim().to_owned(),
            actions: Vec::new(),
        };
    };
    Suggestion {
        problem: problem(tool, fault),
        actions: actions(tool, fault),
    }
}

/// Classifies an already-lowercased error text.
///
/// Order matters: the table is scanned in declaration order and the first hit wins, which is why
/// the permission entry precedes the `not found` entry. `Operation not permitted` and
/// `Permission denied` share a kind because the reaction to both is the same — escalate — which is
/// the test [`crate::tool_contracts`] applies to its own variants.
fn classify(text: &str) -> Option<Fault> {
    NEEDLES
        .iter()
        .find(|(_, needles)| needles.iter().any(|needle| text.contains(needle)))
        .map(|(fault, _)| *fault)
}

/// Per-fault action builders, kept per fault so the wording can reference the tool that failed.
fn actions(tool: &str, fault: Fault) -> Vec<Action> {
    match fault {
        Fault::NotFound => not_found(tool),
        Fault::PermissionDenied => permission_denied(),
        Fault::IsDirectory => is_directory(tool),
        Fault::NotDirectory => not_directory(tool),
        Fault::NotText => not_text(tool),
        Fault::TooLarge => too_large(tool),
        Fault::Stale => stale(tool),
        Fault::MissingArgument => missing_argument(tool),
        Fault::ProgramMissing => program_missing(tool),
        Fault::TimedOut => timed_out(tool),
        Fault::NoSpace => no_space(),
        Fault::AmbiguousMatch => ambiguous_match(tool),
    }
}

/// The one-line problem statement for a recognised fault.
fn problem(tool: &str, fault: Fault) -> String {
    let tool = named(tool);
    match fault {
        Fault::NotFound => format!("{tool} needs something that does not exist"),
        Fault::PermissionDenied => {
            format!("{tool} was refused because this session does not have permission for it")
        }
        Fault::IsDirectory => format!("{tool} was given a directory where it needed a file"),
        Fault::NotDirectory => format!("a path component {tool} needs is a file, not a directory"),
        Fault::NotText => format!("the target of {tool} is not text, so it cannot be read as one"),
        Fault::TooLarge => format!("the target of {tool} is too big for one call"),
        Fault::Stale => format!("the file {tool} was holding changed before the write landed"),
        Fault::MissingArgument => format!("{tool} was called without a value it requires"),
        Fault::ProgramMissing => format!("the program {tool} tried to run is not installed"),
        Fault::TimedOut => format!("{tool} did not finish before its deadline"),
        Fault::NoSpace => format!("{tool} could not write because the device is full"),
        Fault::AmbiguousMatch => {
            format!("the edit {tool} was asked for matches more than one place")
        }
    }
}

/// How to name the tool in a sentence, with a fallback for an unnamed one.
///
/// The fallback is not cosmetic: a caller that passes an empty name would otherwise produce a
/// sentence beginning with an empty pair of backticks, which reads as a typo to a model and hides
/// the actual instruction.
fn named(tool: &str) -> String {
    let tool = tool.trim();
    if tool.is_empty() {
        "the tool".to_owned()
    } else {
        format!("`{tool}`")
    }
}

/// Whether the failed tool is itself a discovery tool.
fn is_discovery(tool: &str) -> bool {
    matches!(tool.trim(), "glob" | "grep")
}

/// The tool to go and look with when `tool` failed.
///
/// Never `tool` itself where that can be avoided: recommending the call that just failed is how a
/// model gets stuck in a loop that looks like diligence. When the failure *is* the discovery tool,
/// the shell is the honest fallback, because a listing does not depend on the pattern's syntax.
fn witness(tool: &str) -> &'static str {
    // `grep` and everything that is not a search tool fall through to `glob`, which answers "where
    // is this file". A failed `glob` is the one case `glob` cannot answer, so it gets the shell:
    // `ls`/`find` do not depend on the pattern syntax that just failed.
    if tool.trim() == "glob" {
        "bash"
    } else {
        "glob"
    }
}

fn not_found(tool: &str) -> Vec<Action> {
    let display = named(tool);
    let witness = witness(tool);
    if is_discovery(tool) {
        return vec![
            Action {
                describe: format!(
                    "Go and look with `{witness}` first: the start directory the pattern names \
                     must exist, and running the same pattern again cannot create it."
                ),
                kind: Kind::Investigate,
            },
            Action {
                describe: format!(
                    "Once `{witness}` shows where the tree actually starts, re-run {display} with \
                     the directory spelled the way it is on disk."
                ),
                kind: Kind::ChangeArgument,
            },
        ];
    }
    vec![
        Action {
            describe: format!(
                "Run `{witness}` with a pattern for the file's name (for example `**/session*.rs`) \
                 to find where it actually lives. Guessing at a second path is the same mistake \
                 with different spelling."
            ),
            kind: Kind::Investigate,
        },
        Action {
            describe: format!(
                "If `{witness}` finds it elsewhere, call {tool} again with exactly the path it \
                 reported — a path is a fact about the workspace, not a guess."
            ),
            kind: Kind::ChangeArgument,
        },
        Action {
            describe: format!(
                "If `{witness}` finds nothing, the file may be outside the workspace or may not \
                 exist yet: say so to the user and ask where it should be rather than creating a \
                 file at a path you invented."
            ),
            kind: Kind::Escalate,
        },
    ]
}

fn permission_denied() -> Vec<Action> {
    vec![
        Action {
            describe: "Stop calling tools against this path: the refusal is a property of the \
                       environment, so a further attempt returns the same error."
                .to_owned(),
            kind: Kind::Escalate,
        },
        Action {
            describe: "Tell the user which path was refused and that this session needs the \
                       permission — or the sandbox widened for this project — before the operation \
                       can run."
                .to_owned(),
            kind: Kind::Escalate,
        },
    ]
}

fn is_directory(tool: &str) -> Vec<Action> {
    let display = named(tool);
    // A discovery tool given a directory is not a request for another tool: its pattern just needs
    // to descend into the directory rather than name it.
    if is_discovery(tool) {
        return vec![Action {
            describe: format!(
                "The pattern names a directory. Append `/**/*` to it so {display} walks the \
                 contents instead of trying to match the directory itself."
            ),
            kind: Kind::ChangeArgument,
        }];
    }
    vec![
        Action {
            describe: format!(
                "Use `glob` with `<that path>/**/*` to list what the directory holds, then pick \
                 the file {display} needs from the list."
            ),
            kind: Kind::UseOtherTool,
        },
        Action {
            describe: format!(
                "Call {display} again with a file inside the directory rather than the directory \
                 itself."
            ),
            kind: Kind::ChangeArgument,
        },
    ]
}

fn not_directory(tool: &str) -> Vec<Action> {
    let tool = named(tool);
    vec![
        Action {
            describe: "Run `glob` on the path to see which component exists as a file; the path \
                       assumes a directory that is really a file."
                .to_owned(),
            kind: Kind::Investigate,
        },
        Action {
            describe: format!(
                "Fix the path so it descends into the real directory, then call {tool} again."
            ),
            kind: Kind::ChangeArgument,
        },
    ]
}

fn not_text(tool: &str) -> Vec<Action> {
    let _ = tool;
    vec![
        Action {
            describe: "Use a tool that understands the format instead of reading the bytes as \
                       text: a document, an archive, or an image has no lines to read."
                .to_owned(),
            kind: Kind::UseOtherTool,
        },
        Action {
            describe: "If only part of the file matters, search it with `grep` or inspect it with \
                       a shell command that knows the format."
                .to_owned(),
            kind: Kind::Investigate,
        },
    ]
}

fn too_large(tool: &str) -> Vec<Action> {
    let tool = named(tool);
    vec![
        Action {
            describe: format!(
                "Call {tool} with a range — `offset` and `limit` — so it returns the part that \
                 matters; the whole file was refused, not truncated."
            ),
            kind: Kind::ChangeArgument,
        },
        Action {
            describe:
                "Use `grep` to find the line the question is about, then read only the range \
                       around it."
                    .to_owned(),
            kind: Kind::Investigate,
        },
    ]
}

fn stale(tool: &str) -> Vec<Action> {
    let tool = named(tool);
    vec![
        Action {
            describe: "Read the file again with `read` before writing: someone else's change is \
                       why the write was refused, and forcing it would discard their work."
                .to_owned(),
            kind: Kind::Investigate,
        },
        Action {
            describe: format!(
                "Then call {tool} again with the edit applied to what was just read."
            ),
            kind: Kind::ChangeArgument,
        },
    ]
}

fn missing_argument(tool: &str) -> Vec<Action> {
    let tool = named(tool);
    vec![
        Action {
            describe: "Add the argument the error names and resend the same call; nothing ran, \
                       so there is no partial result to reconcile."
                .to_owned(),
            kind: Kind::ChangeArgument,
        },
        Action {
            describe: format!(
                "Check the tool's schema if the argument was supplied: {tool} rejects a value of \
                 the wrong type as firmly as a missing one."
            ),
            kind: Kind::Investigate,
        },
    ]
}

fn program_missing(tool: &str) -> Vec<Action> {
    let tool = named(tool);
    vec![
        Action {
            describe: "Run `bash -lc 'command -v <program>'` to confirm the program is absent \
                       rather than misspelled."
                .to_owned(),
            kind: Kind::Investigate,
        },
        Action {
            describe: format!(
                "Then either use a program that exists for this workspace, or tell the user that \
                 {tool} needs a program that is not installed."
            ),
            kind: Kind::Escalate,
        },
    ]
}

fn timed_out(tool: &str) -> Vec<Action> {
    let tool = named(tool);
    vec![
        Action {
            describe: format!(
                "Make the next call smaller: {tool} may have made partial progress, so narrow the \
                 work rather than re-running the whole thing unchanged."
            ),
            kind: Kind::ChangeArgument,
        },
        Action {
            describe: format!(
                "Investigate what {tool} was waiting on before retrying — the same call against \
                 the same slow resource times out again."
            ),
            kind: Kind::Investigate,
        },
    ]
}

fn no_space() -> Vec<Action> {
    vec![
        Action {
            describe: "Stop writing: a full device does not free space between attempts."
                .to_owned(),
            kind: Kind::Escalate,
        },
        Action {
            describe: "Tell the user that the write failed for lack of space and which path it \
                       targeted, so they can clear room or choose another location."
                .to_owned(),
            kind: Kind::Escalate,
        },
    ]
}

fn ambiguous_match(tool: &str) -> Vec<Action> {
    let tool = named(tool);
    vec![
        Action {
            describe: format!(
                "Expand the text {tool} matches on — include the surrounding lines — so exactly one \
                 place matches."
            ),
            kind: Kind::ChangeArgument,
        },
        Action {
            describe: "Read the file first to see every place the text occurs, so the edit you \
                       send is unambiguous rather than a longer guess."
                .to_owned(),
            kind: Kind::Investigate,
        },
    ]
}
