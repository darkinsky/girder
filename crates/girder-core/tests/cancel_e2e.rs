//! Cancellation as a *client* sees it, on real files and a real runtime.
//!
//! `tests/cancel_scope.rs` pins the scope tree, `tests/subagent_interrupt.rs` pins the child
//! interrupt verb, and `tests/remote.rs` checks that `session.cancel` is accepted. None of them
//! answers the question an operator asks after pressing stop: **what did the log end up saying, and
//! can the session still be read?** A cancel that is accepted and silently swallowed, or one that
//! writes half a record, passes all three of those tests.
//!
//! Every test here therefore drives the real path — `session.prompt` / `session.cancel` through a
//! [`RemoteRegistry`] over a [`DirectoryCatalog`], the real [`AgentRuntime`], zstd log files in a
//! per-test directory — and asserts on the *contents* of the log and of `session.state`, not on
//! "nothing panicked".
//!
//! # The marker this file treats as "interrupted"
//!
//! This build writes no synthetic event on cancellation. The loop yields
//! [`LoopError::Cancelled`](girder_core::LoopError::Cancelled) and returns, so the last records are
//! a `turn/start` (and the `step/start` it had begun) with no closer. That shape is exactly what
//! [`girder_core::open_turn`], [`girder_core::digest_session`] and the `interrupted` field of
//! `session.state` read as "the process stopped inside a turn". So the assertions below are written
//! against it: one `turn/start`, no `turn/end`, `open_turn == Some(1)`, `digest.finished == false`.
//! A silent stop and a cancelled stop are then distinguishable, which is the point.
//!
//! # What this file found
//!
//! Cancellation reaches the client as **`AgentEvent::TurnEnded { turn: 0, reason: Error { message:
//! "loop cancelled" } }`**, not as `FinishReason::Aborted`: the runtime maps *every* loop error —
//! including `LoopError::Cancelled` — onto `FinishReason::Error` with turn `0`, and `Aborted` is
//! only produced by the `OpenAI` provider's own chunk mapping. A UI rendering that event shows "turn
//! `0` failed" for a user's `Ctrl-C`, and a client that groups deltas by turn number has nothing to
//! attach the failure to. The test below pins only the part that must hold (an end *is* announced,
//! and the reason names cancellation) so that fixing the reason does not require rewriting the
//! test.
//!
//! Also confirmed here: a scope `cancel()` marks the tree but does **not** stop in-flight work by
//! itself — the work's token is only cancelled by the owner's `run_cleanups`. That is the documented
//! design, and it means an owner that forgets `run_cleanups` leaves the loop running under a
//! cancelled scope. `parent_cancel_marks_and_releases_the_child` asserts the two phases separately
//! so a future change that makes `cancel` sufficient, or that makes `run_cleanups` unnecessary,
//! fails loudly.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use girder_core::{
    AgentEvent, AgentId, AgentRuntime, CancelReason, CancelScope as Scope, Ceiling, ChatMessage,
    ChunkStream, ContentBlock, DirectoryCatalog, EventBus, FinishReason, LiveTurns, LlmError,
    LlmProvider, LlmRequest, LoopContext, LoopError, ObservedEvent, ProjectionRegistry, RemoteCall,
    RemoteError, RemoteRegistry, RuntimeConfig, Session, SessionCatalog, SessionEvent,
    SessionStore, StreamChunk, TokenUsage, ToolRegistry, ToolRegistryView, digest_session,
    open_turn, read_session_file, register_session_state_method_with, registry_with_runtime,
    run as run_loop, run_cleanups,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// ---- fixtures ----------------------------------------------------------------------------------

/// An isolated scratch directory, unique per test *and* per call.
///
/// The counter matters as much as the process id: a directory named from the process id alone is
/// shared by every test in this binary, so one test's log would appear in another's listing and the
/// `types` assertions would compare against another conversation.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-cancel-e2e-{name}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("the scratch directory is created");
    dir
}

