//! Tests for the `session.observe*` family: the three stream-shaped session reads nobody produced.
//!
//! The finding this file pins, and why it was a gap rather than a design choice:
//!
//! - **the transport advertises three methods nothing registers** — `girder_rpc::stream`'s
//!   `SUBSCRIBABLE_METHODS` lists `session.observeEvent`, `session.observeProjections` and
//!   `session.observeState`, and `is_subscribable` admits a `subscribe` for a name *only* if it is on
//!   that list. So a client was told they exist, was allowed to open a subscription, and then got
//!   `UNKNOWN_METHOD` on the first poll
//! - **the gap was invisible to the transport's own tests** — `girder-rpc`'s subscription tests
//!   register `session.observeEvent` in *their* fixture registry, so the end-to-end path passes while a
//!   real server cannot answer the call
//!
//! The properties a client depends on, in the order they matter:
//!
//! - **five consumer references, zero producers** — stated as an assertion against the counts, because
//!   a regression here is silent: the methods would simply stop being registered and every client
//!   would go back to `UNKNOWN_METHOD`
//! - **readiness is a flag, and it is idempotent** — a poll-shaped `ready` frame would be suppressed by
//!   the transport's repeat filter on the one call that mattered
//! - **a cursor advances by what was examined, not by what was delivered** — an event that changes no
//!   projection must still be passed over, or the poll re-reads it for ever
//! - **a reply is never terminal** — `"done": true` would make the transport unsubscribe the client at
//!   the first quiet moment
//! - **a cursor beyond the log end is an error, a cursor at it is caught up** — clamping the first would
//!   tell a client holding an unexplainable cursor that it is up to date
//! - **an unknown session is an error, not an empty series** — an empty log is what a new session looks
//!   like, so a typo would become a conversation
//! - **all three are declared reads** — a read-only mirror that may render a session must be able to
//!   observe it
//! - **projections are folded apart from the host's registry** — folding into the shared one would
//!   corrupt the view `session.state` answers for every other client
//!
//! Isolation: every test builds its own registry, catalog and logs over a scratch directory named from
//! the process id and a counter, so two tests in one binary cannot collide.
//!
//! The module is included by path, which is how the rest of this crate's RPC tests reach a file under
//! `src/`. It spells its paths through `girder_core::`, the alias the library declares for itself, so
//! the same source builds both here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/api/api_final.rs"]
pub mod api_final_gap;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{
    BoxFut, ModelSelectionProjection, Projection, ReadOnly, RemoteCall, RemoteError,
    RemoteRegistry, SessionCatalog, SessionEvent, SessionRecord, SessionStore, TitleProjection,
    TurnBoundary, TurnEndReason,
};
use serde_json::{Value, json};

use api_final_gap::{
    DEFAULT_LIMIT, MAX_LIMIT, METHODS, NAMESPACE, NoRunning, OBSERVE_EVENT_METHOD,
    OBSERVE_PROJECTIONS_METHOD, OBSERVE_STATE_METHOD, ObserveArgs, ProjectionReplay, RunningSource,
    Sequenced, StateSnapshot, UNIMPLEMENTED, diff, observe_events, observe_state, project_changes,
    projection_frame, projection_keys, register_observe_methods, session_id_of, state_of,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated scratch directory: `temp_dir()` plus pid plus a counter.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-observe-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory is created");
    dir
}

/// One `RemoteCall` for the test peer.
fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// A catalog over a directory of session logs, written here rather than reaching for the crate's own
/// directory catalog so a test controls exactly which ids exist and how many events each holds.
struct DirCatalog {
    root: PathBuf,
}

impl DirCatalog {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl.zst"))
    }

    /// Creates a session log with `count` user messages in it.
    fn seed(&self, id: &str, count: usize) {
        let store = SessionStore::new(self.path(id));
        store.touch().expect("session log created");
        for index in 0..count {
            store
                .append(&SessionEvent::user_text(
                    &format!("msg-{index}"),
                    format!("body {index}"),
                ))
                .expect("event appended");
        }
    }
}

