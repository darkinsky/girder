//! Tests for live stream liveness detection.
//!
//! The property under test is that the two kinds of lateness stay apart: a request that has not
//! started yet is a queue delay, and one that started and then went quiet is a dead connection.
//! Every case below pins one side of that difference, because a detector that merged them would
//! pass any single-timeout test and still abort the wrong requests in production.
//!
//! The module is included by path rather than through the crate root, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/stream_stall.rs"]
mod stream_stall;

use stream_stall::{Detector, Health, Policy};

// --- normal flow ---------------------------------------------------------------------------------

#[test]
fn a_steady_stream_is_flowing_throughout() {
    let mut detector = Detector::new(1_000);
    detector.start(0);

    for at in [100, 200, 300, 400] {
        detector.note_chunk(at);
        // Half the allowance after each chunk: never a stall, and never the boundary either.
        assert_eq!(detector.check(at + 500), Health::Flowing);
    }
}

#[test]
fn a_chunk_resets_the_silence_measured_by_the_idle_bound() {
    let mut detector = Detector::new(1_000);
    detector.note_chunk(0);

    // Quiet for almost the whole allowance...
    assert_eq!(detector.check(999), Health::Flowing);
    // ...then a chunk arrives, which must restart the clock rather than accumulate it.
    detector.note_chunk(999);
    assert_eq!(detector.check(1_998), Health::Flowing);
    assert_eq!(detector.check(1_999), Health::Stalled { idle_ms: 1_000 });
}

#[test]
fn the_idle_bound_is_inclusive() {
    let mut detector = Detector::new(1_000);
    detector.note_chunk(0);

    // One millisecond short is flowing; exactly at the bound the allowance is spent.
    assert_eq!(detector.check(999), Health::Flowing);
    assert_eq!(detector.check(1_000), Health::Stalled { idle_ms: 1_000 });
}

#[test]
fn a_stall_reports_the_observed_silence_not_the_threshold() {
    let mut detector = Detector::new(1_000);
    detector.note_chunk(5_000);

    // The threshold is 1_000 but the stream has been quiet for far longer, and that is the number
    // worth logging.
    assert_eq!(detector.check(9_000), Health::Stalled { idle_ms: 4_000 });
}

// --- waiting for the first chunk -----------------------------------------------------------------

#[test]
fn a_stream_that_has_not_started_is_waiting_not_stalled() {
    let mut detector = Detector::new(1_000);
    detector.start(0);

    // Well past the idle bound: from the detector's own numbers a dead socket and a busy queue look
    // the same, so it reports the wait instead of guessing.
    assert_eq!(
        detector.check(60_000),
        Health::WaitingForFirst { elapsed_ms: 60_000 }
    );
}

#[test]
fn a_freshly_started_detector_waits_with_no_elapsed_time() {
    let detector = Detector::new(30_000);
    assert_eq!(detector.check(0), Health::WaitingForFirst { elapsed_ms: 0 });
}

#[test]
fn the_first_chunk_budget_is_enforced_by_the_policy_not_the_detector() {
    // The detector alone stays patient forever.
    let mut detector = Detector::new(30_000);
    detector.start(0);
    assert!(matches!(
        detector.check(600_000),
        Health::WaitingForFirst { .. }
    ));

    // The policy is what turns a long first wait into a decision. Two minutes of queueing is within
    // budget; two minutes and one millisecond is not.
    let policy = Policy::new(120_000, 30_000);
    assert_eq!(
        policy.check(&detector, 119_999),
        Health::WaitingForFirst {
            elapsed_ms: 119_999
        }
    );
    assert_eq!(
        policy.check(&detector, 120_000),
        Health::Stalled { idle_ms: 120_000 }
    );
}

#[test]
fn the_first_chunk_budget_is_measured_from_the_recorded_start() {
    let mut detector = Detector::new(30_000);
    // An absolute clock reading: the detector must measure from here, not from zero.
    detector.start(1_000_000);

    let policy = Policy::new(120_000, 30_000);
    assert_eq!(
        policy.check(&detector, 1_060_000),
        Health::WaitingForFirst { elapsed_ms: 60_000 }
    );
    assert_eq!(
        policy.check(&detector, 1_120_000),
        Health::Stalled { idle_ms: 120_000 }
    );
}

