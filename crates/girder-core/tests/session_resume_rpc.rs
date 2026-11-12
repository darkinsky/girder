//! Tests for `session.resume`, re-attaching an existing session.
//!
//! The properties a client depends on:
//!
//! - **the cursor matches the log** — `events` and `lastSeq` are the numbers the client hands back
//!   to `session.follow`/`session.page`, so an off-by-one would duplicate or drop exactly one event
//!   on every reconnect, which is the hardest kind of bug to see in a UI
//! - **an empty session resumes** — a session created and never used is a normal state, not a
//!   defect, and it must report zero events rather than a missing field
//! - **an unknown session is an error, not an empty success** — "resumed, zero events" is
//!   indistinguishable from "here is your new session", so a typo would silently discard history
//! - **a session that cannot be opened reports that, naming the real cause** — a catalog that can
//!   decode a log but not hand out a writable handle would otherwise promise a prompt it must
//!   refuse
//! - **resume is enough to prompt afterwards** — the point of the method is that the conversation
//!   continues, and the turn numbering has to continue with it rather than restart
//! - **an interrupted log is reported and not repaired** — this build deliberately does not write
//!   synthetic closers, so the flag has to be true and the log has to be byte-identical afterwards
//! - **the call declares a write** — a read-only mirror must not claim it can resume a session it
//!   can never accept a prompt for
//!
//! Isolation: every test builds its own registry and catalog over a scratch directory named from
//! the process id and a counter, so tests cannot collide.
//!
//! The method is reached through the crate's public re-export rather than by including the module
//! by path: a missing export would otherwise be invisible here, which is exactly how a method ends
//! up implemented but unreachable. The wire names and the response shape are spelled out locally
//! instead of imported, because they are the contract a client codes against — a test that read
//! them from the implementation would keep passing after the implementation was renamed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{
    AgentRuntime, BoxFut, ChunkStream, DirectoryCatalog, EventBus, FinishReason, LlmError,
    LlmProvider, LlmRequest, ReadOnly, RemoteCall, RemoteError, RemoteRegistry, RuntimeConfig,
    SessionCatalog, SessionEvent, SessionRecord, SessionStore, StreamChunk, ToolRegistry,
    ToolRegistryView, TurnEndReason, register_session_resume_method,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// The namespace, spelled as a client spells it.
const NAMESPACE: &str = "session";

/// The method, spelled as upstream spells it.
const METHOD: &str = "resume";

/// What a client reads out of the response.
///
/// Declared here rather than imported from the implementation so this file is an independent
/// statement of the wire contract: renaming a field in the implementation has to break this.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reply {
    session_id: String,
    resumed: bool,
    events: u64,
    last_seq: u64,
    running: bool,
    interrupted: bool,
}

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated scratch directory: `temp_dir()` plus pid plus a counter.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-resume-{name}-{}-{n}", std::process::id()));
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

/// A provider that answers with a fixed text, so a prompt runs a real turn without a network.
struct EchoProvider;

impl LlmProvider for EchoProvider {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>> {
        let chunks = vec![
            StreamChunk::BlockStart {
                index: 0,
                block_type: "text".to_owned(),
            },
            StreamChunk::TextDelta {
                index: 0,
                text: "echoed".to_owned(),
            },
            StreamChunk::Finish {
                reason: FinishReason::Stop,
            },
        ];
        Box::pin(async move { Ok(Box::pin(futures::stream::iter(chunks)) as ChunkStream) })
    }
}

fn runtime() -> Arc<AgentRuntime> {
    let tools: Arc<dyn ToolRegistryView> = Arc::new(ToolRegistry::new());
    AgentRuntime::new(RuntimeConfig {
        provider: Arc::new(EchoProvider),
        tools,
        bus: Arc::new(EventBus::new()),
        model: "test".to_owned(),
        max_turns: 1,
        token_budget: girder_core::Ceiling::Unlimited,
    })
}

fn registry(catalog: &Arc<dyn SessionCatalog>, runtime: &Arc<AgentRuntime>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    register_session_resume_method(&mut registry, catalog, runtime);
    registry
}

fn catalog(root: &Path) -> Arc<dyn SessionCatalog> {
    Arc::new(DirectoryCatalog::new(root))
}

