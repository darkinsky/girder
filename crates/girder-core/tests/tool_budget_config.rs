//! Tests for the stored tool budget and the watcher that notices it changing.
//!
//! The premise comes from `config_reload_e2e`, which pinned the gap this module fills: a
//! `tools.maxCalls` write reaches the store, and a `ToolTracker` built before it keeps its old
//! ceiling forever. The properties below, in the order they matter:
//!
//! - **the initial read resolves what the store holds** — an absent key uses the default, a stored
//!   count is used, and `limit()` is the tracker budget those imply
//! - **`refresh` reports a change exactly when the stored configuration changed** — the core of the
//!   module: after a write it returns `true` and `current()` holds the new number; with nothing
//!   written it returns `false` and leaves the budget, version, and warnings alone
//! - **a rebuild happens once per change, not once per call** — the failure mode the boolean exists
//!   to prevent, asserted by counting how many refreshes answered `true` across a run of calls
//! - **an invalid value falls back to the default and says so** — and a later valid value clears the
//!   warning rather than leaving a stale one
//! - **the version counts observed changes** — `0` at construction, one per change, unchanged by a
//!   refresh that finds nothing, and unchanged by a write that leaves the same number
//! - **an empty store is the documented default, not an error** — no warning, because a store nobody
//!   configured is not a mistake
//!
//! Included by path rather than through the crate's re-exports, so these tests compile against the
//! module as written even before the crate root wires it in. It spells its paths through
//! `girder_core::`, which is the alias the library declares for itself, so the same source builds
//! here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/tool_budget_config.rs"]
mod tool_budget_config;

use std::sync::Arc;

use girder_core::{MemorySettingsStore, SettingsStore, ToolTracker, ToolVerdict};
use serde_json::json;

use tool_budget_config::{Budget, DEFAULT_MAX_CALLS, MAX_CALLS_KEY, Warning, Watcher};

// ---- fixtures ---------------------------------------------------------------------------------

/// A store holding `count` under [`MAX_CALLS_KEY`].
fn store_with(count: u64) -> MemorySettingsStore {
    let store = MemorySettingsStore::new();
    store
        .set(MAX_CALLS_KEY, json!(count))
        .expect("the write succeeds");
    store
}

/// Writes `value` under [`MAX_CALLS_KEY`], which may be any JSON at all.
fn write(store: &dyn SettingsStore, value: serde_json::Value) {
    store.set(MAX_CALLS_KEY, value).expect("the write succeeds");
}

// ---- 1. the initial read ----------------------------------------------------------------------

#[test]
fn an_empty_store_uses_the_default_with_no_warning() {
    // A store nobody configured is the ordinary case, not a fault: the default applies and there is
    // nothing to report. A warning here would make every unconfigured deployment log noise.
    let store = MemorySettingsStore::new();
    let watcher = Watcher::new(&store);

    assert_eq!(watcher.current().max_calls, DEFAULT_MAX_CALLS);
    assert_eq!(
        watcher.current().version,
        0,
        "the initial read is version 0"
    );
    assert!(
        watcher.warnings().is_empty(),
        "nothing stored is not something wrong, got {:?}",
        watcher.warnings()
    );
}

#[test]
fn the_initial_read_uses_the_stored_count() {
    // The value has to reach the struct, so a store written before the watcher is built starts at the
    // written number — the half that already worked in `config_reload_e2e`.
    let store = store_with(7);
    let watcher = Watcher::new(&store);

    assert_eq!(watcher.current().max_calls, 7);
    assert_eq!(watcher.current().version, 0);
    assert!(watcher.warnings().is_empty());
}

#[test]
fn a_stored_zero_is_a_hard_stop_not_an_off_switch() {
    // `0` and `unlimited` are easy to confuse. The stored value must mean the former: reading zero as
    // "no ceiling" would invert a deployment's intent silently, and the first call would prove it.
    let store = store_with(0);
    let watcher = Watcher::new(&store);
    assert_eq!(watcher.current().max_calls, 0);

    let mut tracker = ToolTracker::new(watcher.limit());
    assert_eq!(
        tracker.record("bash", true),
        ToolVerdict::TooManyCalls {
            tool: "bash".to_owned(),
            limit: 0,
        },
        "a budget of zero refuses the first call rather than allowing every call"
    );
}

#[test]
fn the_limit_is_the_tracker_budget_the_stored_count_implies() {
    // The one place the two modules meet. `limit()` must produce a tracker that actually enforces the
    // number, or the wiring would hand the loop a budget that bounds nothing.
    let store = store_with(2);
    let watcher = Watcher::new(&store);
    let mut tracker = ToolTracker::new(watcher.limit());

    assert_eq!(tracker.record("bash", true), ToolVerdict::Allow);
    assert_eq!(tracker.record("bash", true), ToolVerdict::Allow);
    assert_eq!(
        tracker.record("bash", true),
        ToolVerdict::TooManyCalls {
            tool: "bash".to_owned(),
            limit: 2,
        }
    );
}

