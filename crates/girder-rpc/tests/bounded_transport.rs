//! Tests for the bounded outbound adapter (`bounded_transport`).
//!
//! The claim these tests have to settle is not "a bounded queue is bounded" — `girder-core` already
//! proves that. It is that the transport can adopt the bound **without** making `send` asynchronous,
//! and that the resulting loss is attributable rather than silent. Concretely:
//!
//! - `send` stays synchronous, so the four existing downlink call sites compile unchanged.
//! - A consumer that never reads cannot make the producer block or the buffer grow.
//! - Every dropped frame is counted, and counted against its own kind.
//! - The channel ends the same way `mpsc::UnboundedSender` did, so the writer loop keeps its shape.
//!
//! No test uses `unwrap()` on a channel operation: a failure is reported as the assertion message,
//! which is what makes a red test say what broke instead of where it panicked.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use girder_rpc::DownlinkFrame;
use girder_rpc::bounded_transport::{FrameKind, Outbound, SendOutcome};

/// Builds an `emit` frame; the payload value distinguishes frames in ordering assertions.
fn emit(marker: u64) -> DownlinkFrame {
    DownlinkFrame::Emit {
        event: "test/emit".to_owned(),
        args: vec![serde_json::json!(marker)],
    }
}

/// Reads the marker back out of an `emit` frame, or `None` for any other kind.
fn marker_of(frame: &DownlinkFrame) -> Option<u64> {
    match frame {
        DownlinkFrame::Emit { args, .. } => args.first().and_then(serde_json::Value::as_u64),
        _ => None,
    }
}

#[test]
fn sends_within_capacity_are_queued() {
    let (outbound, mut receiver) = Outbound::new(4);

    for marker in 0..4 {
        assert_eq!(
            outbound.send(emit(marker)),
            SendOutcome::Queued,
            "frame {marker} fits in a capacity-4 channel"
        );
    }

    assert_eq!(outbound.queued(), 4);
    assert_eq!(outbound.dropped(), 0, "nothing was lost");

    // The receiving side sees them in order, which is the guarantee a bounded `Reject` queue keeps
    // and a `DropOldest` one would not.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime for the ordering check");
    runtime.block_on(async {
        for expected in 0..4 {
            let frame = receiver.recv().await.expect("a queued frame");
            assert_eq!(marker_of(&frame), Some(expected), "FIFO order is preserved");
        }
    });
}

#[test]
fn full_queue_drops_and_counts_per_kind() {
    let (outbound, _receiver) = Outbound::new(2);

    // Fill the channel.
    assert_eq!(outbound.send(emit(0)), SendOutcome::Queued);
    assert_eq!(outbound.send(emit(1)), SendOutcome::Queued);

    // Now every one of these is refused. Mixing kinds is the point: a single total would not tell
    // an operator whether the connection lost a notification or its handshake.
    assert_eq!(outbound.send(emit(2)), SendOutcome::Dropped);
    assert_eq!(outbound.send(emit(3)), SendOutcome::Dropped);
    assert_eq!(
        outbound.send(DownlinkFrame::Ready {
            client_id: "client-1".to_owned(),
            host: girder_rpc::RemoteEventHostInfo {
                home: "/home/test".to_owned(),
            },
        }),
        SendOutcome::Dropped
    );
    assert_eq!(
        outbound.send(DownlinkFrame::Cancel {
            event_id: "event-1".to_owned(),
        }),
        SendOutcome::Dropped
    );
    assert_eq!(
        outbound.send(DownlinkFrame::Waterfall {
            event: "test/waterfall".to_owned(),
            event_id: "event-2".to_owned(),
            agent_id: "agent-1".to_owned(),
            request: serde_json::Map::new(),
        }),
        SendOutcome::Dropped
    );

    assert_eq!(
        outbound.queued(),
        2,
        "the bound held; nothing extra was buffered"
    );
    assert_eq!(
        outbound.dropped(),
        5,
        "every refused frame is counted exactly once"
    );

    // Per-kind attribution, which is the reason this adapter exists rather than a bare channel.
    assert_eq!(outbound.dropped_by_kind(FrameKind::Emit), 2);
    assert_eq!(outbound.dropped_by_kind(FrameKind::Ready), 1);
    assert_eq!(outbound.dropped_by_kind(FrameKind::Cancel), 1);
    assert_eq!(outbound.dropped_by_kind(FrameKind::Waterfall), 1);
    assert_eq!(outbound.dropped_by_kind(FrameKind::Subscription), 0);
    assert_eq!(outbound.dropped_by_kind(FrameKind::SubscribeResult), 0);

    // The parts must add up to the whole, or one of the two readings is lying.
    let summed: u64 = [
        FrameKind::Ready,
        FrameKind::Emit,
        FrameKind::Waterfall,
        FrameKind::Cancel,
        FrameKind::Subscription,
        FrameKind::SubscribeResult,
    ]
    .iter()
    .map(|kind| outbound.dropped_by_kind(*kind))
    .sum();
    assert_eq!(
        summed,
        outbound.dropped(),
        "per-kind counts sum to the total"
    );
}

