//! Cancellation scopes: propagation, reasons, and the exactly-once cleanup contract.
//!
//! The two properties worth stating outright, because they are the ones a refactor would break
//! without failing anything else: a child follows its parent (`child_follows_parent`), and a child
//! never cancels its parent (`child_cancel_does_not_reach_parent`).
//!
//! These go through the crate's public surface rather than a private path, so a re-export that
//! stopped carrying an item would fail here instead of leaving the API untested. `Scope` is bound
//! to [`CancelScope`], the name the crate exports it under: `scope::Scope` already owns the plain
//! name.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use girder_core::{CancelReason, CancelScope as Scope, run_cleanups};

/// A cleanup that counts its own runs.
fn counter() -> (Arc<AtomicUsize>, Box<dyn FnOnce() + Send>) {
    let runs = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&runs);
    let action = Box::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
    });
    (runs, action)
}

#[test]
fn fresh_root_is_not_cancelled() {
    let root = Scope::root();
    assert!(!root.is_cancelled());
    assert_eq!(root.reason(), None);
}

#[test]
fn root_cancel_marks_itself() {
    let root = Scope::root();
    root.cancel();
    assert!(root.is_cancelled());
    assert_eq!(root.reason(), Some(CancelReason::UserRequest));
}

// The core property: cancelling the parent stops the child, without the child being told directly.
#[test]
fn child_follows_parent() {
    let root = Scope::root();
    let child = root.child();
    assert!(!child.is_cancelled());

    root.cancel();

    assert!(root.is_cancelled());
    assert!(
        child.is_cancelled(),
        "a child must stop when its parent does"
    );
}

// The other core property: propagation is one-way.
#[test]
fn child_cancel_does_not_reach_parent() {
    let root = Scope::root();
    let child = root.child();

    child.cancel();

    assert!(child.is_cancelled());
    assert!(
        !root.is_cancelled(),
        "a child stopping says nothing about its parent"
    );
    assert_eq!(root.reason(), None);
    assert_eq!(child.reason(), Some(CancelReason::UserRequest));
}

// A child stops because of *its* parent, and says so; copying the parent's reason would name the
// wrong scope as the one that was asked to stop.
#[test]
fn child_reason_is_parent_not_the_ancestors_reason() {
    let root = Scope::root();
    let child = root.child();

    root.cancel_with(CancelReason::Timeout { after_ms: 500 });

    assert_eq!(root.reason(), Some(CancelReason::Timeout { after_ms: 500 }));
    assert_eq!(
        child.reason(),
        Some(CancelReason::Parent),
        "the child was never asked to stop; its parent was"
    );
}

#[test]
fn cancellation_reaches_every_level() {
    let root = Scope::root();
    let child = root.child();
    let grandchild = child.child();
    let great_grandchild = grandchild.child();

    root.cancel();

    assert!(child.is_cancelled());
    assert!(grandchild.is_cancelled());
    assert!(great_grandchild.is_cancelled());
    assert_eq!(grandchild.reason(), Some(CancelReason::Parent));
    assert_eq!(great_grandchild.reason(), Some(CancelReason::Parent));
}

#[test]
fn sibling_isolation() {
    let root = Scope::root();
    let first = root.child();
    let second = root.child();

    first.cancel();

    assert!(first.is_cancelled());
    assert!(
        !second.is_cancelled(),
        "cancelling one branch must not stop another"
    );
    assert!(!root.is_cancelled());
}

#[test]
fn only_the_first_reason_is_kept() {
    let root = Scope::root();

    root.cancel_with(CancelReason::Timeout { after_ms: 30 });
    // A second, later cause describes a cancellation that had already happened.
    root.cancel_with(CancelReason::Error {
        detail: "late".to_owned(),
    });
    root.cancel();

    assert_eq!(
        root.reason(),
        Some(CancelReason::Timeout { after_ms: 30 }),
        "the cause that stopped the work is the one worth reporting"
    );
}

#[test]
fn cancelling_twice_is_not_an_error() {
    let root = Scope::root();
    let child = root.child();

    root.cancel();
    root.cancel();
    child.cancel();

    assert!(root.is_cancelled());
    assert!(child.is_cancelled());
    assert_eq!(root.reason(), Some(CancelReason::UserRequest));
}

#[test]
fn child_created_under_a_cancelled_parent_starts_cancelled() {
    let root = Scope::root();
    root.cancel();

    let late = root.child();

    assert!(
        late.is_cancelled(),
        "work started in a stopped scope must not run"
    );
    assert_eq!(late.reason(), Some(CancelReason::Parent));
}

#[test]
fn cleanups_do_not_run_while_live() {
    let root = Scope::root();
    let (runs, action) = counter();
    root.on_cancel(action);

    run_cleanups(&root);

    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "teardown of work that was never stopped is the caller's business, not a cancellation"
    );
}

