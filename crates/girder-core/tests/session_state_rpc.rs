//! Tests for `session.state`, the one-read render snapshot.
//!
//! The properties a client depends on, in the order they matter:
//!
//! - **the counts match the log** — `events` and `lastSeq` are what a follow subscription continues
//!   from, so an off-by-one would duplicate or drop exactly one event on every reconnect, which is the
//!   hardest kind of bug to see in a UI
//! - **the projections are in the answer** — the point of the method is that a client renders from one
//!   response; a snapshot without the folded views would send it straight back for a second call
//! - **an unknown session is an error** — "zero events, no title, nothing folded" is exactly what a
//!   new session looks like, so an empty success would let a typo become a fresh conversation
//! - **an empty session succeeds with zeroes** — created and never used is a normal state, and the
//!   projection map must still be present rather than omitted
//! - **a read-only policy can call it** — the method is declared a read, and a mirror that can render
//!   a session but not prompt into it is a supported deployment
//! - **a cut-off log is reported as interrupted and not repaired** — the flag has to be true and the
//!   log has to be byte-identical afterwards
//! - **`running` comes from the live-turn source, not the log** — the log cannot answer it, and a
//!   hardcoded `false` would report an executing session as idle
//!
//! Isolation: every test builds its own registry, catalog, and projection registry over a scratch
//! directory named from the process id and a counter, so tests cannot collide.
//!
//! The module is included by path, which is how the rest of this crate's RPC tests reach a file under
//! `src/`: it lets these tests name [`LiveTurns`] and [`SessionStateValue`], which the crate root does
//! not re-export. One test additionally builds its registry through the crate's public re-export, so a
//! method that was implemented but never wired is caught here rather than in a client. The include
//! spells its paths through `girder_core::`, the alias the library declares for itself, so the same
//! source builds both here and inside the crate. The wire names and the response shape are declared
//! locally rather than imported: they are the contract a client codes against, and a test that read
//! them from the implementation would keep passing after a field was renamed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/session_state_rpc.rs"]
mod session_state_rpc;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{
    BoxFut, ModelSelectionProjection, ProjectionRegistry, ReadOnly, RemoteCall, RemoteError,
    RemoteRegistry, SessionCatalog, SessionEvent, SessionRecord, SessionStore, TitleProjection,
    TurnBoundary,
};
use serde::Deserialize;
use serde_json::{Value, json};

use session_state_rpc::{
    LiveTurns, METHOD, NAMESPACE, SessionStateValue, register_session_state_method,
    register_session_state_method_with,
};

/// What a client reads out of the response.
///
/// Declared here rather than imported from the implementation so this file is an independent statement
/// of the wire contract: renaming a field in the implementation has to break this.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reply {
    session_id: String,
    events: u64,
    last_seq: u64,
    #[serde(default)]
    title: Option<String>,
    projections: serde_json::Map<String, Value>,
    running: bool,
    interrupted: bool,
}

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated scratch directory: `temp_dir()` plus pid plus a counter.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-state-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory is created");
    dir
}

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

fn store(dir: &Path, id: &str) -> SessionStore {
    SessionStore::new(dir.join(format!("{id}.jsonl.zst")))
}

/// Writes `count` user messages into one session, creating the log.
fn seed(dir: &Path, id: &str, count: usize) {
    let store = store(dir, id);
    store.touch().expect("created");
    for index in 0..count {
        store
            .append(&SessionEvent::user_text(
                &format!("msg-{index}"),
                format!("body {index}"),
            ))
            .expect("appended");
    }
}

/// A projection registry holding the three reference folds.
fn projections() -> Arc<ProjectionRegistry> {
    let mut registry = ProjectionRegistry::new();
    registry
        .register(Arc::new(TurnBoundary))
        .expect("turnBoundary is registered once");
    registry
        .register(Arc::new(ModelSelectionProjection))
        .expect("modelSelection is registered once");
    registry
        .register(Arc::new(TitleProjection))
        .expect("title is registered once");
    Arc::new(registry)
}

/// A catalog over `dir` that also answers `title` from a sidecar, the way a directory-backed host does.
///
/// Written here rather than reaching for `DirectoryCatalog` so a test can rename without the log
/// existing, and so the title is provably read through the trait rather than through a concrete type.
struct TitledCatalog {
    root: PathBuf,
}

impl SessionCatalog for TitledCatalog {
    fn list(&self) -> BoxFut<Vec<SessionRecord>> {
        Box::pin(async { Vec::new() })
    }

