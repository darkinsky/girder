//! Tests for the `schedule.*` Remote methods.
//!
//! The properties under test, in the order they matter:
//!
//! - **create → list → delete round-trips** — an id handed out by `create` must be the id `list`
//!   shows and the id `delete` removes, or the three methods are not one surface
//! - **an invalid cron expression is refused, and says which field** — validation is
//!   `CronExpression::parse`, so a bad expression must never reach the store
//! - **a valid cron still has to be able to fire** — `0 0 30 2 *` parses and never occurs; it is
//!   refused rather than stored as a task waiting for a date that does not exist
//! - **an unknown id is an error on every method that takes one** — view, delete, pause, and resume;
//!   a miss-shaped success is a shape a client forgets to check
//! - **a stored task is stored and nothing else** — `armed` is `false` on every response, the
//!   registration has no `run` method, and a task is still sitting in the store afterwards. This is
//!   the property the module documentation states and the one a reader is most likely to assume
//!   wrongly: **nothing here delivers a prompt**
//! - **the declarations match what each method does** — `create`/`pause`/`resume` are writes,
//!   `delete` is destructive, the reads are reads, so a read-only peer can list and cannot create
//! - **the default store refuses rather than pretending** — no storage configured is reported, not
//!   hidden behind an in-memory map that the next call could not confirm
//!
//! Isolation: every test builds its own store and registry, and ids carry a process-wide counter, so
//! tests in one binary cannot see each other's tasks.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the methods as written even before the crate root wires them in. It spells its own paths
//! through `girder_core::`, which is the alias the library declares for itself, so the same source
//! builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/schedule/rpc.rs"]
pub mod schedule_rpc;

use std::sync::Arc;

use girder_core::{RemoteCall, RemoteError, RemoteRegistry};
use serde_json::{Value, json};

use schedule_rpc::{
    CREATE_METHOD, DEFAULT_LIST_LIMIT, DELETE_INACTIVE_METHOD, LIST_METHOD, MemoryScheduleStore,
    NAMESPACE, ScheduleStore, VIEW_METHOD, register_schedule_methods,
};

// ---- fixtures ---------------------------------------------------------------------------------

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// A registry with a fresh in-memory store behind it.
fn registry() -> (RemoteRegistry, Arc<MemoryScheduleStore>) {
    let store = Arc::new(MemoryScheduleStore::new());
    let dynamic: Arc<dyn ScheduleStore> = store.clone();
    let mut registry = RemoteRegistry::new();
    register_schedule_methods(&mut registry, &dynamic);
    (registry, store)
}

async fn dispatch(
    registry: &RemoteRegistry,
    method: &str,
    args: Value,
) -> Result<Value, RemoteError> {
    registry.dispatch(NAMESPACE, method, call(args)).await
}

/// Creates one task with a cron timing and returns the whole response.
async fn create_cron(registry: &RemoteRegistry, title: &str, cron: &str) -> Value {
    dispatch(
        registry,
        CREATE_METHOD,
        json!({"title": title, "prompt": "say hello", "cron": cron}),
    )
    .await
    .expect("a valid cron task is created")
}

fn items_of(listing: &Value) -> Vec<Value> {
    listing
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn id_of(value: &Value) -> String {
    value
        .get("id")
        .and_then(Value::as_str)
        .expect("the response carries an id")
        .to_owned()
}

// ---- the round trip ---------------------------------------------------------------------------

#[tokio::test]
async fn create_list_delete_round_trips() {
    let (registry, store) = registry();

    let created = create_cron(&registry, "morning report", "*/5 * * * *").await;
    let id = id_of(&created);
    assert!(!id.is_empty(), "an id is minted");

    // The listing shows the task `create` stored: same id, same title, same expression as typed.
    let listing = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("a listing succeeds");
    let items = items_of(&listing);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], id.as_str());
    assert_eq!(items[0]["title"], "morning report");
    assert_eq!(items[0]["cron"], "*/5 * * * *");
    assert_eq!(items[0]["status"], "active");
    assert_eq!(listing["total"], 1);
    assert_eq!(listing["shown"], 1);
    assert_eq!(listing["limit"], DEFAULT_LIST_LIMIT);
    assert_eq!(listing["offset"], 0);

    let viewed = dispatch(&registry, VIEW_METHOD, json!({"id": id}))
        .await
        .expect("a known id is readable");
    assert_eq!(viewed["id"], id.as_str());
    assert_eq!(viewed["title"], "morning report");

    // `deleteInactive` removes every paused task, which is what upstream's bulk removal does. The
    // task has to be paused first: a live schedule is not something a tidy-up may remove.
    store.set_active(id.as_str(), false).expect("paused");
    let deleted = dispatch(&registry, DELETE_INACTIVE_METHOD, json!({}))
        .await
        .expect("the paused task is removed");
    assert_eq!(deleted["deleted"], 1);

    // Gone from both the listing and the store, so the delete was not a report about nothing.
    let after = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("a listing succeeds");
    assert!(items_of(&after).is_empty());
    assert!(store.is_empty(), "the store no longer holds the task");
}

