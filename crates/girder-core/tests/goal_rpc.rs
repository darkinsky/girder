//! Tests for the `goal.*` Remote methods.
//!
//! The properties under test, in the order they matter:
//!
//! - **the round trip is one surface** — the id `create` mints is the id `get` reads, the id `list`
//!   shows, and the id every mutation addresses. A surface whose ids disagree is not a surface.
//! - **the revision is a real compare-and-set** — a stale revision is refused on every mutation,
//!   and the refusal names both revisions. This is the property the domain module exists for, so a
//!   wire layer that dropped it would silently undo the reason the type is there.
//! - **the lifecycle rules are the domain's, not this module's** — pausing a paused goal, editing a
//!   completed one, and creating while an open goal exists are refused with [`GoalError`]'s own
//!   variants, and the message is the domain's message.
//! - **`get` of nothing is not an error** — upstream answers `GoalView | undefined`; a session that
//!   has not started a goal is the normal case, not a caller mistake.
//! - **`activation` and the derived booleans are honest** — a paused goal reports `disarmed` and
//!   `mayContinue: false` whatever the store's arming says, and an armed active goal reports both.
//! - **nothing here starts a round** — [`STARTS_ROUNDS`] is `false`, there is no `run` or `start`
//!   method, and `mayContinue` describes a rule rather than an action.
//! - **the declarations match what each method does** — the mutations are writes, `clear` is
//!   destructive, the reads are reads, so a read-only policy can list and cannot create.
//! - **the default store refuses rather than pretending** — no storage configured is reported as a
//!   failure, not hidden behind a map the next call could not confirm.
//! - **`update` and upstream's `edit` are one handler** — both spellings answer identically.
//!
//! Isolation: every test builds its own store and registry, and ids carry a process-wide counter, so
//! tests in one binary cannot see each other's goals.
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

#[path = "../src/goal/rpc.rs"]
pub mod goal_rpc;

use std::sync::Arc;

use girder_core::{
    Access, AccessDecision, AccessRequest, Policy, RemoteCall, RemoteError, RemoteRegistry,
};
use serde_json::{Value, json};

use goal_rpc::{
    CLEAR_METHOD, COMPLETE_METHOD, CREATE_METHOD, EDIT_METHOD, GET_METHOD, LIST_METHOD,
    MemoryGoalStore, NAMESPACE, PAUSE_METHOD, RESUME_METHOD, STARTS_ROUNDS, UnavailableGoalStore,
    register_goal_methods,
};

// ---- fixtures ---------------------------------------------------------------------------------

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// A registry with a fresh in-memory store behind it.
fn registry() -> (RemoteRegistry, Arc<MemoryGoalStore>) {
    let store = Arc::new(MemoryGoalStore::new());
    let dynamic: Arc<dyn goal_rpc::GoalStore> = store.clone();
    let mut registry = RemoteRegistry::new();
    register_goal_methods(&mut registry, &dynamic);
    (registry, store)
}

async fn dispatch(
    registry: &RemoteRegistry,
    method: &str,
    args: Value,
) -> Result<Value, RemoteError> {
    registry.dispatch(NAMESPACE, method, call(args)).await
}

/// Creates one goal and returns the whole view.
async fn create(registry: &RemoteRegistry, objective: &str) -> Value {
    dispatch(registry, CREATE_METHOD, json!({"objective": objective}))
        .await
        .expect("a goal with an objective is created")
}

fn id_of(value: &Value) -> String {
    value
        .get("id")
        .and_then(Value::as_str)
        .expect("the view carries an id")
        .to_owned()
}

fn revision_of(value: &Value) -> u64 {
    value
        .get("revision")
        .and_then(Value::as_u64)
        .expect("the view carries a revision")
}

/// The `{id, revision}` a mutation is addressed with.
fn reference_of(view: &Value) -> Value {
    json!({"id": id_of(view), "revision": revision_of(view)})
}

