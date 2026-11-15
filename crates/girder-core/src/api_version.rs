//! The API version a client speaks, and whether a given method exists in it.
//!
//! An RPC surface that changes over time has three failure modes, and this module exists to turn
//! all three from silent into explicit:
//!
//! 1. A client that predates a method calls it anyway. Without a version check the call reaches
//!    dispatch and fails as `unknown method`, which reads like a typo and sends the client author
//!    looking in the wrong place.
//! 2. A client newer than the build calls a method the build has never heard of. The same
//!    `unknown method` comes back, but the fix is on the other side — the server is old — and
//!    nothing in the error says so.
//! 3. A method still exists but its behavior changed. Nothing fails at all, and the client gets
//!    plausible-looking wrong answers. This is the worst of the three, because it is the only one
//!    nobody reports.
//!
//! [`check`] answers "is this method callable at this version, and if so should the caller be
//! warned" for one method at one version. It is deliberately a pure function over plain data — no
//! registry lookup, no I/O — so a caller can evaluate a whole method table at startup or per call
//! without either decision constraining the other.
//!
//! # Why a version this build does not know is an error, not a fallback
//!
//! [`Version::parse`] refuses anything newer than [`Version::CURRENT`]. The tempting alternative is
//! to clamp — treat `v99` as `v2` and hope the intersection of the two surfaces covers the call.
//! That turns an honest "your client is newer than this server" into a puzzle: some methods work,
//! others fail as though the client had guessed wrong, and nothing anywhere says why. A client that
//! is told it is too new can upgrade the server or downgrade itself; a client that is silently
//! downgraded can only guess. [`VersionError::TooNew`] therefore carries both numbers, so the
//! message can name which direction the fix lies in.
//!
//! # Why the type is closed
//!
//! [`Version`]'s field is private, and the only ways to obtain one are [`Version::parse`] and
//! [`Version::CURRENT`]. Every `Version` value that exists therefore names a version this build can
//! actually serve, which is what keeps the ceiling check at the boundary instead of asking every
//! downstream `match` to re-decide what an out-of-range version means.
//!
//! # Why there is exactly one spelling per version
//!
//! `v2` and `2` are both accepted because both appear on the wire — a URL segment and a JSON field
//! rarely agree on convention. Everything else is refused: `V2`, ` 2`, `02`, and `2.0` all name
//! version two, and accepting them would mean one version has five tokens. Tokens end up in log
//! lines, cache keys, and feature-compare tables, and any of those keyed on a token that has five
//! spellings is wrong in a way that only shows up under load. Normalizing instead of refusing would
//! work, but it hides a defect in the caller that is cheap to fix once and expensive to debug later.
//!
//! # Why the current version is 2
//!
//! Version 1 is the surface that predates this check — a build that could not answer questions
//! about versions. Version 2 is the surface that can, which is a behavior change a client is
//! entitled to detect. Keeping both alive is also what makes the window logic in [`check`]
//! meaningful: with a single version, "introduced after" and "removed before" are both
//! unrepresentable, so the module could not be tested against a real requirement table. The
//! `assert!` below makes that reasoning a compile-time constraint rather than a comment.
//!
//! # Why absence and removal share one outcome
//!
//! A method that never existed and a method that has been retired are the same fact to a caller:
//! there is no call that works. Splitting them would force every client to handle a distinction it
//! cannot act on, and the client most likely to see the second case is the stale one least likely
//! to have been updated for a new enum variant. [`Compatibility::NotInVersion`] carries the lower
//! bound of the window, so a caller whose own version is at or above that bound can still tell that
//! the cause is removal rather than a bad guess.

use std::fmt;

/// One API version.
///
/// Ordered by the number it wraps, so `v1 < v2` holds by construction: there is no field ordering
/// to get wrong and no separate rank table to drift out of date. `u32` rather than a string so that
/// comparison is the ordinary integer comparison and cannot depend on how the token was spelled.
///
/// See the module documentation for why the field is private.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(u32);