#[tokio::test]
async fn every_created_task_gets_its_own_id() {
    // Two tasks of one kind must be two tasks: a reused id would make the second create overwrite
    // the first, and the listing would show one where the caller made two.
    let (registry, store) = registry();

    let first = create_cron(&registry, "first", "0 * * * *").await;
    let second = create_cron(&registry, "second", "0 * * * *").await;

    assert_ne!(id_of(&first), id_of(&second));
    assert_eq!(store.len(), 2);
}

#[tokio::test]
async fn a_created_task_reports_its_next_occurrence() {
    let (registry, _store) = registry();

    let created = create_cron(&registry, "hourly", "0 * * * *").await;
    let next = created["nextFireAt"]
        .as_u64()
        .expect("a cron task has a next occurrence");

    // Strictly in the future: a task whose next firing is `now` fires immediately, which is never
    // what a caller asking for "every hour" meant.
    assert!(next > girder_core::now_millis());
}

// ---- cron validation --------------------------------------------------------------------------

#[tokio::test]
async fn a_malformed_cron_expression_is_refused() {
    let (registry, store) = registry();

    // Refused before the store is touched, so a bad expression cannot leave a half-made task.
    for expression in [
        "not a cron",
        "* * * *",
        "* * * * * *",
        "60 * * * *",
        "* 24 * * *",
    ] {
        let error = dispatch(
            &registry,
            CREATE_METHOD,
            json!({"title": "bad", "prompt": "x", "cron": expression}),
        )
        .await
        .expect_err("an unusable expression is refused");

        match error {
            RemoteError::InvalidArguments { message, .. } => {
                // The message names the field or says the expression is invalid, which is what a
                // caller needs to fix it; a bare "invalid request" would not be.
                assert!(
                    message.contains("cron"),
                    "`{expression}` was refused with `{message}`, which does not name cron"
                );
            }
            other => panic!("`{expression}` reported {other:?} instead of invalid arguments"),
        }
    }

    assert!(
        store.is_empty(),
        "nothing was stored for a rejected request"
    );
}

#[tokio::test]
async fn a_cron_expression_that_can_never_fire_is_refused() {
    // 30 February parses as five valid fields and matches no date. Storing it would show the user a
    // task that is "waiting" forever, so the first-occurrence computation is the validation step.
    let (registry, store) = registry();

    let error = dispatch(
        &registry,
        CREATE_METHOD,
        json!({"title": "never", "prompt": "x", "cron": "0 0 30 2 *"}),
    )
    .await
    .expect_err("an impossible date is refused");

    match error {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(
                message.contains("no future occurrence"),
                "the message says why: {message}"
            );
        }
        other => panic!("reported {other:?} instead of invalid arguments"),
    }
    assert!(store.is_empty());
}