#[test]
fn cleanup_runs_when_cancelled() {
    let root = Scope::root();
    let (runs, action) = counter();
    root.on_cancel(action);

    root.cancel();
    run_cleanups(&root);

    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

// Exactly once, however many times cancellation and teardown happen.
#[test]
fn cleanup_runs_only_once() {
    let root = Scope::root();
    let (runs, action) = counter();
    root.on_cancel(action);

    root.cancel();
    run_cleanups(&root);
    run_cleanups(&root);
    root.cancel();
    run_cleanups(&root);

    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "a cleanup that runs twice releases something twice"
    );
}

#[test]
fn every_registered_cleanup_runs() {
    let root = Scope::root();
    let (first, first_action) = counter();
    let (second, second_action) = counter();
    root.on_cancel(first_action);
    root.on_cancel(second_action);

    root.cancel();
    run_cleanups(&root);

    assert_eq!(first.load(Ordering::SeqCst), 1);
    assert_eq!(second.load(Ordering::SeqCst), 1);
}

// A cancelled parent's teardown includes the children it stopped, without the caller holding a
// handle to them.
#[test]
fn parent_teardown_runs_descendant_cleanups() {
    let root = Scope::root();
    let child = root.child();
    let (child_runs, child_action) = counter();
    let (root_runs, root_action) = counter();
    child.on_cancel(child_action);
    root.on_cancel(root_action);

    root.cancel();
    run_cleanups(&root);

    assert_eq!(root_runs.load(Ordering::SeqCst), 1);
    assert_eq!(child_runs.load(Ordering::SeqCst), 1);
}

// A subagent that stopped on its own is torn down through its own scope, which its owner still
// holds; the parent's teardown must not run it a second time.
#[test]
fn descendant_cleanup_runs_once_across_scopes() {
    let root = Scope::root();
    let child = root.child();
    let (runs, action) = counter();
    child.on_cancel(action);

    child.cancel();
    run_cleanups(&child);
    run_cleanups(&root);
    run_cleanups(&child);

    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[test]
fn cleanup_registered_after_cancellation_runs_immediately() {
    let root = Scope::root();
    root.cancel();

    let (runs, action) = counter();
    root.on_cancel(action);

    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "dropping the action would leak what the caller is releasing"
    );
}

// A cleanup is allowed to inspect the scope it belongs to; holding the cleanup lock while it runs
// would deadlock here.
#[test]
fn cleanup_may_read_the_scope() {
    let root = Scope::root();
    let observed = Arc::new(std::sync::Mutex::new(None));
    let slot = Arc::clone(&observed);
    let handle = root.clone();

    root.on_cancel(Box::new(move || {
        let reason = handle.reason();
        if let Ok(mut slot) = slot.lock() {
            *slot = reason;
        }
    }));

    root.cancel_with(CancelReason::Error {
        detail: "provider refused".to_owned(),
    });
    run_cleanups(&root);

    let observed = observed.lock().map(|slot| slot.clone()).ok();
    assert_eq!(
        observed,
        Some(Some(CancelReason::Error {
            detail: "provider refused".to_owned()
        }))
    );
}

// Handles clone the node, so a cleanup registered through one handle is seen by another and one
// cancellation stops all of them.
#[test]
fn cloned_handles_share_one_node() {
    let root = Scope::root();
    let alias = root.clone();
    let (runs, action) = counter();
    alias.on_cancel(action);

    root.cancel();

    assert!(alias.is_cancelled());
    run_cleanups(&alias);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

// The wait a background task uses to park until its scope ends. Driven from another thread so a
// wait that never wakes shows up as a timeout rather than a hang.
#[test]
fn wait_returns_when_cancelled_from_another_thread() {
    let root = Scope::root();
    let waiter = root.clone();

    let waiting = thread::spawn(move || {
        waiter.wait_until_cancelled();
        waiter.is_cancelled()
    });

    thread::sleep(Duration::from_millis(20));
    root.cancel();

    let woken = waiting.join().is_ok_and(|woken| woken);
    assert!(woken, "a task waiting on its scope must be woken by it");
}

// Cancellation reaches a child while the parent's own walk is in progress from another thread, and
// no child is left live afterwards.
#[test]
fn concurrent_cancel_still_stops_children() {
    let root = Scope::root();
    let children: Vec<Scope> = (0..8).map(|_| root.child()).collect();

    let mut threads = Vec::new();
    for _ in 0..4 {
        let scope = root.clone();
        threads.push(thread::spawn(move || scope.cancel()));
    }
    for thread in threads {
        thread.join().expect("canceller thread panicked");
    }
    root.cancel();

    for (index, child) in children.iter().enumerate() {
        assert!(child.is_cancelled(), "child {index} survived cancellation");
        assert_eq!(child.reason(), Some(CancelReason::Parent));
    }
}

#[test]
fn reasons_render_for_an_operator() {
    assert!(!CancelReason::UserRequest.to_string().is_empty());
    assert!(
        CancelReason::Timeout { after_ms: 12 }
            .to_string()
            .contains("12")
    );
    assert!(CancelReason::Parent.to_string().contains("parent"));
    assert!(
        CancelReason::Error {
            detail: "disk full".to_owned()
        }
        .to_string()
        .contains("disk full")
    );
}