#[test]
fn dropped_count_is_exact_under_repeated_overflow() {
    let (outbound, _receiver) = Outbound::new(1);
    assert_eq!(outbound.send(emit(0)), SendOutcome::Queued);

    // Each refused send increments the total by exactly one — no more (a retry counted twice) and no
    // less (a drop that vanished).
    for expected in 1..=100 {
        assert_eq!(outbound.send(emit(expected)), SendOutcome::Dropped);
        assert_eq!(outbound.dropped(), expected);
    }

    // Interleaving kinds must not disturb the total either.
    assert_eq!(
        outbound.send(DownlinkFrame::Cancel {
            event_id: "e".to_owned()
        }),
        SendOutcome::Dropped
    );
    assert_eq!(outbound.dropped(), 101);
    assert_eq!(outbound.dropped_by_kind(FrameKind::Cancel), 1);
    assert_eq!(outbound.dropped_by_kind(FrameKind::Emit), 100);
}

#[test]
fn dropping_the_last_sender_closes_the_channel_after_draining() {
    let (outbound, mut receiver) = Outbound::new(4);
    assert_eq!(outbound.send(emit(7)), SendOutcome::Queued);

    // A clone keeps the channel open; dropping only one of the two must not end the stream.
    let second = outbound.clone();
    drop(outbound);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime");
    runtime.block_on(async {
        let frame = receiver
            .recv()
            .await
            .expect("the queued frame survives a sender drop");
        assert_eq!(
            marker_of(&frame),
            Some(7),
            "queued frames drain before the close"
        );
    });

    drop(second);
    runtime.block_on(async {
        assert!(
            receiver.recv().await.is_none(),
            "recv ends once the last sender is gone and the queue is drained"
        );
    });
}

#[test]
fn send_after_close_returns_closed_and_is_not_counted_as_a_drop() {
    let (outbound, receiver) = Outbound::new(4);
    drop(receiver);

    // A closed channel is terminal, not overflow: counting these as drops would make `dropped()`
    // grow without bound on a dead connection and hide the real backpressure signal.
    assert_eq!(outbound.send(emit(1)), SendOutcome::Closed);
    assert_eq!(outbound.dropped(), 0);

    let outcome = outbound.send(emit(2));
    assert!(outcome.is_closed(), "the terminal condition is reportable");
    assert!(!outcome.is_queued());
}

#[test]
fn receiver_closes_when_the_writer_goes_away() {
    let (outbound, receiver) = Outbound::new(4);
    drop(receiver);

    // The writer is the only drain. Without this, a dead writer would leave the producer filling a
    // buffer nobody reads — the original bug, just smaller.
    assert_eq!(outbound.queued(), 0);
    assert_eq!(outbound.send(emit(0)), SendOutcome::Closed);
}

/// How many frames the flood test sends.
// Enough to prove the producer never blocks, small enough that the test finishes quickly. The property is "it does
// not wait", which the first refusal already demonstrates; five thousand iterations repeat it rather than establish it.
const SENT: u64 = 5_000;

#[test]
fn producer_never_blocks_when_the_consumer_never_reads() {
    // The core claim. A consumer that only writes and never reads is a legal WebSocket peer, and the
    // producer must not wait for it — that is how one stalled client would otherwise stall the host.
    let (outbound, mut receiver) = Outbound::new(8);

    // Hoisted out of the function body: an item declared after a statement exists from the start of the scope
    // anyway, which reads as though it did not.
    // Run the producer on another thread and require it to finish; a blocking send would hang here
    // instead of returning `Queued`/`Dropped`.
    let finished = Arc::new(AtomicBool::new(false));
    let producer = {
        let outbound = outbound.clone();
        let finished = Arc::clone(&finished);
        std::thread::spawn(move || {
            let mut queued = 0_u64;
            for marker in 0..SENT {
                match outbound.send(emit(marker)) {
                    SendOutcome::Queued => queued += 1,
                    // Dropping is the designed outcome under pressure, not a failure.
                    SendOutcome::Dropped | SendOutcome::Closed => {}
                }
            }
            finished.store(true, Ordering::Release);
            queued
        })
    };

    // Bounded wait: if the producer blocks, this test fails by timeout rather than hanging forever.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !finished.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < deadline,
            "the producer blocked on a consumer that never reads"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let queued = producer
        .join()
        .expect("the producer thread finished without panicking");

    // Memory is bounded by construction: exactly `capacity` frames can be in flight, and the rest are
    // accounted for as drops. `queued + dropped == SENT` is what makes the loss explicit.
    assert_eq!(
        outbound.queued(),
        8,
        "the buffer never exceeded its capacity"
    );
    assert!(
        queued <= 8,
        "at most the capacity can be in flight, saw {queued}"
    );
    assert_eq!(
        queued + outbound.dropped(),
        SENT,
        "every frame is either queued or counted as dropped"
    );

    // The producer handle must be gone before draining: `recv` ends only when the last sender is
    // dropped *and* the queue is drained, so keeping this one alive would park the drain loop.
    drop(outbound);

    // And the consumer, whenever it does get around to reading, sees the frames that were kept.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime");
    let drained = runtime.block_on(async {
        let mut count = 0_u64;
        while receiver.recv().await.is_some() {
            count += 1;
        }
        count
    });
    assert_eq!(
        drained, queued,
        "the queued frames are all still deliverable"
    );
}