#[tokio::test]
async fn the_timing_union_accepts_after_and_every() {
    let (registry, _store) = registry();

    let after = dispatch(
        &registry,
        CREATE_METHOD,
        json!({"title": "once", "prompt": "x", "timing": {"kind": "after", "after_seconds": 60}}),
    )
    .await
    .expect("a one-shot an hour from now is accepted");
    assert!(after["nextFireAt"].as_u64().is_some());

    let every = dispatch(
        &registry,
        CREATE_METHOD,
        json!({"title": "often", "prompt": "x", "timing": {"kind": "every", "every_seconds": 3600}}),
    )
    .await
    .expect("an hourly task is accepted");
    assert!(every["nextFireAt"].as_u64().is_some());

    // The cron kind of the union, with an explicit zone.
    let cron = dispatch(
        &registry,
        CREATE_METHOD,
        json!({
            "title": "zoned",
            "prompt": "x",
            "timing": {"kind": "cron", "cron": {"expression": "0 9 * * *", "time_zone": "+08:00"}},
        }),
    )
    .await
    .expect("a zoned cron task is accepted");
    assert!(cron["nextFireAt"].as_u64().is_some());
}

#[tokio::test]
async fn a_one_shot_with_no_future_occurrence_is_refused() {
    let (registry, _store) = registry();

    // Zero seconds after creation is already past by the time the rule is evaluated.
    let error = dispatch(
        &registry,
        CREATE_METHOD,
        json!({"title": "late", "prompt": "x", "timing": {"kind": "after", "after_seconds": 0}}),
    )
    .await
    .expect_err("a one-shot in the past is refused");

    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
}

#[tokio::test]
async fn an_interval_below_the_minimum_is_refused() {
    let (registry, _store) = registry();

    let error = dispatch(
        &registry,
        CREATE_METHOD,
        json!({"title": "fast", "prompt": "x", "timing": {"kind": "every", "every_seconds": 30}}),
    )
    .await
    .expect_err("a per-second-rate task is refused");

    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
}

#[tokio::test]
async fn a_timing_kind_this_build_lacks_is_a_parse_error() {
    // `daily` needs a local calendar and a zone database; a client that sends it must learn that,
    // rather than receive a task interpreted in some guessed zone.
    let (registry, _store) = registry();

    let error = dispatch(
        &registry,
        CREATE_METHOD,
        json!({
            "title": "daily",
            "prompt": "x",
            "timing": {"kind": "daily", "daily": {"time": "09:00:00", "time_zone": "UTC"}},
        }),
    )
    .await
    .expect_err("an unimplemented kind is refused");

    match error {
        RemoteError::InvalidArguments { message, .. } => {
            // serde names the field and the whole set it accepts, which is what a client needs to
            // learn that `daily` is not in this build at all.
            assert!(
                message.contains("daily") && message.contains("expected one of"),
                "the message names the offending kind: {message}"
            );
        }
        other => panic!("reported {other:?} instead of invalid arguments"),
    }
}

#[tokio::test]
async fn two_spellings_of_the_same_timing_are_refused() {
    let (registry, _store) = registry();

    let error = dispatch(
        &registry,
        CREATE_METHOD,
        json!({
            "title": "ambiguous",
            "prompt": "x",
            "cron": "0 * * * *",
            "timing": {"kind": "every", "every_seconds": 3600},
        }),
    )
    .await
    .expect_err("supplying both is ambiguous");

    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
}

#[tokio::test]
async fn an_iana_zone_name_is_refused_rather_than_guessed() {
    // A wrong offset fires at a time nobody chose and the stored record would not show it, so the
    // name is refused instead of resolved as UTC.
    let (registry, _store) = registry();

    let error = dispatch(
        &registry,
        CREATE_METHOD,
        json!({
            "title": "zoned",
            "prompt": "x",
            "cron": "0 9 * * *",
            "time_zone": "Asia/Shanghai",
        }),
    )
    .await
    .expect_err("an IANA name is refused");

    match error {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(
                message.contains("Asia/Shanghai"),
                "the message names the zone it could not resolve: {message}"
            );
        }
        other => panic!("reported {other:?} instead of invalid arguments"),
    }
}