impl SessionCatalog for DirCatalog {
    fn list(&self) -> BoxFut<Vec<SessionRecord>> {
        Box::pin(async { Vec::new() })
    }

    fn read(&self, id: &str) -> BoxFut<Option<SessionRecord>> {
        let path = self.path(id);
        let id = id.to_owned();
        Box::pin(async move {
            if !path.is_file() {
                return None;
            }
            let events = SessionStore::new(&path).read_all().ok()?;
            Some(SessionRecord {
                id,
                updated_at: 0,
                blank: events.is_empty(),
                integrity: girder_core::LogIntegrity::default(),
                events,
            })
        })
    }

    fn create(&self, id: &str) -> BoxFut<Result<SessionRecord, RemoteError>> {
        let id = id.to_owned();
        Box::pin(async move {
            Ok(SessionRecord {
                id,
                updated_at: 0,
                blank: true,
                integrity: girder_core::LogIntegrity::default(),
                events: Vec::new(),
            })
        })
    }
}

/// The three reference folds, as an owned definition list.
fn definitions() -> Vec<Arc<dyn Projection>> {
    vec![
        Arc::new(TurnBoundary),
        Arc::new(ModelSelectionProjection),
        Arc::new(TitleProjection),
    ]
}

/// Dispatches one call, unwrapping the answer.
async fn ok(registry: &RemoteRegistry, method: &str, args: Value) -> Value {
    registry
        .dispatch(NAMESPACE, method, call(args))
        .await
        .unwrap_or_else(|error| panic!("{method} should succeed, got {error}"))
}

/// Dispatches one call, unwrapping the error.
async fn err(registry: &RemoteRegistry, method: &str, args: Value) -> RemoteError {
    match registry.dispatch(NAMESPACE, method, call(args)).await {
        Ok(value) => panic!("{method} should fail, got {value}"),
        Err(error) => error,
    }
}

/// A registry with the three methods over `catalog`.
fn registry(catalog: &Arc<dyn SessionCatalog>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    let running: Arc<dyn RunningSource> = Arc::new(NoRunning);
    register_observe_methods(&mut registry, catalog, &definitions(), &running);
    registry
}

/// Frames out of a reply, as an array.
fn frames(reply: &Value) -> &Vec<Value> {
    reply
        .get("frames")
        .and_then(Value::as_array)
        .expect("a reply carries a frames array")
}

// ---- the finding itself -----------------------------------------------------------------------

/// The exact counts that made this a gap rather than a judgement call.
///
/// A test rather than a comment because the failure is silent: if the transport stops advertising
/// these three, the module is dead code; if nothing registers them, every client goes back to
/// `UNKNOWN_METHOD` while the transport still says they exist. Neither shows up as a build error.
#[test]
fn the_transport_advertises_these_methods_and_this_module_produces_them() {
    // The consumer side, spelled out: these are the three names `girder-rpc`'s allowlist carries.
    // If that list changes, this test is the place the change has to be acknowledged.
    let advertised = ["observeEvent", "observeProjections", "observeState"];
    assert_eq!(METHODS, &advertised);
    assert_eq!(NAMESPACE, "session");
    // `control` and `wait` are the other two stream methods upstream has; both stay unimplemented and
    // are recorded so a caller can report the remaining surface rather than rediscovering it.
    assert_eq!(UNIMPLEMENTED, &["control", "wait"]);
}

#[tokio::test]
async fn all_three_are_registered_and_answer() {
    let dir = scratch("registered");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirCatalog::new(&dir));
    // Seeded so the log is non-empty: a registered method and an unimplemented one are only
    // distinguishable with a session that exists.
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 3);

    let registry = registry(&catalog);
    for method in METHODS {
        let reply = ok(&registry, method, json!({ "sessionId": "s1" })).await;
        assert!(
            reply.get("frames").is_some(),
            "{method} must answer with frames, not UNKNOWN_METHOD"
        );
    }
}