// ---- 2. refresh detects a change --------------------------------------------------------------

#[test]
fn refresh_reports_a_change_after_the_store_is_written() {
    // The core property of the module, and the one `config_reload_e2e` could not get from `ToolLimit`
    // alone: a watcher built before the write notices it on the next refresh.
    let store = store_with(5);
    let mut watcher = Watcher::new(&store);
    assert_eq!(watcher.current().max_calls, 5);

    write(&store, json!(1));

    assert!(
        watcher.refresh(),
        "the stored value changed from 5 to 1, so refresh must report it"
    );
    assert_eq!(
        watcher.current().max_calls,
        1,
        "the budget is updated before refresh returns, so the caller can rebuild from it"
    );
    assert_eq!(watcher.current().version, 1, "one change, one version bump");
}

#[test]
fn a_rebuilt_tracker_enforces_the_number_refresh_reported() {
    // A `true` is only useful if acting on it changes what happens. This walks the whole loop — write,
    // refresh, rebuild, judge — and asserts the verdict, not the struct field.
    let store = store_with(3);
    let mut watcher = Watcher::new(&store);
    let mut tracker = ToolTracker::new(watcher.limit());

    // Three calls fit the budget of three.
    for call in 1..=3 {
        assert_eq!(
            tracker.record("bash", true),
            ToolVerdict::Allow,
            "call {call} is within the stored budget"
        );
    }

    // The change, noticed the way the loop would notice it.
    write(&store, json!(5));
    assert!(watcher.refresh(), "the ceiling changed");
    // A rebuilt tracker starts from zero counts, so it is the new ceiling — not the calls the old
    // tracker already recorded — that decides what it refuses. The old tracker refused a fourth call;
    // this one does not.
    let mut tracker = ToolTracker::new(watcher.limit());
    for call in 1..=5 {
        assert_eq!(
            tracker.record("bash", true),
            ToolVerdict::Allow,
            "call {call} fits the new budget of five, which the old tracker would have refused at 4"
        );
    }
    assert_eq!(
        tracker.record("bash", true),
        ToolVerdict::TooManyCalls {
            tool: "bash".to_owned(),
            limit: 5,
        }
    );
}

#[test]
fn refresh_notices_loosening_as_well_as_tightening() {
    // A watcher that only reacted to a smaller number would be comparing with `<` and calling the
    // result equality. Both directions have to report a change.
    let store = store_with(1);
    let mut watcher = Watcher::new(&store);

    write(&store, json!(9));
    assert!(watcher.refresh(), "1 to 9 is a change");
    assert_eq!(watcher.current().max_calls, 9);

    write(&store, json!(2));
    assert!(watcher.refresh(), "9 to 2 is a change");
    assert_eq!(watcher.current().max_calls, 2);
    assert_eq!(watcher.current().version, 2, "two changes, two bumps");
}

#[test]
fn refresh_notices_a_key_being_removed() {
    // Deleting the setting returns the deployment to the default, which is a change like any other.
    let store = store_with(8);
    let mut watcher = Watcher::new(&store);
    assert_eq!(watcher.current().max_calls, 8);

    assert_eq!(
        store.remove(MAX_CALLS_KEY),
        Ok(true),
        "the fixture wrote the key, so removal must find it"
    );

    assert!(
        watcher.refresh(),
        "a removed key is a changed configuration"
    );
    assert_eq!(
        watcher.current().max_calls,
        DEFAULT_MAX_CALLS,
        "with nothing stored, the default applies again"
    );
    assert_eq!(watcher.current().version, 1);
}

// ---- 3. refresh with nothing to do returns false ----------------------------------------------

#[test]
fn refresh_returns_false_when_the_store_did_not_move() {
    // The other half of the core property. If this returned `true`, a caller would rebuild its tracker
    // on every call: the counts of the turn would be discarded each time and no ceiling would ever be
    // reached, so the budget would stop working while appearing to be enforced.
    let store = store_with(4);
    let mut watcher = Watcher::new(&store);

    assert!(!watcher.refresh(), "nothing was written");
    assert!(
        !watcher.refresh(),
        "a second refresh with nothing written must also report nothing"
    );
    assert_eq!(watcher.current().max_calls, 4);
    assert_eq!(
        watcher.current().version,
        0,
        "a refresh that found nothing is not a change, so the version does not move"
    );
}

