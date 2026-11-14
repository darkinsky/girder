//! How much disk a deployment's sessions occupy, and which ones to give up when that is too much.
//!
//! Two neighbouring modules already bound a session, but neither bounds the *directory*.
//! [`crate::session_archive`] moves a log's oldest events into a sibling archive so that the live log
//! stays small — the bytes are kept, only their location changes. [`crate::usage_quota`] bounds
//! tokens spent per window, which is a cost ceiling at the provider, not a statement about local
//! storage. So a deployment with ten thousand sessions has no number that answers "how much is on
//! this disk", and no policy that decides what to do when the filesystem is full.
//!
//! That is the gap this module fills, and it fills exactly one half of it: the arithmetic. Deciding a
//! victim and deleting it are separated on purpose.
//!
//! # Why footprints are passed in rather than measured here
//!
//! Measuring means `stat`-ing every session's files, which is I/O, which means an error path, a
//! timeout, and a lock held across a directory walk. None of that belongs in a policy decision, and
//! all of it makes the decision untestable without a filesystem. The caller already has the layouts
//! and the metadata: it builds a `&[Footprint]` and this module answers a pure question about it.
//! The same arrangement as [`crate::attachment_limits`], and for the same reason.
//!
//! # Why eviction is by age and not by size
//!
//! Deleting the largest sessions frees the most space per delete, which is the tempting metric and
//! the wrong one. A large session is usually a *recent* one — a long working conversation that is
//! still being read — while a hundred small ones are usually last quarter's leftovers. Ranking by
//! size therefore spends the deployment's most valuable data to save the smallest amount of work,
//! and it does so in a way that looks efficient in a report. Age is the property that actually
//! tracks "nobody will miss this", so [`evict`] sorts by modification time, oldest first, and the
//! byte count only decides how far down the list it has to go.
//!
//! # Why [`Quota::evict_fraction`] exists
//!
//! Freeing exactly the overflow would leave the directory at precisely its ceiling, so the next
//! append crosses it again and the next pass evicts again. A deployment doing that is not enforcing
//! a quota, it is deleting one session per write forever. The fraction makes one pass free enough
//! headroom that the next pass is a scheduled check rather than an immediate consequence of the
//! previous one. It is a fraction of the *ceiling* rather than of the usage: a budget stated as "keep
//! 90% of the limit free" survives a change in how full the disk already was.
//!
//! # Why [`is_oversized`] is a separate question
//!
//! A single session larger than the per-session cap cannot be fixed by evicting anything else.
//! Evicting its neighbours to make room for it is how one runaway log starves a deployment, and the
//! loop that does it terminates only when the directory is empty. Reporting it as its own predicate
//! lets a caller take a different remedy — refuse the write, archive harder, or ask a human — without
//! this module having to know which. [`evict`] does not consult it: the two questions have different
//! answers and different fixes.
//!
//! # Why the candidate list is the caller's
//!
//! A session can be in use: a running turn holds its write lock and appends to it, so deleting it
//! loses the events of the turn in progress and leaves a writer appending to an unlinked file. This
//! module cannot tell which sessions those are — that knowledge lives with the session store — so it
//! does not guess. [`usage`] is computed over everything the caller passes (a total that omitted the
//! live sessions would understate the disk), and [`evict`] returns victims from the same slice. The
//! caller keeps the live ones out of the slice it hands to [`evict`], or filters the result; either
//! way the set of untouchable sessions is expressed where it can actually be known.
//!
//! # A zero ceiling is a hard stop
//!
//! `Quota::max_bytes == 0` means nothing may be stored, so every candidate is a victim. Reading zero
//! as "unlimited" would invert a hard stop into an open door, the same convention
//! [`crate::usage_quota`] states for its token ceilings.

use std::cmp::Ordering;