/// The first version this module can name.
///
/// Versions are 1-based because a zero would have to mean "no version" or "every version" and the
/// two are not distinguishable at the call site. `parse` refuses `0` for that reason.
const FIRST: u32 = 1;

impl Version {
    /// The version this build speaks.
    ///
    /// A compile-time constant on purpose: the answer to "which version am I" must not depend on
    /// configuration, on a table that might be stale, or on a call that can fail. A build that has
    /// to be asked at runtime can be asked the wrong question.
    pub const CURRENT: Self = Self(2);

    /// Parses a version token, such as `v2` or `2`.
    ///
    /// Accepts exactly two spellings: an optional lowercase `v` followed by decimal digits with no
    /// leading zero. Refuses an empty token, any other shape, the number zero, and any version
    /// newer than [`Version::CURRENT`] — the last as [`VersionError::TooNew`] rather than as a
    /// parse failure, because "this build is older than you" is actionable and "bad token" is not.
    pub fn parse(text: &str) -> Result<Self, VersionError> {
        if text.is_empty() {
            return Err(VersionError::Empty);
        }

        let digits = text.strip_prefix('v').unwrap_or(text);
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(malformed(text));
        }
        // `01` and `1` are the same version; only one of them is allowed to be the spelling of it.
        if digits.len() > 1 && digits.starts_with('0') {
            return Err(malformed(text));
        }
        // A token wider than `u32` cannot be reported as `TooNew` without truncating the number the
        // client sent, so it is a malformed token instead: the raw text is carried in the error, and
        // either way the caller learns nothing is going to work.
        let Ok(number) = digits.parse::<u32>() else {
            return Err(malformed(text));
        };
        if number < FIRST {
            return Err(malformed(text));
        }
        if number > Self::CURRENT.0 {
            return Err(VersionError::TooNew {
                found: number,
                supported: Self::CURRENT.0,
            });
        }
        Ok(Self(number))
    }

    /// The number behind the version, for logs and for building a response field.
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

/// The token form of a version, `v2`, so a message can quote a version the way the wire spells it.
impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "v{}", self.0)
    }
}

// The window logic in `check` is only meaningful when at least two versions are live; see the
// module documentation under "Why the current version is 2". This is a compile error, not a
// comment, because lowering `CURRENT` to 1 silently turns two of `check`'s three outcomes into
// unreachable code.
const _: () = assert!(
    Version::CURRENT.0 > FIRST,
    "the version check needs at least two live versions to have anything to check"
);

/// Why a version token could not be used.
///
/// The three cases are kept apart because they call for three different responses: fix the caller
/// (empty), fix the caller (malformed), or fix the deployment (too new).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    /// The token was empty.
    ///
    /// Separate from `Malformed` because an empty value is what a missing or defaulted field looks
    /// like, and that is a different bug from a client that sent something it thought was a version.
    #[error("an API version is required, but the version token was empty")]
    Empty,

    /// The token is not a version this module can read.
    ///
    /// Carries the offending text so the error can quote what arrived instead of describing it.
    #[error(
        "`{text}` is not an API version; expected `v2` or `2`, naming a version this build serves"
    )]
    Malformed {
        /// What the caller sent, verbatim.
        text: String,
    },

    /// The token names a version newer than this build has ever heard of.
    ///
    /// Both numbers are present so the message can say which side to change. This is the variant
    /// that makes a silent downgrade impossible.
    #[error(
        "API version v{found} is newer than v{supported}, the newest this build speaks; upgrade the server or use a v{supported} client"
    )]
    TooNew {
        /// The version the caller asked for.
        found: u32,
        /// The newest version this build serves.
        supported: u32,
    },
}

/// Builds the error for a token whose shape is wrong.
///
/// A free function rather than a closure so the three rejection paths above cannot drift into
/// slightly different messages.
fn malformed(text: &str) -> VersionError {
    VersionError::Malformed {
        text: text.to_owned(),
    }
}

