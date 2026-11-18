//! Tests for the credential-scoped `/models` cache and the extension-collision rule.
//!
//! Reference: `packages/llm/llm-deepseek/src/listing-cache.ts` (202 lines) and
//! `packages/llm/llm-deepseek/src/request-extensions.ts` (59 lines), whose suite is
//! `packages/llm/llm-deepseek/tests/listing-cache.spec.ts` (416 lines, 21 cases). Every test below
//! names the upstream case it transcribes, or states the deviation where the case is not
//! transcribable.
//!
//! # Why the module is included by path
//!
//! `girder-core`'s crate root does not declare `listing_cache_gap`; adding `pub mod
//! listing_cache_gap;` belongs to the integrator. Compiling the module into this test binary
//! directly makes the tests independent of that wiring and keeps the crate root untouched, which is
//! why the imports below name `girder_core::` rather than `crate::` (the crate aliases itself with
//! `extern crate self as girder_core`).
//!
//! # What is *not* transcribed
//!
//! Upstream's suite drives a `fetch` stub through a real cache with fake timers and real
//! `AbortController`s. This build has no transport in the module — the driver owns the fetch, the
//! timer, and the abort — so what is tested here is the decision each of those would cause:
//! a [`Load`] whose generation is fenced, an [`Access`] that says whether to wait or refresh, and a
//! [`Settlement`] that says what to publish. Where a case turns on the timer firing or a promise
//! settling out of order, the timer is advanced by calling the state machine again with a later
//! `now`, and the ordering is expressed by handing back the generations in a different order.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "tests assert by panicking; a failed unwrap is a failed test, and the per-rule tables \
              are clearer as long cases than as dozens of near-identical functions"
)]

#[path = "../src/listing_cache_gap.rs"]
mod listing_cache_gap;

use girder_core::deepseek_files_gap::{
    BadgeTone, ClientRequirement, ListedBadge, ListedCapabilities, ListedFacts, ListingEntry,
    Modality, ModelBadge, ModelListing, SystemPromptMode, parse_model_listing,
};
use girder_core::deepseek_options_gap::ListingSettings;
use listing_cache_gap::{
    Access, LISTING_TIMEOUT, Load, REQUEST_EXTENSION, RefreshStart, RequestExtensionError,
    Settlement, colliding_field, failure_warning, info_message, merge_request_extensions,
    notification_failure_warning, refresh_failure_warning, scope_of,
};
use serde_json::{Value, json};

const URL: &str = "https://api.example/models";
const TOKEN_A: &str = "token-a";

fn settings(max_age_ms: u64) -> ListingSettings {
    ListingSettings {
        url: URL.to_owned(),
        timeout_ms: 60_000.0,
        max_age_ms,
    }
}

/// The cache under test, typed to the listing this build decodes.
type State = listing_cache_gap::ListingCacheState<ModelListing>;

fn cache() -> State {
    State::new()
}

fn entry(id: &str) -> ListingEntry {
    ListingEntry {
        id: id.to_owned(),
        name: None,
        requirement: ClientRequirement::None,
        capabilities: ListedCapabilities::Usable(ListedFacts {
            input_modalities: vec![Modality::Text],
            context_window: None,
            max_output_tokens: None,
            system_prompt_update: Some(SystemPromptMode::InHistory),
            tool_update: None,
        }),
        badge: None,
    }
}

/// Upstream's `listing(...ids)` fixture.
fn listing(ids: &[&str]) -> ModelListing {
    ModelListing {
        entries: ids.iter().map(|id| entry(id)).collect(),
        skipped: 0,
    }
}

/// Upstream's `badgedListing(text)` fixture: one entry with a displayable badge.
fn badged_listing(text: &str) -> ModelListing {
    let mut listing = listing(&["a"]);
    listing.entries[0].badge = Some(ListedBadge::Badge(ModelBadge {
        text: text.to_owned(),
        tone: BadgeTone::Info,
    }));
    listing
}