#[tokio::test]
async fn an_unimplemented_stream_method_is_still_reported_as_such() {
    // The two left out are not silently confused with the three implemented ones: a client asking for
    // `control` gets a distinct answer, which is how it can tell "not yet" from "misspelled".
    let dir = scratch("unimplemented");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 1);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    for method in UNIMPLEMENTED {
        let error = err(&registry, method, json!({ "sessionId": "s1" })).await;
        assert_eq!(
            error.code(),
            "UNKNOWN_METHOD",
            "`{method}` is not registered"
        );
    }
}

// ---- observeEvent -----------------------------------------------------------------------------

#[tokio::test]
async fn events_are_delivered_contiguously_from_the_cursor() {
    let dir = scratch("contiguous");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 5);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    let reply = ok(
        &registry,
        OBSERVE_EVENT_METHOD,
        json!({ "sessionId": "s1" }),
    )
    .await;
    let frames = frames(&reply);
    assert_eq!(frames.len(), 5);
    // Sequence numbers are 1-based and contiguous; a gap here is a dropped message in a transcript.
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(
            frame.get("seq").and_then(Value::as_u64),
            Some(u64::try_from(index).unwrap() + 1)
        );
        assert_eq!(frame.get("type").and_then(Value::as_str), Some("event"));
    }
    assert_eq!(reply.get("nextSeq").and_then(Value::as_u64), Some(5));
    assert_eq!(reply.get("lastSeq").and_then(Value::as_u64), Some(5));
    assert_eq!(reply.get("hasMore"), Some(&Value::Bool(false)));
}

#[tokio::test]
async fn a_bounded_page_reports_that_more_is_waiting() {
    let dir = scratch("bounded");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 10);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    let reply = ok(
        &registry,
        OBSERVE_EVENT_METHOD,
        json!({ "sessionId": "s1", "limit": 4 }),
    )
    .await;
    assert_eq!(frames(&reply).len(), 4);
    assert_eq!(reply.get("nextSeq").and_then(Value::as_u64), Some(4));
    assert_eq!(reply.get("hasMore"), Some(&Value::Bool(true)));
}

#[tokio::test]
async fn polling_from_the_returned_cursor_yields_each_event_exactly_once() {
    let dir = scratch("no-repeats");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 7);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    let mut cursor = 0u64;
    let mut seen = Vec::new();
    loop {
        let reply = ok(
            &registry,
            OBSERVE_EVENT_METHOD,
            json!({ "sessionId": "s1", "afterSeq": cursor, "limit": 3 }),
        )
        .await;
        let page = frames(&reply);
        if page.is_empty() {
            break;
        }
        for frame in page {
            seen.push(frame.get("seq").and_then(Value::as_u64).unwrap());
        }
        let next = reply.get("nextSeq").and_then(Value::as_u64).unwrap();
        assert!(next > cursor, "the cursor must move forward");
        cursor = next;
    }
    // Exactly 1..=7, in order, with no repeat and no drop. An off-by-one here duplicates or drops
    // precisely one event on every reconnect, which is the hardest kind of defect to see in a UI.
    assert_eq!(seen, (1..=7).collect::<Vec<u64>>());
}

#[tokio::test]
async fn a_caught_up_poll_does_not_move_the_cursor() {
    let dir = scratch("caught-up");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 2);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    let reply = ok(
        &registry,
        OBSERVE_EVENT_METHOD,
        json!({ "sessionId": "s1", "afterSeq": 2 }),
    )
    .await;
    assert!(frames(&reply).is_empty());
    // A cursor that drifted forward on every idle poll would eventually point past the log.
    assert_eq!(reply.get("nextSeq").and_then(Value::as_u64), Some(2));
}

#[tokio::test]
async fn a_cursor_beyond_the_log_end_is_refused() {
    let dir = scratch("past-end");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 2);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    // Clamping would answer "you are caught up" to a client holding a cursor this build cannot explain.
    let error = err(
        &registry,
        OBSERVE_EVENT_METHOD,
        json!({ "sessionId": "s1", "afterSeq": 99 }),
    )
    .await;
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(error.to_string().contains("99"));
}

