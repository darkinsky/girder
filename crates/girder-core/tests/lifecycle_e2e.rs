//! End-to-end lifecycle tests: several modules driven through one another, on real files.
//!
//! Every other integration test in this crate exercises one seam. `tests/session_resume_rpc.rs`
//! builds a catalog and checks `resume`; `tests/session_manage.rs` checks `fork`; `tests/remote.rs`
//! checks `list` against a `FixedCatalog` that returns its records with `events` populated. Each of
//! those passes while the composition of all of them is broken, because a mock at the boundary
//! replaces exactly the behaviour the bugs live in. The two defects these tests found are of that
//! shape: `session.list` reads `record.events`, which `DirectoryCatalog::list` deliberately leaves
//! empty (`BUG-2`), and two prompts in one session both write `turn/start turn=1` (`BUG-3`).
//!
//! So these tests deliberately use the real [`DirectoryCatalog`], a real [`AgentRuntime`], real
//! zstd log files in a per-test temporary directory, and assert on *specific* values — event
//! sequences, counts, byte offsets, ids — rather than on `is_ok()`. A test that only asserts success
//! cannot tell a correct answer from a plausible one.
//!
//! # Offline provider
//!
//! [`EchoProvider`] is a real `LlmProvider` that streams a fixed four-chunk response. It is not a
//! mock of the runtime: the loop, the log writer, the turn accounting, and the export renderers all
//! run for real. It only removes the network.
//!
//! # What is asserted, and why each assertion matters
//!
//! See the comment above each block. The short version: the *sequence* of events is what a replay
//! reads, so a missing `turn/end` is a bug even when the text is right; `lastSeq` is the cursor a
//! client hands back, so an off-by-one duplicates or drops exactly one event on every reconnect;
//! the byte offset a repair cuts at is the difference between losing a torn write and losing a
//! conversation.
//!
//! # What this file found
//!
//! Four defects were found here and fixed: session ids collided within a millisecond and merged two conversations into one
//! log; every list row reported `blank` because `list` returns records with no events and the constructor reading events
//! was used instead of the one written for records; a second prompt reopened turn 1, so the log disagreed with the turn
//! number the client was told; and an archive was listed as an openable session.
//!
//! Each has a test now, and the tests are ordinary ones. They were written as `#[ignore]`d assertions of the contract
//! first, which is what made the defects visible as failures rather than as prose.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    // A lifecycle test is long because the chain is long, and splitting it into helpers would move
    // the assertions away from the sequence they are about.
    clippy::too_many_lines,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use girder_core::{
    AgentRuntime, ArchivePolicy, BoxFut, Ceiling, ChunkStream, DirectoryCatalog, EventBus,
    FinishReason, LiveTurns, LlmError, LlmProvider, LlmRequest, ProjectionRegistry, Recovery,
    RemoteCall, RemoteError, RemoteRegistry, RuntimeConfig, SessionCatalog, SessionEvent,
    SessionStore, StreamChunk, TokenUsage, ToolRegistry, ToolRegistryView, TurnEndReason,
    analyze_log, archive_session, diagnose_session, find_resume, read_session_file,
    register_session_resume_method, register_session_state_method_with, registry_with_runtime,
    repair_session, resume_is_safe,
};
use serde_json::{Value, json};

// ---- fixtures ----------------------------------------------------------------------------------

/// An isolated scratch directory: `temp_dir()`, this binary's process id, and a counter.
///
/// The counter is what makes two tests in this binary independent: a directory named from the
/// process id alone would be shared by every test that ran, and one test's log would appear in
/// another's listing.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-lifecycle-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory is created");
    dir
}

/// Removes a scratch directory when the test ends, so a failing run does not leak hundreds of them.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// A provider that answers with one text block, one usage record, and a normal stop.
///
/// The usage record is not decoration: it is what makes the loop write a `model/usage` event, which
/// is the only event carrying token counts. Without it `session.state.counts.input_tokens` would be
/// zero and the assertion on it would be vacuous.
struct EchoProvider {
    text: String,
    calls: Arc<AtomicUsize>,
}

impl EchoProvider {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl LlmProvider for EchoProvider {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let chunks = vec![
            StreamChunk::BlockStart {
                index: 0,
                block_type: "text".to_owned(),
            },
            StreamChunk::TextDelta {
                index: 0,
                text: self.text.clone(),
            },
            StreamChunk::Usage(TokenUsage {
                input: 11,
                output: 7,
                cached: 0,
            }),
            StreamChunk::Finish {
                reason: FinishReason::Stop,
            },
        ];
        Box::pin(async move { Ok(Box::pin(futures::stream::iter(chunks)) as ChunkStream) })
    }
}

/// A provider that keeps producing text until its token is cancelled, like a long answer.
///
/// Cancellation is observed *between chunks*, which is the loop's actual contract: the token is
/// checked per chunk, so a cancelled turn stops within one chunk rather than after the whole answer.
struct SlowProvider;