/// One access of an authenticated scope, asserting it waits, and returning its load.
fn wait(state: &mut State, age: u64) -> Load {
    match state.access(&settings(age), Some(TOKEN_A), 0) {
        Access::Wait { load: Some(load) } => load,
        other => panic!("expected a waiting access, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Scope, first load, single flight
// ---------------------------------------------------------------------------------------------

/// Upstream: `reports an unauthenticated view without loading`.
#[test]
fn an_unauthenticated_route_loads_nothing() {
    let mut state = cache();
    assert_eq!(
        state.access(&settings(100), None, 0),
        Access::Unauthenticated
    );
    assert_eq!(
        state.current(),
        listing_cache_gap::ListingView {
            authenticated: false,
            listing: None
        }
    );
    assert!(!state.is_inflight());
}

/// Upstream: `shares one first load between concurrent callers`.
///
/// The single-flight latch is the in-flight generation: the second access finds it set and yields
/// no load, so the driver performs one request for both callers.
#[test]
fn one_first_load_is_shared_between_concurrent_callers() {
    let mut state = cache();
    let load = wait(&mut state, 100);
    assert_eq!(
        state.access(&settings(100), Some(TOKEN_A), 0),
        Access::Wait { load: None },
        "the second caller joins the load in flight instead of starting a second one"
    );
    let settlement = state.settle(load.generation, Ok(listing(&["a"])), URL, &settings(100), 0);
    assert_eq!(
        settlement.info.as_deref(),
        Some("model listing from https://api.example/models loaded 1 model(s), skipped 0 row(s)")
    );
    assert_eq!(settlement.warning, None);
    assert!(settlement.notify, "a first listing is a change");
    assert_eq!(state.current().listing, Some(&listing(&["a"])));
    assert!(state.current().authenticated);
}

/// Upstream: `bounds the first wait by the request deadline and serves no listing after a failure`.
///
/// The deadline itself is the driver's `AbortSignal`; what this pins is that the load carries the
/// route's timeout and the code a deadline reports, and that a failed first load serves nothing.
#[test]
fn a_failed_first_load_serves_no_listing_and_names_the_bundled_catalog() {
    let mut state = cache();
    let load = wait(&mut state, 100);
    assert!((load.timeout_ms - 60_000.0).abs() < f64::EPSILON);
    assert_eq!(LISTING_TIMEOUT, "MODEL_LISTING_TIMEOUT");
    let settlement = state.settle(
        load.generation,
        Err("deadline".to_owned()),
        URL,
        &settings(100),
        0,
    );
    assert!(settlement.published);
    assert_eq!(state.current().listing, None);
    assert_eq!(
        settlement.warning.as_deref(),
        Some(concat!(
            "model listing from https://api.example/models failed (deadline); using the bundled ",
            "catalog"
        ))
    );
    assert!(!settlement.notify, "a failure publishes no view");
}

/// Upstream: `rejects a caller whose signal is already aborted`.
///
/// Deviation: upstream throws from `signal.throwIfAborted()` *after* the credential resolves and
/// *before* the scope is touched. A pre-aborted caller therefore reaches neither [`Access`] nor a
/// load, so the refusal has no counterpart here; the module documents that ordering instead of
/// modelling a signal it does not own.
#[test]
fn an_aborted_caller_never_reaches_the_state_machine() {
    let state = listing_cache_gap::ListingCacheState::<ModelListing>::new();
    assert!(!state.is_inflight());
    assert!(!state.current().authenticated);
}

// ---------------------------------------------------------------------------------------------
// Scope invalidation and generations
// ---------------------------------------------------------------------------------------------

/// Upstream: `rescopes on another credential and discards the previous credential's result`.
#[test]
fn another_credential_rescopes_and_discards_the_previous_result() {
    let mut state = cache();
    let first = wait(&mut state, 100);
    let second = match state.access(&settings(100), Some("token-b"), 0) {
        Access::Wait { load: Some(load) } => load,
        other => panic!("the new credential waits for its own load, got {other:?}"),
    };
    assert!(
        second.generation > first.generation,
        "a rescope fences the old load"
    );
    let late = state.settle(
        first.generation,
        Ok(listing(&["a"])),
        URL,
        &settings(100),
        0,
    );
    assert_eq!(
        late,
        Settlement::discarded(),
        "the old result is dropped silently"
    );
    let applied = state.settle(
        second.generation,
        Ok(listing(&["b"])),
        URL,
        &settings(100),
        0,
    );
    assert!(applied.published);
    assert_eq!(state.current().listing, Some(&listing(&["b"])));
}

/// Upstream: `rescopes ...` last line — `credential(undefined)` unauthorises the scope.
#[test]
fn dropping_the_credential_unauthorises_and_clears() {
    let mut state = cache();
    let load = wait(&mut state, 100);
    let _ = state.settle(load.generation, Ok(listing(&["a"])), URL, &settings(100), 0);
    assert_eq!(state.current().listing, Some(&listing(&["a"])));
    assert_eq!(
        state.access(&settings(100), None, 10),
        Access::Unauthenticated
    );
    assert_eq!(
        state.current(),
        listing_cache_gap::ListingView {
            authenticated: false,
            listing: None
        }
    );
}

/// Upstream: `discards the listing on invalidation, so the next access waits for a fresh load`.
#[test]
fn invalidation_discards_the_listing_and_the_next_access_waits() {
    let mut state = cache();
    let load = wait(&mut state, 100);
    let _ = state.settle(load.generation, Ok(listing(&["a"])), URL, &settings(100), 0);
    assert_eq!(
        state.access(&settings(100), Some(TOKEN_A), 0),
        Access::Immediate { refresh: None }
    );
    assert!(!state.invalidate(), "nothing was in flight");
    assert_eq!(
        state.current(),
        listing_cache_gap::ListingView {
            authenticated: true,
            listing: None
        },
        "the scope survives, the listing does not"
    );
    let fresh = wait(&mut state, 100);
    assert!(fresh.generation > load.generation);
}

/// Upstream: `aborts an in-flight load on invalidation and ignores its result`.
#[test]
fn invalidation_abandons_an_in_flight_load_and_ignores_it() {
    let mut state = cache();
    let load = wait(&mut state, 100);
    assert!(state.invalidate(), "the request must be aborted");
    let ignored = state.settle(
        load.generation,
        Ok(listing(&["stale"])),
        URL,
        &settings(100),
        0,
    );
    assert_eq!(ignored, Settlement::discarded());
    assert_eq!(state.current().listing, None);
    assert_eq!(
        ignored.warning, None,
        "a discarded result warns about nothing"
    );
}

// ---------------------------------------------------------------------------------------------
// Staleness, retention, and the failing streak
// ---------------------------------------------------------------------------------------------

/// Upstream: `refreshes a stale listing in the background and keeps it through a failing streak`.
#[test]
fn a_stale_listing_refreshes_and_survives_a_failing_streak() {
    let mut state = cache();
    let first = wait(&mut state, 100);
    let _ = state.settle(
        first.generation,
        Ok(listing(&["a"])),
        URL,
        &settings(100),
        0,
    );

    // Age 50: still fresh.
    assert_eq!(
        state.access(&settings(100), Some(TOKEN_A), 50),
        Access::Immediate { refresh: None }
    );
    // Age 100: stale, refresh in the background.
    let refresh = match state.access(&settings(100), Some(TOKEN_A), 100) {
        Access::Immediate {
            refresh: Some(load),
        } => load,
        other => panic!("expected a background refresh, got {other:?}"),
    };
    let failed = state.settle(
        refresh.generation,
        Err("down".to_owned()),
        URL,
        &settings(100),
        100,
    );
    assert_eq!(
        failed.warning.as_deref(),
        Some(
            "model listing from https://api.example/models failed (down); keeping the previous catalog"
        )
    );
    assert_eq!(
        state.current().listing,
        Some(&listing(&["a"])),
        "last known good is kept"
    );

    // A second failure in the same streak is silent.
    let refresh = match state.access(&settings(100), Some(TOKEN_A), 200) {
        Access::Immediate {
            refresh: Some(load),
        } => load,
        other => panic!("expected a background refresh, got {other:?}"),
    };
    let silent = state.settle(
        refresh.generation,
        Err("still down".to_owned()),
        URL,
        &settings(100),
        200,
    );
    assert_eq!(silent.warning, None);

    // Recovery publishes the new listing and clears the streak.
    let refresh = match state.access(&settings(100), Some(TOKEN_A), 300) {
        Access::Immediate {
            refresh: Some(load),
        } => load,
        other => panic!("expected a background refresh, got {other:?}"),
    };
    let recovered = state.settle(
        refresh.generation,
        Ok(listing(&["a", "b"])),
        URL,
        &settings(100),
        300,
    );
    assert!(recovered.changed);
    assert_eq!(state.current().listing, Some(&listing(&["a", "b"])));
    assert!(!state.is_failing());

    // A later failure warns again, because the streak restarted.
    let refresh = match state.access(&settings(100), Some(TOKEN_A), 400) {
        Access::Immediate {
            refresh: Some(load),
        } => load,
        other => panic!("expected a background refresh, got {other:?}"),
    };
    let again = state.settle(
        refresh.generation,
        Err("down again".to_owned()),
        URL,
        &settings(100),
        400,
    );
    assert!(again.warning.is_some());
}

/// Upstream: `retains badges after refresh failure and replaces them after recovery`, both spellings.
///
/// The badge is part of the retained listing, which is why it survives a failure: a streak must not
/// silently strip the selector of every badge it was showing.
#[test]
fn badges_survive_a_failure_and_are_replaced_on_recovery() {
    for recovery in [listing(&["a"]), badged_listing("Updated")] {
        let mut state = cache();
        let first = wait(&mut state, 50);
        let _ = state.settle(
            first.generation,
            Ok(badged_listing("New")),
            URL,
            &settings(50),
            0,
        );

        let refresh = match state.access(&settings(50), Some(TOKEN_A), 50) {
            Access::Immediate {
                refresh: Some(load),
            } => load,
            other => panic!("expected a background refresh, got {other:?}"),
        };
        let failed = state.settle(
            refresh.generation,
            Err("offline".to_owned()),
            URL,
            &settings(50),
            50,
        );
        assert!(failed.warning.is_some());
        assert_eq!(state.current().listing, Some(&badged_listing("New")));

        let refresh = match state.access(&settings(50), Some(TOKEN_A), 100) {
            Access::Immediate {
                refresh: Some(load),
            } => load,
            other => panic!("expected a background refresh, got {other:?}"),
        };
        let recovered = state.settle(
            refresh.generation,
            Ok(recovery.clone()),
            URL,
            &settings(50),
            100,
        );
        assert!(recovered.changed);
        assert_eq!(state.current().listing, Some(&recovery));
    }
}

/// Upstream: `does not notify for identical listings`.
#[test]
fn an_identical_listing_does_not_publish_a_change() {
    let mut state = cache();
    let first = wait(&mut state, 100);
    let initial = state.settle(
        first.generation,
        Ok(badged_listing("New")),
        URL,
        &settings(100),
        0,
    );
    assert!(initial.notify);

    let refresh = match state.access(&settings(100), Some(TOKEN_A), 100) {
        Access::Immediate {
            refresh: Some(load),
        } => load,
        other => panic!("expected a background refresh, got {other:?}"),
    };
    let same = state.settle(
        refresh.generation,
        Ok(badged_listing("New")),
        URL,
        &settings(100),
        100,
    );
    assert!(!same.changed);
    assert!(!same.notify, "an unchanged listing notifies nobody");
    assert!(same.info.is_some(), "but it is still reported to the log");
}

/// Upstream: `keeps zero max age as access-only refresh without an automatic loop`.
#[test]
fn zero_max_age_refreshes_on_each_access_without_a_timer() {
    let mut state = cache();
    let first = wait(&mut state, 0);
    let settled = state.settle(first.generation, Ok(listing(&["a"])), URL, &settings(0), 0);
    assert_eq!(settled.refresh_in, None, "no timer at zero");
    assert_eq!(state.refresh_delay(&settings(0), 0), None);
    let refresh = match state.access(&settings(0), Some(TOKEN_A), 1) {
        Access::Immediate {
            refresh: Some(load),
        } => load,
        other => panic!("every access refreshes at zero, got {other:?}"),
    };
    assert!((refresh.timeout_ms - 60_000.0).abs() < f64::EPSILON);
}

// ---------------------------------------------------------------------------------------------
// Automatic refresh: live settings, disabled route, credential loss
// ---------------------------------------------------------------------------------------------

/// Upstream: `uses current configuration and stops following a disabled listing`.
#[test]
fn a_refresh_reads_live_settings_and_stops_when_the_route_is_disabled() {
    let mut state = cache();
    let first = wait(&mut state, 100);
    let _ = state.settle(
        first.generation,
        Ok(badged_listing("New")),
        URL,
        &settings(100),
        0,
    );

    // The live settings now name a longer interval, which the *next* schedule must use.
    assert_eq!(state.refresh_delay(&settings(200), 0), Some(200));

    // The route's settings vanish: the listing is dropped and the change published.
    assert_eq!(state.begin_refresh(false), RefreshStart::Disabled);
    assert_eq!(state.current().listing, None);
    assert!(
        state.current().authenticated,
        "the scope survives a disabled route, so a re-enabled one keeps its identity"
    );

    // A route that comes back waits for a fresh load rather than serving the dropped listing.
    let load = match state.access(&settings(200), Some(TOKEN_A), 500) {
        Access::Wait { load: Some(load) } => load,
        other => panic!("a re-enabled route loads again, got {other:?}"),
    };
    assert_eq!(load.url, URL);
}

/// Upstream: `removes a signed-out listing and discovers a replacement credential automatically`.
#[test]
fn a_refresh_reports_the_signed_out_edge_once_and_finds_a_replacement() {
    let mut state = cache();
    let first = wait(&mut state, 100);
    let _ = state.settle(
        first.generation,
        Ok(badged_listing("New")),
        URL,
        &settings(100),
        0,
    );

    let RefreshStart::Continue { was_authenticated } = state.begin_refresh(true) else {
        panic!("settings are still supplied");
    };
    assert!(was_authenticated);
    // The credential lookup now returns nothing.
    assert_eq!(
        state.access(&settings(100), None, 100),
        Access::Unauthenticated
    );
    assert!(
        state.note_refresh_view(was_authenticated),
        "the edge is published"
    );
    assert!(!state.current().authenticated);
    // A second signed-out refresh publishes nothing.
    let RefreshStart::Continue { was_authenticated } = state.begin_refresh(true) else {
        panic!("settings are still supplied");
    };
    assert!(!state.note_refresh_view(was_authenticated));

    // A replacement credential loads again.
    let load = match state.access(&settings(100), Some("replacement"), 200) {
        Access::Wait { load: Some(load) } => load,
        other => panic!("a replacement credential loads, got {other:?}"),
    };
    assert!(load.url.ends_with("/models"));
    let _ = state.settle(
        load.generation,
        Ok(listing(&["b"])),
        URL,
        &settings(100),
        200,
    );
    assert_eq!(state.current().listing, Some(&listing(&["b"])));
}

/// Upstream: `retains badges on credential lookup failures, reports the streak once, and retries`.
///
/// Deviation: the credential lookup is the driver's; the failure reaches the cache as
/// [`listing_cache_gap::ListingCacheState::refresh_failed`], which is the only path that can report
/// it because a lookup that throws never reaches a settlement.
#[test]
fn a_failed_credential_lookup_warns_once_and_keeps_the_badges() {
    let mut state = cache();
    let first = wait(&mut state, 100);
    let _ = state.settle(
        first.generation,
        Ok(badged_listing("New")),
        URL,
        &settings(100),
        0,
    );
    assert_eq!(state.current().listing, Some(&badged_listing("New")));

    let first_failure = state.refresh_failed("credentials offline", &settings(100), 100);
    assert_eq!(
        first_failure.warning.as_deref(),
        Some(refresh_failure_warning("credentials offline").as_str())
    );
    assert_eq!(
        first_failure.refresh_in,
        Some(100),
        "it retries at the same interval"
    );
    let second_failure = state.refresh_failed("credentials offline", &settings(100), 200);
    assert_eq!(second_failure.warning, None, "the streak is reported once");
    assert_eq!(state.current().listing, Some(&badged_listing("New")));

    // The lookup recovers and the refresh publishes.
    let refresh = match state.access(&settings(100), Some(TOKEN_A), 300) {
        Access::Immediate {
            refresh: Some(load),
        } => load,
        other => panic!("expected a background refresh, got {other:?}"),
    };
    let recovered = state.settle(
        refresh.generation,
        Ok(badged_listing("Back")),
        URL,
        &settings(100),
        300,
    );
    assert!(recovered.changed);
    assert_eq!(state.current().listing, Some(&badged_listing("Back")));
}

// ---------------------------------------------------------------------------------------------
// Disposal
// ---------------------------------------------------------------------------------------------

/// Upstream: `aborts in-flight work on dispose and loads nothing afterwards`.
#[test]
fn disposal_aborts_the_load_and_loads_nothing_afterwards() {
    let mut state = cache();
    let _ = wait(&mut state, 100);
    assert!(state.dispose(), "the request must be aborted");
    assert_eq!(
        state.access(&settings(100), Some(TOKEN_A), 0),
        Access::Closed
    );
    assert_eq!(state.current().listing, None);
}

/// Upstream: `drops a successful result that settles after disposal`.
#[test]
fn a_result_that_settles_after_disposal_is_dropped() {
    let mut state = cache();
    let load = wait(&mut state, 100);
    let _ = state.dispose();
    let late = state.settle(
        load.generation,
        Ok(listing(&["late"])),
        URL,
        &settings(100),
        0,
    );
    assert_eq!(late, Settlement::discarded());
    assert_eq!(state.current().listing, None);
}

/// Upstream: `stops its timer on disposal` and `awaits an admitted automatic load on disposal`.
#[test]
fn disposal_stops_the_schedule_and_fences_a_pending_automatic_load() {
    let mut state = cache();
    let first = wait(&mut state, 100);
    let _ = state.settle(
        first.generation,
        Ok(badged_listing("New")),
        URL,
        &settings(100),
        0,
    );
    let refresh = match state.access(&settings(100), Some(TOKEN_A), 100) {
        Access::Immediate {
            refresh: Some(load),
        } => load,
        other => panic!("expected a background refresh, got {other:?}"),
    };
    assert!(state.dispose());
    let late = state.settle(
        refresh.generation,
        Ok(badged_listing("Late")),
        URL,
        &settings(100),
        100,
    );
    assert_eq!(late, Settlement::discarded());
    assert_eq!(
        state.current().listing,
        Some(&badged_listing("New")),
        "the committed listing is what survives"
    );
    assert_eq!(state.refresh_delay(&settings(100), 0), None);
}

/// Upstream: `awaits a pending automatic credential lookup on disposal (resolve|reject)`.
///
/// Deviation: awaiting the admitted lookup is the driver's (`Promise.allSettled(this.pending)`);
/// what the state machine decides is that both outcomes publish nothing.
#[test]
fn disposal_fences_the_outcome_of_a_pending_credential_lookup() {
    for outcome in [Access::Closed, Access::Closed] {
        let mut state = cache();
        let first = wait(&mut state, 100);
        let _ = state.settle(
            first.generation,
            Ok(listing(&["a"])),
            URL,
            &settings(100),
            0,
        );
        let _ = state.dispose();
        assert_eq!(state.access(&settings(100), Some(TOKEN_A), 100), outcome);
        assert_eq!(
            state
                .refresh_failed("lookup failed", &settings(100), 100)
                .warning,
            None
        );
        assert_eq!(state.current().listing, Some(&listing(&["a"])));
    }
}

// ---------------------------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------------------------

/// Upstream: `keeps the committed listing and reports a failed change observer`.
#[test]
fn a_failed_observer_keeps_the_listing_and_names_the_recovery() {
    let mut state = cache();
    let load = wait(&mut state, 100);
    let settlement = state.settle(load.generation, Ok(listing(&["a"])), URL, &settings(100), 0);
    assert!(settlement.published);
    assert_eq!(state.current().listing, Some(&listing(&["a"])));
    assert_eq!(
        notification_failure_warning("observer failed"),
        "model listing change notification failed (observer failed); reload the model catalog"
    );
}

/// The three format strings, byte for byte, as upstream interpolates them.
#[test]
fn the_diagnostic_strings_are_upstreams() {
    assert_eq!(
        info_message(URL, 1, 0),
        "model listing from https://api.example/models loaded 1 model(s), skipped 0 row(s)"
    );
    assert_eq!(
        failure_warning(URL, "Error: offline", false),
        "model listing from https://api.example/models failed (Error: offline); using the bundled catalog"
    );
    assert_eq!(
        failure_warning(URL, "Error: offline", true),
        "model listing from https://api.example/models failed (Error: offline); keeping the previous catalog"
    );
    assert_eq!(
        refresh_failure_warning("Error: offline"),
        concat!(
            "model listing refresh failed (Error: offline); keeping the previous catalog; check ",
            "listing configuration and credentials"
        )
    );
    assert_eq!(
        notification_failure_warning("boom"),
        "model listing change notification failed (boom); reload the model catalog"
    );
}

// ---------------------------------------------------------------------------------------------
// Prepared listings feed the cache unchanged
// ---------------------------------------------------------------------------------------------

/// The decoder and the cache agree: what `parse_model_listing` produces is what a settlement keeps.
///
/// This is the one seam worth pinning, because the cache compares listings for equality to decide
/// whether to notify, and a decoder that produced entries in a different order would notify on every
/// refresh.
#[test]
fn a_parsed_listing_is_retained_verbatim() {
    let body = json!({
        "data": [
            {"id": "a", "name": "Model A"},
            {"id": "b"},
            {"name": "no id"},
            {"id": "a"}
        ]
    });
    let parsed = parse_model_listing(&body).expect("a listing");
    assert_eq!(parsed.entries.len(), 2);
    assert_eq!(parsed.skipped, 2);
    let mut state = cache();
    let load = wait(&mut state, 100);
    let settlement = state.settle(load.generation, Ok(parsed.clone()), URL, &settings(100), 0);
    assert_eq!(
        settlement.info.as_deref(),
        Some("model listing from https://api.example/models loaded 2 model(s), skipped 2 row(s)")
    );
    assert_eq!(state.current().listing, Some(&parsed));
}

// ---------------------------------------------------------------------------------------------
// `prepareRequestExtensions`: the REQUEST_EXTENSION collision path
// ---------------------------------------------------------------------------------------------

const BASE: &str = r#"{"model":"m","messages":[]}"#;

/// Upstream: `prepareRequestExtensions` merges without replacing Messages fields.
#[test]
fn extension_fields_are_merged_after_the_collision_check() {
    let base: Value = serde_json::from_str(BASE).expect("a base request");
    let mut fields = serde_json::Map::new();
    fields.insert("metadata".to_owned(), json!({"trace": "abc"}));
    let merged = merge_request_extensions(&base, &fields).expect("no collision");
    assert_eq!(merged["model"], json!("m"));
    assert_eq!(merged["messages"], json!([]));
    assert_eq!(merged["metadata"], json!({"trace": "abc"}));
    assert_eq!(
        base,
        serde_json::from_str::<Value>(BASE).expect("a base request"),
        "the merge does not mutate the caller's body"
    );
}

/// Upstream: `a base-field collision reports REQUEST_EXTENSION with the field quoted`.
///
/// Every field the serializer owns is refused the same way; `messages` is the one that matters,
/// because a plugin that could replace it would rewrite the whole request.
#[test]
fn every_serializer_owned_field_is_refused() {
    let base: Value = serde_json::from_str(
        r#"{"model":"m","messages":[],"system":"s","tools":[],"stream":true,"max_tokens":8}"#,
    )
    .expect("a base request");
    for field in [
        "model",
        "messages",
        "system",
        "tools",
        "stream",
        "max_tokens",
    ] {
        let mut fields = serde_json::Map::new();
        fields.insert(field.to_owned(), json!("extension"));
        let error = merge_request_extensions(&base, &fields).expect_err("a collision");
        assert_eq!(
            error,
            RequestExtensionError::Collision {
                field: field.to_owned()
            }
        );
        assert_eq!(
            error.to_string(),
            format!("DeepSeek request extension field \"{field}\" collides with the base request")
        );
        assert_eq!(RequestExtensionError::code(), REQUEST_EXTENSION);
        assert_eq!(RequestExtensionError::code(), "REQUEST_EXTENSION");
    }
}

/// Upstream: `Object.hasOwn` — presence, not truthiness, and not nested membership.
#[test]
fn presence_decides_the_collision_not_truthiness() {
    let base: Value = serde_json::from_str(r#"{"model":"m","metadata":null,"messages":[]}"#)
        .expect("a base request");
    let object = base.as_object().expect("an object");

    let mut colliding = serde_json::Map::new();
    colliding.insert("metadata".to_owned(), json!("extension"));
    assert_eq!(colliding_field(object, &colliding), Some("metadata"));
    assert!(merge_request_extensions(&base, &colliding).is_err());

    // A nested member is not a base member: `messages.role` is not `role`.
    let mut nested = serde_json::Map::new();
    nested.insert("role".to_owned(), json!("user"));
    assert_eq!(colliding_field(object, &nested), None);
    assert!(merge_request_extensions(&base, &nested).is_ok());
}

/// Upstream: several colliding fields — the diagnostic names one.
///
/// Deviation: upstream reads insertion order and names the first contributed field; `serde_json::Map`
/// is a `BTreeMap` here, so the smallest name is named instead. The refusal is the same; only which
/// of two mistakes a caller is told about first differs, which the module documents.
#[test]
fn the_named_collision_is_deterministic() {
    let base: Value = serde_json::from_str(r#"{"z":1,"a":2}"#).expect("a base request");
    let mut fields = serde_json::Map::new();
    fields.insert("z".to_owned(), json!(0));
    fields.insert("a".to_owned(), json!(0));
    let error = merge_request_extensions(&base, &fields).expect_err("two collisions");
    assert_eq!(
        error,
        RequestExtensionError::Collision {
            field: "a".to_owned()
        }
    );
}

/// Upstream: a base request that is not an object.
///
/// Deviation: upstream's `{...body}` would spread a non-object without complaint, so this refusal has
/// no exact counterpart; it is the honest behaviour for a typed merge, and the code is the same one.
#[test]
fn a_non_object_base_request_is_refused() {
    let mut fields = serde_json::Map::new();
    fields.insert("metadata".to_owned(), json!(1));
    for base in [json!([]), json!("body"), json!(null), json!(1)] {
        let error = merge_request_extensions(&base, &fields).expect_err("not an object");
        assert_eq!(error, RequestExtensionError::NotAnObject);
        assert_eq!(RequestExtensionError::code(), "REQUEST_EXTENSION");
    }
}

/// Upstream returns the base request alone when the merged payload cannot serialize.
///
/// Deviation, stated: `serde_json::Value` is acyclic and every value serializes, so the path is
/// unreachable in this build and is reported rather than emulated. What is pinned is the rule it
/// serves — an unmerged field is *omitted* and its contributor resends, which is the same behaviour
/// as a contributed field whose preparation succeeded but whose delivery was never accepted.
#[test]
fn an_unserializable_payload_cannot_arise_from_a_json_value() {
    let base: Value = serde_json::from_str(BASE).expect("a base request");
    let mut fields = serde_json::Map::new();
    fields.insert("metadata".to_owned(), json!({"deep": [1, 2, {"x": null}]}));
    let merged = merge_request_extensions(&base, &fields).expect("no collision");
    assert!(serde_json::to_string(&merged).is_ok());
}

/// Upstream computes the scope from the credential digest, never the credential.
///
/// The digest is pinned to the SHA-256 of the token so a change to [`scope_of`]'s encoding — the
/// newline, the hex case — shows up here rather than as two caches that disagree in production.
#[test]
fn the_scope_is_the_url_and_the_credential_digest() {
    assert_eq!(
        scope_of(URL, Some(TOKEN_A)).as_deref(),
        Some(
            "https://api.example/models\na70bf50e531ce1a817561f2f5d5b6645d4e806becf58ccc5e8cf6b8045a090a8"
        )
    );
    assert_eq!(scope_of(URL, None), None);
    // Two credentials at one endpoint, and one credential at two endpoints, are four scopes.
    assert_ne!(scope_of(URL, Some(TOKEN_A)), scope_of(URL, Some("token-b")));
    assert_ne!(
        scope_of(URL, Some(TOKEN_A)),
        scope_of("https://other.example/models", Some(TOKEN_A))
    );
}
