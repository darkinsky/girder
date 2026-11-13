//! Tests for the subagent admission pool.
//!
//! The cases that matter are the ones a bare counter gets wrong: whether a full pool refuses instead
//! of queueing, whether a released place is usable again, whether waiting callers are served in
//! arrival order, and whether the live count can ever exceed the bound under concurrency.
//!
//! # Why the module is compiled by path
//!
//! These tests drive [`Pool`] through a `#[path]` module so that every case exercises the exact
//! source file a consumer reaches through `girder_core::subagent_pool`, and so that the behaviour is
//! pinned even while the crate-root re-export is being wired. One extra test
//! ([`the_crate_root_export_is_usable`]) goes through the public re-export instead, which is what
//! proves the wiring itself works.
//!
//! # How "the same slot cannot be released twice" is covered
//!
//! That guarantee is a *compile-time* one: [`Pool::release`] takes [`Slot`] by value and `Slot` is
//! neither `Copy` nor `Clone`, so a second release has no value to pass and does not build
//! (`E0382`). This crate has no compile-fail harness, so a test cannot assert the error itself; what
//! is tested here at runtime is the mechanism that makes the guarantee safe rather than merely
//! tidy. Ownership transfer is exercised directly — a slot moved out of an `Option` leaves nothing
//! behind to release — and a slot dropped without an explicit release still returns its place, so an
//! aborted task cannot leak capacity and no caller ever needs a second release path.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/subagent_pool.rs"]
mod subagent_pool;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use subagent_pool::{Pool, Slot};

/// Waits long enough for a task that was going to block to be observed blocking.
///
/// Only used to make a negative assertion meaningful: a `yield_now` may let a task run but says
/// nothing about whether it reached the await it was supposed to be stuck on.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[tokio::test]
async fn acquires_up_to_capacity() {
    let pool = Pool::new(3);

    let mut slots: Vec<Slot> = Vec::new();
    for _ in 0..3 {
        slots.push(pool.try_acquire().expect("a pool of three admits three"));
    }

    assert_eq!(pool.capacity(), 3);
    assert_eq!(pool.active(), 3);
    assert!(!pool.has_room(), "a full pool reports no room");

    // The diagnostics handle is unique per acquisition: a log line that names it identifies one
    // child, not one position.
    let indices: Vec<usize> = slots.iter().map(Slot::index).collect();
    assert_eq!(indices, vec![0, 1, 2]);
}

#[tokio::test]
async fn try_acquire_returns_none_when_full() {
    let pool = Pool::new(2);
    let first = pool.try_acquire().unwrap();
    let second = pool.try_acquire().unwrap();

    assert!(
        pool.try_acquire().is_none(),
        "a full pool must refuse rather than wait: the caller decides whether to queue"
    );
    assert_eq!(pool.active(), 2, "a refusal must not change the count");
    assert!(!pool.has_room());

    pool.release(first);
    pool.release(second);
}

#[tokio::test]
async fn a_released_place_can_be_taken_again() {
    let pool = Pool::new(1);
    let first = pool.try_acquire().expect("the empty pool has room");
    assert_eq!(pool.active(), 1);

    pool.release(first);
    assert_eq!(pool.active(), 0, "release returns the place immediately");
    assert!(pool.has_room());

    let second = pool.try_acquire().expect("the returned place is reusable");
    assert_eq!(pool.active(), 1);
    pool.release(second);
}

#[tokio::test]
async fn acquire_waits_for_a_release() {
    let pool = Arc::new(Pool::new(1));
    let held = pool.try_acquire().expect("the empty pool has room");

    let waiter = {
        let pool = Arc::clone(&pool);
        tokio::spawn(async move {
            let slot = pool.acquire().await;
            pool.release(slot);
        })
    };

    settle().await;
    assert!(
        !waiter.is_finished(),
        "acquire must wait while the pool is full, not fail and not over-admit"
    );
    assert_eq!(pool.active(), 1, "a waiting caller is not an admitted one");

    pool.release(held);
    tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("release wakes the waiter")
        .expect("the waiter did not panic");
    assert_eq!(pool.active(), 0);
}

#[tokio::test]
async fn a_slot_that_is_dropped_returns_its_place() {
    // Cancellation safety: a task aborted while holding a slot must not strand capacity. The slot
    // owns its permit, so dropping it releases the place without an explicit `release` call.
    let pool = Pool::new(1);
    {
        let _slot = pool.try_acquire().expect("the empty pool has room");
        assert_eq!(pool.active(), 1);
    }

    assert_eq!(
        pool.active(),
        0,
        "a dropped slot must not leak the place it held"
    );
    assert!(pool.has_room());
}

#[tokio::test]
async fn releasing_through_a_transfer_releases_exactly_once() {
    // Ownership is the double-release guard: moving the slot into an `Option` and taking it back out
    // leaves `None` behind, so there is no second value that could be released. The count has to end
    // at zero and the pool has to be fully usable afterwards, which is what a double release would
    // break.
    let pool = Pool::new(1);
    let mut handoff = Some(pool.try_acquire().expect("the empty pool has room"));

    let slot = handoff.take().expect("the slot was placed in the handoff");
    pool.release(slot);
    assert!(
        handoff.is_none(),
        "the slot moved out, and nothing can move it again"
    );
    assert_eq!(pool.active(), 0);

    let again = pool
        .try_acquire()
        .expect("the place is intact after one release");
    assert_eq!(pool.active(), 1, "one release returns exactly one place");
    pool.release(again);
    assert_eq!(pool.active(), 0);
}