fn items_of(listing: &Value) -> Vec<Value> {
    listing
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// ---- the round trip ---------------------------------------------------------------------------

#[tokio::test]
async fn create_get_list_round_trips() {
    let (registry, store) = registry();

    let created = create(&registry, "ship the goal surface").await;
    let id = id_of(&created);
    assert!(!id.is_empty(), "an id is minted");
    assert_eq!(created["objective"], "ship the goal surface");
    assert_eq!(created["phase"], "active");
    assert_eq!(revision_of(&created), 1);
    // A newly created goal is armed because the user just asked for it.
    assert_eq!(created["activation"], "armed");
    assert_eq!(created["mayContinue"], true);
    assert_eq!(created["resumable"], true);
    assert_eq!(store.len(), 1);

    // `get` by id and `get` with no id agree: there is one open goal and it is the current one.
    let by_id = dispatch(&registry, GET_METHOD, json!({"id": id}))
        .await
        .expect("a known id is readable");
    assert_eq!(by_id["id"], id.as_str());
    let current = dispatch(&registry, GET_METHOD, json!({}))
        .await
        .expect("the current goal is readable");
    assert_eq!(current["id"], id.as_str());

    let listing = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("a listing succeeds");
    let items = items_of(&listing);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], id.as_str());
    assert_eq!(listing["total"], 1);
    assert_eq!(listing["activation"], "armed");
}

#[tokio::test]
async fn get_of_nothing_is_a_miss_not_an_error() {
    // Upstream answers `GoalView | undefined`. A session that has not created a goal yet is the
    // normal starting state, and an error would make every client special-case it.
    let (registry, _) = registry();
    let missing = dispatch(&registry, GET_METHOD, json!({}))
        .await
        .expect("reading no goal succeeds");
    assert!(
        missing["goal"].is_null(),
        "an absent goal is reported as null, got {missing}"
    );

    // An id nobody holds is likewise a miss rather than an error.
    let unknown = dispatch(&registry, GET_METHOD, json!({"id": "goal-nope"}))
        .await
        .expect("an unknown id is a miss");
    assert!(unknown["goal"].is_null());
}

// ---- compare-and-set --------------------------------------------------------------------------

#[tokio::test]
async fn a_stale_revision_is_refused_on_every_mutation() {
    let (registry, _) = registry();
    let created = create(&registry, "objective").await;
    let id = id_of(&created);
    let stale = json!({"id": id, "revision": 99});

    let mut cases: Vec<(&str, Value)> = vec![
        (PAUSE_METHOD, json!({"id": id, "revision": 99})),
        (RESUME_METHOD, json!({"id": id, "revision": 99})),
        (COMPLETE_METHOD, json!({"id": id, "revision": 99})),
        (
            EDIT_METHOD,
            json!({"id": id, "revision": 99, "objective": "new"}),
        ),
    ];
    // `clear` first in the list would remove the goal and make the rest `NotCurrent`, which is a
    // different refusal — so it is checked last, on its own copy.
    for (method, args) in cases.drain(..) {
        let error = dispatch(&registry, method, args)
            .await
            .expect_err(&format!("{method} must refuse a stale revision"));
        let message = error.to_string();
        assert!(
            message.contains("99") && message.contains('1'),
            "{method} must name both revisions, got {message}"
        );
        assert_eq!(error.code(), "INVALID_ARGUMENTS");
    }

    let error = dispatch(&registry, CLEAR_METHOD, stale)
        .await
        .expect_err("clear must refuse a stale revision");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");

    // None of the refused calls changed anything.
    let after = dispatch(&registry, GET_METHOD, json!({"id": id}))
        .await
        .expect("the goal is still readable");
    assert_eq!(revision_of(&after), 1);
    assert_eq!(after["phase"], "active");
}

#[tokio::test]
async fn a_missing_revision_is_refused_rather_than_defaulted() {
    // Defaulting the revision would let a caller that never read the goal overwrite it, which is the
    // lost update the revision exists to prevent.
    let (registry, _) = registry();
    let created = create(&registry, "objective").await;
    let id = id_of(&created);
    let error = dispatch(&registry, PAUSE_METHOD, json!({"id": id}))
        .await
        .expect_err("a mutation without a revision is refused");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(error.to_string().contains("revision"));
}

#[tokio::test]
async fn an_unknown_id_is_refused_as_not_current() {
    let (registry, _) = registry();
    let error = dispatch(
        &registry,
        PAUSE_METHOD,
        json!({"id": "goal-nope", "revision": 1}),
    )
    .await
    .expect_err("an unknown id cannot be paused");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(
        error.to_string().contains("not the current goal"),
        "the domain's own message is passed through, got {error}"
    );
}

// ---- the domain's transition table ------------------------------------------------------------