/// A method's version requirement: when it appeared, when it went away, and what replaced it.
///
/// The three facts live together because they are answers to one question. Splitting availability
/// from deprecation across two tables means two lookups that can disagree, and a disagreement there
/// produces exactly the silent wrong answer this module is built to prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    /// The first version that has the method.
    pub since: Version,

    /// The first version that no longer has it; `None` while it is still served.
    ///
    /// The boundary is inclusive: a version equal to this value does not have the method. "Removed
    /// in v3" has to mean v3 does not serve it, or the removal takes effect one version late and
    /// the client that upgrades to v3 keeps working for a release it should not.
    pub removed: Option<Version>,

    /// The behavior change that makes the method's use a warning, if any.
    ///
    /// Without this field [`Compatibility::Deprecated`] would be unreachable, and a client would
    /// learn about a changed method only by observing the change.
    pub deprecated: Option<Deprecation>,
}

impl Requirement {
    /// A method that has been served since `since` and has never changed.
    pub const fn new(since: Version) -> Self {
        Self {
            since,
            removed: None,
            deprecated: None,
        }
    }

    /// Attaches a behavior change to this requirement, builder-style.
    ///
    /// Takes the whole [`Deprecation`] rather than two loose arguments so a caller cannot supply a
    /// replacement without the version it starts at, which is the combination that would make the
    /// warning fire for versions that predate it.
    pub fn with_deprecation(mut self, deprecation: Deprecation) -> Self {
        self.deprecated = Some(deprecation);
        self
    }
}

/// A behavior change that leaves a method callable but no longer correct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deprecation {
    /// The first version whose callers should switch.
    ///
    /// The version in which the new behavior landed, not the one where the old behavior is removed:
    /// a warning that arrives at the removal has arrived too late to act on.
    pub since: Version,

    /// What to call instead, named so the reader can find it.
    ///
    /// Always a concrete replacement — a method name, or a description of the one call that replaces
    /// the old one. "Deprecated" on its own leaves the caller with no action, which is the same as
    /// no warning at all.
    pub use_instead: String,
}

/// The verdict on one method at one version.
///
/// Three outcomes rather than a boolean because "works", "works but has moved on", and "is not here"
/// are three different things for a client to do, and a boolean would collapse the middle case into
/// silence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compatibility {
    /// The call is available and means what the client thinks it means.
    Supported,

    /// The call works, but its behavior changed in `since`; `use_instead` is what to move to.
    Deprecated {
        /// The first version where the new behavior applies.
        since: Version,
        /// The replacement, never empty.
        use_instead: String,
    },

    /// The version has no such method.
    ///
    /// `available_from` is the window's lower bound, so a caller whose version is at or above it
    /// knows the cause is removal rather than a method that never existed. See the module
    /// documentation for why the two cases share this variant.
    NotInVersion {
        /// The method that was asked for.
        method: String,
        /// The first version that had it.
        available_from: Version,
    },
}

/// Checks one method's requirement against the version a caller speaks.
///
/// The order of the checks is the policy: a version outside the window is refused first, because a
/// method that does not exist cannot also be deprecated; only then does a behavior change inside the
/// window produce a warning; anything left is plain support. A deprecation whose `since` falls
/// outside the window is unreachable by construction — the method is not served there at all.
pub fn check(requirement: &Requirement, version: Version, method: &str) -> Compatibility {
    if version < requirement.since {
        return not_in_version(requirement, method);
    }
    if requirement
        .removed
        .is_some_and(|removed| version >= removed)
    {
        return not_in_version(requirement, method);
    }
    if let Some(deprecation) = requirement
        .deprecated
        .as_ref()
        .filter(|deprecation| version >= deprecation.since)
    {
        return Compatibility::Deprecated {
            since: deprecation.since,
            use_instead: deprecation.use_instead.clone(),
        };
    }
    Compatibility::Supported
}

/// Builds the "not in this version" outcome, which both ends of the window produce.
fn not_in_version(requirement: &Requirement, method: &str) -> Compatibility {
    Compatibility::NotInVersion {
        method: method.to_owned(),
        available_from: requirement.since,
    }
}
