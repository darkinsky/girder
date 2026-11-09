//! Tests for the scheduler (M4 of the v2 plan).
//!
//! The scheduler owns no timer: `tick(now)` answers what is due and when to look again. That is what
//! makes a year of daily reminders testable in milliseconds instead of a year.

use std::time::Duration;

use girder_core::{
    CronExpression, Offset, Rule, Schedule, ScheduleId, Scheduler, TimeOfDay, create_schedule,
    next_fire, pause_schedule, resume_schedule,
};

const T0: u64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;

fn id(value: &str) -> ScheduleId {
    ScheduleId::new(value).expect("valid")
}

fn every_minutes(minutes: u64) -> Schedule {
    create_schedule(
        id("s1"),
        "tick",
        "do the thing",
        Rule::every(Duration::from_secs(minutes * 60), T0).expect("valid"),
    )
    .expect("created")
}

fn daily_at(hour: u32) -> Schedule {
    create_schedule(
        id("s1"),
        "daily",
        "check in",
        Rule::daily(TimeOfDay::new(hour, 0, 0).unwrap(), Offset::UTC),
    )
    .expect("created")
}

// ---- adding -----------------------------------------------------------------------------------

#[test]
fn adding_computes_the_first_occurrence() {
    let mut scheduler = Scheduler::new();
    scheduler.add(every_minutes(10), T0).expect("added");

    assert_eq!(scheduler.next_due("s1"), Some(T0 + 10 * MINUTE));
}

#[test]
fn a_passed_one_shot_is_refused() {
    // Storing it would be a schedule that silently does nothing.
    let mut scheduler = Scheduler::new();
    let past = create_schedule(id("s1"), "t", "p", Rule::At { at: T0 }).unwrap();

    let error = scheduler.add(past, T0).expect_err("exhausted");

    assert!(matches!(error, girder_core::ScheduleError::Exhausted));
}

#[test]
fn removing_drops_the_schedule() {
    let mut scheduler = Scheduler::new();
    scheduler.add(every_minutes(10), T0).unwrap();

    assert!(scheduler.remove("s1").is_some());
    assert!(scheduler.next_due("s1").is_none());
    assert!(scheduler.remove("s1").is_none());
}

// ---- ticking ----------------------------------------------------------------------------------

#[test]
fn nothing_is_due_before_its_time() {
    let mut scheduler = Scheduler::new();
    scheduler.add(every_minutes(10), T0).unwrap();

    let tick = scheduler.tick(T0 + 5 * MINUTE);

    assert!(tick.firings.is_empty());
    assert_eq!(tick.next_wake, Some(T0 + 10 * MINUTE));
}

#[test]
fn a_due_schedule_fires() {
    let mut scheduler = Scheduler::new();
    scheduler.add(every_minutes(10), T0).unwrap();

    let tick = scheduler.tick(T0 + 10 * MINUTE);

    assert_eq!(tick.firings.len(), 1);
    assert_eq!(tick.firings[0].id.as_str(), "s1");
    assert_eq!(tick.firings[0].prompt, "do the thing");
    assert_eq!(tick.firings[0].due, T0 + 10 * MINUTE);
}

#[test]
fn a_firing_advances_to_the_next_occurrence() {
    let mut scheduler = Scheduler::new();
    scheduler.add(every_minutes(10), T0).unwrap();

    scheduler.tick(T0 + 10 * MINUTE);
    let tick = scheduler.tick(T0 + 10 * MINUTE);

    assert!(tick.firings.is_empty(), "it must not fire twice");
    assert_eq!(tick.next_wake, Some(T0 + 20 * MINUTE));
}

#[test]
fn a_long_gap_produces_one_firing_not_a_burst() {
    // A machine asleep for a day must not wake to 144 firings. A missed reminder should arrive once.
    let mut scheduler = Scheduler::new();
    scheduler.add(every_minutes(10), T0).unwrap();

    let tick = scheduler.tick(T0 + DAY);

    assert_eq!(
        tick.firings.len(),
        1,
        "one firing regardless of how many were missed"
    );
}

#[test]
fn several_due_schedules_fire_in_a_stable_order() {
    let mut scheduler = Scheduler::new();
    let mut a = every_minutes(5);
    a.id = id("a");
    let mut b = every_minutes(5);
    b.id = id("b");
    scheduler.add(a, T0).unwrap();
    scheduler.add(b, T0).unwrap();

    let first = scheduler.tick(T0 + 5 * MINUTE);
    let ids: Vec<&str> = first.firings.iter().map(|f| f.id.as_str()).collect();

    assert_eq!(ids, vec!["a", "b"], "sorted, so two runs agree");
}