impl LlmProvider for SlowProvider {
    fn stream(
        &self,
        _request: LlmRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>> {
        let stream = async_stream::stream! {
            yield StreamChunk::BlockStart { index: 0, block_type: "text".to_owned() };
            let mut index = 0u64;
            loop {
                if cancel.is_cancelled() {
                    yield StreamChunk::Finish { reason: FinishReason::Aborted };
                    break;
                }
                yield StreamChunk::TextDelta { index: 0, text: format!("{index}") };
                index += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        Box::pin(async move { Ok(Box::pin(stream) as ChunkStream) })
    }
}

fn build_runtime(provider: Arc<dyn LlmProvider>) -> Arc<AgentRuntime> {
    AgentRuntime::new(RuntimeConfig {
        provider,
        tools: Arc::new(ToolRegistry::new()) as Arc<dyn ToolRegistryView>,
        bus: Arc::new(EventBus::new()),
        model: "test-model".to_owned(),
        // One turn per prompt: the conversation-loop behaviour (`max_turns > 1`, which continues on
        // queued input) is a different seam and would make the expected event sequence depend on it.
        max_turns: 1,
        token_budget: Ceiling::Unlimited,
    })
}

/// Everything a lifecycle test needs, built over one scratch directory.
struct Harness {
    /// Removes the directory on drop; the path itself is reached through `Harness::dir`.
    scratch: Scratch,
    catalog: Arc<dyn SessionCatalog>,
    runtime: Arc<AgentRuntime>,
    registry: RemoteRegistry,
}

impl Harness {
    fn new(name: &str, provider: Arc<dyn LlmProvider>) -> Self {
        let scratch = Scratch(scratch(name));
        let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&scratch.0));
        let runtime = build_runtime(provider);
        let mut registry = registry_with_runtime(Arc::clone(&catalog), Arc::clone(&runtime));

        // `state` and `resume` are registered by their own modules rather than by
        // `registry_with_runtime`, which is the composition a server performs. Registering them here
        // is the point of the test: a method that exists but is never wired is a method a client
        // cannot reach.
        let projections = Arc::new(ProjectionRegistry::new());
        let runtime_for_turns = Arc::clone(&runtime);
        let turns: Arc<dyn LiveTurns> = Arc::new(move |id: &str| runtime_for_turns.is_running(id));
        register_session_state_method_with(&mut registry, &catalog, &projections, &turns);
        register_session_resume_method(&mut registry, &catalog, &runtime);

        Self {
            scratch,
            catalog,
            runtime,
            registry,
        }
    }

    fn dir(&self) -> &Path {
        &self.scratch.0
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.dir().join(format!("{id}.jsonl.zst"))
    }

    async fn dispatch(&self, method: &str, args: Value) -> Result<Value, RemoteError> {
        self.registry.dispatch("session", method, call(args)).await
    }

    async fn create(&self, id: &str) -> Value {
        self.dispatch("create", json!({"sessionId": id}))
            .await
            .expect("a session is created")
    }

    async fn prompt(&self, id: &str, text: &str) -> Value {
        self.dispatch("prompt", json!({"sessionId": id, "text": text}))
            .await
            .expect("the prompt is accepted")
    }

    /// Blocks until no turn is in flight for `id`, or panics after five seconds.
    ///
    /// A fixed `sleep` would either be too short on a loaded machine or waste seconds on an idle
    /// one; waiting on the runtime's own answer is the condition the test actually needs.
    async fn idle(&self, id: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.runtime.is_running(id) {
            assert!(
                Instant::now() < deadline,
                "the turn for `{id}` never finished"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn events(&self, id: &str) -> Vec<SessionEvent> {
        read_session_file(self.log_path(id)).expect("the session log is readable")
    }

    /// The event types in log order, which is the shape a replay sees.
    fn types(&self, id: &str) -> Vec<&'static str> {
        self.events(id)
            .iter()
            .map(SessionEvent::type_name)
            .collect()
    }
}

/// The markdown `session.export` returns by default.
async fn export_markdown(harness: &Harness, id: &str) -> Value {
    harness
        .dispatch("export", json!({"sessionId": id}))
        .await
        .expect("markdown exports")
}

// ---- 1. the whole lifecycle ----------------------------------------------------------------------

#[tokio::test]
async fn a_full_lifecycle_produces_the_same_conversation_in_every_view() {
    // The property: create, prompt, state, page, export (twice), and fork all describe ONE
    // conversation. Each view is built by a different module from the same log, so a disagreement
    // between any two of them is a client rendering two different sessions.
    let harness = Harness::new("lifecycle", Arc::new(EchoProvider::new("pong")));

    // A prompt carrying markup, so the HTML assertion below is about escaping rather than about
    // whether the text survived.
    let prompt = "hello <script>alert(1)</script>";

    let created = harness.create("s1").await;
    assert_eq!(
        created["sessionId"], "s1",
        "create echoes the id it was given, so a client can address the session it just made"
    );
    assert_eq!(
        harness.events("s1").len(),
        0,
        "a created session is an empty log, not a placeholder turn"
    );

    let accepted = harness.prompt("s1", prompt).await;
    assert_eq!(accepted["accepted"], true);
    assert_eq!(
        accepted["turn"], 1,
        "the first prompt is turn 1, counted as user messages rather than as log records"
    );
    harness.idle("s1").await;

    // -- the log: the source of truth, and what every other view folds --------------------------
    assert_eq!(
        harness.types("s1"),
        vec![
            "user/message",
            "session/title",
            "turn/start",
            "step/start",
            "assistant/message",
            "model/usage",
            "step/end",
            "turn/end",
        ],
        "the turn is opened and closed around exactly one step; a missing `turn/end` would make \
         every later read report the log as interrupted"
    );
    // The prompt is the log's first record and the answer is inside the turn it opened: the order is
    // what a replay shows, and `user/message` before `turn/start` is this build's documented
    // deviation from upstream (a prompt that reached the server must survive a crash before the
    // model answered, so it is durable first).
    assert_eq!(
        harness.events("s1")[0],
        SessionEvent::user_text("msg-1", prompt),
        "the recorded prompt is the text that was sent, under the id the runtime assigned"
    );

    // -- session.state: one read, so its counts cannot disagree with each other ---------------
    let state = harness
        .dispatch("state", json!({"sessionId": "s1"}))
        .await
        .expect("state reads a known session");

    assert_eq!(state["events"], 8, "eight durable events");
    assert_eq!(
        state["lastSeq"], 8,
        "sequence numbers are one-based positions, so the last of eight events is 8 -- the number a \
         client hands back to `follow`"
    );
    assert_eq!(state["interrupted"], false, "the turn closed");
    assert_eq!(
        state["running"], false,
        "the turn finished before this read"
    );
    assert_eq!(
        state["lastText"], "pong",
        "the last thing the model said, read from the log rather than from a live buffer"
    );
    assert_eq!(state["lastTurn"], 1);
    assert_eq!(state["openTurn"], Value::Null, "no turn is open");
    assert_eq!(
        state["title"], prompt,
        "the opening prompt supplies the default title until a user renames it"
    );
    assert_eq!(
        state["counts"]["turns"], 1,
        "one closed turn, counted by distinct turn numbers"
    );
    assert_eq!(state["counts"]["steps"], 1);
    assert_eq!(state["counts"]["user_messages"], 1);
    assert_eq!(state["counts"]["assistant_messages"], 1);
    assert_eq!(
        state["counts"]["input_tokens"], 11,
        "the usage chunk the provider sent is what the log recorded"
    );
    assert_eq!(state["counts"]["output_tokens"], 7);
    assert_eq!(state["counts"]["failed_tool_calls"], 0);
    assert_eq!(state["counts"]["unanswered_tool_calls"], 0);
    assert_eq!(
        state["digest"]["requests"],
        json!([prompt]),
        "the digest lists what the person asked, verbatim"
    );
    assert_eq!(state["digest"]["last_answer"], "pong");
    assert_eq!(state["digest"]["finished"], true);
    assert_eq!(
        state["forkedFrom"],
        Value::Null,
        "an un-forked id has no parent in it"
    );
    assert_eq!(
        state["timeline"]
            .as_array()
            .expect("a timeline array")
            .len(),
        1,
        "one line per turn"
    );
    assert_eq!(state["timeline"][0]["turn"], 1);
    assert_eq!(
        state["timeline"][0]["summary"], prompt,
        "the segment is summarised by the prompt that opened it"
    );

    // -- session.page: the cursor and the window arithmetic a client scrolls with --------------
    let page = harness
        .dispatch(
            "page",
            json!({"address": {"kind": "session", "sessionId": "s1"}}),
        )
        .await
        .expect("a page reads a known session");
    assert_eq!(
        page["records"].as_array().expect("records").len(),
        8,
        "the whole log fits in one page"
    );
    assert_eq!(page["cursor"], 8, "the cursor is the last record returned");
    assert_eq!(page["hasMore"], false, "nothing remains before this page");
    assert_eq!(
        page["records"][0]["type"], "event",
        "every record is tagged, so a page can later carry another kind without breaking clients"
    );
    assert_eq!(
        page["records"][0]["event"]["type"], "user/message",
        "records are chronological, and the prompt is durable before the turn opened"
    );

    // A two-record window ends at the same cursor and says more remains: the cut is what a client
    // pins, and `hasMore` describes what is *before* it.
    let window = harness
        .dispatch(
            "page",
            json!({"address": {"kind": "session", "sessionId": "s1"}, "maxMessages": 2}),
        )
        .await
        .expect("a window reads");
    assert_eq!(window["records"].as_array().expect("records").len(), 2);
    assert_eq!(
        window["cursor"], 8,
        "the window still ends at the log's end"
    );
    assert_eq!(window["hasMore"], true, "six records precede the window");

    // -- session.export, markdown: what a person pastes into a ticket ---------------------------
    let markdown = export_markdown(&harness, "s1").await;
    assert_eq!(
        markdown["format"], "markdown",
        "the reported format is the one produced"
    );
    assert_eq!(
        markdown["truncated"], false,
        "a short conversation is not cut"
    );
    let body = markdown["content"].as_str().expect("markdown content");
    assert_eq!(
        usize::try_from(markdown["bytes"].as_u64().expect("a byte count")).expect("fits"),
        body.len(),
        "the reported byte count describes what was actually returned"
    );
    // Headings in order, with the prompt's markup left as text: Markdown is a transcript for a
    // human, and escaping it would show the reader entities instead of what was said.
    let user_at = body.find("### User").expect("a User heading");
    let turn_at = body.find("## Turn 1").expect("a Turn heading");
    let assistant_at = body.find("### Assistant").expect("an Assistant heading");
    assert!(
        user_at < turn_at && turn_at < assistant_at,
        "headings follow log order (user, turn, assistant), got: {body}"
    );
    assert!(
        body.contains(prompt),
        "the prompt appears verbatim in a Markdown export: {body}"
    );
    assert!(body.contains("pong"), "the answer appears: {body}");
    assert!(
        body.starts_with("# Session s1"),
        "the transcript names the session it came from: {body}"
    );

    // -- session.export, html: one self-contained page ------------------------------------------
    let page_value = harness
        .dispatch("export", json!({"sessionId": "s1", "format": "html"}))
        .await
        .expect("html exports");
    assert_eq!(page_value["format"], "html");
    let html = page_value["content"].as_str().expect("html content");
    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "a shareable page is a whole document"
    );
    assert!(
        html.contains("default-src 'none'"),
        "the page carries a CSP that denies every fetch, as a second layer behind escaping"
    );
    assert!(
        !html.contains("<script>"),
        "session content must never reach the page as markup"
    );
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "the prompt's tags are escaped, so a shared link cannot execute what a session contains"
    );

    // -- session.fork: a prefix copy that is a whole conversation --------------------------------
    let forked = harness
        .dispatch("fork", json!({"sessionId": "s1"}))
        .await
        .expect("a completed session forks");
    let child = forked["sessionId"].as_str().expect("a forked id");
    assert_ne!(child, "s1", "a fork is a different session");
    assert!(
        child.starts_with("s1-fork-"),
        "the id records the parent, which is the only place the relationship is written: {child}"
    );

    let parent_events = harness.events("s1");
    let child_events = harness.events(child);
    assert_eq!(
        child_events, parent_events,
        "a fork at the last completed turn copies the parent's events exactly: same values, same \
         order, because a copy that dropped or reordered one would be a different conversation"
    );

    // The prefix property is what makes a fork usable as a branch point: the child can be prompted
    // and its history is already a conversation a provider accepts.
    let point = find_resume(&child_events);
    assert_eq!(
        point.at,
        child_events.len(),
        "the fork ends on a turn boundary, so nothing has to be replayed"
    );
    assert_eq!(point.reason, girder_core::ResumeReason::TurnBoundary);
    assert!(
        resume_is_safe(&child_events, &point),
        "the copied prefix pairs every tool call, so resuming there is safe"
    );

    // The child is independently addressable: the catalog lists both, and each reads its own log.
    let listed = harness
        .dispatch("list", json!({}))
        .await
        .expect("list succeeds");
    let ids: Vec<&str> = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["sessionId"].as_str())
        .collect();
    assert!(ids.contains(&"s1"), "the parent is listed: {ids:?}");
    assert!(ids.contains(&child), "the fork is listed: {ids:?}");

