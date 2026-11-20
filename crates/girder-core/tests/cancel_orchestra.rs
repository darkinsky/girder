//! Tests for `cancel_orchestra`: the two phases of a cancellation as one call, and the question
//! "is it really stopped".
//!
//! The three properties worth stating outright, because they are what the module is for and a
//! refactor could break any of them silently:
//!
//! - `cancel_and_clean` does *both* phases (`cancel_and_clean_marks_and_cleans`).
//! - Marking alone is not stopping: a bare `cancel` leaves `is_quiescent` false while the very work
//!   the mark was supposed to release is still running (`cancel_without_cleanup_is_not_quiescent`).
//! - The answer stays honest across repeated calls and across a later registration
//!   (`repeat_calls_are_idempotent`, `work_registered_after_a_settled_drain_runs_immediately`).
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/core/cancel_orchestra.rs"]
pub mod cancel_orchestra;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cancel_orchestra::{Cancelled, cancel_and_clean, is_quiescent};
use girder_core::CancelScope as Scope;

/// A cleanup that counts its own runs.
fn counter() -> (Arc<AtomicUsize>, Box<dyn FnOnce() + Send>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&runs);
    let action = Box::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    });
    (runs, action)
}

/// How many times `runs` has been incremented.
fn ran(runs: &Arc<AtomicUsize>) -> usize {
    runs.load(Ordering::SeqCst)
}

#[test]
fn cancel_and_clean_marks_and_cleans() {
    let root = Scope::root();
    let (runs, action) = counter();
    root.on_cancel(action);

    let outcome = cancel_and_clean(&root);

    assert_eq!(
        outcome,
        Cancelled {
            scopes: 1,
            cleanups: 1,
            complete: true,
        },
        "one live tree marked, the drain observed, both phases finished"
    );
    assert!(root.is_cancelled(), "the mark is part of the call");
    assert_eq!(ran(&runs), 1, "the cleanup must have run exactly once");
    assert!(is_quiescent(&root), "a drained tree is stopped");
}

#[test]
fn cancel_without_cleanup_is_not_quiescent() {
    let root = Scope::root();
    let (runs, action) = counter();
    root.on_cancel(action);

    // The trap: the mark alone. New work is refused, nothing in flight is released.
    root.cancel();

    assert!(root.is_cancelled(), "the tree is marked");
    assert_eq!(
        ran(&runs),
        0,
        "the cleanup did not run, so the work still holds"
    );
    assert!(
        !is_quiescent(&root),
        "marked is not stopped: quiescence must not restate `is_cancelled`"
    );

    // The same tree, drained afterwards, is stopped — so the `false` above was about the missing
    // phase and not about the tree being unanswerable.
    let outcome = cancel_and_clean(&root);
    assert!(outcome.complete);
    assert_eq!(ran(&runs), 1);
    assert!(is_quiescent(&root));
}

#[test]
fn cleanup_before_mark_does_nothing_and_is_not_quiescent() {
    let root = Scope::root();
    let (runs, action) = counter();
    root.on_cancel(action);

    girder_core::run_cleanups(&root);

    assert_eq!(
        ran(&runs),
        0,
        "`run_cleanups` skips a scope that is not cancelled"
    );
    assert!(!is_quiescent(&root), "a live tree has to answer not-quiet");

    assert!(cancel_and_clean(&root).complete);
    assert!(is_quiescent(&root));
}

#[test]
fn repeated_calls_are_idempotent() {
    let root = Scope::root();
    let (runs, action) = counter();
    root.on_cancel(action);

    let first = cancel_and_clean(&root);
    assert_eq!(
        first,
        Cancelled {
            scopes: 1,
            cleanups: 1,
            complete: true,
        }
    );

    for _ in 0..3 {
        let again = cancel_and_clean(&root);
        assert_eq!(
            again,
            Cancelled {
                scopes: 0,
                cleanups: 0,
                complete: true,
            },
            "a second call marks nothing new and runs nothing new"
        );
        assert!(again.complete, "an already-settled tree is still complete");
        assert!(is_quiescent(&root));
        assert_eq!(
            ran(&runs),
            1,
            "the cleanup action runs exactly once in total"
        );
    }
}

#[test]
fn child_scope_is_cleaned() {
    let root = Scope::root();
    let child = root.child();
    let grandchild = child.child();

    let (root_runs, root_action) = counter();
    let (child_runs, child_action) = counter();
    let (grandchild_runs, grandchild_action) = counter();
    root.on_cancel(root_action);
    child.on_cancel(child_action);
    grandchild.on_cancel(grandchild_action);

    let outcome = cancel_and_clean(&root);

    assert!(outcome.complete);
    assert_eq!(ran(&root_runs), 1, "the root's own cleanup runs");
    assert_eq!(ran(&child_runs), 1, "a descendant's cleanup runs too");
    assert_eq!(ran(&grandchild_runs), 1, "so does a grandchild's");
    assert!(is_quiescent(&root), "the whole tree is stopped");
    assert!(
        child.is_cancelled() && grandchild.is_cancelled(),
        "the mark reached every descendant"
    );
}

#[test]
fn cancelling_a_child_leaves_the_parent_running() {
    let root = Scope::root();
    let child = root.child();
    let (runs, action) = counter();
    child.on_cancel(action);

    assert!(cancel_and_clean(&child).complete);

    assert_eq!(ran(&runs), 1);
    assert!(is_quiescent(&child), "the child's own tree is stopped");
    assert!(!root.is_cancelled(), "cancellation does not travel upward");
    assert!(
        !is_quiescent(&root),
        "the parent is still live, whatever the child's record says"
    );
}

#[test]
fn empty_tree_is_quiescent() {
    let root = Scope::root();

    assert!(
        !is_quiescent(&root),
        "a fresh tree runs work, so it is not stopped"
    );

    let outcome = cancel_and_clean(&root);
    assert_eq!(
        outcome,
        Cancelled {
            scopes: 1,
            cleanups: 1,
            complete: true,
        },
        "an empty tree is still a tree: marked and drained"
    );
    assert!(is_quiescent(&root));
}

#[test]
fn work_registered_after_a_settled_drain_runs_immediately() {
    let root = Scope::root();
    assert!(cancel_and_clean(&root).complete);

    // `Scope::on_cancel` runs on registration when the scope is already cancelled, so no teardown can
    // be left pending behind a tree this module already called quiet.
    let (runs, action) = counter();
    root.on_cancel(action);

    assert_eq!(ran(&runs), 1);
    assert!(
        is_quiescent(&root),
        "quiescence survives a later registration"
    );
}

#[test]
fn distinct_trees_do_not_borrow_each_other_s_quiescence() {
    let settled = Scope::root();
    assert!(cancel_and_clean(&settled).complete);

    let other = Scope::root();
    other.cancel();

    assert!(is_quiescent(&settled));
    assert!(
        !is_quiescent(&other),
        "the record is keyed by scope, not global: another marked tree is not quiet"
    );
}

#[test]
fn a_drain_this_module_did_not_perform_is_not_claimed_as_quiescent() {
    // The documented limit, asserted so it cannot drift into a silent over-claim: quiescence is
    // observed by this module, so a bare `run_cleanups` by someone else reads as not quiet. The
    // false negative is the safe direction — the reverse would call a running tree stopped.
    let root = Scope::root();
    root.cancel();
    girder_core::run_cleanups(&root);

    assert!(!is_quiescent(&root));
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod core {
    pub use super::cancel_orchestra as cancel_orchestra;
}