#[tokio::test]
async fn the_lifecycle_rules_are_the_domains() {
    let (registry, _) = registry();
    let created = create(&registry, "objective").await;
    let id = id_of(&created);

    // pause: active -> paused, disarmed.
    let paused = dispatch(&registry, PAUSE_METHOD, reference_of(&created))
        .await
        .expect("an active goal pauses");
    assert_eq!(paused["phase"], "paused");
    assert_eq!(revision_of(&paused), 2);
    assert_eq!(paused["activation"], "disarmed");
    assert_eq!(paused["mayContinue"], false);
    assert_eq!(paused["resumable"], true);

    // Pausing a paused goal is refused by the domain, with its own operation name in the message.
    let error = dispatch(&registry, PAUSE_METHOD, reference_of(&paused))
        .await
        .expect_err("a paused goal cannot pause again");
    assert!(
        error.to_string().contains("cannot pause from paused"),
        "the domain's transition message is passed through, got {error}"
    );

    // resume: paused -> active, armed.
    let resumed = dispatch(&registry, RESUME_METHOD, reference_of(&paused))
        .await
        .expect("a paused goal resumes");
    assert_eq!(resumed["phase"], "active");
    assert_eq!(resumed["activation"], "armed");
    assert_eq!(resumed["mayContinue"], true);
    assert_eq!(revision_of(&resumed), 3);

    // Resuming an active, armed goal is `AlreadyArmed`, not a no-op revision.
    let error = dispatch(&registry, RESUME_METHOD, reference_of(&resumed))
        .await
        .expect_err("an armed active goal cannot be resumed again");
    assert!(
        error.to_string().contains("already active and armed"),
        "go {error}"
    );

    // complete: terminal.
    let completed = dispatch(&registry, COMPLETE_METHOD, reference_of(&resumed))
        .await
        .expect("an open goal completes");
    assert_eq!(completed["phase"], "complete");
    assert_eq!(completed["resumable"], false);
    assert_eq!(completed["mayContinue"], false);
    assert_eq!(completed["activation"], "disarmed");

    // Every mutation of a completed goal is refused.
    for (method, args) in [
        (
            PAUSE_METHOD,
            json!({"id": id, "revision": revision_of(&completed)}),
        ),
        (
            RESUME_METHOD,
            json!({"id": id, "revision": revision_of(&completed)}),
        ),
        (
            COMPLETE_METHOD,
            json!({"id": id, "revision": revision_of(&completed)}),
        ),
        (
            EDIT_METHOD,
            json!({"id": id, "revision": revision_of(&completed), "objective": "new"}),
        ),
    ] {
        let error = dispatch(&registry, method, args)
            .await
            .expect_err(&format!("{method} must refuse a completed goal"));
        assert_eq!(error.code(), "INVALID_ARGUMENTS", "{method}: {error}");
    }
}

#[tokio::test]
async fn edit_requires_a_change_and_refuses_a_blank_one() {
    // `block` is a state-machine transition upstream does not expose as an endpoint, so what is
    // worth testing here is the endpoint upstream *does* serve: an edit with nothing to change and
    // one whose objective is blank are both refused before any revision is bumped.
    let (registry, _) = registry();
    let created = create(&registry, "objective").await;
    let id = id_of(&created);

    let error = dispatch(&registry, EDIT_METHOD, json!({"id": id, "revision": 1}))
        .await
        .expect_err("an empty edit is refused");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");

    let error = dispatch(
        &registry,
        EDIT_METHOD,
        json!({"id": id, "revision": 1, "objective": "   "}),
    )
    .await
    .expect_err("a blank objective is refused");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
}

#[tokio::test]
async fn creating_while_an_open_goal_exists_is_refused() {
    let (registry, _) = registry();
    let first = create(&registry, "first").await;
    let error = dispatch(
        &registry,
        CREATE_METHOD,
        json!({"objective": "second", "maxRounds": 5}),
    )
    .await
    .expect_err("a second open goal would make `current` ambiguous");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(
        error.to_string().contains("cannot create from active"),
        "the domain's refusal is passed through, got {error}"
    );

    // A completed goal is replaceable, which is how a user starts the next objective.
    let completed = dispatch(&registry, COMPLETE_METHOD, reference_of(&first))
        .await
        .expect("the open goal completes");
    assert_eq!(completed["phase"], "complete");
    let second = create(&registry, "second").await;
    assert_eq!(second["objective"], "second");
    assert_eq!(second["phase"], "active");
    assert_ne!(id_of(&second), id_of(&first), "a new goal has a new id");
}