/// One session's footprint on disk.
///
/// Split into a log and its sidecars because they are produced and replaced by different code: the
/// log grows with every event, while a title or an archive mark is one small write that may or may
/// not exist. Keeping them apart lets a report say *what* is large, which is the difference between
/// "this session is long" and "this session's metadata got out of hand". [`Footprint::total`] is the
/// sum, and it is the only number the quota arithmetic uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Footprint {
    /// The session id this footprint describes.
    pub session: String,
    /// Bytes in the event log (`{id}.jsonl.zst`).
    pub log_bytes: u64,
    /// Bytes in the companion files — the title and the archive mark.
    pub sidecar_bytes: u64,
    /// Last modification, in milliseconds since the Unix epoch.
    ///
    /// Milliseconds and not seconds because eviction decides between sessions that may have been
    /// written within the same second by one import; a coarser unit would rank them arbitrarily.
    pub modified_ms: u64,
}

impl Footprint {
    /// Total bytes this session occupies.
    ///
    /// Saturating: a sum that overflowed `u64` would be a filesystem claiming more bytes than an
    /// address space can express, and wrapping there would report a huge session as a tiny one — the
    /// one direction of error that makes eviction pick the wrong victim. Saturation keeps a broken
    /// measurement visibly enormous instead of silently wrong.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.log_bytes.saturating_add(self.sidecar_bytes)
    }
}

/// What a set of sessions currently occupies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Sum of every footprint's [`Footprint::total`].
    pub total_bytes: u64,
    /// How many sessions were measured.
    pub sessions: usize,
    /// The oldest modification time, or `None` when nothing was measured.
    pub oldest_ms: Option<u64>,
    /// The newest modification time, or `None` when nothing was measured.
    pub newest_ms: Option<u64>,
}

/// Totals a set of footprints.
///
/// The session count is the length of the slice, not the number of non-empty footprints: an empty log
/// is still a session that occupies an inode and appears in a listing, and a count that skipped it
/// would disagree with every other count in this crate.
///
/// The bounds are `Option` rather than `0` for an empty slice. Zero is a valid timestamp (the epoch),
/// so a `0` sentinel would make "no sessions" and "the oldest session is from 1970" the same value,
/// and the caller could not tell a fresh install from a clock bug.
#[must_use]
pub fn usage(footprints: &[Footprint]) -> Usage {
    let mut total_bytes: u64 = 0;
    let mut oldest_ms: Option<u64> = None;
    let mut newest_ms: Option<u64> = None;

    for footprint in footprints {
        total_bytes = total_bytes.saturating_add(footprint.total());
        oldest_ms = Some(oldest_ms.map_or(footprint.modified_ms, |m| m.min(footprint.modified_ms)));
        newest_ms = Some(newest_ms.map_or(footprint.modified_ms, |m| m.max(footprint.modified_ms)));
    }

    Usage {
        total_bytes,
        sessions: footprints.len(),
        oldest_ms,
        newest_ms,
    }
}

/// A storage ceiling, and how much headroom one pass should leave.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quota {
    /// Most bytes the session directory may occupy.
    ///
    /// `0` is a hard stop — nothing may be stored — rather than "unlimited"; see the module
    /// documentation.
    pub max_bytes: u64,
    /// Extra fraction of `max_bytes` to free once the ceiling is crossed, in `0.0..=1.0`.
    ///
    /// `0.1` frees the overflow plus a tenth of the ceiling, so the next write does not immediately
    /// trigger another pass. Values outside the range are clamped rather than refused: a negative or
    /// `NaN` fraction still leaves a working policy (free only the overflow), and refusing the whole
    /// quota would turn a bad tuning knob into a directory that grows without bound.
    pub evict_fraction: f64,
}