#[test]
fn next_wake_reports_the_earliest_pending() {
    let mut scheduler = Scheduler::new();
    let mut soon = every_minutes(5);
    soon.id = id("soon");
    let mut later = every_minutes(60);
    later.id = id("later");
    scheduler.add(soon, T0).unwrap();
    scheduler.add(later, T0).unwrap();

    let tick = scheduler.tick(T0);

    assert_eq!(tick.next_wake, Some(T0 + 5 * MINUTE));
}

#[test]
fn an_empty_scheduler_has_nothing_to_wake_for() {
    let mut scheduler = Scheduler::new();
    let tick = scheduler.tick(T0);

    assert!(tick.firings.is_empty());
    assert_eq!(tick.next_wake, None);
}

#[test]
fn a_one_shot_fires_once_then_disappears() {
    let mut scheduler = Scheduler::new();
    let once = create_schedule(id("s1"), "t", "p", Rule::At { at: T0 + HOUR }).unwrap();
    scheduler.add(once, T0).unwrap();

    let tick = scheduler.tick(T0 + HOUR);
    assert_eq!(tick.firings.len(), 1);

    let after = scheduler.tick(T0 + 2 * HOUR);
    assert!(after.firings.is_empty());
    assert_eq!(after.next_wake, None);
    assert!(!scheduler.schedules()[0].active);
}

// ---- pausing ----------------------------------------------------------------------------------

#[test]
fn a_paused_schedule_does_not_fire() {
    let mut scheduler = Scheduler::new();
    scheduler.add(every_minutes(10), T0).unwrap();
    scheduler.pause("s1");

    let tick = scheduler.tick(T0 + HOUR);

    assert!(tick.firings.is_empty());
    assert_eq!(scheduler.next_due("s1"), None);
}

#[test]
fn resuming_computes_from_now_not_from_the_last_firing() {
    // A schedule paused for a week must not fire a week of missed occurrences at once.
    let mut scheduler = Scheduler::new();
    scheduler.add(every_minutes(10), T0).unwrap();
    scheduler.pause("s1");
    let much_later = T0 + 7 * DAY;

    scheduler.resume("s1", much_later).expect("resumed");
    let tick = scheduler.tick(much_later);

    assert!(
        tick.firings.is_empty(),
        "resuming must not fire immediately"
    );
    assert_eq!(scheduler.next_due("s1"), Some(much_later + 10 * MINUTE));
}

#[test]
fn resuming_an_unknown_schedule_is_refused() {
    let mut scheduler = Scheduler::new();
    assert!(scheduler.resume("nope", T0).is_err());
}

// ---- the loop ---------------------------------------------------------------------------------

#[tokio::test]
async fn the_loop_dispatches_a_firing_and_stops_on_cancel() {
    let scheduler = std::sync::Arc::new(std::sync::Mutex::new(Scheduler::new()));
    {
        let mut guard = scheduler.lock().unwrap();
        // Due almost immediately.
        let now = girder_core::now_millis();
        guard
            .add(
                create_schedule(
                    id("s1"),
                    "quick",
                    "deliver me",
                    Rule::after(Duration::from_millis(50), now),
                )
                .unwrap(),
                now,
            )
            .expect("added");
    }

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let cancel = tokio_util::sync::CancellationToken::new();
    let seen_clone = std::sync::Arc::clone(&seen);
    let cancel_clone = cancel.clone();

    let handle = tokio::spawn(async move {
        girder_core::run_scheduler(
            scheduler,
            move |firing| {
                let seen = std::sync::Arc::clone(&seen_clone);
                async move {
                    seen.lock().unwrap().push(firing.prompt);
                }
            },
            cancel_clone,
        )
        .await;
    });

    // Give it time to fire one occurrence.
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

    assert_eq!(seen.lock().unwrap().as_slice(), &["deliver me".to_owned()]);
}

#[test]
fn the_rule_helpers_still_work() {
    // A sanity check that the scheduler module did not disturb the rule surface.
    let rule = Rule::every(Duration::from_secs(600), T0).unwrap();
    assert_eq!(rule.next_after(T0).unwrap(), T0 + 600_000);

    let mut schedule = daily_at(9);
    assert!(next_fire(&schedule, T0).is_some());
    pause_schedule(&mut schedule);
    assert!(next_fire(&schedule, T0).is_none());
    resume_schedule(&mut schedule);
    assert!(next_fire(&schedule, T0).is_some());

    // A cron rule round-trips through the scheduler.
    let mut scheduler = Scheduler::new();
    let cron = create_schedule(
        id("c"),
        "cron",
        "p",
        Rule::cron(CronExpression::parse("0 9 * * *").unwrap(), Offset::UTC),
    )
    .unwrap();
    scheduler.add(cron, T0).expect("added");
    assert_eq!(scheduler.next_due("c"), Some(T0 + 9 * HOUR));
}
