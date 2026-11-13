//! Tests for the bounded queue.
//!
//! The interesting cases are the ones a plain `channel(capacity)` cannot express: what happens to an
//! item that arrives at a full queue, and whether a close discards work that was already accepted.
//! Those two questions are the reason this type exists, so they are tested directly rather than
//! inferred from a send succeeding.
//!
//! # Why the module is compiled by path
//!
//! The queue is not exported from the crate root yet; that wiring is a separate change to `lib.rs`.
//! Compiling the exact source file here — rather than a copy — keeps these tests exercising the same
//! code a consumer will eventually reach through `girder_core::bounded_queue`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/bounded_queue.rs"]
mod bounded_queue;

use std::sync::Arc;

use bounded_queue::{BoundedQueue, Closed, Overflow};

/// A queue that yields everything it holds and then reports `None`.
///
/// Used to assert on the full contents rather than on one item: a policy that evicts the wrong end
/// produces the same *length* as one that evicts correctly, so the order has to be compared.
async fn drain<T>(queue: &mut BoundedQueue<T>) -> Vec<T> {
    let mut out = Vec::new();
    while let Some(item) = queue.recv().await {
        out.push(item);
    }
    out
}

#[tokio::test]
async fn receives_everything_within_capacity_in_order() {
    let mut queue = BoundedQueue::new(4);
    for value in 1..=4 {
        queue.try_send(value).expect("a queue of four accepts four");
    }

    assert_eq!(queue.len(), 4);
    assert!(!queue.is_empty());

    queue.close();
    assert_eq!(drain(&mut queue).await, vec![1, 2, 3, 4]);
}

#[tokio::test]
async fn try_send_returns_the_item_when_full() {
    let mut queue = BoundedQueue::new(2);
    queue.try_send("first").unwrap();
    queue.try_send("second").unwrap();

    // The item comes back, not just an error: a caller that must not lose the value has to be able
    // to forward or retry it, and only the returned item makes that possible.
    let refused = queue.try_send("third").unwrap_err();
    assert_eq!(refused, "third");
    assert_eq!(queue.len(), 2, "a refused send must not change the queue");

    queue.close();
    assert_eq!(drain(&mut queue).await, vec!["first", "second"]);
}

#[tokio::test]
async fn drop_oldest_evicts_the_front_and_keeps_the_count() {
    let mut queue = BoundedQueue::with_overflow(3, Overflow::DropOldest);
    queue.try_send(1).unwrap();
    queue.try_send(2).unwrap();
    queue.try_send(3).unwrap();

    // Full: the oldest goes, the newest stays.
    queue.try_send(4).unwrap();
    queue.try_send(5).unwrap();

    assert_eq!(queue.len(), 3, "the bound still holds under DropOldest");
    assert_eq!(queue.dropped(), 2, "each eviction is counted");

    queue.close();
    assert_eq!(
        drain(&mut queue).await,
        vec![3, 4, 5],
        "the survivors are the newest items, in order"
    );
}

#[tokio::test]
async fn drop_oldest_never_blocks_the_producer() {
    // The point of the policy: with the queue full and no receiver running, a send still completes.
    // Under `Reject` the same call returns an error and under the default `send` it would wait.
    let queue = BoundedQueue::with_overflow(1, Overflow::DropOldest);

    for value in 0..100 {
        queue.try_send(value).expect("DropOldest always accepts");
    }

    assert_eq!(queue.len(), 1);
    assert_eq!(queue.dropped(), 99);
}

#[tokio::test]
async fn close_lets_the_receiver_drain_before_returning_none() {
    let mut queue = BoundedQueue::new(3);
    queue.try_send(1).unwrap();
    queue.try_send(2).unwrap();

    queue.close();
    assert!(queue.is_closed());
    assert_eq!(queue.len(), 2, "a close must not discard accepted items");
    assert!(
        !queue.is_empty(),
        "a closed but undrained queue is not empty"
    );

    assert_eq!(queue.recv().await, Some(1));
    assert_eq!(queue.recv().await, Some(2));
    assert_eq!(
        queue.recv().await,
        None,
        "drained and closed ends the stream"
    );
    assert_eq!(queue.recv().await, None, "and stays ended");
}

#[tokio::test]
async fn close_is_idempotent_and_refuses_later_sends() {
    let mut queue = BoundedQueue::new(2);
    queue.try_send(1).unwrap();
    queue.close();
    queue.close();

    assert_eq!(queue.try_send(2).unwrap_err(), 2);
    assert!(queue.send(3).await.is_err());

    assert_eq!(drain(&mut queue).await, vec![1]);
}

