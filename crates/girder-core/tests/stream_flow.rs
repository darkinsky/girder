//! Tests for per-stream flow control.
//!
//! The premise this file pins down is that a full window is a *pause*, not a loss. The cases that
//! matter are therefore the refusals and the recoveries, not the happy path: a window that silently
//! dropped a frame when full would pass every test that only checked that `send` succeeded.
//!
//! # Why the module is compiled by path
//!
//! `stream_flow` is not exported from the crate root yet; that wiring is a separate change to
//! `lib.rs`. Compiling the exact source file — rather than a copy — keeps these tests exercising the
//! code a consumer will eventually reach through `girder_core::stream_flow`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/stream_flow.rs"]
mod stream_flow;

use stream_flow::{Flow, SendResult, SlowDetector};

#[test]
fn sends_within_capacity_are_allowed() {
    let mut flow = Flow::new(3);
    assert_eq!(
        flow.capacity(),
        3,
        "the limit is readable from its enforcer"
    );

    for expected_inflight in 1..=3 {
        assert_eq!(flow.send(), SendResult::Allowed);
        assert_eq!(flow.inflight(), expected_inflight);
        assert_eq!(flow.available(), 3 - expected_inflight);
        assert_eq!(flow.sent_total(), u64::try_from(expected_inflight).unwrap());
    }
}

#[test]
fn a_full_window_blocks_and_leaves_the_state_untouched() {
    let mut flow = Flow::new(2);
    assert_eq!(flow.send(), SendResult::Allowed);
    assert_eq!(flow.send(), SendResult::Allowed);
    assert!(flow.is_blocked());

    // The refusal reports both numbers, and changes nothing. The frame is not consumed and not
    // counted, so the caller can still send it once an ack arrives — which is the entire difference
    // between this and a dropping queue.
    assert_eq!(
        flow.send(),
        SendResult::Blocked {
            inflight: 2,
            capacity: 2
        }
    );
    assert_eq!(flow.inflight(), 2);
    assert_eq!(flow.sent_total(), 2);

    // Refusing repeatedly is not progress and must not be mistaken for it.
    assert_eq!(
        flow.send(),
        SendResult::Blocked {
            inflight: 2,
            capacity: 2
        }
    );
    assert_eq!(flow.sent_total(), 2);
}

#[test]
fn acknowledgement_opens_the_window_again() {
    let mut flow = Flow::new(2);
    flow.send();
    flow.send();
    assert!(flow.is_blocked());

    flow.acknowledge(1);
    assert_eq!(flow.inflight(), 1);
    assert!(!flow.is_blocked());

    // The window moved, so the frame the caller held back now fits.
    assert_eq!(flow.send(), SendResult::Allowed);
    assert_eq!(flow.inflight(), 2);
    assert!(flow.is_blocked());

    // Acknowledging everything drains the window; the sent total is history and does not go back.
    flow.acknowledge(2);
    assert_eq!(flow.inflight(), 0);
    assert!(!flow.is_blocked());
    assert_eq!(flow.sent_total(), 3);
}

#[test]
fn a_batched_acknowledgement_advances_by_the_whole_batch() {
    let mut flow = Flow::new(8);
    for _ in 0..8 {
        flow.send();
    }
    assert!(flow.is_blocked());

    // A client acknowledging in one read is the common case; it must free exactly as many slots as
    // it confirms, not just one.
    flow.acknowledge(8);
    assert_eq!(flow.inflight(), 0);
    assert_eq!(flow.available(), 8);
    for _ in 0..8 {
        assert_eq!(flow.send(), SendResult::Allowed);
    }
}

#[test]
fn excessive_acknowledgement_saturates_at_zero() {
    let mut flow = Flow::new(4);
    flow.send();

    flow.acknowledge(100);

    // Not a panic, not a negative, and above all not extra credit: a peer that acks more than it
    // was sent must not be able to enlarge the window beyond its capacity.
    assert_eq!(flow.inflight(), 0);
    assert_eq!(flow.available(), 4);

    for _ in 0..4 {
        assert_eq!(flow.send(), SendResult::Allowed);
    }
    assert_eq!(
        flow.send(),
        SendResult::Blocked {
            inflight: 4,
            capacity: 4
        },
        "the window is still exactly the configured size"
    );

    // Acking a stream that was never sent anything is also absorbed, and the total stays honest:
    // zero sent frames, so no ack can be legitimate.
    let mut untouched = Flow::new(2);
    untouched.acknowledge(usize::MAX);
    assert_eq!(untouched.inflight(), 0);
    assert_eq!(untouched.sent_total(), 0);
}

#[test]
fn inflight_counts_sends_and_acks_together() {
    let mut flow = Flow::new(8);
    assert_eq!(flow.inflight(), 0);
    assert_eq!(flow.available(), 8);

    for _ in 0..5 {
        flow.send();
    }
    assert_eq!(flow.inflight(), 5);
    assert_eq!(flow.available(), 3);

    flow.acknowledge(2);
    assert_eq!(flow.inflight(), 3);
    assert_eq!(flow.available(), 5);

    flow.acknowledge(3);
    assert_eq!(flow.inflight(), 0);
    assert_eq!(flow.sent_total(), 5);
}