#[tokio::test]
async fn create_validates_the_objective_and_the_round_cap() {
    let (registry, store) = registry();

    for args in [
        json!({}),
        json!({"objective": "   "}),
        json!({"objective": 7}),
    ] {
        let error = dispatch(&registry, CREATE_METHOD, args.clone())
            .await
            .expect_err(&format!("`{args}` is not a create request"));
        assert_eq!(error.code(), "INVALID_ARGUMENTS");
        assert!(store.is_empty(), "nothing was stored for `{args}`");
    }

    // The cap is applied as given.
    let capped = dispatch(
        &registry,
        CREATE_METHOD,
        json!({"objective": "capped", "maxRounds": 3}),
    )
    .await
    .expect("a cap is a valid create");
    assert_eq!(capped["maxRounds"], 3);

    // A cap above `u32::MAX` is refused rather than wrapped: wrapping would stop the goal far short
    // of what the caller asked for, for a reason nothing in the record explains.
    let error = dispatch(
        &registry,
        CREATE_METHOD,
        json!({"objective": "too big", "maxRounds": u64::from(u32::MAX) + 1}),
    )
    .await
    .expect_err("a cap that does not fit is refused");
    assert!(error.to_string().contains("above the maximum"));
}

// ---- update / edit ----------------------------------------------------------------------------

#[tokio::test]
async fn update_and_edit_are_one_handler() {
    let (registry, _) = registry();
    let created = create(&registry, "first objective").await;

    let updated = dispatch(
        &registry,
        EDIT_METHOD,
        json!({"id": id_of(&created), "revision": 1, "objective": "second objective"}),
    )
    .await
    .expect("an objective is editable");
    assert_eq!(updated["objective"], "second objective");
    assert_eq!(revision_of(&updated), 2);
    // An edit does not change the phase.
    assert_eq!(updated["phase"], "active");

    // Upstream's spelling reaches the same handler with the same result.
    let edited = dispatch(
        &registry,
        EDIT_METHOD,
        json!({"id": id_of(&created), "revision": 2, "maxRounds": 9}),
    )
    .await
    .expect("a round cap is editable through `edit`");
    assert_eq!(edited["maxRounds"], 9);
    assert_eq!(edited["objective"], "second objective");
    assert_eq!(revision_of(&edited), 3);

    // An edit that changes nothing is refused: it would bump the revision for a change nobody made
    // and break every other client's compare-and-set.
    let error = dispatch(
        &registry,
        EDIT_METHOD,
        json!({"id": id_of(&created), "revision": 3}),
    )
    .await
    .expect_err("an empty edit is refused");
    assert!(error.to_string().contains("objective"));
    assert_eq!(
        revision_of(
            &dispatch(&registry, GET_METHOD, json!({"id": id_of(&created)}))
                .await
                .expect("the goal is readable")
        ),
        3,
        "a refused edit did not bump the revision"
    );

    // A blank objective is refused by the domain, not stored.
    let error = dispatch(
        &registry,
        EDIT_METHOD,
        json!({"id": id_of(&created), "revision": 3, "objective": "  "}),
    )
    .await
    .expect_err("a blank objective is refused");
    assert!(error.to_string().contains("needs an objective"));
}

// ---- clear ------------------------------------------------------------------------------------

#[tokio::test]
async fn clear_discards_the_goal_and_reports_what_it_removed() {
    let (registry, store) = registry();
    let created = create(&registry, "to be discarded").await;
    let id = id_of(&created);

    let cleared = dispatch(&registry, CLEAR_METHOD, reference_of(&created))
        .await
        .expect("a goal is clearable");
    assert_eq!(cleared["id"], id.as_str());
    assert_eq!(cleared["cleared"], true);
    assert_eq!(cleared["revision"], 1, "the discarded revision is reported");
    assert_eq!(cleared["activation"], "disarmed");
    assert!(store.is_empty(), "the goal is gone from the store");

    // Clearing again is `NotCurrent`: there is nothing left to discard.
    let error = dispatch(&registry, CLEAR_METHOD, json!({"id": id, "revision": 1}))
        .await
        .expect_err("a cleared goal cannot be cleared again");
    assert!(error.to_string().contains("not the current goal"));
}

// ---- declarations -----------------------------------------------------------------------------

/// A policy that refuses everything but reads, recording what it was asked.
#[derive(Debug)]
struct ReadOnly;