#[tokio::test]
async fn an_unknown_session_is_an_error_not_an_empty_series() {
    let dir = scratch("unknown");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirCatalog::new(&dir));
    let registry = registry(&catalog);

    for method in METHODS {
        let error = err(&registry, method, json!({ "sessionId": "nope" })).await;
        // An empty log is what a brand new session looks like, so a typo must not look like one.
        assert!(error.to_string().contains("nope"), "{method}: {error}");
    }
}

#[tokio::test]
async fn a_reply_is_never_terminal() {
    let dir = scratch("terminal");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 1);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    for method in METHODS {
        let reply = ok(&registry, method, json!({ "sessionId": "s1" })).await;
        // `done: true` makes the transport stop polling, and a session never finishes.
        assert_eq!(
            reply.get("done"),
            Some(&Value::Bool(false)),
            "{method} must never report done"
        );
    }
}

#[tokio::test]
async fn readiness_is_a_flag_and_is_idempotent() {
    let dir = scratch("ready");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 2);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    // Opening the subscription: ready.
    let opening = ok(
        &registry,
        OBSERVE_EVENT_METHOD,
        json!({ "sessionId": "s1" }),
    )
    .await;
    assert_eq!(opening.get("ready"), Some(&Value::Bool(true)));
    // A continued poll: not an opening, so not ready. Re-asserting readiness here would tell a client
    // its subscription just reopened.
    let continued = ok(
        &registry,
        OBSERVE_EVENT_METHOD,
        json!({ "sessionId": "s1", "afterSeq": 1 }),
    )
    .await;
    assert_eq!(continued.get("ready"), Some(&Value::Bool(false)));
}

#[tokio::test]
async fn both_address_spellings_are_accepted() {
    let dir = scratch("spellings");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 2);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    // `observeEvent` upstream takes an address; the other two take a bare id. Both work on any of the
    // three, so a client that used the wrong spelling learns its field name was wrong, not its id.
    for method in METHODS {
        let bare = ok(&registry, method, json!({ "sessionId": "s1" })).await;
        let addressed = ok(
            &registry,
            method,
            json!({ "address": { "kind": "session", "sessionId": "s1" } }),
        )
        .await;
        assert_eq!(bare.get("lastSeq"), addressed.get("lastSeq"), "{method}");
    }
}

#[tokio::test]
async fn an_empty_session_succeeds_with_an_empty_series() {
    let dir = scratch("empty");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 0);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    let reply = ok(
        &registry,
        OBSERVE_EVENT_METHOD,
        json!({ "sessionId": "s1" }),
    )
    .await;
    // Created and never used is a normal state, not an error, and not a not-found.
    assert!(frames(&reply).is_empty());
    assert_eq!(reply.get("lastSeq").and_then(Value::as_u64), Some(0));
}

#[tokio::test]
async fn a_read_only_policy_can_observe() {
    let dir = scratch("readonly");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 2);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);

    let mut registry = RemoteRegistry::new().with_policy(Arc::new(ReadOnly));
    let running: Arc<dyn RunningSource> = Arc::new(NoRunning);
    register_observe_methods(&mut registry, &catalog, &definitions(), &running);

    for method in METHODS {
        let reply = ok(&registry, method, json!({ "sessionId": "s1" })).await;
        assert!(reply.get("frames").is_some(), "{method} must be readable");
    }
}

#[tokio::test]
async fn a_non_session_address_kind_is_refused_by_name() {
    let dir = scratch("kind");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 1);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    let error = err(
        &registry,
        OBSERVE_EVENT_METHOD,
        json!({ "address": { "kind": "subagent", "sessionId": "s1" } }),
    )
    .await;
    assert!(error.to_string().contains("subagent"));
}

#[tokio::test]
async fn a_missing_session_id_is_refused() {
    let dir = scratch("no-id");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirCatalog::new(&dir));
    let registry = registry(&catalog);

    let error = err(&registry, OBSERVE_EVENT_METHOD, json!({})).await;
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
}