#[tokio::test]
#[should_panic(expected = "can never admit anything")]
async fn zero_capacity_is_refused() {
    // A pool of zero can never admit anything: `try_acquire` always fails and `acquire` waits for a
    // place no release can create. Accepting it would turn a configuration mistake into a hang, so
    // the constructor refuses it.
    let _ = Pool::new(0);
}

#[tokio::test]
async fn concurrent_holders_never_exceed_capacity() {
    const CAPACITY: usize = 4;
    const TASKS: usize = 64;

    let pool = Arc::new(Pool::new(CAPACITY));
    let observed_max = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..TASKS {
        let pool = Arc::clone(&pool);
        let observed_max = Arc::clone(&observed_max);
        handles.push(tokio::spawn(async move {
            let slot = pool.acquire().await;
            // Recorded from inside the critical section, which is the only place an over-admission
            // would be visible: sampling after the fact would miss a momentary excess.
            observed_max.fetch_max(pool.active(), Ordering::SeqCst);
            tokio::task::yield_now().await;
            pool.release(slot);
        }));
    }

    for handle in handles {
        handle.await.expect("no task panicked");
    }

    assert!(
        observed_max.load(Ordering::SeqCst) <= CAPACITY,
        "the live count reached {} with a capacity of {CAPACITY}",
        observed_max.load(Ordering::SeqCst)
    );
    assert_eq!(pool.active(), 0, "every place is returned after the joins");
    assert_eq!(pool.capacity(), CAPACITY);
}

#[tokio::test]
async fn concurrent_try_acquire_admits_exactly_capacity_callers() {
    // `try_acquire` racing from many tasks must hand out exactly `capacity` slots: one fewer leaks
    // capacity, one more breaks the bound.
    const CAPACITY: usize = 5;
    const TASKS: usize = 50;

    let pool = Arc::new(Pool::new(CAPACITY));
    let admitted = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..TASKS {
        let pool = Arc::clone(&pool);
        let admitted = Arc::clone(&admitted);
        let refused = Arc::clone(&refused);
        handles.push(tokio::spawn(async move {
            match pool.try_acquire() {
                Some(slot) => {
                    admitted.fetch_add(1, Ordering::SeqCst);
                    // Held across an await so the racers genuinely overlap.
                    tokio::task::yield_now().await;
                    pool.release(slot);
                }
                None => {
                    refused.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    for handle in handles {
        handle.await.expect("no task panicked");
    }

    assert_eq!(admitted.load(Ordering::SeqCst), CAPACITY);
    assert_eq!(refused.load(Ordering::SeqCst), TASKS - CAPACITY);
    assert_eq!(pool.active(), 0);
}

#[tokio::test]
async fn waiters_are_served_in_arrival_order() {
    // Fairness: a caller that arrives first must be served first, or one busy caller can starve the
    // rest. The test runs on the single-threaded default runtime, so the order in which the tasks
    // reach `acquire` is the order in which they were spawned; the yields give each one a turn to
    // get there before the first place is returned.
    const WAITERS: usize = 8;

    let pool = Arc::new(Pool::new(1));
    let held = pool.try_acquire().expect("the only place is free");

    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
    let (order_tx, mut order_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();

    for index in 0..WAITERS {
        let pool = Arc::clone(&pool);
        let ready_tx = ready_tx.clone();
        let order_tx = order_tx.clone();
        tokio::spawn(async move {
            ready_tx.send(index).ok();
            let slot = pool.acquire().await;
            order_tx.send(index).ok();
            pool.release(slot);
        });
    }

    for _ in 0..WAITERS {
        ready_rx.recv().await.expect("every waiter reports ready");
    }
    for _ in 0..WAITERS {
        tokio::task::yield_now().await;
    }

    pool.release(held);

    let mut served = Vec::new();
    for _ in 0..WAITERS {
        served.push(order_rx.recv().await.expect("every waiter is served"));
    }

    assert_eq!(
        served,
        (0..WAITERS).collect::<Vec<_>>(),
        "the pool served waiters out of arrival order"
    );
    assert_eq!(pool.active(), 0);
}

#[tokio::test]
async fn the_crate_root_export_is_usable() {
    // The tests above reach the pool through a `#[path]` module so they exercise the source file
    // directly. This one goes through the crate-root re-export a real caller would use, which is the
    // only thing that proves the wiring in `lib.rs` names the type the way consumers expect.
    let pool = girder_core::SubagentPool::new(2);
    let first = pool.try_acquire().expect("room for two");
    let second = pool.try_acquire().expect("room for two");
    assert!(pool.try_acquire().is_none());
    assert_eq!(pool.active(), 2);

    pool.release(first);
    pool.release(second);

    let again: girder_core::SubagentSlot = pool.try_acquire().expect("both places returned");
    assert_eq!(again.index(), 2, "indices continue across releases");
    pool.release(again);
    assert_eq!(pool.active(), 0);
}
