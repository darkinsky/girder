//! The manifest of a multi-session export: what is inside a package, without the packages.
//!
//! One session has a transport format already — [`crate::session_portable`] — and one session has a
//! reader-facing form — [`crate::session_static`]. Both answer a question about *one* session. The
//! question this module answers is the one asked before either is called: given a directory of
//! sessions, a project, or a machine that is about to be migrated, which sessions are in the
//! export, and how much of each one?
//!
//! # This is a manifest, not the data
//!
//! A [`Bundle`] here carries identifiers, titles, and event counts. It carries no events, no
//! prompts, no tool output, and no session bytes. The payload of one entry is produced by
//! [`crate::session_portable::export`] and read back by [`crate::session_portable::import`]; a
//! rendered page for a reader is produced by [`crate::session_static::render`]. Keeping the two
//! apart is what makes a backup resumable: the manifest can be read, checked, and listed without
//! decoding a gigabyte of logs, and a payload that fails to transfer does not invalidate the list
//! of what was supposed to be in it.
//!
//! The split also keeps the failure modes separate. A corrupt *manifest* is a broken index and
//! names the package. A corrupt *payload* is one bad session inside a package whose index is still
//! trustworthy, so it can be re-fetched instead of the whole export being redone.
//!
//! # Why [`Bundle::total_events`] exists at all
//!
//! It is redundant: every entry already states its own count. That is exactly why it is useful. A
//! number that can be derived is a checksum on the derivation — if the declared total and the sum
//! over the entries disagree, then either the package was damaged in transit or the code that built
//! it dropped or duplicated an entry, and both are conditions a caller must hear about *before* it
//! deletes the source it exported from. [`validate`] is that comparison, and it is the reason this
//! type is not just a `Vec<Entry>`.
//!
//! # Why an empty package is an error
//!
//! An empty manifest is always representable and never useful. A "backup" with no sessions either
//! means the export pointed at the wrong directory, or that every session was filtered out by
//! mistake, and in both cases a caller that treats the result as success goes on to remove the
//! originals. Refusing [`BundleError::Empty`] turns a silent empty backup into a failure at the
//! moment the caller can still do something about it. A caller that genuinely wants "nothing to
//! export" does not need a package to say so; it can check its own listing first.
//!
//! # Why the version is checked before anything else
//!
//! Version 2 of this format may sum events differently, or add per-entry fields, or stop carrying a
//! total at all. If the version were checked after the consistency rule, a v2 package would be
//! reported as a *corrupt v1 package* — the caller would chase damage that does not exist and never
//! learn that the real problem is an old binary. Checking the version first makes the failure name
//! the format, which is the only actionable sentence for a reader.
//!
//! # Why the sum saturates
//!
//! An event count is a `usize`, and a package is allowed to be large. Summing with `+` in a release
//! build wraps on overflow — a package whose total wrapped to a small number would then *pass*
//! [`validate`] against an equally wrapped total, which is the one way a consistency check can
//! report success on nonsense. Saturating addition cannot wrap, so the declared total and the
//! recomputed total both stop at [`usize::MAX`] and still agree. [`manifest`] and [`validate`] use
//! the same saturation rule on purpose: a builder and a checker that summed differently would
//! reject packages that are in fact intact.

use serde::{Deserialize, Serialize};

/// The format version this build writes and accepts.
///
/// Bump it when the manifest's shape changes incompatibly, and teach [`validate`] to read the
/// previous version in the same change. A reader accepts only the versions it knows, so an
/// unrecognised number is a refusal rather than a guess.
pub const VERSION: u32 = 1;

/// One session listed in an exported package.
///
/// Deliberately carries no events. The count is here so a caller can size a transfer, show a
/// progress total, or compare the package against the source without reading any payload; the
/// payload itself is a [`crate::session_portable::Bundle`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The session id, as it appears in the session layout and in a payload's file name.
    pub id: String,
    /// The title the session had at export time, when it had one.
    ///
    /// Optional because an unnamed session and a session titled with an empty string are different
    /// to a reader, and a listing that rendered both as `""` would hide that difference. It is also
    /// `skip_serializing_if`, so a package of unnamed sessions stays small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// How many events the session's payload holds.
    ///
    /// Not how many events are readable in a rendered page: rendering truncates, the payload does
    /// not. This number describes the transport payload.
    pub events: usize,
}