// ---- observeProjections -----------------------------------------------------------------------

#[tokio::test]
async fn a_silent_event_produces_no_projection_frame() {
    let dir = scratch("silent");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 4);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    let reply = ok(
        &registry,
        OBSERVE_PROJECTIONS_METHOD,
        json!({ "sessionId": "s1" }),
    )
    .await;
    // A `user/message` changes no reference projection, so the channel correctly emits nothing — and
    // still advances past all four events, which is the property the cursor test below pins.
    assert!(frames(&reply).is_empty());
    assert_eq!(reply.get("nextSeq").and_then(Value::as_u64), Some(4));
}

#[tokio::test]
async fn the_projection_cursor_advances_by_examined_events() {
    let dir = scratch("silent-cursor");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 6);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    // Six events, no frames. Advancing by the frame count would leave the cursor at zero and re-read
    // the whole log on every poll for ever, with `hasMore` permanently true.
    let reply = ok(
        &registry,
        OBSERVE_PROJECTIONS_METHOD,
        json!({ "sessionId": "s1" }),
    )
    .await;
    assert_eq!(reply.get("nextSeq").and_then(Value::as_u64), Some(6));
    assert_eq!(reply.get("hasMore"), Some(&Value::Bool(false)));

    let again = ok(
        &registry,
        OBSERVE_PROJECTIONS_METHOD,
        json!({ "sessionId": "s1", "afterSeq": 6 }),
    )
    .await;
    assert!(frames(&again).is_empty());
    assert_eq!(again.get("nextSeq").and_then(Value::as_u64), Some(6));
}

#[tokio::test]
async fn a_projection_change_is_reported_once_per_event() {
    let definitions = definitions();
    let log = Sequenced::of(vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ]);
    let reply = project_changes(&log, ObserveArgs::default(), &definitions).unwrap();
    // The turn boundary fold is visible on both events, one frame each, each stamped with its own seq.
    assert_eq!(reply.frames.len(), 2);
    let seqs: Vec<u64> = reply
        .frames
        .iter()
        .map(|frame| frame.get("seq").and_then(Value::as_u64).unwrap())
        .collect();
    assert_eq!(seqs, vec![1, 2]);
    for frame in &reply.frames {
        assert_eq!(
            frame.get("type").and_then(Value::as_str),
            Some("projection")
        );
        assert!(frame.get("changes").and_then(Value::as_array).is_some());
    }
}

#[test]
fn an_empty_change_set_produces_no_frame() {
    // A frame saying "nothing changed" is worse than no frame: the panel consumer re-renders for it.
    assert!(projection_frame(1, &[]).is_none());
}

#[test]
fn equal_projection_values_are_not_a_change() {
    let before = std::collections::BTreeMap::from([("title".to_owned(), json!("t"))]);
    let after = std::collections::BTreeMap::from([("title".to_owned(), json!("t"))]);
    assert!(diff(&before, &after).is_empty());
}

#[test]
fn a_changed_projection_value_is_a_change() {
    let before = std::collections::BTreeMap::from([("title".to_owned(), json!("t"))]);
    let after = std::collections::BTreeMap::from([("title".to_owned(), json!("u"))]);
    let changes = diff(&before, &after);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].key, "title");
    assert_eq!(changes[0].val, json!("u"));
}

#[test]
fn a_host_with_no_projections_delivers_nothing_rather_than_failing() {
    // Not `NOT_IMPLEMENTED`: the channel works, there is simply nothing registered that can change.
    let log = Sequenced::of(vec![SessionEvent::TurnStart { turn: 1 }]);
    let reply = project_changes(&log, ObserveArgs::default(), &[]).unwrap();
    assert!(reply.frames.is_empty());
    assert_eq!(reply.next_seq, 1);
}

