//! Tests for the loop that turns a schedule into a delivered prompt.
//!
//! `Scheduler` already knew what was due; the claim under test is that a firing reaches the deliverer exactly once and
//! that the wait is derived from the scheduler rather than from a polling interval.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use girder_core::{CronExpression, Offset, Rule, Schedule, ScheduleId, Scheduler, now_millis};
use girder_core::{Deliverer, Delivery, IDLE_WAIT, MAX_WAIT};

/// A deliverer that records every firing it saw.
fn recorder() -> (Deliverer, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let deliver: Deliverer = Arc::new(move |firing| {
        recorded.lock().expect("unpoisoned").push(firing.title);
        Delivery::Delivered
    });
    (deliver, seen)
}

/// A schedule due every minute, active.
fn every_minute(id: &str, title: &str) -> Schedule {
    Schedule {
        id: ScheduleId::new(id).expect("valid id"),
        title: title.to_owned(),
        prompt: format!("prompt for {title}"),
        rule: Rule::Cron {
            expression: CronExpression::parse("* * * * *").expect("valid cron"),
            offset: Offset::UTC,
        },
        active: true,
    }
}

#[test]
fn a_due_schedule_reaches_the_deliverer() {
    // The claim the whole module exists for: a created task is not merely stored.
    let mut scheduler = Scheduler::new();
    scheduler
        .add(every_minute("s1", "first"), now_millis())
        .expect("added");
    let (deliver, seen) = recorder();

    // A minute past the schedule's first occurrence is due.
    let result = girder_core::run_schedule_pass(&mut scheduler, &deliver, now_millis() + 120_000);

    assert_eq!(result.delivered, 1, "{result:?}");
    assert_eq!(seen.lock().expect("unpoisoned").as_slice(), ["first"]);
}

#[test]
fn one_tick_delivers_each_due_schedule_once() {
    // A second tick in the same instant must not deliver the same firing again: the next occurrence is in the future.
    let mut scheduler = Scheduler::new();
    scheduler
        .add(every_minute("s1", "once"), now_millis())
        .expect("added");
    let (deliver, seen) = recorder();
    let now = now_millis() + 120_000;

    let first = girder_core::run_schedule_pass(&mut scheduler, &deliver, now);
    let second = girder_core::run_schedule_pass(&mut scheduler, &deliver, now);

    assert_eq!(first.delivered, 1);
    assert_eq!(second.delivered, 0, "the same instant must not re-deliver");
    assert_eq!(seen.lock().expect("unpoisoned").len(), 1);
}

#[test]
fn a_failing_delivery_is_reported_and_does_not_stop_the_others() {
    // One unreachable session must not silently stop every other schedule.
    let mut scheduler = Scheduler::new();
    scheduler
        .add(every_minute("s1", "fails"), now_millis())
        .expect("added");
    scheduler
        .add(every_minute("s2", "works"), now_millis())
        .expect("added");
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let deliver: Deliverer = Arc::new(move |firing| {
        counted.fetch_add(1, Ordering::SeqCst);
        if firing.title == "fails" {
            Delivery::Failed {
                reason: "no session".to_owned(),
            }
        } else {
            Delivery::Delivered
        }
    });

    let result = girder_core::run_schedule_pass(&mut scheduler, &deliver, now_millis() + 120_000);

    assert_eq!(calls.load(Ordering::SeqCst), 2, "both were attempted");
    assert_eq!(result.delivered, 1);
    assert_eq!(result.failures.len(), 1);
    assert!(result.failures[0].contains("no session"));
}

#[test]
fn an_idle_scheduler_waits_the_idle_interval() {
    // Nothing pending still means look again, because a task can be created at any moment.
    assert_eq!(girder_core::schedule_wait_for(None, 1_000), IDLE_WAIT);
}

#[test]
fn a_wake_time_derives_the_wait() {
    // The whole point of next_wake: the wait is exact rather than a polling interval.
    let wait = girder_core::schedule_wait_for(Some(61_000), 1_000);

    assert_eq!(wait, Duration::from_secs(60));
}

#[test]
fn a_wake_time_in_the_past_wakes_immediately() {
    // Work is already due; a long sleep here would delay it by the whole interval.
    let wait = girder_core::schedule_wait_for(Some(500), 1_000);

    assert_eq!(wait, Duration::from_millis(1));
}

#[test]
fn a_wake_time_far_away_is_capped() {
    // A clock change must not park the loop for a year.
    let wait = girder_core::schedule_wait_for(Some(u64::MAX / 2), 0);

    assert_eq!(wait, MAX_WAIT);
}

#[tokio::test]
async fn the_loop_can_be_cancelled() {
    // A loop that could not be stopped would hold a task forever.
    let scheduler = Scheduler::new();
    let (deliver, _seen) = recorder();
    let runner = Arc::new(girder_core::Runner::new(scheduler, deliver));
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();

    // Returns rather than hanging: the cancellation is checked before the first wait.
    tokio::time::timeout(Duration::from_secs(5), runner.run(cancel))
        .await
        .expect("the loop returns on cancellation");
}

#[test]
fn a_runner_reports_whether_it_started() {
    let scheduler = Scheduler::new();
    let (deliver, _seen) = recorder();
    let runner = girder_core::Runner::new(scheduler, deliver);

    assert!(!runner.is_started());
}

#[test]
fn an_inactive_schedule_is_not_delivered() {
    let mut scheduler = Scheduler::new();
    let mut schedule = every_minute("s1", "paused");
    schedule.active = false;
    scheduler.add(schedule, now_millis()).expect("added");
    let (deliver, seen) = recorder();

    let result = girder_core::run_schedule_pass(&mut scheduler, &deliver, now_millis() + 120_000);

    assert_eq!(result.delivered, 0);
    assert!(seen.lock().expect("unpoisoned").is_empty());
}