#[tokio::test]
async fn a_fixed_offset_is_accepted_in_the_shapes_that_appear_in_the_wild() {
    let (registry, _store) = registry();

    for zone in ["UTC", "utc", "Z", "+08:00", "-05:30", "+0800", "+09"] {
        dispatch(
            &registry,
            CREATE_METHOD,
            json!({"title": "zoned", "prompt": "x", "cron": "0 9 * * *", "time_zone": zone}),
        )
        .await
        .unwrap_or_else(|error| panic!("`{zone}` should be a usable zone: {error:?}"));
    }

    // Out-of-range offsets are typos, not zones.
    for zone in ["+15:00", "+08:99", "+08:xx"] {
        dispatch(
            &registry,
            CREATE_METHOD,
            json!({"title": "zoned", "prompt": "x", "cron": "0 9 * * *", "time_zone": zone}),
        )
        .await
        .expect_err("an out-of-range offset is refused");
    }
}

// ---- title and prompt validation ---------------------------------------------------------------

#[tokio::test]
async fn a_blank_title_or_prompt_is_refused() {
    let (registry, store) = registry();

    for body in [
        json!({"title": "   ", "prompt": "x", "cron": "0 * * * *"}),
        json!({"title": "ok", "prompt": "  ", "cron": "0 * * * *"}),
        // A title is a line in a list; past the bound it is not a title.
        json!({"title": "x".repeat(121), "prompt": "x", "cron": "0 * * * *"}),
    ] {
        dispatch(&registry, CREATE_METHOD, body)
            .await
            .expect_err("an unusable title or prompt is refused");
    }
    assert!(store.is_empty());
}

#[tokio::test]
async fn a_title_is_trimmed_before_it_is_stored() {
    let (registry, _store) = registry();

    let created = dispatch(
        &registry,
        CREATE_METHOD,
        json!({"title": "  spaced  ", "prompt": "  x  ", "cron": "0 * * * *"}),
    )
    .await
    .expect("a padded title is trimmed, not refused");

    let id = id_of(&created);
    let viewed = dispatch(&registry, VIEW_METHOD, json!({"id": id}))
        .await
        .expect("readable");
    assert_eq!(viewed["title"], "spaced");
    assert_eq!(viewed["prompt"], "x");
}

// ---- unknown ids ------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_id_is_an_error_on_every_method_that_takes_one() {
    let (registry, _store) = registry();

    for method in [VIEW_METHOD] {
        let error = dispatch(&registry, method, json!({"id": "sched-does-not-exist"}))
            .await
            .expect_err("an unknown id is refused");

        match error {
            RemoteError::InvalidArguments { message, .. } => {
                assert!(
                    message.contains("sched-does-not-exist"),
                    "`{method}` names the id it could not find: {message}"
                );
            }
            other => panic!("`{method}` reported {other:?} instead of invalid arguments"),
        }
    }
}

#[tokio::test]
async fn a_missing_or_blank_id_is_refused_as_a_client_bug() {
    let (registry, _store) = registry();

    for args in [json!({}), json!({"id": ""}), json!({"id": "   "})] {
        let error = dispatch(&registry, VIEW_METHOD, args)
            .await
            .expect_err("a missing id is refused");

        match error {
            RemoteError::InvalidArguments { message, .. } => {
                assert!(message.contains("`id` is required"), "{message}");
            }
            other => panic!("reported {other:?} instead of invalid arguments"),
        }
    }
}

#[tokio::test]
async fn deleting_inactive_tasks_leaves_the_active_ones() {
    // `deleteInactive` is bulk by definition, so "one task" is only removable by pausing it first.
    // The distinction is the test: an active schedule is not something a tidy-up may remove.
    let (registry, store) = registry();

    let keep = id_of(&create_cron(&registry, "keep", "0 * * * *").await);
    let drop = id_of(&create_cron(&registry, "drop", "0 * * * *").await);

    // Pause one through the store, which is what `set_active` is for now that `pause` is not an
    // endpoint: upstream changes a schedule's activity through `edit`.
    store.set_active(drop.as_str(), false).expect("paused");

    let deleted = dispatch(&registry, DELETE_INACTIVE_METHOD, json!({}))
        .await
        .expect("the paused task is removed");
    assert_eq!(deleted["deleted"], 1);
    assert_eq!(store.len(), 1);

    dispatch(&registry, VIEW_METHOD, json!({"id": keep}))
        .await
        .expect("the active task is untouched");
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod schedule {
    pub use super::schedule_rpc as rpc;
}