/// The manifest of an exported package.
///
/// This is the whole contract with another instance or another machine: a version a reader can
/// check, a timestamp for provenance, the list of sessions, and a total it can be held to. Nothing
/// in it is derived at read time, so a package validated once does not need to be validated against
/// a live session store again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    /// The format version, so a future reader knows what it is looking at.
    pub version: u32,
    /// When the package was exported, in milliseconds since the Unix epoch.
    ///
    /// Provenance only: [`validate`] never consults it, so a package written by a machine with a
    /// skewed clock still validates.
    pub exported_at: u64,
    /// The sessions in the package, in the order the caller listed them.
    ///
    /// Order is preserved rather than sorted: the caller may have listed by recency, and a package
    /// that reordered its own contents would be describing a different listing than the one asked
    /// for.
    pub entries: Vec<Entry>,
    /// The sum of [`Entry::events`] over [`Bundle::entries`], as declared by the exporter.
    ///
    /// Redundant by construction, and checked by [`validate`]: see the module documentation for why
    /// a derivable number is still worth carrying.
    pub total_events: usize,
}

/// Why a package manifest was refused.
///
/// Three variants, because there are exactly three ways a manifest can be wrong before any payload
/// is considered: it is from another format version, it describes nothing, or it contradicts
/// itself.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BundleError {
    /// The manifest's format version is not one this build reads.
    #[error(
        "package format version {found} is not supported; this build reads version {supported}"
    )]
    UnsupportedVersion {
        /// The version the manifest declares.
        found: u32,
        /// The version this build writes and reads.
        supported: u32,
    },

    /// The declared total does not equal the sum over the entries.
    #[error(
        "package declares {declared} events but its {actual} entries sum differently: \
         the manifest was damaged or built incorrectly"
    )]
    CountMismatch {
        /// [`Bundle::total_events`], as the package states it.
        declared: usize,
        /// The saturation-checked sum over [`Bundle::entries`].
        actual: usize,
    },

    /// The package lists no sessions.
    #[error("the package lists no sessions; an empty backup is not a backup")]
    Empty,
}

/// Builds a manifest from a listing.
///
/// Each tuple is `(id, title, event count)`, taken from whatever the caller enumerated — a project
/// directory, a session index, a set of payloads already exported. Passing tuples rather than
/// [`Entry`] values keeps the call site from repeating field names for data it already has in that
/// shape, and it keeps this function the single place that computes the total.
///
/// `now_ms` is a parameter rather than a call to the clock so the builder is deterministic: a test
/// can assert exact bytes, and a caller exporting many sessions can stamp them all with one instant
/// instead of a different one per entry.
///
/// The returned manifest is not validated: this function cannot produce a count mismatch, and it
/// will happily return an empty manifest when `entries` is empty, because refusing here would only
/// move [`BundleError::Empty`] from the reader's report to an unwrap at the writer's call site. Run
/// [`validate`] on the result before publishing it.
#[must_use]
pub fn manifest(entries: Vec<(String, Option<String>, usize)>, now_ms: u64) -> Bundle {
    let mut total_events = 0usize;
    let mut listed = Vec::with_capacity(entries.len());
    for (id, title, events) in entries {
        // Saturating, and matched exactly by `validate`, so a package too large to total in a
        // `usize` still validates instead of being rejected as corrupt.
        total_events = total_events.saturating_add(events);
        listed.push(Entry { id, title, events });
    }

    Bundle {
        version: VERSION,
        exported_at: now_ms,
        entries: listed,
        total_events,
    }
}

/// Checks that a manifest is one this build can act on.
///
/// The order of the checks is the contract, not an implementation detail:
///
/// 1. **Version.** A future format must not be measured against this build's rules, so an unknown
///    version is reported before any content is inspected — including before the empty check, since
///    "your package is empty" would be a false and misleading diagnosis of a v2 file.
/// 2. **Emptiness.** A package with no entries has no total worth checking; reporting a count
///    mismatch on `0 == 0` would only be possible by inventing an error, and reporting success
///    would accept the one manifest that is certain to be a mistake.
/// 3. **Consistency.** The declared total is recomputed from the entries and compared.
///
/// The recomputation uses the same saturating addition as [`manifest`], so the check accepts every
/// package that builder could have produced, however large.
///
/// # Errors
///
/// - [`BundleError::UnsupportedVersion`] when [`Bundle::version`] is not [`VERSION`].
/// - [`BundleError::Empty`] when [`Bundle::entries`] is empty.
/// - [`BundleError::CountMismatch`] when [`Bundle::total_events`] differs from the sum of the
///   entries' event counts.
pub fn validate(bundle: &Bundle) -> Result<(), BundleError> {
    if bundle.version != VERSION {
        return Err(BundleError::UnsupportedVersion {
            found: bundle.version,
            supported: VERSION,
        });
    }

    if bundle.entries.is_empty() {
        return Err(BundleError::Empty);
    }

    let actual = bundle
        .entries
        .iter()
        .fold(0usize, |sum, entry| sum.saturating_add(entry.events));

    if actual != bundle.total_events {
        return Err(BundleError::CountMismatch {
            declared: bundle.total_events,
            actual,
        });
    }

    Ok(())
}