/// The byte total eviction must leave behind.
///
/// Kept as a fraction of the ceiling rather than of the current usage: the target then depends only
/// on configuration, so two deployments with different amounts of data converge on the same
/// steady-state size.
///
/// The cast is bounded before it happens — the ratio is clamped to `0.0..=1.0` and the product to
/// `max_bytes` — so truncation there cannot produce a target above the ceiling or below zero.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a byte ceiling above 2^53 is unreachable, and the product is clamped into range before the cast"
)]
fn target_bytes(quota: &Quota) -> u64 {
    // A `NaN` comparison is false everywhere, so it is normalized explicitly instead of relying on
    // `clamp`'s behaviour, which propagates `NaN` and would make the loop's stop condition false
    // forever.
    let fraction = if quota.evict_fraction.is_nan() {
        0.0
    } else {
        quota.evict_fraction.clamp(0.0, 1.0)
    };
    let scaled = quota.max_bytes as f64 * (1.0 - fraction);
    if scaled <= 0.0 {
        0
    } else if scaled >= quota.max_bytes as f64 {
        quota.max_bytes
    } else {
        scaled as u64
    }
}

/// Chooses which sessions to delete so the directory fits under its quota.
///
/// Victims are taken oldest first — see the module documentation for why age rather than size — and
/// the walk stops as soon as the remaining total is at or below the post-eviction target. Only when
/// the ceiling is actually crossed does anything get returned: a directory that is already inside its
/// quota is left completely alone, even if `evict_fraction` is large, because a policy that trimmed
/// on every pass would delete data to celebrate a budget it was not over.
///
/// The returned names are in eviction order, so a caller can delete in that order and log a
/// meaningful trail. Ties on `modified_ms` are broken by session id: eviction is destructive, and a
/// choice that depended on the order the caller happened to list its directory would pick different
/// victims for the same disk on two consecutive runs.
///
/// # Untouchable sessions
///
/// This function has no notion of a session in use. A caller must pass only the footprints it is
/// willing to lose; see the module documentation for why that set belongs to the caller.
#[must_use]
pub fn evict(footprints: &[Footprint], quota: &Quota) -> Vec<String> {
    let measured = usage(footprints);
    if measured.total_bytes <= quota.max_bytes {
        return Vec::new();
    }

    let mut order: Vec<&Footprint> = footprints.iter().collect();
    // `sort_by` rather than `sort_unstable_by`: the comparator is total, so both are correct, but a
    // stable sort keeps `order` and `eviction_order` trivially in agreement about the result.
    order.sort_by(|left, right| eviction_order(left, right));

    let target = target_bytes(quota);
    let mut remaining = measured.total_bytes;
    let mut victims = Vec::new();

    for footprint in order {
        if remaining <= target {
            break;
        }
        remaining = remaining.saturating_sub(footprint.total());
        victims.push(footprint.session.clone());
    }

    victims
}

/// Whether one session is too large to be allowed regardless of how much room the disk has.
///
/// A single session over the per-session cap is a different failure from a full disk: evicting its
/// neighbours frees space that the offending log will simply consume again, so [`evict`] can empty
/// the directory without ever bringing the deployment under its limit. The comparison is inclusive —
/// a session of exactly `max_single_bytes` is admissible — matching every other size limit in this
/// crate, so that a cap of one megabyte admits a one-megabyte file.
#[must_use]
pub fn is_oversized(footprint: &Footprint, max_single_bytes: u64) -> bool {
    footprint.total() > max_single_bytes
}

/// The ordering [`evict`] sorts by: oldest first, then by id.
///
/// The id tiebreak is what makes the order *total*. Eviction is destructive, so an order that left
/// ties unresolved would pick different victims for the same disk on two consecutive runs, purely
/// because the caller listed its directory in a different order.
///
/// Kept private: a caller that wants a general sort has [`crate::session_filter`], and exporting a
/// second public ordering would invite two answers to "which session is older" that could drift
/// apart.
fn eviction_order(left: &Footprint, right: &Footprint) -> Ordering {
    left.modified_ms
        .cmp(&right.modified_ms)
        .then_with(|| left.session.cmp(&right.session))
}