#[test]
fn zero_capacity_refuses_every_send() {
    let mut flow = Flow::new(0);

    // A zero window is not "unlimited": a zero read as infinity would reintroduce exactly the
    // unbounded buffer the window exists to replace.
    assert!(flow.is_blocked(), "an empty window has no room");
    assert_eq!(
        flow.send(),
        SendResult::Blocked {
            inflight: 0,
            capacity: 0
        }
    );
    assert_eq!(flow.inflight(), 0);
    assert_eq!(flow.sent_total(), 0);

    // Nothing was ever sent, so an ack has nothing to confirm and cannot conjure room.
    flow.acknowledge(1);
    assert!(flow.is_blocked());
    assert_eq!(
        flow.send(),
        SendResult::Blocked {
            inflight: 0,
            capacity: 0
        }
    );
    assert_eq!(flow.sent_total(), 0);
}

#[test]
fn slow_detector_triggers_after_the_threshold() {
    let mut detector = SlowDetector::new(1_000);

    detector.note_blocked(5_000);
    assert_eq!(detector.threshold_ms(), 1_000);
    assert!(!detector.is_slow(5_999), "still inside the threshold");
    assert!(detector.is_slow(6_000), "the threshold is inclusive");
    assert!(detector.is_slow(9_000), "and it stays slow while blocked");
    assert_eq!(detector.blocked_for_ms(6_000), 1_000);

    // The streak is timed from the first block, so repeated refusals in the same streak must not
    // push the deadline out — otherwise a client refused every 900 ms would never be judged slow.
    detector.note_blocked(8_000);
    assert_eq!(detector.blocked_for_ms(6_000), 1_000);
    assert!(detector.is_slow(6_000));
}

#[test]
fn slow_detector_does_not_fire_on_a_healthy_stream() {
    let mut detector = SlowDetector::new(1_000);

    // Never blocked: silent by construction, however much time passes.
    assert!(!detector.is_slow(100_000));
    assert_eq!(detector.blocked_for_ms(100_000), 0);

    // Blocked briefly and then cleared: the next block starts a fresh streak instead of inheriting
    // the earlier one. Without this a busy stream would accumulate every blip and eventually be
    // condemned.
    detector.note_blocked(0);
    assert!(!detector.is_slow(500));
    detector.note_progress();
    assert!(!detector.is_slow(10_000));

    detector.note_blocked(20_000);
    assert!(!detector.is_slow(20_500));
    assert_eq!(detector.blocked_for_ms(20_500), 500);
}

#[test]
fn slow_detector_survives_a_backwards_clock() {
    let mut detector = SlowDetector::new(1_000);
    detector.note_blocked(10_000);

    // A clock correction must not read as a long block: the elapsed span saturates at zero rather
    // than wrapping to ~584 million years and disconnecting a healthy client.
    assert_eq!(detector.blocked_for_ms(4_000), 0);
    assert!(!detector.is_slow(4_000));

    // And the streak is still recoverable once the clock catches up.
    assert!(detector.is_slow(11_000));
}

#[test]
fn zero_threshold_marks_the_first_block_slow() {
    let mut detector = SlowDetector::new(0);
    assert!(!detector.is_slow(0), "no block recorded yet");

    detector.note_blocked(42);
    assert!(detector.is_slow(42));
}

#[test]
fn a_full_window_is_only_slow_once_it_stays_full() {
    // The two types are used together by the transport, so their agreement is worth pinning: a
    // momentary full window is normal, and only an unbroken one is an incident.
    let mut flow = Flow::new(1);
    let mut detector = SlowDetector::new(250);

    assert_eq!(flow.send(), SendResult::Allowed);
    assert!(flow.is_blocked());

    detector.note_blocked(1_000);
    assert!(!detector.is_slow(1_249));
    assert!(detector.is_slow(1_250));

    // The ack is the progress signal: it reopens the window and clears the verdict together.
    flow.acknowledge(1);
    detector.note_progress();
    assert!(!flow.is_blocked());
    assert_eq!(flow.send(), SendResult::Allowed);
    assert!(!detector.is_slow(99_000));
}

#[test]
fn inflight_stays_within_capacity_across_a_random_trace() {
    // Hand-written cases cover the transitions; this covers the arithmetic. A window that leaks or
    // gains a slot shows up here as a capacity violation on some later step — a class of bug that is
    // tedious to construct by hand and easy to generate.
    const CAPACITY: usize = 4;

    let mut flow = Flow::new(CAPACITY);
    let mut state = 0x2545_F491_4F6C_DD1D_u64;

    for _ in 0..10_000 {
        // A linear congruential generator: deterministic, dependency-free, and enough to exercise
        // the transitions.
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);

        match (state >> 33) % 3 {
            0 => {
                flow.send();
            }
            1 => flow.acknowledge(usize::try_from((state >> 40) % 3).unwrap()),
            _ => flow.acknowledge(usize::try_from(state >> 32).unwrap_or(usize::MAX)),
        }

        assert!(
            flow.inflight() <= CAPACITY,
            "inflight exceeded the window capacity"
        );
        assert!(
            u64::try_from(flow.inflight()).unwrap() <= flow.sent_total(),
            "more frames in flight than were ever sent"
        );
        assert_eq!(flow.available() + flow.inflight(), CAPACITY);
    }

    assert!(flow.sent_total() > 0, "the trace sent something");
}
