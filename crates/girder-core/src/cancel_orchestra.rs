//! One call that performs both halves of a cancellation, so that neither half can be forgotten.
//!
//! # The trap this module exists for
//!
//! `Scope::cancel` marks a tree. It does not stop any work. The token a running loop polls is
//! cancelled by a cleanup its owner registered with `Scope::on_cancel`, and those actions run only
//! from [`girder_core::run_cleanups`]. A caller that reads `cancel` as "done" therefore gets the
//! worst of both worlds: new work is refused, because the tree is marked, while the work already in
//! flight keeps running and keeps holding the resources the cancellation was meant to free.
//!
//! Neither half is usable on its own, and each failure is silent:
//!
//! - *Mark without draining* releases nothing. The only difference from a finished teardown is the
//!   work that quietly continues, which is exactly what an operator does not look at.
//! - *Drain before marking* does nothing at all: `run_cleanups` skips every node that is not
//!   cancelled, so the teardown never happens and the caller has no way to tell.
//!
//! Making the pair one call removes the possibility of writing only one of them. The order inside
//! is marking first: a cleanup that releases a token must not run while a concurrent caller can
//! still be admitted by the tree.
//!
//! # Why quiescence is not cancellation
//!
//! `Scope::is_cancelled` answers "was this asked to stop". The question an operator or a test
//! actually has is "is it stopped", and the two differ by the drain. [`is_quiescent`] answers the
//! second one, because a tree that is still running under a mark is the state this module is about.
//!
//! # Why this module keeps a record
//!
//! The crate publishes no "cleanups have run" flag, and that is deliberate: the drain happens
//! inside one call and leaves no mark on the [`Scope`] that a later reader could inspect. So
//! quiescence is answered from what this module observed itself:
//!
//! - Before marking a live tree, [`cancel_and_clean`] registers an action of its own, a *probe*. A
//!   probe registered on a still-live scope is queued, and a queued action runs only if a drain
//!   reaches that scope. Its firing is therefore evidence that the drain happened, rather than a
//!   restatement of the documented contract.
//! - When that evidence arrives, the tree is remembered as settled, and [`is_quiescent`] answers
//!   from that record.
//!
//! The consequence, stated plainly: a tree torn down by a bare `run_cleanups` reads as *not*
//! quiescent. That is a false negative, and it is the safe direction — claiming quiet while work
//! still runs is the bug this module exists to make impossible.
//!
//! Quiescence is monotone. After a drain, an action registered later on the marked scope runs
//! immediately instead of being queued (that is `Scope::on_cancel`'s contract for a cancelled
//! scope), so no pending teardown can reappear behind a tree this module already called quiet.
//!
//! # Identity, and why it comes from `Debug`
//!
//! [`Scope`] publishes no handle that can be hashed or compared, and the tree cannot be walked from
//! outside: there is no public accessor for children. The `Debug` rendering is the only public
//! surface that names a scope, and it prints the id as its first field. A rendering that does not
//! parse yields `None`, which is never treated as quiet — so a change to that format degrades into
//! "cannot say quiet" instead of into a wrong "quiet".
//!
//! A descendant is not tracked separately, for the same reason it cannot be enumerated: ask about
//! the scope the tree was torn down through, which is the handle [`cancel_and_clean`] was given.
//!
//! # The record is bounded
//!
//! A dropped tree leaves nothing behind to notify anyone, so an entry cannot be reclaimed when its
//! tree dies. The set is capped at [`REMEMBERED_TREES`] trees and the oldest entries are evicted
//! first; a tree old enough to be evicted reads as not quiet again, which is the same safe
//! direction as a false negative above.
//!
//! # Example
//!
//! ```text
//! let root = Scope::root();
//! let child = root.child();
//! child.on_cancel(Box::new(|| eprintln!("child released its slot")));
//! let outcome = cancel_and_clean(&root);   // marks the tree and runs the child's action
//! assert!(outcome.complete);
//! assert!(is_quiescent(&root));            // asked "is it stopped", not "was it asked"
//! ```

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use girder_core::CancelScope as Scope;

/// How many settled trees the record keeps before it forgets the oldest one.
///
/// A cap rather than an unbounded set: a tree that is dropped cannot announce it, so the only
/// lifecycle event available is eviction. Four thousand is far more trees than a session creates,
/// and small enough that a long-lived process cannot grow this without bound.
const REMEMBERED_TREES: usize = 4096;

/// Trees this module has marked and then verified as drained.
///
/// Ids come from the scope's `Debug` rendering and rise monotonically, so the smallest id is the
/// oldest tree and `pop_first` is the eviction order.
static QUIET_TREES: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());