    // The parent was not disturbed by being forked.
    assert_eq!(
        harness.events("s1"),
        parent_events,
        "forking is a read of the source"
    );
    assert_eq!(
        harness.events("s1").len(),
        8,
        "the source still holds its own eight events"
    );
}

// ---- 2. cancel, then resume ---------------------------------------------------------------------

#[tokio::test]
async fn a_cancelled_turn_is_reported_as_interrupted_and_nothing_needs_replaying() {
    // The property: cancelling lands between steps, the log says so, and the position a restart
    // continues from is one that does not replay what already happened. The three questions a
    // reconnecting client asks are `interrupted`, `lastSeq`, and "where do I resume".
    let harness = Harness::new("cancel", Arc::new(SlowProvider));

    harness.create("c").await;
    harness.prompt("c", "talk forever").await;

    // Wait until the turn is genuinely streaming, so the cancel lands mid-answer rather than before
    // it started. Cancelling too early would leave the same log shape by a different route and the
    // test would not be about cancellation at all.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        harness.runtime.is_running("c"),
        "the turn must still be running, or this test is not cancelling anything"
    );

    let cancel = harness
        .dispatch("cancel", json!({"sessionId": "c"}))
        .await
        .expect("cancel is accepted");
    assert_eq!(
        cancel["accepted"], true,
        "cancel acknowledges the request; the turn lands at the next step boundary"
    );
    harness.idle("c").await;

    // -- the log the cancellation left ---------------------------------------------------------
    assert_eq!(
        harness.types("c"),
        vec!["user/message", "session/title", "turn/start", "step/start"],
        "cancellation stops the loop between records, so the turn is opened and the step begun but \
         never closed -- this shape is the whole reason `interrupted` exists"
    );
    let events = harness.events("c");
    assert_eq!(events.len(), 4);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::TurnEnd { .. })),
        "a cancelled turn writes no closer; this build refuses to record an outcome that did not \
         happen"
    );

    // -- session.resume: the reconnection answer -----------------------------------------------
    let resumed = harness
        .dispatch("resume", json!({"sessionId": "c"}))
        .await
        .expect("a cancelled session resumes");
    assert_eq!(resumed["sessionId"], "c");
    assert_eq!(resumed["resumed"], true);
    assert_eq!(resumed["events"], 4, "four durable events");
    assert_eq!(
        resumed["lastSeq"], 4,
        "the cursor is the last event, so `afterSeq: 4` is the exact continuation -- nothing \
         repeated, nothing lost"
    );
    assert_eq!(
        resumed["running"], false,
        "the turn released the session, so a client may prompt again"
    );
    assert_eq!(
        resumed["interrupted"], true,
        "a `turn/start` after the last `turn/end` is a turn the process did not finish"
    );

    // -- where execution continues -------------------------------------------------------------
    // The log holds no half-written tool exchange, so there is nothing to redo: the position is the
    // END of the log rather than the turn boundary, and resuming there re-runs nothing. This is the
    // distinction `find_resume` exists to make -- a boundary is where a *turn* completed, while a
    // resume point is where the next event may be written.
    let point = find_resume(&events);
    assert_eq!(
        point.reason,
        girder_core::ResumeReason::CleanCallPair,
        "no boundary and no unanswered call, so the log is kept whole"
    );
    assert_eq!(
        point.at,
        events.len(),
        "the resume point is the end of the log: every recorded event has been handled"
    );
    assert_eq!(point.turn, 1, "the open turn is the one being continued");
    assert!(
        resume_is_safe(&events, &point),
        "the prefix through the resume point is a conversation a provider accepts"
    );

    // -- the boundary case, for contrast --------------------------------------------------------
    // The same question asked of a log that ended on a completed turn: the position is one past the
    // `turn/end`, which is the only position safe by construction.
    let completed = Harness::new("cancel-completed", Arc::new(EchoProvider::new("done")));
    completed.create("d").await;
    completed.prompt("d", "finish this").await;
    completed.idle("d").await;
    let closed = completed.events("d");
    let last_end = closed
        .iter()
        .rposition(|event| matches!(event, SessionEvent::TurnEnd { .. }))
        .expect("a completed turn wrote its closer");
    let point = find_resume(&closed);
    assert_eq!(
        point.reason,
        girder_core::ResumeReason::TurnBoundary,
        "a finished turn is the boundary a restart resumes from"
    );
    assert_eq!(
        point.at,
        last_end + 1,
        "the resume point is the position AFTER the last `turn/end`, not the boundary itself"
    );
    assert_eq!(
        point.at,
        closed.len(),
        "and for a log that ends on its boundary, that position is the end of the log"
    );
    assert_eq!(point.turn, 1, "it names the turn that closed");

    let resumed = completed
        .dispatch("resume", json!({"sessionId": "d"}))
        .await
        .expect("a completed session resumes");
    assert_eq!(
        resumed["interrupted"], false,
        "the same flag is false here, so the two logs are distinguishable by it"
    );
}

