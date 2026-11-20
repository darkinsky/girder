//! Admission policy for attachments: size, type, and per-session capacity.
//!
//! `attachment.rs` decides whether a *specific* payload is internally consistent (canonical base64,
//! magic bytes agreeing with the declared type). This module answers a different question, one that
//! comes earlier: is this upload something the session is willing to hold at all? The two are
//! separate because the answers come from different places. Consistency is a property of the bytes;
//! admissibility is a property of the deployment, which is why it arrives as a [`Policy`] instead of
//! a constant.
//!
//! # The function is pure
//!
//! [`check`] takes `existing` from its caller rather than counting attachments itself. Counting
//! belongs to whoever owns the store: a policy check that also queried storage would need a lock, a
//! bound on how long it may wait, and would still race against concurrent uploads in the same
//! session. Keeping the arithmetic here and the bookkeeping there leaves this module free of I/O and
//! trivially testable — and it makes the one race that remains visible to the caller instead of
//! hiding it behind a lock that cannot span the store's own write.
//!
//! # Why the checks run in this order
//!
//! `check` reports exactly one reason, so the order is a decision about what the user is told.
//! Ordering by cost would be meaningless here: every input is already in hand, so all four tests
//! cost the same. The order below is instead "what can the user do about it":
//!
//! 1. [`RejectReason::MissingName`] — no policy is involved and no resizing helps. A nameless
//!    upload cannot be shown, selected, or deleted later, so the request is malformed rather than
//!    disallowed.
//! 2. [`RejectReason::TypeNotAllowed`] — a static property of the file. Compressing a banned format
//!    does not make it acceptable, so reporting size first would send the user to shrink a file
//!    that is refused regardless. It would also hand an attacker a way to probe the size limit
//!    with formats that can never be stored.
//! 3. [`RejectReason::TooLarge`] — repairable, but only by changing the file. Reported after type,
//!    because "too large" is advice the user should only receive when following it could succeed.
//! 4. [`RejectReason::SessionFull`] — the only condition that can be fixed without touching the
//!    file, by deleting an older attachment. It is reported last so that the message always points
//!    at the cheapest remedy that actually exists.
//!
//! Concretely: a 100 GB `.exe` is reported as `TypeNotAllowed`, not `TooLarge`. That is deliberate.
//! The type is unfixable, and it is the reason no upload of that file will ever be admitted.
//!
//! # Type matching
//!
//! An entry in [`Policy::allowed_types`] is either an exact media type (`application/pdf`) or a
//! prefix ending in `/` (`image/`, matching `image/png` and `image/webp`). A trailing slash is the
//! only wildcard syntax: `image/*` is *not* interpreted as a glob, because two spellings of the same
//! rule invite policies that silently disagree. Matching ignores case and media type parameters, so
//! `Image/PNG` and `text/plain; charset=utf-8` are judged by their kind rather than their spelling.

/// What a session is willing to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Largest single attachment, in bytes. The limit is inclusive: exactly this many bytes is
    /// accepted.
    pub max_bytes: u64,
    /// Allowed media types. An entry may be an exact type or a prefix ending in `/`. An empty list
    /// means every type is allowed; a non-empty list never admits an upload that declares no type.
    pub allowed_types: Vec<String>,
    /// How many attachments one session may hold, counting the ones already stored.
    pub max_per_session: usize,
}

/// The outcome of admitting one attachment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Accept.
    Accept,
    /// Reject, with the reason shown to the user.
    Reject(RejectReason),
}

/// Why an attachment was refused.
///
/// Each variant carries the numbers that produced it rather than a pre-rendered sentence: the
/// wording belongs to the caller's presentation layer, and the policy that judged the upload lives
/// in [`Policy`], which a stored reason does not hold. Use [`RejectReason::explain`] to turn a
/// reason into user-facing text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// Larger than the policy allows.
    TooLarge {
        /// The configured limit, in bytes.
        limit: u64,
        /// The size that was offered, in bytes.
        got: u64,
    },
    /// The media type is not one the policy admits.
    TypeNotAllowed {
        /// The declared media type, with case and parameters normalized.
        mime: String,
    },
    /// The session already holds as many attachments as it may.
    SessionFull {
        /// The configured per-session limit.
        limit: usize,
    },
    /// No usable file name was supplied.
    MissingName,
}