/// The complete result of one cancellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cancelled {
    /// How many scopes this call marked.
    ///
    /// `1` when this call found the tree live and marked it, `0` when it was already marked.
    /// Cancellation does propagate to descendants inside `cancel`, but the public surface offers no
    /// way to enumerate a tree, so descendants are not counted: `1` means *this* call marked the
    /// tree, and [`is_quiescent`] is the tree-wide question.
    pub scopes: usize,

    /// How many cleanup actions this call ran and could account for.
    ///
    /// The module arms one probe action of its own before marking, and the probe is the only action
    /// it can observe. `1` means the probe fired, so the drain reached the scope; `0` means this call
    /// had nothing of its own to count — the tree was already marked, so a probe would have fired
    /// immediately and proved nothing about the drain. Actions other callers registered run in the
    /// same drain, but the crate reports no count for them.
    pub cleanups: usize,

    /// Whether both phases finished, with the drain observed rather than assumed.
    ///
    /// `false` means the tree is marked but the drain did not reach the probe. The mark stands, so
    /// new work is still refused, but [`is_quiescent`] will not call this tree settled.
    pub complete: bool,
}

/// Cancels `root`'s tree and runs its cleanups, in that order.
///
/// **The two phases belong together** — marking without draining leaves the work running, and
/// draining before marking does nothing at all. This call is the only supported way to do both, and
/// it reports what it could verify rather than what it hoped for: see [`Cancelled`].
///
/// Safe to call more than once: the second call finds the tree marked, re-runs the drain (a no-op
/// once everything has been drained, and the drain itself is documented as idempotent), and reports
/// `scopes: 0, cleanups: 0, complete: true`.
#[must_use]
pub fn cancel_and_clean(root: &Scope) -> Cancelled {
    let was_live = !root.is_cancelled();

    // Armed while the tree is still live on purpose: a probe queued on a live scope runs only when
    // a drain reaches it, so its firing says something the caller could not have known otherwise.
    // On an already-marked tree the probe would run immediately and prove nothing, which is why it
    // is not armed there.
    let probe = if was_live {
        Some(arm_probe(root))
    } else {
        None
    };

    root.cancel();
    girder_core::run_cleanups(root);

    let fired = probe
        .as_ref()
        .is_some_and(|probe| probe.load(Ordering::SeqCst));
    let drained = probe.is_none() || fired;
    let complete = root.is_cancelled() && drained;

    // Only a verified teardown is recorded. Recording an unverified one would make
    // `is_quiescent` agree with a caller's assumption instead of with the tree.
    if complete {
        if let Some(id) = scope_id(root) {
            remember_settled(id);
        }
    }

    Cancelled {
        scopes: usize::from(was_live),
        cleanups: usize::from(fired),
        complete,
    }
}

/// Whether `root`'s tree is marked **and** its cleanups have been verified to run.
///
/// This is the question "is it really stopped", not "was it asked to stop"; a tree that was only
/// cancelled answers `false`. Pass the scope whose tree was torn down — normally the same handle
/// given to [`cancel_and_clean`].
///
/// The answer is a record kept by this module, not a flag read from the tree, because the tree has
/// no such flag: a settled tree that was drained by a bare `run_cleanups` also answers `false`.
#[must_use]
pub fn is_quiescent(root: &Scope) -> bool {
    // A live tree is not quiet whatever the record says: new work is still being admitted, and work
    // can start under it at any moment.
    if !root.is_cancelled() {
        return false;
    }
    scope_id(root).is_some_and(was_settled)
}

/// Registers a probe cleanup and returns the flag it sets when it runs.
fn arm_probe(scope: &Scope) -> Arc<AtomicBool> {
    let fired = Arc::new(AtomicBool::new(false));
    let marker = Arc::clone(&fired);
    // `SeqCst` on both sides: the flag is how a drain on this thread is observed from the caller's,
    // and the cost is one atomic per teardown.
    scope.on_cancel(Box::new(move || marker.store(true, Ordering::SeqCst)));
    fired
}

/// The scope's id, parsed from the only public surface that exposes one.
///
/// `None` rather than a made-up id when the rendering does not parse: an unknown identity must not
/// be able to make two different trees look like the same one.
fn scope_id(scope: &Scope) -> Option<u64> {
    const PREFIX: &str = "id: ";

    // The id is the first field of the rendering (`Scope { id: 7, .. }`), so the first match is the
    // real one even if a cancel reason's detail text happens to contain the same prefix later on.
    let rendered = format!("{scope:?}");
    let start = rendered.find(PREFIX)? + PREFIX.len();
    let digits: String = rendered[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// The settled-tree record, ignoring poisoning.
///
/// A panic in a cleanup must not turn every later query into a panic: the set holds plain ids and is
/// never left half-written, so the inner value is sound.
fn lock_quiet() -> MutexGuard<'static, BTreeSet<u64>> {
    QUIET_TREES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Records `id` as a tree this module marked and then verified as drained.
fn remember_settled(id: u64) {
    let mut quiet = lock_quiet();
    if quiet.contains(&id) {
        return;
    }
    if quiet.len() >= REMEMBERED_TREES {
        // Discarded on purpose: eviction is best-effort housekeeping, and the returned entry has no
        // use here.
        let _evicted = quiet.pop_first();
    }
    quiet.insert(id);
}

/// Whether `id` is in the settled-tree record.
fn was_settled(id: u64) -> bool {
    lock_quiet().contains(&id)
}