// ---- 3. two sessions created together -----------------------------------------------------------

#[tokio::test]
async fn two_sessions_created_together_are_independent_and_individually_readable() {
    // The property: concurrent creates do not interfere. Two conversations that share a log are one
    // conversation, which is why the id and the file behind each one are both asserted.
    let harness = Harness::new("concurrent", Arc::new(EchoProvider::new("pong")));

    // Created together rather than one after another: a create that raced would show up here and
    // nowhere in a sequential test.
    let (first, second) = tokio::join!(
        harness.dispatch("create", json!({"sessionId": "same-second-a"})),
        harness.dispatch("create", json!({"sessionId": "same-second-b"})),
    );
    let first = first.expect("the first create succeeds")["sessionId"]
        .as_str()
        .expect("an id")
        .to_owned();
    let second = second.expect("the second create succeeds")["sessionId"]
        .as_str()
        .expect("an id")
        .to_owned();

    assert_ne!(first, second, "two sessions cannot share one identity");
    assert_eq!(first, "same-second-a");
    assert_eq!(second, "same-second-b");
    assert!(
        harness.log_path(&first).is_file() && harness.log_path(&second).is_file(),
        "each session has its own log file"
    );

    // Each is written to and read back independently: the ids address different logs.
    harness.prompt(&first, "belongs to A").await;
    harness.idle(&first).await;
    harness.prompt(&second, "belongs to B").await;
    harness.idle(&second).await;

    let prompts = |id: &str| -> Vec<String> {
        harness
            .events(id)
            .into_iter()
            .filter_map(|event| match event {
                SessionEvent::UserMessage { message } => Some(message.text()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        prompts(&first),
        vec!["belongs to A".to_owned()],
        "A's log holds A's prompt and nothing else"
    );
    assert_eq!(
        prompts(&second),
        vec!["belongs to B".to_owned()],
        "B's log holds B's prompt, so neither create wrote into the other's file"
    );

    // Both are listed, which is what makes them addressable at all.
    let listed = harness
        .dispatch("list", json!({}))
        .await
        .expect("list succeeds");
    let ids: Vec<&str> = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["sessionId"].as_str())
        .collect();
    assert_eq!(
        ids.len(),
        2,
        "exactly the two sessions, no phantom rows: {ids:?}"
    );
    assert!(ids.contains(&first.as_str()), "{ids:?}");
    assert!(ids.contains(&second.as_str()), "{ids:?}");
}

#[tokio::test]
async fn two_sessions_created_in_the_same_millisecond_get_distinct_ids() {
    // The contract a client relies on when it omits an id: `session.create` makes a NEW session.
    // It does not, so a user who opens two chats at once loses one of them -- the second prompt is
    // appended to the first conversation, and no error is reported anywhere.
    let harness = Harness::new("minted", Arc::new(EchoProvider::new("pong")));

    let mut ids = Vec::new();
    for _ in 0..3 {
        let value = harness
            .dispatch("create", json!({}))
            .await
            .expect("a create with no id mints one");
        ids.push(value["sessionId"].as_str().expect("an id").to_owned());
    }

    let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "three creates in one millisecond minted {} distinct ids out of three: {ids:?}",
        unique.len()
    );

    // And the merged-conversation symptom, which is what a user sees.
    let a = harness.catalog.open(&ids[0]).expect("opens");
    a.append(SessionEvent::user_text("m1", "conversation A"))
        .await
        .expect("appends");
    let b = harness.catalog.open(&ids[1]).expect("opens");
    b.append(SessionEvent::user_text("m2", "conversation B"))
        .await
        .expect("appends");
    assert!(
        !harness.events(&ids[0])
            .iter()
            .any(|event| matches!(event, SessionEvent::UserMessage { message } if message.text() == "conversation B")),
        "one session's log holds the other's message"
    );
}

// ---- 4. archive --------------------------------------------------------------------------------

#[tokio::test]
async fn archiving_shrinks_the_log_while_every_event_stays_accounted_for() {
    // The property: archiving bounds the live log without losing anything. "Smaller" alone would
    // pass for a deletion, so the events are counted on both sides of the split.
    let harness = Harness::new("archive", Arc::new(EchoProvider::new("pong")));
    let path = harness.log_path("s1");
    let store = SessionStore::new(&path);
    store.touch().expect("the log is created");
    // Three complete turns: twelve events, and every one of them is inside a turn boundary.
    for turn in 1..=3u64 {
        store
            .append(&SessionEvent::TurnStart { turn })
            .expect("appends");
        store
            .append(&SessionEvent::user_text(
                &format!("m{turn}"),
                format!("body {turn}"),
            ))
            .expect("appends");
        store
            .append(&SessionEvent::assistant_text(
                &format!("a{turn}"),
                "hi",
                "test",
            ))
            .expect("appends");
        store
            .append(&SessionEvent::TurnEnd {
                turn,
                reason: TurnEndReason::Completed,
            })
            .expect("appends");
    }

    let before_bytes = std::fs::metadata(&path).expect("the log exists").len();
    let before_events = harness.events("s1");
    assert_eq!(before_events.len(), 12);

    let outcome = archive_session(
        &path,
        // Trigger above six, keep four: `keep_last` must be smaller than `max_events`, and a policy
        // where they overlap is refused rather than silently doing nothing.
        &ArchivePolicy {
            max_events: 6,
            keep_last: 4,
        },
    )
    .expect("the policy is satisfiable")
    .expect("a twelve-event log is above the trigger");

    let after_bytes = std::fs::metadata(&path)
        .expect("the log still exists")
        .len();
    let kept = harness.events("s1");
    let archived = read_session_file(&outcome.archive).expect("the archive is a readable log");

    assert!(
        after_bytes < before_bytes,
        "the live log shrank: {before_bytes} -> {after_bytes} bytes"
    );
    assert_eq!(
        outcome.kept,
        kept.len(),
        "the outcome reports the live log's size, and it matches what was read back"
    );
    assert_eq!(outcome.archived, archived.len());
    assert_eq!(
        kept.len() + archived.len(),
        before_events.len(),
        "the two files partition the original log: {} + {} = 12. An archive that dropped or \
         duplicated an event would fail here",
        kept.len(),
        archived.len()
    );
    assert_eq!(
        kept,
        before_events[8..],
        "the live log keeps the most recent events, byte for byte: a re-serialised event is not the \
         event that happened"
    );
    assert_eq!(archived, before_events[..8], "the archive holds the prefix");

    // The live log still ends on a turn boundary, so it is still a conversation a provider accepts
    // and a restart can continue from.
    assert_eq!(
        kept.last().map(SessionEvent::type_name),
        Some("turn/end"),
        "the split lands on a boundary because the policy's numbers happen to; the reader below is \
         what proves the log is still usable"
    );
    let point = find_resume(&kept);
    assert_eq!(point.reason, girder_core::ResumeReason::TurnBoundary);
    assert_eq!(point.at, kept.len());
    assert!(resume_is_safe(&kept, &point));

    // The archive's name carries the session id and an unambiguous stamp, so a directory scan can
    // attribute it and two archives never overwrite each other.
    let archive_name = outcome
        .archive
        .file_name()
        .expect("a file name")
        .to_string_lossy()
        .into_owned();
    assert!(
        archive_name.starts_with("s1.archive-"),
        "the archive is named after its session: {archive_name}"
    );
    assert!(
        archive_name.ends_with(".jsonl.zst"),
        "and is an ordinary log the existing reader understands: {archive_name}"
    );

    // The analysis view agrees with what the reader returned: one scan, so a summary cannot
    // disagree with its own breakdown.
    let summary = analyze_log(&path).expect("the log is analysable");
    assert_eq!(summary.events, kept.len());
    assert_eq!(summary.bytes_on_disk, after_bytes);
    assert_eq!(
        summary.by_type.get("turn/start").copied(),
        Some(1),
        "the kept suffix holds the start of the last turn and no other: {:?}",
        summary.by_type
    );

    // And the session is still readable through the RPC surface after the cut.
    let state = harness
        .dispatch("state", json!({"sessionId": "s1"}))
        .await
        .expect("an archived session is still readable");
    assert_eq!(state["events"], 4);
    assert_eq!(state["lastSeq"], 4);
}

#[tokio::test]
async fn an_archive_is_not_listed_as_a_session() {
    // `session_archive`'s docs promise the archive sits "beside" the log and that session discovery
    // will not mistake the evidence for a log. The evidence file (`.torn-N`) honours that; the
    // archive does not, because it keeps the `.jsonl.zst` suffix.
    let harness = Harness::new("archive-list", Arc::new(EchoProvider::new("pong")));
    let path = harness.log_path("s1");
    let store = SessionStore::new(&path);
    store.touch().expect("created");
    store
        .append(&SessionEvent::TurnStart { turn: 1 })
        .expect("appends");
    store
        .append(&SessionEvent::user_text("m1", "hi"))
        .expect("appends");
    store
        .append(&SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .expect("appends");

    archive_session(
        &path,
        &ArchivePolicy {
            max_events: 2,
            keep_last: 1,
        },
    )
    .expect("satisfiable")
    .expect("above the trigger");

    let listed = harness
        .dispatch("list", json!({}))
        .await
        .expect("list succeeds");
    let ids: Vec<&str> = listed["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|item| item["sessionId"].as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["s1"],
        "the directory holds one session and one archive, and only the session is listed: {ids:?}"
    );
}

// ---- 5. a damaged log ---------------------------------------------------------------------------

#[tokio::test]
async fn a_torn_tail_is_diagnosed_and_repaired_until_the_log_reads_again() {
    // The property: damage from an interrupted append is recoverable, and the repair removes exactly
    // the bytes that were never fully written. Cutting one byte too far loses a conversation, so the
    // offset is asserted as a number rather than "some cut".
    let harness = Harness::new("torn", Arc::new(EchoProvider::new("pong")));
    let path = harness.log_path("s1");
    let store = SessionStore::new(&path);
    store.touch().expect("created");

    // A reference log holding the first three events, whose length IS the offset a repair must cut
    // to: frames are written one per append, so the first three frames of a four-event log are
    // byte-identical to a log that only ever had three events.
    let reference = harness.dir().join("reference.jsonl.zst");
    let reference_store = SessionStore::new(&reference);
    reference_store.touch().expect("created");
    for index in 1..=3u64 {
        let event = SessionEvent::user_text(&format!("m{index}"), format!("body {index}"));
        store.append(&event).expect("appends");
        reference_store.append(&event).expect("appends");
    }
    // The fourth event is written only to the log under test.
    store
        .append(&SessionEvent::user_text("m4", "body 4"))
        .expect("appends");

    let clean_len = std::fs::metadata(&path).expect("exists").len();
    let intact = harness.events("s1");
    assert_eq!(intact.len(), 4);

    // A process killed mid-`write_all` leaves a PREFIX of the final frame. Cut eight bytes off the
    // file: the frames before it are complete, because each was a write that finished.
    let damaged =
        &std::fs::read(&path).expect("reads")[..usize::try_from(clean_len).expect("small") - 8];
    std::fs::write(&path, damaged).expect("the damage is written");

    // First, the failure that makes this worth diagnosing: the log is now unreadable *entirely*,
    // including the three events that are perfectly intact.
    let error = read_session_file(&path).expect_err("a truncated frame breaks every reader");
    assert!(
        error.to_string().contains("incomplete frame"),
        "the reader names the compression failure rather than silently returning a short log: {error}"
    );

    let diagnosis = diagnose_session(&path).expect("a damaged log is diagnosable");
    assert_eq!(
        diagnosis.readable, 3,
        "three events decode, counted by parsing them rather than by counting lines"
    );
    assert!(
        diagnosis.torn_tail,
        "the file ENDS inside a frame, which is what an interrupted append leaves"
    );
    let expected_cut = std::fs::metadata(&reference).expect("exists").len();
    assert_eq!(
        diagnosis.torn_at,
        Some(expected_cut),
        "the damage starts exactly where the fourth frame began -- the length of a log holding the \
         first three"
    );
    assert_eq!(
        diagnosis.action,
        Recovery::TruncateTo {
            bytes: expected_cut
        },
        "and the recommended repair is to cut there and nowhere earlier"
    );

    repair_session(&path, &diagnosis).expect("the repair applies");

    let repaired = harness.events("s1");
    assert_eq!(
        repaired.len(),
        3,
        "the log reads again, holding the events that were fully written"
    );
    assert_eq!(
        repaired,
        intact[..3],
        "the surviving events are unchanged, not re-encoded"
    );
    assert_eq!(
        std::fs::metadata(&path).expect("exists").len(),
        expected_cut,
        "the file is now exactly the reference log"
    );

    // The damaged bytes are kept: they are the only record of what went wrong, and the operation
    // that fixes the log is the one that would destroy it.
    let evidence = harness.dir().join("s1.jsonl.zst.torn-0");
    assert!(
        evidence.is_file(),
        "the repair keeps the original beside the log as evidence"
    );
    let kept = std::fs::read(&evidence).expect("the evidence is readable");
    assert_eq!(
        kept.len(),
        usize::try_from(clean_len).expect("small") - 8,
        "the evidence is the file as it was, including the torn bytes"
    );
    assert_eq!(kept, damaged, "byte for byte");
    assert!(
        !evidence.to_string_lossy().ends_with(".jsonl.zst"),
        "the evidence does not carry a log's suffix, so session discovery cannot mistake it for one"
    );

    // A repair is convergent: running it again on a healthy log does nothing.
    let healthy = diagnose_session(&path).expect("diagnosable");
    assert_eq!(healthy.action, Recovery::Healthy);
    assert_eq!(healthy.torn_at, None);
    repair_session(&path, &healthy).expect("a healthy log needs no repair");
}

#[tokio::test]
async fn a_tear_with_readable_frames_after_it_is_refused_rather_than_cut() {
    // The property: a repair never deletes an event. This build appends onto a torn tail on the next
    // run, so the file can become `frames, torn prefix, frames`; the frames AFTER the tear are
    // perfectly readable, and cutting at the tear would destroy them. Refusing is the only answer
    // that cannot lose data.
    let harness = Harness::new("mid-tear", Arc::new(EchoProvider::new("pong")));
    let path = harness.log_path("s2");
    let store = SessionStore::new(&path);
    store.touch().expect("created");

    // Learn the frame boundaries by writing the events one at a time and measuring.
    let boundary = harness.dir().join("three.jsonl.zst");
    let boundary_store = SessionStore::new(&boundary);
    boundary_store.touch().expect("created");
    for index in 1..=3u64 {
        let event = SessionEvent::user_text(&format!("m{index}"), format!("body {index}"));
        store.append(&event).expect("appends");
        boundary_store.append(&event).expect("appends");
    }
    let third_frame_end = std::fs::metadata(&boundary).expect("exists").len();
    store
        .append(&SessionEvent::user_text("m4", "body 4"))
        .expect("appends");
    let four = std::fs::read(&path).expect("reads");
    let third_end = usize::try_from(third_frame_end).expect("small");

    // `frames through 3`, then half of the fourth frame's bytes, then the fourth frame in full --
    // exactly the shape a second run produces after appending past an interrupted write.
    let torn_prefix = &four[third_end..third_end + (four.len() - third_end) / 2];
    let mut damaged = four[..third_end].to_vec();
    damaged.extend_from_slice(torn_prefix);
    damaged.extend_from_slice(&four[third_end..]);
    std::fs::write(&path, &damaged).expect("the damage is written");

    let diagnosis = diagnose_session(&path).expect("diagnosable");
    assert_eq!(
        diagnosis.readable, 3,
        "the frames before the tear still decode, and the ones after it are not counted as \
         readable because the walk stops at the tear"
    );
    assert!(
        !diagnosis.torn_tail,
        "the file does not END inside a frame: its last record is complete, and the problem is that \
         the tear cannot be cut away"
    );
    assert_eq!(
        diagnosis.torn_at,
        Some(third_frame_end),
        "the first unreadable record begins where the torn prefix was spliced in"
    );
    match &diagnosis.action {
        Recovery::Unusable { reason } => {
            assert!(
                reason.contains("a readable frame starts at byte"),
                "the reason says what was found, so an operator can act: {reason}"
            );
            assert!(
                reason.contains("would discard readable events"),
                "and says why it will not act: {reason}"
            );
        }
        other => panic!("expected UNUSABLE, got {other:?}"),
    }

    let before = std::fs::read(&path).expect("reads");
    let error = repair_session(&path, &diagnosis).expect_err("a tear it cannot cut is refused");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::InvalidData,
        "the refusal is a data problem, not an I/O one: {error}"
    );
    assert!(error.to_string().contains("cannot be repaired"), "{error}");
    assert_eq!(
        std::fs::read(&path).expect("reads"),
        before,
        "nothing was written: refusing is what keeps the readable events after the tear"
    );
}

// ---- 6. defects this suite found, as executable statements of the contract -----------------------

#[tokio::test]
async fn a_session_with_messages_is_listed_as_not_blank() {
    // A client uses `blank` to decide whether to open a session by replaying it or to show an empty
    // composer. Reporting `true` for a conversation with eight events makes it look unused.
    //
    // The existing test for this (`tests/remote.rs::list_marks_a_session_without_messages_as_blank`)
    // passes, because its `FixedCatalog` returns records WITH `events` populated -- a mock at exactly
    // the boundary the real catalog differs at.
    let harness = Harness::new("blank", Arc::new(EchoProvider::new("pong")));
    harness.create("talked").await;
    harness.prompt("talked", "hello").await;
    harness.idle("talked").await;

    let read = harness
        .catalog
        .read("talked")
        .await
        .expect("the session reads");
    assert_eq!(read.events.len(), 8);
    assert!(
        !read.blank,
        "read() decides `blank` from the events it decoded"
    );

    let listed = harness
        .dispatch("list", json!({}))
        .await
        .expect("list succeeds");
    let row = &listed["items"].as_array().expect("items")[0];
    assert_eq!(row["sessionId"], "talked");
    assert_eq!(
        row["blank"], false,
        "the list row must agree with `read` about the same session"
    );
}

#[tokio::test]
async fn a_second_prompt_advances_the_turn_number_in_the_log() {
    // Turn numbers are how every projection identifies a turn: `SessionStats::turns` counts distinct
    // numbers, `session.state.timeline` has one segment per turn, and `session.resume` reports the
    // last one. Two prompts sharing turn 1 collapse the conversation into a single turn everywhere.
    let harness = Harness::new("turns", Arc::new(EchoProvider::new("pong")));
    harness.create("s").await;

    let first = harness.prompt("s", "one").await;
    harness.idle("s").await;
    let second = harness.prompt("s", "two").await;
    harness.idle("s").await;

    assert_eq!(first["turn"], 1);
    assert_eq!(
        second["turn"], 2,
        "the second prompt is acknowledged as turn 2"
    );

    let starts: Vec<u64> = harness
        .events("s")
        .into_iter()
        .filter_map(|event| match event {
            SessionEvent::TurnStart { turn } => Some(turn),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        vec![1, 2],
        "and the log agrees with the acknowledgement, rather than reopening turn 1"
    );

    let state = harness
        .dispatch("state", json!({"sessionId": "s"}))
        .await
        .expect("state reads");
    assert_eq!(
        state["counts"]["turns"], 2,
        "two turns happened, so the counts say two"
    );
    assert_eq!(
        state["timeline"].as_array().expect("a timeline").len(),
        2,
        "and the timeline has a segment per turn rather than two segments claiming the same number"
    );
}