#[test]
fn send_is_synchronous_and_usable_from_a_sync_context() {
    // The reason this adapter exists: `ClientHandle::send` and the event forwarder call it from a
    // context that cannot `.await`. This test calls `send` from a plain synchronous function with no
    // runtime in scope, which would not compile against `tokio::sync::mpsc::Sender::send`.
    fn publish(outbound: &Outbound, marker: u64) -> SendOutcome {
        outbound.send(emit(marker))
    }

    let (outbound, _receiver) = Outbound::new(2);
    assert_eq!(publish(&outbound, 1), SendOutcome::Queued);
    assert_eq!(publish(&outbound, 2), SendOutcome::Queued);
    assert_eq!(publish(&outbound, 3), SendOutcome::Dropped);
}

#[test]
fn zero_capacity_delivers_nothing_and_does_not_block() {
    let (outbound, mut receiver) = Outbound::new(0);

    // Zero is a misconfiguration, not a rendezvous. It must behave as a closed channel: report
    // itself immediately instead of parking a producer on a slot that no receive can create.
    assert_eq!(outbound.send(emit(0)), SendOutcome::Closed);
    assert_eq!(outbound.queued(), 0);
    assert_eq!(outbound.dropped(), 0, "closed is not overflow");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime");
    runtime.block_on(async {
        assert!(
            receiver.recv().await.is_none(),
            "a zero-capacity channel ends immediately rather than waiting forever"
        );
    });
}

#[test]
fn frame_kind_classification_matches_the_wire_names() {
    // The names are used in logs and metrics; a mismatch with the protocol spelling would make a
    // dashboard disagree with a captured trace.
    let cases = [
        (
            DownlinkFrame::Ready {
                client_id: "c".to_owned(),
                host: girder_rpc::RemoteEventHostInfo {
                    home: "/".to_owned(),
                },
            },
            FrameKind::Ready,
            "ready",
        ),
        (emit(0), FrameKind::Emit, "emit"),
        (
            DownlinkFrame::Waterfall {
                event: "e".to_owned(),
                event_id: "i".to_owned(),
                agent_id: "a".to_owned(),
                request: serde_json::Map::new(),
            },
            FrameKind::Waterfall,
            "waterfall",
        ),
        (
            DownlinkFrame::Cancel {
                event_id: "i".to_owned(),
            },
            FrameKind::Cancel,
            "cancel",
        ),
        (
            DownlinkFrame::SubscribeResult {
                id: "s".to_owned(),
                ok: true,
                error: None,
            },
            FrameKind::SubscribeResult,
            "subscribeResult",
        ),
    ];

    for (frame, kind, wire) in cases {
        assert_eq!(FrameKind::of(&frame), kind, "classification for {wire}");
        assert_eq!(kind.as_str(), wire);
        assert_eq!(kind.to_string(), wire);
    }

    // A subscription frame is the one variant that needs its inner value; classifying the envelope
    // must not require one.
    let subscription = DownlinkFrame::Subscription {
        frame: girder_rpc::SubscriptionFrame::End {
            subscription_id: girder_rpc::SubscriptionId::new("sub").expect("a non-empty id"),
            reason: girder_rpc::EndReason::Completed,
        },
    };
    assert_eq!(FrameKind::of(&subscription), FrameKind::Subscription);
    assert_eq!(FrameKind::Subscription.as_str(), "subscription");
}

#[test]
fn concurrent_senders_respect_one_bound_and_one_total() {
    // Several producers share the channel in the real server (the handle, the hub, the forwarder).
    // The counts must stay consistent when they race: a lost increment would understate the loss.
    const THREADS: u64 = 4;
    const PER_THREAD: u64 = 1_000;

    let (outbound, mut receiver) = Outbound::new(4);
    let accepted = Arc::new(AtomicU64::new(0));

    let threads: Vec<_> = (0..THREADS)
        .map(|thread| {
            let outbound = outbound.clone();
            let accepted = Arc::clone(&accepted);
            std::thread::spawn(move || {
                for marker in 0..PER_THREAD {
                    if outbound.send(emit(thread * PER_THREAD + marker)) == SendOutcome::Queued {
                        accepted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    for thread in threads {
        thread.join().expect("each producer thread finished");
    }

    let queued = outbound.queued() as u64;
    assert!(queued <= 4, "the bound held under contention, saw {queued}");
    assert_eq!(
        accepted.load(Ordering::Relaxed),
        queued,
        "the outcome agreed with the observed queue length"
    );
    assert_eq!(
        queued + outbound.dropped(),
        THREADS * PER_THREAD,
        "every frame is accounted for exactly once"
    );

    // Drain so the receiver is not left holding a live queue at the end of the test. The original
    // handle has to go first, otherwise `recv` waits for a producer that will never send again.
    drop(outbound);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime");
    runtime.block_on(async { while receiver.recv().await.is_some() {} });
}
