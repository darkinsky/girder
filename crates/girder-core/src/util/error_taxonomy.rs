//! A branchable answer to "what kind of failure was that".
//!
//! [`RemoteError`](girder_core::RemoteError) describes the *method* that failed, not the *thing*
//! that was missing. Measured against one `DirectoryCatalog` and one absent id, that produces four
//! incompatible shapes for one fact:
//!
//! | call | what the caller receives |
//! |---|---|
//! | `session.delete` | `Ok(false)` — a success |
//! | `session.rename` | `UNKNOWN_METHOD` — reads as a typo in the method name |
//! | `session.fork` | `INVALID_ARGUMENTS` — reads as a malformed request |
//! | `session.state` | `FAILED` — the fallback bucket |
//!
//! A client cannot branch on that: `UNKNOWN_METHOD` invites "fix your call", `FAILED` invites a
//! retry, and neither answer lets the caller ask the only question it has — *should I create this
//! session, or stop?*
//!
//! # A mapping, not a rewrite
//!
//! The missing variant belongs in `RemoteError`, and adding it there means editing
//! `remote.rs`, which this module deliberately does not do. Instead it maps the errors that exist
//! onto [`Category`], a parallel vocabulary with one entry per *cause*. [`categorize`] is a pure
//! function of `(namespace, method, error)`, so the classification can be adopted by the dispatcher
//! later without the call sites moving, and every case above collapses onto
//! [`Category::NotFound`] today.
//!
//! # Why the evidence is read three ways
//!
//! None of the existing variants carry a `NotFound` flag, so the only truthful signal differs per
//! variant: for `rename` it is the *shape* of the method name (a synthesized `rename '<id>'`
//! rather than the requested `rename`), for `fork` it is the *message* (`no session '<id>'`
//! inside an argument error), and for `delete` it is the *success value*. Reading each variant on
//! its own terms is why this cannot be one string check, and why [`looks_missing`] is kept
//! conservative: guessing "missing" from a message that does not say so would relabel a genuine
//! argument error as an absence, which is the exact mistake this module exists to undo.
//!
//! # The stability contract
//!
//! [`tag`] is the part clients branch on, so its strings are an API: they are fixed at
//! `NOT_FOUND`, `INVALID_ARGUMENTS`, `UNKNOWN_METHOD`, `REFUSED`, and `FAILED`, and must not be
//! renamed once a client depends on them.

/// A failure category a caller can branch on.
///
/// Each variant names a *cause* rather than the method that reported it, which is what makes two
/// different methods failing for the same reason comparable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Category {
    /// The target does not exist.
    ///
    /// The answer a caller needed all along: creating the session is sensible, retrying is not, and
    /// the id is carried so a client multiplexing several calls knows which one was absent.
    NotFound {
        /// What kind of thing was missing, taken from the namespace (for example `session`).
        what: String,
        /// The id that was asked for.
        id: String,
    },

    /// The request's shape was wrong: a field missing, blank, or of the wrong type.
    ///
    /// Distinct from [`Category::NotFound`] because the caller can fix it by changing the request,
    /// where an absence can only be fixed by changing the world.
    InvalidArguments {
        /// What was wrong, in the wording the method already produced.
        detail: String,
    },

    /// No method is registered under that name.
    ///
    /// Distinct from [`Category::NotFound`] by construction: this is a typo in the *method*, and it
    /// must never be reported for a missing session, which is the bug this module documents.
    UnknownMethod {
        /// Requested namespace.
        namespace: String,
        /// Requested method.
        method: String,
    },

    /// The access policy refused the call.
    ///
    /// Not retryable: the same rule refuses the same call however many times it is sent.
    Refused {
        /// The rule or reason the policy gave.
        rule: String,
    },

    /// The method ran and failed for a reason that is not one of the above.
    ///
    /// The honest bucket rather than a story: a caller that treats this as "try again later" is
    /// right about transient faults and wrong about nothing, because an absence never lands here.
    Failed {
        /// What failed, in the wording the method produced.
        detail: String,
    },
}

/// Phrases that state an absence outright.
///
/// Only phrases that *say* something is missing: `error`, `failed`, and `invalid` are not here,
/// because a message carrying one of those describes the call rather than the world, and treating
/// it as an absence would turn a fixable request into a phantom "create it" branch.
const MISSING_PHRASES: &[&str] = &[
    "no such",
    "does not exist",
    "doesn't exist",
    "not found",
    "cannot find",
    "could not find",
];

/// Phrases that state a missing *session* specifically.
///
/// Kept apart from [`MISSING_PHRASES`] because `no session` on its own is ambiguous: `no session id
/// given` is a request whose argument is absent, not a session that is absent. The leading phrase
/// only counts when an id follows it, which is what the crate's own wording does
/// (`no session '<id>'`, `no session named '<id>'`).
const MISSING_SESSION_PHRASES: &[&str] = &["no session `", "no session named", "unknown session `"];