#[test]
fn a_refresh_that_finds_nothing_leaves_the_warnings_alone() {
    // A refresh that returns `false` claims nothing happened. Clearing the warnings while claiming
    // that would make an invalid value silently self-heal on the next idle tick.
    let store = MemorySettingsStore::new();
    write(&store, json!("three"));
    let mut watcher = Watcher::new(&store);
    assert_eq!(
        watcher.warnings().len(),
        1,
        "the fixture is invalid on purpose"
    );

    assert!(!watcher.refresh(), "the store did not move");
    assert_eq!(
        watcher.warnings().len(),
        1,
        "an untouched store keeps the warning it produced"
    );
}

#[test]
fn a_rebuild_happens_once_per_change_not_once_per_call() {
    // The property a caller actually depends on, stated as the caller would experience it: across a
    // run of calls, how many times does the watcher ask for a rebuild? Once per change, never once per
    // refresh.
    let store = store_with(2);
    let mut watcher = Watcher::new(&store);

    let mut rebuilds = 0_u32;
    // Ten calls with one write between the fifth and the sixth.
    for call in 1..=10 {
        if call == 6 {
            write(&store, json!(6));
        }
        if watcher.refresh() {
            rebuilds += 1;
        }
    }

    assert_eq!(
        rebuilds, 1,
        "ten refreshes around one write must ask for exactly one rebuild, got {rebuilds}"
    );
    assert_eq!(watcher.current().version, 1);
}

// ---- 4. an invalid value falls back and is reported -------------------------------------------

#[test]
fn a_non_numeric_value_falls_back_to_the_default_and_warns() {
    // A string is the shape a hand-edited settings file produces. The turn must still be bounded, so
    // the default applies; the warning is what stops that from being invisible.
    let store = MemorySettingsStore::new();
    write(&store, json!("three"));
    let watcher = Watcher::new(&store);

    assert_eq!(watcher.current().max_calls, DEFAULT_MAX_CALLS);
    assert_eq!(watcher.warnings().len(), 1);

    let Warning::InvalidMaxCalls { key, stored, used } = &watcher.warnings()[0];
    assert_eq!(key, MAX_CALLS_KEY, "the warning names the key to fix");
    assert_eq!(stored, "\"three\"", "the warning repeats what was found");
    assert_eq!(*used, DEFAULT_MAX_CALLS, "and the ceiling actually in use");
}

#[test]
fn a_negative_value_falls_back_to_the_default_and_warns() {
    // `-1` cannot be read as a `u32`, and reading it as "no limit" instead of as an error would invert
    // the setting. It is reported like any other unusable value.
    let store = MemorySettingsStore::new();
    write(&store, json!(-1));
    let watcher = Watcher::new(&store);

    assert_eq!(watcher.current().max_calls, DEFAULT_MAX_CALLS);
    assert_eq!(watcher.warnings().len(), 1);
    assert!(matches!(
        watcher.warnings()[0],
        Warning::InvalidMaxCalls { used, .. } if used == DEFAULT_MAX_CALLS
    ));
}

#[test]
fn a_fractional_value_falls_back_to_the_default_and_warns() {
    // `1.5` calls is not a budget. Truncating it to one would silently enforce a number nobody wrote.
    let store = MemorySettingsStore::new();
    write(&store, json!(1.5));
    let watcher = Watcher::new(&store);

    assert_eq!(watcher.current().max_calls, DEFAULT_MAX_CALLS);
    assert_eq!(watcher.warnings().len(), 1);
}

#[test]
fn a_value_too_large_for_a_u32_falls_back_to_the_default_and_warns() {
    // Past `u32::MAX` the count cannot be represented, and clamping it to `u32::MAX` would turn a typo
    // into an unlimited budget — the opposite of what a configured ceiling is for.
    let store = MemorySettingsStore::new();
    write(&store, json!(u64::from(u32::MAX) + 1));
    let watcher = Watcher::new(&store);

    assert_eq!(watcher.current().max_calls, DEFAULT_MAX_CALLS);
    assert_eq!(watcher.warnings().len(), 1);
}

#[test]
fn a_stored_null_is_reported_rather_than_read_as_absent() {
    // Absent and `null` are different stored states, and the settings surface preserves the
    // difference. A written `null` is a value a person put there, so it is reported; an absent key is
    // an unconfigured store and is not.
    let store = MemorySettingsStore::new();
    write(&store, serde_json::Value::Null);
    let watcher = Watcher::new(&store);

    assert_eq!(watcher.current().max_calls, DEFAULT_MAX_CALLS);
    assert_eq!(
        watcher.warnings().len(),
        1,
        "a stored null is not the same as nothing stored"
    );
}

#[test]
fn an_invalid_value_still_counts_as_a_change() {
    // The stored configuration moved even though the effective ceiling did not. Reporting `false`
    // would deny the caller the one signal it has that a written setting is not doing anything.
    let store = store_with(3);
    let mut watcher = Watcher::new(&store);

    write(&store, json!("lots"));

    assert!(
        watcher.refresh(),
        "the stored value changed, even though both before and after resolve to a usable ceiling"
    );
    assert_eq!(watcher.current().max_calls, DEFAULT_MAX_CALLS);
    assert_eq!(watcher.current().version, 1);
    assert_eq!(watcher.warnings().len(), 1);
}