#[test]
fn the_first_chunk_budget_is_longer_than_the_idle_budget() {
    // The whole point of two numbers: the wait a caller tolerates before any output is much longer
    // than the silence it tolerates once output has begun.
    let mut queued = Detector::new(0);
    queued.start(0);
    let mut streaming = Detector::new(0);
    streaming.note_chunk(0);

    let policy = Policy::new(120_000, 30_000);
    // The same 45 seconds means "still queueing" before the first chunk...
    assert!(matches!(
        policy.check(&queued, 45_000),
        Health::WaitingForFirst { .. }
    ));
    // ...and "dead" after one.
    assert!(matches!(
        policy.check(&streaming, 45_000),
        Health::Stalled { .. }
    ));
}

#[test]
fn the_first_chunk_ends_the_waiting_state_even_immediately() {
    let mut detector = Detector::new(30_000);
    detector.start(0);
    assert!(matches!(detector.check(10), Health::WaitingForFirst { .. }));

    detector.note_chunk(10);
    // The same instant, judged by the idle rule now: a stream that just spoke is flowing.
    assert_eq!(detector.check(10), Health::Flowing);
    assert_eq!(
        Policy::new(120_000, 30_000).check(&detector, 10),
        Health::Flowing
    );
}

// --- chunk-to-chunk timeout ----------------------------------------------------------------------

#[test]
fn a_started_stream_that_goes_quiet_is_stalled_under_the_policy() {
    let mut detector = Detector::new(0);
    detector.start(0);
    detector.note_chunk(1_000);

    let policy = Policy::new(120_000, 30_000);
    assert_eq!(policy.check(&detector, 30_999), Health::Flowing);
    assert_eq!(
        policy.check(&detector, 31_000),
        Health::Stalled { idle_ms: 30_000 }
    );
}

#[test]
fn the_idle_bound_is_measured_from_the_last_chunk_not_the_first() {
    let mut detector = Detector::new(0);
    detector.start(0);
    for at in [1_000, 2_000, 3_000] {
        detector.note_chunk(at);
    }

    // A stream that has been running for two and a half seconds is not one second from death; the
    // silence is counted from the newest chunk.
    let policy = Policy::new(120_000, 30_000);
    assert_eq!(policy.check(&detector, 32_999), Health::Flowing);
    assert_eq!(
        policy.check(&detector, 33_000),
        Health::Stalled { idle_ms: 30_000 }
    );
}

#[test]
fn the_policys_idle_bound_overrides_the_detectors_own() {
    let mut detector = Detector::new(1_000);
    detector.note_chunk(0);

    // The detector would call this stalled at 1_000ms; the policy is the production configuration
    // and says otherwise.
    let policy = Policy::new(120_000, 60_000);
    assert_eq!(policy.check(&detector, 5_000), Health::Flowing);
    assert_eq!(
        policy.check(&detector, 60_000),
        Health::Stalled { idle_ms: 60_000 }
    );
}

// --- ttft ----------------------------------------------------------------------------------------

#[test]
fn ttft_freezes_at_the_first_chunk() {
    let mut detector = Detector::new(30_000);
    detector.start(0);

    detector.note_chunk(2_500);
    assert_eq!(detector.ttft_ms(2_500), Some(2_500));
    // Asking again much later reports the same measurement: a fast answer must not appear slow
    // because the rest of it took a while.
    assert_eq!(detector.ttft_ms(90_000), Some(2_500));
}

#[test]
fn ttft_before_the_first_chunk_reports_the_wait_so_far() {
    let mut detector = Detector::new(30_000);
    detector.start(0);

    // A heartbeat log wants the current queueing time, not `None`.
    assert_eq!(detector.ttft_ms(4_000), Some(4_000));
    assert_eq!(detector.ttft_ms(4_001), Some(4_001));
}

#[test]
fn ttft_is_measured_from_the_recorded_start() {
    let mut detector = Detector::new(30_000);
    detector.start(1_000_000);
    detector.note_chunk(1_007_500);

    assert_eq!(detector.ttft_ms(9_999_999), Some(7_500));
}

#[test]
fn an_unstarted_detector_reports_no_ttft() {
    // No origin and no chunk: the only number available is the caller's raw clock reading, and
    // reporting that as a duration would be a measurement the detector never made.
    assert_eq!(Detector::new(1_000).ttft_ms(50_000), None);

    // Recording the start is enough to make the waiting time meaningful.
    let mut started = Detector::new(1_000);
    started.start(40_000);
    assert_eq!(started.ttft_ms(50_000), Some(10_000));
}

// --- chunks --------------------------------------------------------------------------------------