/// Decide whether one attachment may be stored.
///
/// `existing` is the number of attachments the session already holds, supplied by the caller so this
/// function stays free of I/O. The checks run in the order documented at the module level; only the
/// first failing check is reported.
#[must_use]
pub fn check(policy: &Policy, name: &str, mime: &str, bytes: u64, existing: usize) -> Verdict {
    // Trimmed, because a name of spaces renders as a blank row in any list and cannot be searched
    // for; it is missing in every way that matters to the user.
    if name.trim().is_empty() {
        return Verdict::Reject(RejectReason::MissingName);
    }

    if !type_allowed(policy, mime) {
        return Verdict::Reject(RejectReason::TypeNotAllowed { mime: kind(mime) });
    }

    // `>` rather than `>=`: the limit names the largest permitted file, so it must itself be legal.
    if bytes > policy.max_bytes {
        return Verdict::Reject(RejectReason::TooLarge {
            limit: policy.max_bytes,
            got: bytes,
        });
    }

    if existing >= policy.max_per_session {
        return Verdict::Reject(RejectReason::SessionFull {
            limit: policy.max_per_session,
        });
    }

    Verdict::Accept
}

/// Whether `mime` names an image, and so may need derived work such as a thumbnail.
///
/// This is a separate question from admission: an image can be refused by policy, and a non-image
/// can be accepted, but only images need a preview generated. Kept as a prefix test rather than a
/// list of decoders because it answers "is preprocessing likely", not "can we decode this".
#[must_use]
pub fn is_image(mime: &str) -> bool {
    kind(mime).starts_with("image/")
}

impl RejectReason {
    /// Render the reason as a sentence for the user, quoting the policy that produced it.
    ///
    /// Takes `policy` because two of the messages can only be actionable if they say what *is*
    /// allowed: telling a user their file is the wrong type without naming the accepted types
    /// leaves them guessing, and guessing at an allowlist is how people resend the same rejection.
    #[must_use]
    pub fn explain(&self, policy: &Policy) -> String {
        match self {
            Self::MissingName => "the upload has no file name; a name is required".to_owned(),
            Self::TypeNotAllowed { mime } => {
                let allowed = allowed_summary(&policy.allowed_types);
                if mime.is_empty() {
                    format!("the upload declares no media type; allowed: {allowed}")
                } else {
                    format!("`{mime}` is not allowed; allowed: {allowed}")
                }
            }
            Self::TooLarge { limit, got } => format!(
                "the file is {} ({} bytes); the limit is {} ({} bytes)",
                human_bytes(*got),
                got,
                human_bytes(*limit),
                limit
            ),
            Self::SessionFull { limit } => format!(
                "this session already holds {limit} attachments, which is its limit; \
                 delete one and try again"
            ),
        }
    }
}

/// Whether the policy admits `mime`.
fn type_allowed(policy: &Policy, mime: &str) -> bool {
    // An empty allowlist is the documented "allow everything" spelling. It is not the same as an
    // allowlist that happens to contain an empty entry: that entry matches nothing.
    if policy.allowed_types.is_empty() {
        return true;
    }

    let candidate = kind(mime);
    // A non-empty allowlist cannot admit an undeclared type, since no entry can speak for it.
    if candidate.is_empty() {
        return false;
    }

    policy
        .allowed_types
        .iter()
        .any(|entry| entry_matches(entry, &candidate))
}

/// Whether one allowlist entry admits an already-normalized media type.
fn entry_matches(entry: &str, candidate: &str) -> bool {
    let entry = kind(entry);
    if entry.is_empty() {
        // A blank entry is a mistake in configuration; treating it as "allow all" would silently
        // widen the policy, so it matches nothing.
        return false;
    }

    if entry.ends_with('/') {
        candidate.starts_with(&entry)
    } else {
        candidate == entry
    }
}

/// Normalize a media type value: drop parameters, trim, and lowercase.
///
/// Parameters describe an encoding, not the kind of content, so `text/plain; charset=utf-8` must
/// match a policy entry of `text/plain`. Case is folded because media types are case-insensitive by
/// specification and senders do not agree on which casing to use.
fn kind(mime: &str) -> String {
    // `split` always yields at least one item, so the fallback only satisfies the type system.
    let head = mime.split(';').next().unwrap_or_default();
    head.trim().to_ascii_lowercase()
}

/// List the allowlist for a rejection message.
fn allowed_summary(allowed: &[String]) -> String {
    if allowed.is_empty() {
        return "any media type".to_owned();
    }
    let entries: Vec<&str> = allowed
        .iter()
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .collect();
    if entries.is_empty() {
        // Every configured entry was blank, so nothing can be admitted; saying "anything" or
        // showing an empty list would both mislead.
        return "no media type".to_owned();
    }
    entries.join(", ")
}

/// Render a byte count with a binary unit for counts a person would not read as digits.
fn human_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes < MIB {
        return format!("{bytes} bytes");
    }
    let whole = bytes / MIB;
    // One fractional digit, computed with integer arithmetic so no rounding helper is needed.
    let tenths = (bytes % MIB) * 10 / MIB;
    format!("{whole}.{tenths} MiB")
}