#[test]
fn a_fixed_value_clears_the_warning_it_replaced() {
    // A stale warning is worse than none: a caller logging it would keep reporting a broken setting
    // after someone repaired it.
    let store = MemorySettingsStore::new();
    write(&store, json!("three"));
    let mut watcher = Watcher::new(&store);
    assert_eq!(watcher.warnings().len(), 1);

    write(&store, json!(6));
    assert!(watcher.refresh(), "an invalid value was replaced");
    assert!(
        watcher.warnings().is_empty(),
        "the warning is replaced wholesale by the refresh, got {:?}",
        watcher.warnings()
    );
    assert_eq!(watcher.current().max_calls, 6);
}

#[test]
fn a_reported_value_is_shortened_but_still_names_the_value() {
    // The warning lands in a log line. A megabyte stored under the key must not bury the fact that it
    // is not a number, and the shortening must not panic on a multi-byte character.
    let store = MemorySettingsStore::new();
    write(&store, json!("数".repeat(200)));
    let watcher = Watcher::new(&store);

    let Warning::InvalidMaxCalls { stored, .. } = &watcher.warnings()[0];
    assert!(
        stored.chars().count() <= 41,
        "the rendered value is bounded, got {} characters",
        stored.chars().count()
    );
    assert!(
        stored.ends_with('…'),
        "a shortened value says so, got {stored:?}"
    );
}

#[test]
fn a_warning_names_the_key_and_the_ceiling_in_its_message() {
    // The message is read by a person who has to find the setting. It must contain both the key to
    // look at and the number that is in force instead.
    let store = MemorySettingsStore::new();
    write(&store, json!("three"));
    let watcher = Watcher::new(&store);

    let message = watcher.warnings()[0].to_string();
    assert!(
        message.contains(MAX_CALLS_KEY),
        "the message names the key, got {message:?}"
    );
    assert!(
        message.contains(&DEFAULT_MAX_CALLS.to_string()),
        "the message names the ceiling in use, got {message:?}"
    );
}

// ---- 5. the version counts observed changes ---------------------------------------------------

#[test]
fn the_version_increases_by_one_for_each_change() {
    // A caller logging a rebuild wants to name which one it was. One bump per change, no more.
    let store = store_with(1);
    let mut watcher = Watcher::new(&store);
    assert_eq!(watcher.current().version, 0);

    for (expected, count) in [(1_u64, 2_u64), (2, 3), (3, 10)] {
        write(&store, json!(count));
        assert!(watcher.refresh(), "{count} is a change");
        assert_eq!(
            watcher.current().version,
            expected,
            "the version counts observed changes, expected {expected}"
        );
    }
}

#[test]
fn the_version_does_not_move_when_the_stored_value_is_rewritten_unchanged() {
    // A store may be written the same value again — a settings form saved without edits does exactly
    // that. It is not a change, and a caller must not rebuild for it.
    let store = store_with(4);
    let mut watcher = Watcher::new(&store);

    write(&store, json!(4));

    assert!(
        !watcher.refresh(),
        "the same value written again is not a change"
    );
    assert_eq!(watcher.current().version, 0);
}

// ---- 6. owning the store ----------------------------------------------------------------------

#[test]
fn a_watcher_that_owns_its_store_sees_the_same_changes() {
    // The borrowed form cannot outlive the caller that built it, which is exactly the loop this module
    // is for. The owned form must behave identically, or the wiring would depend on a lifetime.
    let store: Arc<dyn SettingsStore> = Arc::new(MemorySettingsStore::new());
    let mut watcher = Watcher::from_arc(Arc::clone(&store));
    assert_eq!(watcher.current().max_calls, DEFAULT_MAX_CALLS);

    store.set(MAX_CALLS_KEY, json!(2)).expect("written");
    assert!(watcher.refresh(), "the write is visible through the Arc");
    assert_eq!(watcher.current().max_calls, 2);
    assert_eq!(watcher.current().version, 1);

    assert!(!watcher.refresh(), "and a second read finds nothing new");
}

// ---- 7. the budget type itself ----------------------------------------------------------------

#[test]
fn budget_compares_by_value() {
    // `Budget` is read back by callers that compare what they had against what they got, so equality
    // has to mean field-by-field equality rather than identity.
    assert_eq!(
        Budget {
            max_calls: 3,
            version: 1,
        },
        Budget {
            max_calls: 3,
            version: 1,
        }
    );
    assert_ne!(
        Budget {
            max_calls: 3,
            version: 1,
        },
        Budget {
            max_calls: 3,
            version: 2,
        },
        "two reads of different versions are different budgets"
    );
}