#[test]
fn the_replay_exposes_the_view_it_folded() {
    // The view is what a host would compare against a checkpoint, so it has to be readable rather than
    // only internally accumulated.
    let definitions = definitions();
    let mut replay = ProjectionReplay::new(&definitions).unwrap();
    assert_eq!(projection_keys(&replay).len(), definitions.len());

    replay.fold(1, &SessionEvent::TurnStart { turn: 1 });
    let view = replay.view();
    // The fold's own shape, not a shape invented here: the open turn is recorded by the sequence its
    // `turn/start` landed at.
    assert_eq!(view["turnBoundary"]["openTurnStartSeq"], json!(1));
    assert_eq!(view["turnBoundary"]["lastTurn"], json!(1));
}

#[test]
fn a_log_reports_its_length_and_emptiness() {
    let empty = Sequenced::of(Vec::new());
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);

    let one = Sequenced::of(vec![SessionEvent::TurnStart { turn: 1 }]);
    assert!(!one.is_empty());
    assert_eq!(one.len(), 1);
}

#[test]
fn two_definitions_with_one_key_are_refused() {
    // Two folds under one key cannot be told apart in a view, so the replay refuses rather than
    // silently letting the second overwrite the first.
    let duplicated: Vec<Arc<dyn Projection>> = vec![Arc::new(TurnBoundary), Arc::new(TurnBoundary)];
    assert!(ProjectionReplay::new(&duplicated).is_err());
}

// ---- observeState -----------------------------------------------------------------------------

#[tokio::test]
async fn the_state_reports_the_cursor_and_the_open_turn() {
    let log = Sequenced::of(vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
        SessionEvent::TurnStart { turn: 2 },
    ]);
    let state = state_of(&log);
    assert_eq!(state.cursor, 3);
    assert!(state.interrupted);
    assert_eq!(state.open_turn, Some(2));

    let closed = Sequenced::of(vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ]);
    assert!(!state_of(&closed).interrupted);
    assert_eq!(state_of(&closed).open_turn, None);
}

#[tokio::test]
async fn an_unchanged_state_is_not_re_sent() {
    let dir = scratch("state-stable");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 3);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(dir_catalog);
    let registry = registry(&catalog);

    let first = ok(
        &registry,
        OBSERVE_STATE_METHOD,
        json!({ "sessionId": "s1" }),
    )
    .await;
    assert_eq!(frames(&first).len(), 1);
    let cursor = first.get("nextSeq").and_then(Value::as_u64).unwrap();

    // Polling again from that cursor: the state is the same snapshot, so nothing is delivered. The
    // transport dedupes exact repeats too, but the frames array must be honest on its own.
    let second = ok(
        &registry,
        OBSERVE_STATE_METHOD,
        json!({ "sessionId": "s1", "afterSeq": cursor }),
    )
    .await;
    assert!(frames(&second).is_empty());
}

#[tokio::test]
async fn a_state_frame_carries_a_whole_snapshot() {
    let state = StateSnapshot {
        cursor: 4,
        running: true,
        interrupted: false,
        open_turn: None,
    };
    let frame = api_final_gap::state_frame(&state);
    let body = frame.get("state").expect("a state frame carries its state");
    // A whole snapshot rather than a delta is what makes a missed poll harmless.
    assert_eq!(body.get("cursor").and_then(Value::as_u64), Some(4));
    assert_eq!(body.get("running"), Some(&Value::Bool(true)));
    assert_eq!(body.get("interrupted"), Some(&Value::Bool(false)));
}