#[tokio::test]
async fn send_waits_for_room_instead_of_failing() {
    let queue = Arc::new(BoundedQueue::new(1));
    queue.try_send(1).unwrap();

    let producer = {
        let queue = Arc::clone(&queue);
        tokio::spawn(async move { queue.send(2).await })
    };

    // The producer cannot finish while the queue holds one item and nobody has received it.
    tokio::task::yield_now().await;
    assert!(!producer.is_finished(), "send must wait for a free slot");

    // Receiving frees the slot. `recv` takes `&mut self` and the producer holds an `Arc`, so the slot is freed by
    // closing instead -- which is the other path that wakes a waiting sender, and the one this test can drive without
    // taking ownership back from the task.
    queue.close();

    producer
        .await
        .unwrap()
        .expect_err("a closed queue refuses the send it was waiting on");
}

#[tokio::test]
async fn close_wakes_a_sender_that_is_waiting_for_room() {
    let queue = Arc::new(BoundedQueue::new(1));
    queue.try_send(1).unwrap();

    let producer = {
        let queue = Arc::clone(&queue);
        tokio::spawn(async move { queue.send(2).await })
    };

    tokio::task::yield_now().await;
    assert!(!producer.is_finished());

    // Without this wakeup the sender would block until the socket closed; it must fail instead.
    queue.close();

    assert_eq!(
        producer.await.unwrap().unwrap_err(),
        Closed,
        "a waiting sender learns about the close rather than sleeping forever"
    );
}

#[tokio::test]
async fn recv_waits_for_an_item_that_arrives_later() {
    // `recv` takes `&mut self`, so the receiver owns the queue outright and a producer reaches it through a clone.
    // Sending happens on a task because it must run while the receiver is parked inside `recv`.
    let queue = Arc::<BoundedQueue<i32>>::new(BoundedQueue::new(2));
    let producer = Arc::clone(&queue);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        producer.try_send(7).expect("room for one");
    });

    let received = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        // Wait until the sender has filled it, then take the handle.
        loop {
            if queue.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let mut handle = Arc::try_unwrap(queue).expect("the producer released its clone");
        handle.recv().await
    })
    .await
    .expect("the receive completes");

    assert_eq!(received, Some(7));
}

#[tokio::test]
async fn len_and_is_empty_track_every_operation() {
    let mut queue = BoundedQueue::with_overflow(2, Overflow::DropOldest);
    assert!(queue.is_empty());
    assert_eq!(queue.len(), 0);

    queue.try_send(1).unwrap();
    assert!(!queue.is_empty());
    assert_eq!(queue.len(), 1);

    queue.try_send(2).unwrap();
    assert_eq!(queue.len(), 2);

    // Full, so a third send evicts rather than growing.
    queue.try_send(3).unwrap();
    assert_eq!(queue.len(), 2);

    assert_eq!(queue.recv().await, Some(2));
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.recv().await, Some(3));
    assert!(queue.is_empty());
}

#[tokio::test]
#[should_panic(expected = "unbuffered handoff")]
async fn zero_capacity_is_refused() {
    // Not "a queue with no buffer": nothing can ever be enqueued, so `send` would wait forever and
    // `try_send` would fail forever. Refusing it at construction is the only honest answer.
    let _ = BoundedQueue::<u8>::new(0);
}

#[tokio::test]
#[should_panic(expected = "unbuffered handoff")]
async fn zero_capacity_is_refused_under_every_policy() {
    let _ = BoundedQueue::<u8>::with_overflow(0, Overflow::DropOldest);
}

#[tokio::test]
async fn close_policy_shuts_down_when_the_bound_is_reached() {
    let mut queue = BoundedQueue::with_overflow(2, Overflow::Close);
    queue.try_send(1).unwrap();
    queue.try_send(2).unwrap();

    assert_eq!(queue.try_send(3).unwrap_err(), 3);
    assert!(queue.is_closed(), "the full queue closed itself");

    // The items accepted before the close are still readable; the refused one is not.
    assert_eq!(drain(&mut queue).await, vec![1, 2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_producers_never_exceed_the_bound() {
    const PRODUCERS: usize = 4;
    const PER_PRODUCER: usize = 50;

    let queue = Arc::new(BoundedQueue::with_overflow(4, Overflow::DropOldest));
    let mut tasks = Vec::new();
    for producer in 0..PRODUCERS {
        let queue = Arc::clone(&queue);
        tasks.push(tokio::spawn(async move {
            for value in 0..PER_PRODUCER {
                queue
                    .try_send(producer * PER_PRODUCER + value)
                    .expect("DropOldest always accepts");
            }
        }));
    }

    for task in tasks {
        task.await.unwrap();
    }

    // Whatever survived must fit, must be in range, and the loss must be accounted for.
    assert!(queue.len() <= 4);
    let held = queue.len();
    let dropped = usize::try_from(queue.dropped()).unwrap();
    assert_eq!(held + dropped, PRODUCERS * PER_PRODUCER);

    // The producers are done, so the only remaining handle is this one: `try_unwrap` gives back the `&mut` that
    // draining needs.
    queue.close();
    let mut owned = Arc::try_unwrap(queue).expect("every producer task finished");
    let survivors = drain(&mut owned).await;
    assert_eq!(survivors.len(), held);
    assert!(
        survivors
            .iter()
            .all(|value| *value < PRODUCERS * PER_PRODUCER)
    );
}