/// Categorizes an existing error by its cause.
///
/// This is the entry point that makes the four shapes comparable: called with the `namespace` and
/// `method` the caller dispatched, it returns the same [`Category::NotFound`] for a missing session
/// whether the method reported it as an unknown method, an argument error, or a bare failure.
///
/// The `namespace` and `method` arguments are the ones *requested*, not the ones inside the error:
/// the difference between the two is itself evidence (see [`categorize`]'s handling of
/// `UnknownMethod`), and a caller that passed the error's own names would erase it.
#[must_use]
pub fn categorize(namespace: &str, method: &str, error: &girder_core::RemoteError) -> Category {
    match error {
        // The registry synthesizes this variant for an id it could not find, so a method name that
        // *extends* the requested one with a backticked id is an absence wearing a typo's clothes.
        // A genuine typo reports exactly the name that was asked for, which is why the extension
        // check is what separates the two.
        girder_core::RemoteError::UnknownMethod {
            namespace: reported_ns,
            method: reported,
        } => match synthesized_id(method, reported) {
            Some(id) => Category::NotFound {
                what: what(namespace),
                id,
            },
            None => Category::UnknownMethod {
                namespace: reported_ns.clone(),
                method: reported.clone(),
            },
        },

        // `fork` reports a missing source as `INVALID_ARGUMENTS`, with the id in the message. The
        // message is the only signal there is, so it is read conservatively rather than trusted.
        girder_core::RemoteError::InvalidArguments { message, .. } => {
            if looks_missing(message) {
                Category::NotFound {
                    what: what(namespace),
                    id: id_from(message).unwrap_or_else(|| message.trim().to_owned()),
                }
            } else {
                Category::InvalidArguments {
                    detail: message.clone(),
                }
            }
        }

        // A gap in this build is not a typo: reporting it as `UnknownMethod` would send an operator
        // looking for a spelling mistake in a call that is spelled correctly.
        girder_core::RemoteError::NotImplemented { .. } => Category::Failed {
            detail: error.to_string(),
        },

        girder_core::RemoteError::Refused { reason, .. } => Category::Refused {
            rule: reason.clone(),
        },

        // The fallback bucket is where an absence most often hides, so it is the one place the
        // message is read at all: `no session '<id>'` arrives here for `session.state`.
        girder_core::RemoteError::Failed(detail) => {
            if looks_missing(detail) {
                Category::NotFound {
                    what: what(namespace),
                    id: id_from(detail).unwrap_or_else(|| detail.trim().to_owned()),
                }
            } else {
                Category::Failed {
                    detail: detail.clone(),
                }
            }
        }
    }
}

/// Classifies the `Ok(false)` a delete-style method returns for an id it could not find.
///
/// `session.delete` reports an absence through its success value, so [`categorize`] never sees it:
/// there is no error to pass. Returning `None` when something *was* removed keeps the caller's
/// branch honest — a successful delete has no category, because nothing went wrong.
#[must_use]
pub fn categorize_removal(namespace: &str, id: &str, removed: bool) -> Option<Category> {
    (!removed).then(|| Category::NotFound {
        what: what(namespace),
        id: id.to_owned(),
    })
}

/// Whether a message states that something does not exist.
///
/// Conservative on purpose: only wording that says so outright counts, and `no session id given` —
/// a request with a missing *argument* — is not one of those, even though it contains `no session`.
/// A caller that over-reports absence creates sessions that should have failed; a caller that
/// under-reports it merely shows the original error, so the asymmetry decides the rule.
#[must_use]
pub fn looks_missing(message: &str) -> bool {
    let lowered = message.to_lowercase();
    if MISSING_PHRASES
        .iter()
        .any(|phrase| lowered.contains(phrase))
    {
        return true;
    }
    MISSING_SESSION_PHRASES
        .iter()
        .any(|phrase| lowered.contains(phrase))
}

/// The stable string a client branches on.
///
/// These strings are an API: they match [`girder_core::RemoteError::code`] where a code already
/// exists, so a client that switches on a tag is doing the same thing it does today, with
/// `NOT_FOUND` finally available as the one it was missing. Renaming one breaks every client that
/// matched on it, so treat this list as append-only.
#[must_use]
pub fn tag(category: &Category) -> &'static str {
    match category {
        Category::NotFound { .. } => "NOT_FOUND",
        Category::InvalidArguments { .. } => "INVALID_ARGUMENTS",
        Category::UnknownMethod { .. } => "UNKNOWN_METHOD",
        Category::Refused { .. } => "REFUSED",
        Category::Failed { .. } => "FAILED",
    }
}

/// The id a synthesized method name carries, if it carries one.
///
/// The shape `rename` is replaced with is `rename '<id>'`, so the id is only accepted when it
/// directly extends the requested method name; anything else is a method name the caller mistyped.
fn synthesized_id(requested: &str, reported: &str) -> Option<String> {
    let rest = reported.strip_prefix(requested)?;
    let rest = rest.trim_start();
    if !rest.starts_with('`') {
        return None;
    }
    id_from(rest)
}

/// The first non-empty backtick-quoted span in `text`.
///
/// The crate quotes values rather than delimiting them some other way, so this reads the id out of
/// `no session '<id>'` without needing the message format to be anything more than that.
fn id_from(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after_open = &rest[open + 1..];
        let close = after_open.find('`')?;
        let candidate = after_open[..close].trim();
        if !candidate.is_empty() {
            return Some(candidate.to_owned());
        }
        rest = &after_open[close + 1..];
    }
    None
}

/// The kind of thing a namespace names, for [`Category::NotFound::what`].
///
/// The namespace is already singular in this crate (`session`, `workspace`), so it is used as-is;
/// the empty string still needs a word, because a category that names nothing tells a reader
/// nothing about what to look for.
fn what(namespace: &str) -> String {
    let trimmed = namespace.trim();
    if trimmed.is_empty() {
        "resource".to_owned()
    } else {
        trimmed.to_owned()
    }
}