async fn resume(registry: &RemoteRegistry, args: Value) -> Result<Reply, RemoteError> {
    let value = registry.dispatch(NAMESPACE, METHOD, call(args)).await?;
    Ok(serde_json::from_value(value).expect("the response matches the documented shape"))
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

// ---- the cursor ---------------------------------------------------------------------------------

#[tokio::test]
async fn resuming_a_session_with_events_reports_its_count_and_last_seq() {
    let dir = scratch("count");
    seed(&dir, "s1", 4);
    let runtime = runtime();

    let value = resume(
        &registry(&catalog(&dir), &runtime),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("a known session resumes");

    assert_eq!(value.session_id, "s1");
    assert!(value.resumed);
    assert_eq!(value.events, 4);
    assert_eq!(value.last_seq, 4, "sequence numbers are one-based");
    assert!(!value.running, "nothing is running for a cold session");
    assert!(!value.interrupted, "a plain log of messages ends no turn");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_reported_cursor_matches_session_follow() {
    // The two numbers are only useful if they agree with the subscription the client starts next:
    // `afterSeq = lastSeq` must be the exact continuation, with nothing repeated and nothing lost.
    let dir = scratch("cursor");
    seed(&dir, "s1", 3);
    let runtime = runtime();

    let resumed = resume(
        &registry(&catalog(&dir), &runtime),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("resumes");

    // `session.follow` is not registered here, so the same arithmetic is checked against the log:
    // skipping `lastSeq` records must leave nothing.
    let events = store(&dir, "s1").read_all().expect("reads");
    assert_eq!(
        events
            .iter()
            .skip(usize::try_from(resumed.last_seq).expect("a test log is small"))
            .count(),
        0,
        "lastSeq is the last event, so skipping it exhausts the log"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the empty session --------------------------------------------------------------------------

#[tokio::test]
async fn resuming_an_empty_session_succeeds_with_no_events() {
    // A session that was created and never used is a normal state. Refusing it would make a client
    // treat a blank conversation as a broken one.
    let dir = scratch("empty");
    store(&dir, "s1").touch().expect("created");
    let runtime = runtime();

    let value = resume(
        &registry(&catalog(&dir), &runtime),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("an empty session still resumes");

    assert!(value.resumed);
    assert_eq!(value.events, 0);
    assert_eq!(
        value.last_seq, 0,
        "an empty log has no last sequence number"
    );
    assert!(!value.interrupted);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the errors ---------------------------------------------------------------------------------

#[tokio::test]
async fn resuming_an_unknown_session_is_an_error() {
    // Not an empty success: that would be indistinguishable from a fresh session, and the caller
    // would prompt into a conversation that lost its history.
    let dir = scratch("unknown");
    let runtime = runtime();

    let error = resume(
        &registry(&catalog(&dir), &runtime),
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
    let runtime = runtime();
    let registry = registry(&catalog(&dir), &runtime);

    for args in [
        json!({}),
        json!({"sessionId": ""}),
        json!({"sessionId": "   "}),
    ] {
        let error = resume(&registry, args)
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

#[tokio::test]
async fn a_session_with_no_writable_handle_reports_that() {
    // A catalog that can decode a log but cannot hand out a session would otherwise answer
    // `resumed: true` and then refuse the prompt that follows, blaming the wrong call.
    struct ReadOnlyCatalog {
        events: Vec<SessionEvent>,
    }

    impl SessionCatalog for ReadOnlyCatalog {
        fn list(&self) -> BoxFut<Vec<SessionRecord>> {
            Box::pin(async { Vec::new() })
        }

        fn read(&self, id: &str) -> BoxFut<Option<SessionRecord>> {
            let events = self.events.clone();
            let id = id.to_owned();
            Box::pin(async move {
                Some(SessionRecord {
                    id,
                    updated_at: 0,
                    blank: false,
                    events,
                })
            })
        }

        fn create(&self, id: &str) -> BoxFut<Result<SessionRecord, RemoteError>> {
            let id = id.to_owned();
            Box::pin(async move {
                Err(RemoteError::Failed(format!(
                    "read-only catalog cannot create `{id}`"
                )))
            })
        }
        // `open` keeps its default: `None`, which is what a read-only backing store answers.
    }

    let catalog: Arc<dyn SessionCatalog> = Arc::new(ReadOnlyCatalog {
        events: vec![SessionEvent::user_text("msg-1", "hello")],
    });
    let runtime = runtime();

    let error = resume(&registry(&catalog, &runtime), json!({"sessionId": "s1"}))
        .await
        .expect_err("a session that cannot be opened for writing is not resumable");

    match error {
        RemoteError::Failed(message) => assert!(
            message.contains("cannot be opened for writing"),
            "the message names the real cause, got `{message}`"
        ),
        other => panic!("expected FAILED, got {other:?}"),
    }
}

// ---- continuing the conversation ----------------------------------------------------------------

#[tokio::test]
async fn a_resumed_session_accepts_a_prompt_and_continues_the_turn_numbering() {
    // The whole point of resume: the conversation continues. The turn number counts user messages,
    // so a resumed session with two prompts must answer turn 3 — a restart at 1 would show a client
    // a conversation that had forgotten itself.
    let dir = scratch("prompt");
    seed(&dir, "s1", 2);
    let runtime = runtime();
    let catalog = catalog(&dir);
    let registry = registry(&catalog, &runtime);

    let resumed = resume(&registry, json!({"sessionId": "s1"}))
        .await
        .expect("resumes");
    assert_eq!(resumed.events, 2);

    // Prompting goes through the same catalog handle resume checked, which is exactly the promise
    // `resume` makes.
    let session: Arc<dyn girder_core::Session> =
        catalog.open("s1").expect("resume promised it opens");
    let turn = AgentRuntime::prompt(&runtime, "s1", "again", session)
        .await
        .expect("a resumed session accepts a prompt");
    assert_eq!(turn, 3, "two user messages precede this prompt");

    // The prompt is durable before the model answers, so the log grew immediately.
    let after = resume(&registry, json!({"sessionId": "s1"}))
        .await
        .expect("resumes again");
    assert_eq!(after.events, 3);
    assert_eq!(after.last_seq, 3);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn resuming_reports_a_running_turn() {
    // `running` comes from the runtime, not the log: a turn in flight has usually not written its
    // `turn/end` yet, so the two sources answer different questions and the runtime's is the one a
    // client needs to avoid sending a second prompt.
    let dir = scratch("running");
    seed(&dir, "s1", 1);
    let runtime = runtime();
    let catalog = catalog(&dir);
    let registry = registry(&catalog, &runtime);

    let session = catalog.open("s1").expect("opens");
    let _turn = AgentRuntime::prompt(&runtime, "s1", "go", session)
        .await
        .expect("starts");

    // The turn may finish before this read; both answers are correct, so the assertion is that the
    // field tracks the runtime rather than that it is specifically true.
    for _ in 0..200 {
        if !runtime.is_running("s1") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let value = resume(&registry, json!({"sessionId": "s1"}))
        .await
        .expect("resumes");
    assert_eq!(value.running, runtime.is_running("s1"));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the interrupted log ------------------------------------------------------------------------

#[tokio::test]
async fn an_interrupted_log_is_reported_and_left_untouched() {
    // Upstream repairs a crashed turn by appending closers. This build reports instead, because a
    // fabricated `tool/result` would hand the loop an observation it never made. The flag has to be
    // right, and the log has to be byte-identical afterwards.
    let dir = scratch("interrupted");
    let store = store(&dir, "s1");
    store.touch().expect("created");
    store
        .append(&SessionEvent::TurnStart { turn: 1 })
        .expect("appended");
    store
        .append(&SessionEvent::user_text("msg-1", "hello"))
        .expect("appended");

    let before = std::fs::read(dir.join("s1.jsonl.zst")).expect("the log exists");
    let runtime = runtime();

    let value = resume(
        &registry(&catalog(&dir), &runtime),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("an interrupted session still resumes");

    assert!(value.interrupted, "a start with no end is a cut-off turn");
    assert_eq!(value.events, 2);

    let after = std::fs::read(dir.join("s1.jsonl.zst")).expect("the log still exists");
    assert_eq!(before, after, "resume must not write to the log");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_completed_turn_is_not_reported_as_interrupted() {
    let dir = scratch("completed");
    let store = store(&dir, "s1");
    store.touch().expect("created");
    store
        .append(&SessionEvent::TurnStart { turn: 1 })
        .expect("appended");
    store
        .append(&SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .expect("appended");
    let runtime = runtime();

    let value = resume(
        &registry(&catalog(&dir), &runtime),
        json!({"sessionId": "s1"}),
    )
    .await
    .expect("resumes");

    assert!(!value.interrupted, "the turn boundary is closed");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the declaration ----------------------------------------------------------------------------

#[tokio::test]
async fn a_read_only_policy_refuses_resume() {
    // Resuming commits the deployment to accepting a prompt afterwards, so a peer that may only
    // read is refused rather than told `resumed: true` for a session it can never prompt.
    let dir = scratch("policy");
    seed(&dir, "s1", 1);
    let runtime = runtime();
    let mut registry = RemoteRegistry::new();
    register_session_resume_method(&mut registry, &catalog(&dir), &runtime);
    let registry = registry.with_policy(Arc::new(ReadOnly));

    let error = registry
        .dispatch(NAMESPACE, METHOD, call(json!({"sessionId": "s1"})))
        .await
        .expect_err("a read-only peer cannot resume");

    assert!(
        matches!(error, RemoteError::Refused { .. }),
        "got {error:?}"
    );
    assert_eq!(error.code(), "REFUSED");

    let _ = std::fs::remove_dir_all(&dir);
}