    fn read(&self, id: &str) -> BoxFut<Option<SessionRecord>> {
        let path = self.root.join(format!("{id}.jsonl.zst"));
        let id = id.to_owned();
        Box::pin(async move {
            if !path.is_file() {
                return None;
            }
            let events = SessionStore::new(&path).read_all().ok()?;
            let blank = !events
                .iter()
                .any(|event| matches!(event, SessionEvent::UserMessage { .. }));
            Some(SessionRecord {
                id,
                updated_at: 0,
                blank,
                events,
            })
        })
    }

    fn create(&self, id: &str) -> BoxFut<Result<SessionRecord, RemoteError>> {
        let id = id.to_owned();
        Box::pin(async move {
            Err(RemoteError::Failed(format!(
                "the test catalog does not create `{id}`"
            )))
        })
    }

    fn title(&self, id: &str) -> BoxFut<Option<String>> {
        let path = self.root.join(format!("{id}.title"));
        let id = id.to_owned();
        Box::pin(async move {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                let _ = id;
                return None;
            };
            let title = raw.trim().to_owned();
            (!title.is_empty()).then_some(title)
        })
    }
}

/// A source that reports exactly one session as running.
///
/// Declared at module scope rather than inside the test: an `impl` after a statement is confusing to
/// read, and clippy says so.
struct Only(Arc<str>);

impl LiveTurns for Only {
    fn is_running(&self, session_id: &str) -> bool {
        session_id == &*self.0
    }
}

fn catalog(root: &Path) -> Arc<dyn SessionCatalog> {
    Arc::new(TitledCatalog {
        root: root.to_path_buf(),
    })
}

fn registry(
    catalog: &Arc<dyn SessionCatalog>,
    projections: &Arc<ProjectionRegistry>,
) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    register_session_state_method(&mut registry, catalog, projections);
    registry
}

async fn state(registry: &RemoteRegistry, args: Value) -> Result<Reply, RemoteError> {
    let value = registry.dispatch(NAMESPACE, METHOD, call(args)).await?;
    Ok(serde_json::from_value(value).expect("the response matches the documented shape"))
}