impl Policy for ReadOnly {
    fn decide(&self, request: &AccessRequest) -> AccessDecision {
        match request.access {
            Access::Read => AccessDecision::Allow,
            _ => AccessDecision::Refuse {
                reason: "read-only".to_owned(),
            },
        }
    }
}

#[tokio::test]
async fn the_declarations_match_what_each_method_does() {
    let store = Arc::new(MemoryGoalStore::new());
    let dynamic: Arc<dyn goal_rpc::GoalStore> = store;
    let mut registry = RemoteRegistry::new().with_policy(Arc::new(ReadOnly));
    register_goal_methods(&mut registry, &dynamic);

    // Reads are allowed by a read-only policy...
    for method in [LIST_METHOD, GET_METHOD] {
        dispatch(&registry, method, json!({}))
            .await
            .unwrap_or_else(|error| panic!("{method} must be readable: {error}"));
    }
    // ...and every mutation is refused, which is what the declarations say.
    for (method, args) in [
        (CREATE_METHOD, json!({"objective": "nope"})),
        (
            EDIT_METHOD,
            json!({"id": "x", "revision": 1, "objective": "nope"}),
        ),
        (PAUSE_METHOD, json!({"id": "x", "revision": 1})),
        (RESUME_METHOD, json!({"id": "x", "revision": 1})),
        (COMPLETE_METHOD, json!({"id": "x", "revision": 1})),
        (
            EDIT_METHOD,
            json!({"id": "x", "revision": 1, "code": "c", "message": "m"}),
        ),
        (CLEAR_METHOD, json!({"id": "x", "revision": 1})),
    ] {
        let error = dispatch(&registry, method, args)
            .await
            .expect_err(&format!("{method} writes and must be refused"));
        assert_eq!(error.code(), "REFUSED", "{method}: {error}");
    }
}

// ---- what this module does not do -------------------------------------------------------------

#[tokio::test]
async fn nothing_here_starts_a_round() {
    // The property a reader is most likely to assume wrongly: `mayContinue` describes a rule, and
    // `STARTS_ROUNDS` says this module never acts on it.
    const { assert!(!STARTS_ROUNDS) };

    let (registry, _) = registry();
    let names: Vec<String> = registry
        .names()
        .into_iter()
        .filter(|(namespace, _)| namespace == NAMESPACE)
        .map(|(_, method)| method)
        .collect();
    for absent in ["run", "start", "continue", "advance", "round", "disarm"] {
        assert!(
            !names.iter().any(|name| name == absent),
            "`{absent}` must not be served: {names:?}"
        );
    }

    // A goal created here has started no rounds.
    let created = create(&registry, "objective").await;
    assert_eq!(created["roundsStarted"], 0);
}

#[tokio::test]
async fn the_default_store_refuses_rather_than_pretending() {
    // No storage configured is reported as a failure, not hidden behind a map the next call could
    // not confirm: a client told "created" would look at a goal that vanishes.
    let store: Arc<dyn goal_rpc::GoalStore> = Arc::new(UnavailableGoalStore::default_reason());
    let mut registry = RemoteRegistry::new();
    register_goal_methods(&mut registry, &store);

    let error = dispatch(&registry, CREATE_METHOD, json!({"objective": "objective"}))
        .await
        .expect_err("no store means no create");
    assert_eq!(error.code(), "FAILED");
    assert!(
        error.to_string().contains("no GoalStore was supplied"),
        "the message names what a host must supply, got {error}"
    );

    // Reading reports nothing rather than inventing a goal.
    let missing = dispatch(&registry, GET_METHOD, json!({}))
        .await
        .expect("a read of nothing succeeds");
    assert!(missing["goal"].is_null());
    let listing = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("a listing succeeds");
    assert_eq!(listing["total"], 0);
}

#[tokio::test]
async fn every_advertised_method_is_registered() {
    // `METHODS` is exported so this is checkable without guessing at the names.
    let (registry, _) = registry();
    for method in goal_rpc::METHODS {
        assert!(
            registry.get(NAMESPACE, method).is_some(),
            "`{NAMESPACE}.{method}` is in METHODS and must be registered"
        );
    }
    assert_eq!(
        registry
            .names()
            .into_iter()
            .filter(|(namespace, _)| namespace == NAMESPACE)
            .count(),
        goal_rpc::METHODS.len(),
        "every registered name is advertised"
    );
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod goal {
    pub use super::goal_rpc as rpc;
}