/// Removes only the directory this test created, so a run does not leak hundreds of them.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every text block of a request, joined — enough to pick a behaviour from the prompt.
fn prompt_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The text of one logged message.
fn message_text(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// A real provider whose behaviour is chosen by the prompt, so one runtime can host two different
/// sessions in one test.
///
/// Session isolation is a property of the runtime's per-session token table, not of the provider, so
/// giving each session its own provider would test the wrong thing — and a runtime holds exactly one.
///
/// * `slow` — streams until its token is cancelled, like a long answer.
/// * `steady` — streams twenty chunks 5 ms apart and then finishes, so a test that cancels a sibling
///   has a real window in which the survivor is demonstrably still working.
/// * anything else — answers at once with one text block and a usage record.
struct Scripted {
    /// How many times the endless behaviour was actually reached, so a test that waits for the loop
    /// to start can assert it did rather than hope a sleep was long enough.
    slow_started: Arc<AtomicUsize>,
}

impl LlmProvider for Scripted {
    fn stream(
        &self,
        request: LlmRequest,
        cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<ChunkStream, LlmError>> {
        let prompt = prompt_text(&request);

        if prompt.contains("slow") {
            self.slow_started.fetch_add(1, Ordering::SeqCst);
            let stream = async_stream::stream! {
                yield StreamChunk::BlockStart { index: 0, block_type: "text".to_owned() };
                let mut index = 0u64;
                loop {
                    // Cancellation is observed between chunks, which is the loop's contract: a
                    // provider that only checked at the end would keep the turn alive for the whole
                    // answer, and the loop's own per-chunk check would then be the only thing
                    // stopping it -- which is exactly what a never-ending stream cannot rely on.
                    if cancel.is_cancelled() {
                        yield StreamChunk::Finish { reason: FinishReason::Aborted };
                        break;
                    }
                    yield StreamChunk::TextDelta { index: 0, text: format!("{index}") };
                    index = index.wrapping_add(1);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            };
            return Box::pin(async move { Ok(Box::pin(stream) as ChunkStream) });
        }

        if prompt.contains("steady") {
            let stream = async_stream::stream! {
                yield StreamChunk::BlockStart { index: 0, block_type: "text".to_owned() };
                for index in 0..20u64 {
                    if cancel.is_cancelled() {
                        yield StreamChunk::Finish { reason: FinishReason::Aborted };
                        return;
                    }
                    yield StreamChunk::TextDelta { index: 0, text: format!("{index},") };
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                yield StreamChunk::Usage(TokenUsage { input: 3, output: 20, cached: 0 });
                yield StreamChunk::Finish { reason: FinishReason::Stop };
            };
            return Box::pin(async move { Ok(Box::pin(stream) as ChunkStream) });
        }

        let chunks = vec![
            StreamChunk::BlockStart {
                index: 0,
                block_type: "text".to_owned(),
            },
            StreamChunk::TextDelta {
                index: 0,
                text: "pong".to_owned(),
            },
            StreamChunk::Usage(TokenUsage {
                input: 5,
                output: 1,
                cached: 0,
            }),
            StreamChunk::Finish {
                reason: FinishReason::Stop,
            },
        ];
        Box::pin(async move { Ok(Box::pin(futures::stream::iter(chunks)) as ChunkStream) })
    }
}

/// One deployment: a scratch directory, a catalog over it, a runtime, and the registry a client
/// would actually talk to.
struct Harness {
    /// Removed on drop; the path is reached through [`Harness::dir`].
    scratch: Scratch,
    catalog: Arc<dyn SessionCatalog>,
    runtime: Arc<AgentRuntime>,
    registry: RemoteRegistry,
    bus: Arc<EventBus>,
    slow_started: Arc<AtomicUsize>,
}

impl Harness {
    fn new(name: &str) -> Self {
        let scratch = Scratch(scratch(name));
        let bus = Arc::new(EventBus::new());
        let slow_started = Arc::new(AtomicUsize::new(0));
        let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&scratch.0));
        let runtime = AgentRuntime::new(RuntimeConfig {
            provider: Arc::new(Scripted {
                slow_started: Arc::clone(&slow_started),
            }),
            tools: Arc::new(ToolRegistry::new()) as Arc<dyn ToolRegistryView>,
            bus: Arc::clone(&bus),
            model: "test-model".to_owned(),
            // One turn per prompt: a conversation loop would make the expected log depend on the
            // continuation logic rather than on cancellation.
            max_turns: 1,
            token_budget: Ceiling::Unlimited,
        });

        let mut registry = registry_with_runtime(Arc::clone(&catalog), Arc::clone(&runtime));

        // Registered the way a server does it, with a live-turn source backed by the runtime.
        // Without that source `session.state.running` is always false, and the "nothing is running
        // any more" assertion after a cancel would be vacuous.
        let projections = Arc::new(ProjectionRegistry::new());
        let turns: Arc<dyn LiveTurns> = {
            let runtime = Arc::clone(&runtime);
            let source = move |id: &str| runtime.is_running(id);
            Arc::new(source)
        };
        register_session_state_method_with(&mut registry, &catalog, &projections, &turns);

        Self {
            scratch,
            catalog,
            runtime,
            registry,
            bus,
            slow_started,
        }
    }

    fn dir(&self) -> &Path {
        &self.scratch.0
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.dir().join(format!("{id}.jsonl.zst"))
    }

    async fn dispatch(&self, method: &str, args: Value) -> Result<Value, RemoteError> {
        self.registry
            .dispatch(
                "session",
                method,
                RemoteCall {
                    args,
                    peer: "cancel-e2e".to_owned(),
                },
            )
            .await
    }

    async fn create(&self, id: &str) {
        self.dispatch("create", json!({ "sessionId": id }))
            .await
            .expect("a session is created");
    }

    async fn prompt(&self, id: &str, text: &str) -> Value {
        self.dispatch("prompt", json!({ "sessionId": id, "text": text }))
            .await
            .expect("the prompt is accepted")
    }

    /// Asks through the RPC a client uses, and reports the acknowledgement.
    async fn cancel(&self, id: &str) -> bool {
        let value = self
            .dispatch("cancel", json!({ "sessionId": id }))
            .await
            .expect("cancel is acknowledged");
        value["accepted"].as_bool().unwrap_or(false)
    }

    /// Blocks until no turn is in flight for `id`, or panics after five seconds.
    ///
    /// Waiting on the runtime's own answer rather than a fixed sleep: a sleep long enough for a
    /// loaded machine wastes seconds on an idle one, and a short one makes the test flaky.
    async fn idle(&self, id: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.runtime.is_running(id) {
            assert!(
                Instant::now() < deadline,
                "the turn for `{id}` never released the session"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The log, read back through a fresh store so no cache can hide a missing write.
    fn events(&self, id: &str) -> Vec<SessionEvent> {
        SessionStore::new(self.log_path(id))
            .read_all()
            .expect("the session log is readable")
    }

    /// The log as event types, which is the shape a replay and a client both see.
    fn types(&self, id: &str) -> Vec<&'static str> {
        self.events(id)
            .iter()
            .map(SessionEvent::type_name)
            .collect()
    }

    async fn state(&self, id: &str) -> Value {
        self.dispatch("state", json!({ "sessionId": id }))
            .await
            .expect("session.state answers for a session that exists")
    }

    /// The catalog's own record, which is what `session.history` reads from.
    async fn record(&self, id: &str) -> girder_core::SessionRecord {
        self.catalog
            .read(id)
            .await
            .unwrap_or_else(|| panic!("the catalog knows `{id}`"))
    }

    /// Subscribes to every event the runtime publishes, as a live client would.
    fn observe(&self) -> tokio::sync::mpsc::Receiver<ObservedEvent> {
        self.bus.observe(None)
    }

    /// Waits until the log holds `type_name`, tolerating a read that catches a frame mid-append.
    ///
    /// The read retries on a decode error rather than treating it as a failed assertion: the log is
    /// appended to while this polls, and a partial zstd frame is a read racing a write, not a bug.
    /// The final assertions all read strictly, so a genuinely corrupt log still fails the test.
    async fn wait_for_event(&self, id: &str, type_name: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let seen = SessionStore::new(self.log_path(id))
                .read_all()
                .is_ok_and(|events| events.iter().any(|event| event.type_name() == type_name));
            if seen {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the log for `{id}` never reached a `{type_name}` record"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// `(turn/start count, turn/end count)`, the pair that says whether a log stops inside a turn.
fn turn_boundaries(events: &[SessionEvent]) -> (usize, usize) {
    let starts = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::TurnStart { .. }))
        .count();
    let ends = events
        .iter()
        .filter(|event| matches!(event, SessionEvent::TurnEnd { .. }))
        .count();
    (starts, ends)
}

/// Waits for the runtime to announce that a turn ended, and returns that event.
///
/// A cancelled turn must still be announced: a renderer that only learns about turns from this event
/// would otherwise spin forever after a stop, which is the "silent stop" this file exists to rule
/// out.
async fn next_turn_end(observed: &mut tokio::sync::mpsc::Receiver<ObservedEvent>) -> AgentEvent {
    let announced = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = observed.recv().await {
            if let Ok(agent) = serde_json::from_value::<AgentEvent>(event.args) {
                if matches!(agent, AgentEvent::TurnEnded { .. }) {
                    return agent;
                }
            }
        }
        panic!("the bus closed without ever announcing a turn end");
    })
    .await;

    announced.expect("a cancelled turn must be announced as ended, not left hanging")
}

// ---- 1. the log records the interruption -------------------------------------------------------

#[tokio::test]
async fn a_cancelled_turn_leaves_one_open_turn_in_the_log() {
    // The property: a stop is a *recorded* event, not the absence of one. Cancelling mid-answer must
    // leave the turn opened and unclosed -- three records in a shape a replay reads as "stopped
    // inside turn 1" -- and the client must be told the turn ended. A cancel that wrote nothing at
    // all would leave a reader unable to tell it from a crash before the prompt.
    let harness = Harness::new("marker");
    harness.create("s").await;
    let mut observed = harness.observe();

    harness.prompt("s", "slow: talk forever").await;
    // Wait for the step, not for a wall-clock delay: the marker asserted below is exactly this
    // record, so waiting for it is what makes the test about cancellation rather than about timing.
    harness.wait_for_event("s", "step/start").await;
    assert!(
        harness.runtime.is_running("s"),
        "the turn must still be running, or this test cancels nothing"
    );
    assert!(
        harness.slow_started.load(Ordering::SeqCst) >= 1,
        "the endless provider must actually have been reached"
    );

    assert!(harness.cancel("s").await);
    harness.idle("s").await;

    // -- the log the cancellation left ---------------------------------------------------------
    let events = harness.events("s");
    assert_eq!(
        harness.types("s"),
        vec!["user/message", "turn/start", "step/start"],
        "a cancelled turn stops between records: the turn and its step are opened and never closed, \
         and no assistant message is invented from a partial answer"
    );
    assert_eq!(
        turn_boundaries(&events),
        (1, 0),
        "exactly one turn opened and none closed is the interruption marker this build writes"
    );
    assert_eq!(
        open_turn(&events),
        Some(1),
        "the turn still open is turn 1, which is what `session.state.openTurn` reports"
    );
    let digest = digest_session(&events);
    assert!(
        !digest.finished,
        "the digest must read as unfinished: a cancelled turn that looked finished would let a \
         resume skip the work the user was in the middle of"
    );
    assert_eq!(
        digest.requests,
        vec!["slow: talk forever".to_owned()],
        "the prompt itself survives the cancellation, because it was written before the loop started"
    );

    // -- and the client is told -----------------------------------------------------------------
    let ended = next_turn_end(&mut observed).await;
    let AgentEvent::TurnEnded { reason, .. } = ended else {
        panic!("the awaited event is a turn end");
    };
    let names_cancellation = match reason {
        // Either shape is accepted on purpose. `Aborted` is the honest one, and is what a fixed
        // runtime should send; today the runtime maps every loop error -- including
        // `LoopError::Cancelled` -- onto `Error`, so the message is what identifies a cancel. Pinning
        // the exact variant here would either lock in the defect or force a rewrite when it is fixed.
        FinishReason::Aborted => true,
        FinishReason::Error { message } => message.contains("cancel"),
        FinishReason::Stop | FinishReason::Length | FinishReason::ToolCalls => false,
    };
    assert!(
        names_cancellation,
        "the announced end must identify a cancellation; a stop reported as a plain success or a \
         truncation misleads every client that renders the reason"
    );
}

// ---- 2. cancelling twice is idempotent ---------------------------------------------------------

#[tokio::test]
async fn cancelling_twice_writes_one_marker_and_changes_nothing_else() {
    // The property: a second cancel is a no-op. Two interrupts usually arrive together -- a button
    // pressed twice, a stop racing a deadline -- and a second one that appended another marker, or
    // that panicked on a poisoned lock, would make the log depend on how many times a user clicked.
    let harness = Harness::new("idempotent");
    harness.create("s").await;

    harness.prompt("s", "slow: talk forever").await;
    harness.wait_for_event("s", "step/start").await;

    assert!(harness.cancel("s").await, "the first cancel is accepted");
    harness.idle("s").await;
    let after_first = harness.events("s");
    let state_after_first = harness.state("s").await;

    // The session has released its turn, so this second cancel has nothing to interrupt -- which is
    // the same code path a double click takes, one scheduler tick apart.
    assert!(harness.cancel("s").await, "the second cancel is accepted");
    let after_second = harness.events("s");
    let state_after_second = harness.state("s").await;

    assert_eq!(
        after_first, after_second,
        "a repeated cancel must not append a second marker: one interruption, one record"
    );
    assert_eq!(
        state_after_first, state_after_second,
        "the whole session state -- counts, digest, open turn, running flag -- must be identical \
         after a second cancel, not merely non-panicking"
    );
    assert_eq!(
        turn_boundaries(&after_second),
        (1, 0),
        "the log still holds a single open turn, not two"
    );
    assert_eq!(
        harness
            .types("s")
            .iter()
            .filter(|name| **name == "turn/start")
            .count(),
        1,
        "exactly one turn-start record, which is the count `openTurn` derives from"
    );
    assert_eq!(
        state_after_second["running"], false,
        "the session is idle after the deduplicated cancel"
    );
}

// ---- 3. a cancelled session is still readable --------------------------------------------------

#[tokio::test]
async fn a_cancelled_session_reads_back_with_every_event_it_wrote() {
    // The property: cancellation does not damage the log. A stop that left the file unreadable, or
    // truncated at the last record, would turn "stop the answer" into "lose the conversation" -- and
    // the events written before the stop are the only evidence of what the model was doing.
    let harness = Harness::new("readable");
    harness.create("s").await;

    harness.prompt("s", "slow: talk forever").await;
    harness.wait_for_event("s", "step/start").await;
    assert!(harness.cancel("s").await);
    harness.idle("s").await;

    // Read three ways: the store the runtime writes through, the free-function reader a tool uses,
    // and the event list a replay would fold. All three must agree.
    let store = SessionStore::new(harness.log_path("s"));
    let read: Vec<SessionEvent> = store.read_all().expect("a cancelled log is readable");
    let raw = read_session_file(harness.log_path("s")).expect("the file decodes after a cancel");
    assert_eq!(
        read, raw,
        "the store's cached read and a cold read of the same file must agree, or one of them is \
         hiding a write"
    );
    assert_eq!(
        read,
        harness.events("s"),
        "the log a client re-reads after a cancel is the log the runtime wrote"
    );

    assert!(
        read.iter().any(|event| matches!(
            event,
            SessionEvent::UserMessage { message } if message_text(message) == "slow: talk forever"
        )),
        "the prompt the user typed is still in the log, which is what makes the session resumable"
    );
    assert!(
        read.iter()
            .any(|event| matches!(event, SessionEvent::TurnStart { turn } if *turn == 1)),
        "the open turn is recorded with its number, so a reader can say which turn stopped"
    );
    assert!(
        matches!(read.last(), Some(SessionEvent::StepStart { .. })),
        "the last record is a complete `step/start`, not a torn tail: the cancel landed between \
         records rather than inside one"
    );
    assert!(
        read.len() >= 3,
        "the events written before the stop must survive it, and there are at least three"
    );

    // The catalog's verdict on the same file, which is what `session.history` and the session list
    // report. A torn tail would be *repairable* and a broken middle is not; neither may be the
    // result of a cancel, because the loop stops between appends.
    let record = harness.record("s").await;
    assert_eq!(
        record.integrity,
        girder_core::LogIntegrity::Readable { events: read.len() },
        "cancellation must leave a fully readable log, not a torn one: {read:?}"
    );
    assert!(
        !record.blank,
        "the session is not blank -- the prompt is recorded -- so a list view must not render it \
         as one a user never used"
    );
    assert_eq!(
        record.events, read,
        "the catalog's snapshot is the same log the store and the free reader returned"
    );
}

// ---- 4. cancelling one session does not stop another -------------------------------------------

#[tokio::test]
async fn cancelling_one_session_does_not_stop_a_concurrent_sibling() {
    // The property: cancellation is per session. The runtime keeps one token per running turn; a
    // cancel that reached the shared provider or the shared registry would stop every conversation
    // on the deployment, and "stop my answer" would silently become "stop everything".
    let harness = Harness::new("isolation");
    harness.create("doomed").await;
    harness.create("survivor").await;

    harness.prompt("doomed", "slow: talk forever").await;
    harness.wait_for_event("doomed", "step/start").await;

    // The survivor is deliberately slow enough to still be streaming when the other is cancelled;
    // an instant answer would finish before the cancel and prove nothing about concurrency.
    harness.prompt("survivor", "steady: keep going").await;
    harness.wait_for_event("survivor", "step/start").await;
    assert!(
        harness.runtime.is_running("survivor"),
        "the sibling must be mid-turn, or isolation is untested"
    );

    assert!(harness.cancel("doomed").await);
    assert!(
        harness.runtime.is_running("survivor"),
        "cancelling `doomed` must leave `survivor` running: one session's stop says nothing about \
         another's work"
    );

    harness.idle("doomed").await;
    harness.idle("survivor").await;

    // The cancelled one: opened and never closed.
    let doomed = harness.events("doomed");
    assert_eq!(
        turn_boundaries(&doomed),
        (1, 0),
        "the cancelled session's turn is still open"
    );
    assert_eq!(open_turn(&doomed), Some(1));

    // The survivor: a complete turn, with the model's answer intact.
    let survivor = harness.events("survivor");
    assert_eq!(
        turn_boundaries(&survivor),
        (1, 1),
        "the sibling's turn opened and closed normally, so the cancel did not reach it"
    );
    assert_eq!(
        open_turn(&survivor),
        None,
        "nothing is left open in the session that was allowed to finish"
    );
    let digest = digest_session(&survivor);
    assert!(
        digest.finished,
        "the survivor reached a boundary; a cancel that stopped it would leave this false"
    );
    assert_eq!(
        digest.last_answer.as_deref(),
        Some("0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,"),
        "the survivor's whole answer is recorded, chunk by chunk, not a prefix cut by the other \
         session's cancel"
    );
    assert!(
        harness.slow_started.load(Ordering::SeqCst) >= 1,
        "the endless provider ran for the cancelled session"
    );
}

// ---- 5. a parent cancel reaches the child -------------------------------------------------------

#[tokio::test]
async fn a_parent_cancel_marks_the_child_and_releases_its_work() {
    // The core propagation case, driven through the real cancellation machinery: a cancellation tree
    // whose child owns the token an agent loop is running under. Two phases are asserted separately
    // because they are two different guarantees:
    //
    //   1. `cancel()` *marks* the whole subtree synchronously -- the child reports `Parent`, not
    //      `UserRequest`, because the scope that stopped is not the scope that was asked to stop.
    //   2. `run_cleanups()` is what *releases* the work: the token is cancelled then, and only then.
    //      The design puts teardown in the owner's hands deliberately, so an owner that forgets it
    //      leaves its loop running under a cancelled scope -- which is the failure this ordering makes
    //      visible instead of hiding.
    let dir = Scratch(scratch("parent-child"));
    let root = Scope::root();
    let child = root.child();

    let work_token = CancellationToken::new();
    let released = {
        let token = work_token.clone();
        Box::new(move || token.cancel()) as Box<dyn FnOnce() + Send>
    };
    child.on_cancel(released);

    let cleaned = Arc::new(AtomicUsize::new(0));
    let counted = {
        let cleaned = Arc::clone(&cleaned);
        Box::new(move || {
            cleaned.fetch_add(1, Ordering::SeqCst);
        }) as Box<dyn FnOnce() + Send>
    };
    child.on_cancel(counted);

    // A real loop, writing a real zstd log, watching the token the child will cancel.
    let path = dir.0.join("child.jsonl.zst");
    let store = SessionStore::new(&path);
    store.touch().expect("the child log is created");
    let context = Arc::new(LoopContext::new(
        Arc::new(Scripted {
            slow_started: Arc::new(AtomicUsize::new(0)),
        }),
        Arc::new(ToolRegistry::new()) as Arc<dyn ToolRegistryView>,
        Arc::new(SessionStore::new(&path)) as Arc<dyn Session>,
        Arc::new(EventBus::new()),
        "test-model",
        AgentId::new("cancel-e2e-child").expect("a constant id is valid"),
    ));
    let mut stream = Box::pin(run_loop(context, "slow forever", work_token.clone()));

    // The parent is cancelled from another task once the loop has opened its turn, so the
    // cancellation lands on work that is genuinely in progress rather than on a stream that has not
    // started.
    let canceller = {
        let root = root.clone();
        let child = child.clone();
        let token = work_token.clone();
        let cleaned = Arc::clone(&cleaned);
        let path = path.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let opened = SessionStore::new(&path).read_all().is_ok_and(|events| {
                    events.iter().any(|event| event.type_name() == "step/start")
                });
                if opened {
                    break;
                }
                assert!(Instant::now() < deadline, "the child loop never started");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            root.cancel();

            // Phase 1: the whole subtree is marked, and says why.
            assert!(
                child.is_cancelled(),
                "a child must stop being live the moment its parent is cancelled"
            );
            assert_eq!(
                child.reason(),
                Some(CancelReason::Parent),
                "the child was not asked to stop, so its reason must name the parent rather than \
                 copy the parent's own cause"
            );
            assert!(
                !token.is_cancelled(),
                "cancel marks; it does not run teardown, so the in-flight token is still live here"
            );

            // Phase 2: teardown is the owner's call, and it runs once.
            run_cleanups(&root);
            assert!(
                token.is_cancelled(),
                "run_cleanups must release the work the cancelled scope owned"
            );
            assert_eq!(
                cleaned.load(Ordering::SeqCst),
                1,
                "each cleanup runs exactly once, which is what makes teardown safe to call twice"
            );
            run_cleanups(&root);
            assert_eq!(
                cleaned.load(Ordering::SeqCst),
                1,
                "a second run_cleanups must not run an action again"
            );
        })
    };

    let steps: Vec<Result<girder_core::LoopStep, LoopError>> =
        tokio::time::timeout(Duration::from_secs(5), stream.by_ref().collect())
            .await
            .expect("the child loop must stop once its scope is cancelled");
    canceller.await.expect("the cancelling task did not panic");

    assert!(
        matches!(steps.last(), Some(Err(LoopError::Cancelled))),
        "the loop must report a cancellation rather than a truncated success: {:?}",
        steps.last()
    );

    let events = store.read_all().expect("the child log is readable");
    assert_eq!(
        open_turn(&events),
        Some(1),
        "the child's log records the interruption the same way the parent's does"
    );
    assert!(
        !digest_session(&events).finished,
        "the child's digest must not claim a finished conversation"
    );
}

// ---- 6. cancelling a finished session changes nothing ------------------------------------------

#[tokio::test]
async fn cancelling_a_finished_session_leaves_it_exactly_as_it_was() {
    // The property: a cancel that finds nothing to cancel is a no-op, not a mutation. It is also the
    // race a client hits routinely -- a stop button pressed as the answer lands -- and the state it
    // reads back must describe the finished conversation, not a cancelled one.
    let harness = Harness::new("finished");
    harness.create("s").await;

    harness.prompt("s", "fast: hello").await;
    harness.idle("s").await;

    let before_events = harness.events("s");
    let before_state = harness.state("s").await;
    assert_eq!(
        before_state["interrupted"], false,
        "the finished log ends at a boundary, or this test is not about a completed session"
    );
    assert_eq!(
        before_state["digest"]["finished"], true,
        "the conversation is recorded as finished before the cancel"
    );

    assert!(
        harness.cancel("s").await,
        "cancel is acknowledged even when the turn it named has already ended"
    );
    assert!(
        harness.cancel("s").await,
        "and acknowledged again: a client retrying after a lost response must not see an error"
    );

    let after_events = harness.events("s");
    let after_state = harness.state("s").await;

    assert_eq!(
        before_events, after_events,
        "cancelling a session with no turn in flight must not append, rewrite, or drop a record"
    );
    assert_eq!(
        before_state, after_state,
        "every reported field -- counts, digest, timeline, interrupted, openTurn, running -- must be \
         unchanged, so the client's view survives a stray stop"
    );
    assert_eq!(
        after_state["running"], false,
        "nothing is running, which is exactly why the cancel changed nothing"
    );
    assert_eq!(
        after_state["openTurn"],
        Value::Null,
        "no turn is left open by a cancel that found a finished session"
    );
    assert_eq!(
        digest_session(&after_events).last_answer.as_deref(),
        Some("pong"),
        "the answer that was already produced is still the conversation's last word"
    );
    assert!(
        !harness.runtime.cancel("s"),
        "the runtime reports that no turn was running, which is the fact the acknowledgement hides"
    );
}