// ---- the counts ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_session_with_events_reports_its_count_and_last_seq() {
    let dir = scratch("count");
    seed(&dir, "s1", 4);

    let value = state(
        &registry(&catalog(&dir), &projections()),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("a known session is described");

    assert_eq!(value.session_id, "s1");
    assert_eq!(value.events, 4);
    assert_eq!(value.last_seq, 4, "sequence numbers are one-based");
    assert!(!value.running, "the default source reports no live turn");
    assert!(!value.interrupted, "a plain log of messages ends no turn");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_reported_last_seq_is_the_exact_continuation_point() {
    // `lastSeq` is what a client hands to `session.follow`, so skipping it must exhaust the log:
    // anything else repeats or drops an event on every reconnect.
    let dir = scratch("cursor");
    seed(&dir, "s1", 3);

    let value = state(
        &registry(&catalog(&dir), &projections()),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("describes the session");

    let events = store(&dir, "s1").read_all().expect("reads the log");
    assert_eq!(events.len(), 3);
    assert_eq!(
        events
            .iter()
            .skip(usize::try_from(value.last_seq).expect("a test log is small"))
            .count(),
        0,
        "lastSeq is the last event, so skipping it exhausts the log"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the projections ----------------------------------------------------------------------------

#[tokio::test]
async fn the_folded_projections_are_part_of_the_answer() {
    // The point of the method: one response is enough to render. A snapshot without the views would
    // send the client straight back for a second call.
    let dir = scratch("projections");
    seed(&dir, "s1", 2);

    let projections = projections();
    // Folded by the host, as a live session would fold them: replay from scratch, numbered one-based.
    let events = store(&dir, "s1").read_all().expect("reads the log");
    let numbered: Vec<(u64, SessionEvent)> = events
        .into_iter()
        .enumerate()
        .map(|(index, event)| (u64::try_from(index).unwrap_or(0) + 1, event))
        .collect();
    projections.replay(&numbered);

    let value = state(
        &registry(&catalog(&dir), &projections),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("describes the session");

    assert_eq!(
        value.projections.len(),
        3,
        "every registered projection appears, got {:?}",
        value.projections.keys().collect::<Vec<_>>()
    );
    for key in ["title", "turnBoundary", "modelSelection"] {
        assert!(
            value.projections.contains_key(key),
            "the `{key}` projection is in the snapshot"
        );
    }
    // The views are the registry's own values, not re-derived ones: replaying two user messages
    // leaves no turn open and no title.
    assert!(
        value.projections["turnBoundary"]["openTurnStartSeq"].is_null(),
        "no turn was started"
    );
    assert_eq!(value.projections["turnBoundary"]["lastTurn"], 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unfolded_projection_registry_still_answers_with_its_keys() {
    // A projection's `init` state is a real state: an empty log has one. Reading it must not be
    // confused with "the host folded nothing", and the keys must be present so a client can tell a
    // zero value from a missing field.
    let dir = scratch("unfolded");
    seed(&dir, "s1", 1);

    let value = state(
        &registry(&catalog(&dir), &projections()),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("describes the session");

    assert_eq!(value.projections.len(), 3);
    assert!(
        value.projections["turnBoundary"].is_object(),
        "an initialised projection has its init object, not null"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the title ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_renamed_session_reports_its_stored_title() {
    let dir = scratch("title");
    seed(&dir, "s1", 1);
    std::fs::write(dir.join("s1.title"), "Refactor the parser").expect("writes the sidecar");

    let value = state(
        &registry(&catalog(&dir), &projections()),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("describes the session");

    assert_eq!(value.title.as_deref(), Some("Refactor the parser"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_untitled_session_reports_no_title_rather_than_a_guess() {
    // `None` is the honest answer: a derived label would make a session nobody renamed look renamed.
    let dir = scratch("untitled");
    seed(&dir, "s1", 2);

    let value = state(
        &registry(&catalog(&dir), &projections()),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("describes the session");

    assert_eq!(value.title, None);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the empty session --------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_session_succeeds_with_zero_events() {
    // Created and never used is a normal state. Refusing it would make a client treat a blank
    // conversation as a broken one.
    let dir = scratch("empty");
    store(&dir, "s1").touch().expect("created");

    let value = state(
        &registry(&catalog(&dir), &projections()),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("an empty session is still described");

    assert_eq!(value.events, 0);
    assert_eq!(value.last_seq, 0, "an empty log has no last sequence");
    assert!(!value.interrupted);
    assert_eq!(value.title, None);
    assert_eq!(
        value.projections.len(),
        3,
        "the projection map is present, not omitted"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the errors ---------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_session_is_an_error() {
    // Not an empty success: "zero events" is indistinguishable from a fresh session, so a typo would
    // silently become a new conversation.
    let dir = scratch("unknown");

    let error = state(
        &registry(&catalog(&dir), &projections()),
        json!({"sessionId": "nope"}),
    )
    .await
    .expect_err("an unknown session must fail");

    match error {
        RemoteError::Failed(message) => {
            assert!(
                message.contains("nope"),
                "the message names the missing id, got `{message}`"
            );
        }
        other => panic!("expected FAILED, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_session_id_is_invalid_arguments() {
    let dir = scratch("noid");
    let registry = registry(&catalog(&dir), &projections());

    for args in [
        json!({}),
        json!({"sessionId": ""}),
        json!({"sessionId": "   "}),
    ] {
        let error = state(&registry, args)
            .await
            .expect_err("a blank id is not a session");
        assert!(
            matches!(error, RemoteError::InvalidArguments { .. }),
            "expected INVALID_ARGUMENTS, got {error:?}"
        );
        assert_eq!(error.code(), "INVALID_ARGUMENTS");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the access policy --------------------------------------------------------------------------

#[tokio::test]
async fn a_read_only_policy_can_read_a_snapshot() {
    // The method writes nothing, so a mirror that may render but not prompt must still reach it.
    let dir = scratch("readonly");
    seed(&dir, "s1", 2);

    let mut registry = RemoteRegistry::new();
    register_session_state_method(&mut registry, &catalog(&dir), &projections());
    let registry = registry.with_policy(Arc::new(ReadOnly));

    let value = state(&registry, json!({"sessionId": "s1"}))
        .await
        .expect("a read-only peer can render a session");

    assert_eq!(value.events, 2);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the interrupted log ------------------------------------------------------------------------

#[tokio::test]
async fn a_log_that_ends_mid_turn_is_reported_and_left_untouched() {
    let dir = scratch("interrupted");
    let log = store(&dir, "s1");
    log.touch().expect("created");
    log.append(&SessionEvent::user_text("msg-1", "start"))
        .expect("appended");
    log.append(&SessionEvent::TurnStart { turn: 1 })
        .expect("appended");
    log.append(&SessionEvent::assistant_text("msg-2", "partial", "test"))
        .expect("appended");
    let before = std::fs::read(dir.join("s1.jsonl.zst")).expect("reads the log");

    let value = state(
        &registry(&catalog(&dir), &projections()),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("describes the cut-off session");

    assert!(value.interrupted, "a start with no end is a cut-off turn");
    assert_eq!(value.events, 3);

    let after = std::fs::read(dir.join("s1.jsonl.zst")).expect("reads the log");
    assert_eq!(
        before, after,
        "reporting a cut-off turn must not rewrite the log"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_completed_turn_is_not_reported_as_interrupted() {
    let dir = scratch("closed");
    let log = store(&dir, "s1");
    log.touch().expect("created");
    log.append(&SessionEvent::TurnStart { turn: 1 })
        .expect("appended");
    log.append(&SessionEvent::TurnEnd {
        turn: 1,
        reason: girder_core::TurnEndReason::Completed,
    })
    .expect("appended");

    let value = state(
        &registry(&catalog(&dir), &projections()),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("describes the session");

    assert!(!value.interrupted, "the turn boundary is closed");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- live turns ---------------------------------------------------------------------------------

#[tokio::test]
async fn running_comes_from_the_live_turn_source() {
    // The log cannot answer this, so the source must be consulted rather than assumed: a hardcoded
    // `false` would report an executing session as idle while its output streamed.
    let dir = scratch("running");
    seed(&dir, "busy", 1);
    seed(&dir, "idle", 1);

    let turns: Arc<dyn LiveTurns> = Arc::new(Only(Arc::from("busy")));
    let mut registry = RemoteRegistry::new();
    register_session_state_method_with(&mut registry, &catalog(&dir), &projections(), &turns);

    let busy = state(&registry, json!({"sessionId": "busy"}))
        .await
        .expect("describes the session");
    let idle = state(&registry, json!({"sessionId": "idle"}))
        .await
        .expect("describes the session");

    assert!(busy.running, "the source reports `busy` as running");
    assert!(!idle.running, "the source reports `idle` as not running");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_default_source_reports_nothing_running() {
    // Documented behaviour of the three-argument constructor: a build with no live-agent table cannot
    // claim a turn is executing, and answering `false` is the honest limit rather than a guess.
    let dir = scratch("nolt");
    seed(&dir, "s1", 1);

    let value = state(
        &registry(&catalog(&dir), &projections()),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("describes the session");

    assert!(!value.running);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the wiring ---------------------------------------------------------------------------------

#[tokio::test]
async fn the_crate_re_exports_the_registration_function() {
    // The method is only reachable by a host if the crate root re-exports it. A module that compiles
    // here but was never exported would leave `session.state` unimplemented in every deployment while
    // these tests still passed, which is exactly the gap this test closes.
    let dir = scratch("export");
    seed(&dir, "s1", 2);

    let mut registry = RemoteRegistry::new();
    girder_core::register_session_state_method(&mut registry, &catalog(&dir), &projections());

    let value = state(&registry, json!({"sessionId": "s1"}))
        .await
        .expect("the re-exported registration answers");

    assert_eq!(value.events, 2);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the wire shape -----------------------------------------------------------------------------

#[tokio::test]
async fn the_value_serialises_with_the_documented_field_names() {
    // Spelled out rather than round-tripped through `SessionStateValue`, so a rename of a struct field
    // breaks here — which is where a client's contract is checked.
    let dir = scratch("wire");
    seed(&dir, "s1", 1);

    let raw = registry(&catalog(&dir), &projections())
        .dispatch(NAMESPACE, METHOD, call(json!({"sessionId": "s1"})))
        .await
        .expect("describes the session");

    for field in [
        "sessionId",
        "events",
        "lastSeq",
        "title",
        "projections",
        "running",
        "interrupted",
    ] {
        assert!(
            raw.get(field).is_some(),
            "the response carries `{field}`, got {:?}",
            raw.as_object()
                .map(|object| object.keys().collect::<Vec<_>>())
        );
    }
    assert!(
        raw.get("session_id").is_none(),
        "no snake_case field leaked onto the wire"
    );

    // The exported value type must agree with the wire shape, since a host may build one directly.
    let value: SessionStateValue =
        serde_json::from_value(raw.clone()).expect("the exported type reads its own wire form");
    assert_eq!(value.events, 1);
    assert_eq!(value.session_id, "s1");
    assert_eq!(
        serde_json::to_value(&value).expect("re-serialises"),
        raw,
        "the exported type round-trips the documented shape"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