#[test]
fn every_chunk_is_counted() {
    let mut detector = Detector::new(1_000);
    assert_eq!(detector.chunks(), 0);

    for at in [0, 10, 20, 30, 40] {
        detector.note_chunk(at);
    }
    assert_eq!(detector.chunks(), 5);
}

#[test]
fn chunks_at_the_same_timestamp_all_count() {
    let mut detector = Detector::new(1_000);
    detector.note_chunk(7);
    detector.note_chunk(7);
    detector.note_chunk(7);

    // Identical timestamps are normal — a provider that sends a whole block in one network read —
    // and each chunk still arrived.
    assert_eq!(detector.chunks(), 3);
    assert_eq!(detector.check(7), Health::Flowing);
}

// --- empty detector ------------------------------------------------------------------------------

#[test]
fn an_empty_detector_has_no_measurements() {
    let detector = Detector::new(5_000);

    assert_eq!(detector.chunks(), 0);
    assert_eq!(detector.ttft_ms(123), None);
    assert_eq!(
        detector.check(123),
        Health::WaitingForFirst { elapsed_ms: 123 }
    );
    // The relative convention: without a recorded start, the origin is zero.
    assert_eq!(
        Policy::new(10_000, 5_000).check(&detector, 999),
        Health::WaitingForFirst { elapsed_ms: 999 }
    );
}

#[test]
fn the_detectors_own_bound_is_readable() {
    // Callers that never build a policy configure the detector directly, so the effective bound has
    // to be inspectable rather than only observable through a check.
    assert_eq!(Detector::new(30_000).idle_ms(), 30_000);
    assert_eq!(Policy::new(120_000, 30_000).first_chunk_ms, 120_000);
    assert_eq!(Policy::new(120_000, 30_000).idle_ms, 30_000);
}

#[test]
fn a_zero_idle_bound_stalls_immediately_after_the_first_chunk() {
    // The degenerate configuration: at zero, any instant after the chunk is already too quiet.
    let mut detector = Detector::new(0);
    detector.note_chunk(100);
    assert_eq!(detector.check(100), Health::Stalled { idle_ms: 0 });
    assert_eq!(detector.check(101), Health::Stalled { idle_ms: 1 });
}

// --- clock going backwards -----------------------------------------------------------------------

#[test]
fn a_chunk_timestamped_before_the_previous_one_does_not_manufacture_silence() {
    let mut detector = Detector::new(1_000);
    detector.note_chunk(5_000);
    // A clock correction steps back one second, and a chunk arrives carrying the older stamp.
    detector.note_chunk(4_000);

    // The stream is measured from the newest timestamp it has seen, so it is alive, not stalled by
    // three seconds of imaginary silence.
    assert_eq!(detector.check(5_000), Health::Flowing);
    assert_eq!(detector.chunks(), 2);
    assert_eq!(detector.check(5_999), Health::Flowing);
}

#[test]
fn a_check_timestamp_before_the_last_chunk_saturates_to_no_silence() {
    let mut detector = Detector::new(1_000);
    detector.note_chunk(5_000);

    // `now_ms` behind the chunk it is judging: clamp to zero rather than wrap around to a value
    // that would report the stream as stalled forever.
    assert_eq!(detector.check(4_000), Health::Flowing);

    // The same clamp inside TTFT, with an origin after the chunk's own stamp.
    let mut reordered = Detector::new(1_000);
    reordered.start(6_000);
    reordered.note_chunk(5_000);
    assert_eq!(reordered.ttft_ms(4_000), Some(0));
}

#[test]
fn a_first_chunk_before_the_recorded_start_reports_zero_wait() {
    let mut detector = Detector::new(30_000);
    detector.start(5_000);
    // Stamped before the origin, which a relative/absolute mixup or a clock step can produce.
    detector.note_chunk(4_000);

    assert_eq!(detector.ttft_ms(10_000), Some(0));
    assert_eq!(
        Policy::new(120_000, 30_000).check(&detector, 5_000),
        Health::Flowing
    );
}

#[test]
fn a_wait_that_rewinds_is_reported_as_zero_not_wrapped() {
    let mut detector = Detector::new(30_000);
    detector.start(5_000);

    // `now_ms` before the start: the elapsed time clamps to zero instead of wrapping near `u64::MAX`.
    assert_eq!(
        detector.check(1_000),
        Health::WaitingForFirst { elapsed_ms: 0 }
    );
    assert_eq!(
        Policy::new(120_000, 30_000).check(&detector, 1_000),
        Health::WaitingForFirst { elapsed_ms: 0 }
    );
}