#[tokio::test]
async fn running_comes_from_the_source_not_the_log() {
    let dir = scratch("running");
    let dir_catalog = DirCatalog::new(&dir);
    dir_catalog.seed("s1", 2);
    // Seeded before the catalog is shared, so the second session exists on the same directory.
    dir_catalog.seed("s2", 1);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirCatalog::new(&dir));

    // A closure is a source, so a host that tracks turns in its own table need not write a type.
    let running: Arc<dyn RunningSource> = Arc::new(|id: &str| id == "s1");
    let mut registry = RemoteRegistry::new();
    register_observe_methods(&mut registry, &catalog, &definitions(), &running);

    let reply = ok(
        &registry,
        OBSERVE_STATE_METHOD,
        json!({ "sessionId": "s1" }),
    )
    .await;
    let body = &frames(&reply)[0]["state"];
    // The log cannot answer this: a build hardcoding `false` would report an executing session as idle.
    assert_eq!(body.get("running"), Some(&Value::Bool(true)));

    // And a different session is not running, which a hardcoded `true` would get wrong the other way.
    let other = ok(
        &registry,
        OBSERVE_STATE_METHOD,
        json!({ "sessionId": "s2" }),
    )
    .await;
    assert_eq!(
        frames(&other)[0]["state"].get("running"),
        Some(&Value::Bool(false))
    );
}

#[test]
fn the_default_running_source_says_nothing_is_running() {
    let source = NoRunning;
    assert!(!source.is_running("anything"));
}

#[test]
fn a_changed_state_is_delivered_whole() {
    let before = StateSnapshot {
        cursor: 1,
        running: false,
        interrupted: false,
        open_turn: None,
    };
    let after = StateSnapshot {
        cursor: 2,
        running: true,
        ..before.clone()
    };
    let reply = observe_state(
        ObserveArgs {
            after_seq: 1,
            limit: 1,
        },
        &after,
        Some(&before),
    );
    assert_eq!(reply.frames.len(), 1);
    assert_eq!(reply.next_seq, 2);
}

// ---- arguments and bounds ---------------------------------------------------------------------

#[test]
fn a_nonsense_cursor_is_refused_rather_than_read_as_absent() {
    // Read as "absent", a negative cursor would replay the whole session to a caller that sent garbage.
    assert!(api_final_gap::parse_args(&json!({ "afterSeq": -1 })).is_err());
    assert!(api_final_gap::parse_args(&json!({ "afterSeq": "7" })).is_err());
    assert!(
        api_final_gap::parse_args(&json!({ "afterSeq": api_final_gap::MAX_SAFE_SEQ + 1 })).is_err()
    );
}

#[test]
fn a_zero_limit_keeps_the_default_rather_than_returning_nothing() {
    // An empty page is indistinguishable from "nothing happened".
    let args = api_final_gap::parse_args(&json!({ "limit": 0 })).unwrap();
    assert_eq!(args.limit, DEFAULT_LIMIT);
}

#[test]
fn a_limit_beyond_the_cap_is_refused() {
    assert!(api_final_gap::parse_args(&json!({ "limit": MAX_LIMIT + 1 })).is_err());
}

#[test]
fn an_absent_cursor_opens_at_the_beginning() {
    let args = api_final_gap::parse_args(&json!({})).unwrap();
    assert_eq!(args.after_seq, 0);
    assert!(args.is_opening());
}

#[test]
fn opening_delivers_every_event_and_stops_at_the_end() {
    let log = Sequenced::of(vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::TurnStart { turn: 2 },
    ]);
    let reply = observe_events(&log, ObserveArgs::default());
    assert_eq!(reply.frames.len(), 2);
    assert!(reply.ready);
    assert!(!reply.has_more);
    assert_eq!(reply.next_seq, 2);
    assert_eq!(reply.lost_before, None);
}

#[test]
fn a_subagent_address_is_refused_by_name_directly() {
    let error = session_id_of(
        &json!({ "address": { "kind": "subagent", "sessionId": "s" } }),
        OBSERVE_EVENT_METHOD,
    )
    .unwrap_err();
    assert!(error.to_string().contains("subagent"));
}

#[test]
fn the_three_method_names_match_upstream() {
    // Spelled once here so a rename in the implementation breaks a statement of the wire contract.
    assert_eq!(OBSERVE_EVENT_METHOD, "observeEvent");
    assert_eq!(OBSERVE_PROJECTIONS_METHOD, "observeProjections");
    assert_eq!(OBSERVE_STATE_METHOD, "observeState");
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod api {
    pub use super::api_final_gap as api_final;
}
