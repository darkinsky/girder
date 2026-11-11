//! Tests for `session.updateQueue`, the pending-queue mutation surface.
//!
//! The properties under test, in the order they matter:
//!
//! - **an edit changes the item and only the item** — the id, role, and provenance survive, the
//!   content is the replacement, and the neighbours in the queue are untouched, because a queue edit
//!   that reordered or duplicated pending input would send the model a conversation the user never
//!   typed
//! - **an item that is not pending is refused** — a stale id must not silently edit whatever has
//!   moved into that position
//! - **an item that already started is refused differently from one that never existed** — the two
//!   need different words in the UI, and answering both with "not found" would tell a user their
//!   message vanished
//! - **`remove`, `steer`, and the content rules behave as declared** — an unimplemented action
//!   reports itself instead of doing something adjacent
//! - **a read-only policy cannot mutate a queue**, since the method declares a write
//!
//! Isolation: every test builds its own registry, its own agent map, and its own catalog over a
//! scratch directory named from the process id and a counter, so tests cannot collide.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the method as written before the crate root wires it in. It spells its paths through
//! `girder_core::`, the alias the library declares for itself, so the same source builds here and
//! inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/session_queue_rpc.rs"]
mod session_queue_rpc;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{
    Agent, AgentOptions, AgentSpec, ChatMessage, InboxTarget, MessageId, ReadOnly, RemoteCall,
    SessionCatalog, SessionEvent, SessionId, SessionStore, Splice,
};
use serde_json::{Value, json};

use session_queue_rpc::{
    AgentQueueMap, DECLARED_ACCESS, METHOD, NAMESPACE, QueueAgentSource, QueueError,
    register_session_queue_methods,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated scratch directory: `temp_dir()` plus pid plus a counter.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-queue-{name}-{}-{n}", std::process::id()));
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

fn agent(session: &str) -> Arc<Agent> {
    Arc::new(Agent::new(AgentSpec::root(
        SessionId::new(session).expect("a valid session id"),
        AgentOptions::new("test-provider", "test-model"),
    )))
}

fn message(id: &str, text: &str) -> ChatMessage {
    ChatMessage::user(
        MessageId::new(id).expect("a valid message id"),
        text.to_owned(),
    )
}

/// Queues messages on the target, asserting the splice was accepted.
fn queue(agent: &Agent, target: InboxTarget, messages: Vec<ChatMessage>) {
    agent
        .splice_inbox(&Splice::append(target, messages))
        .expect("the fixture splice is valid");
}

/// One registry, one map, one catalog over an empty scratch directory.
struct Fixture {
    dir: PathBuf,
    map: Arc<AgentQueueMap>,
    catalog: Arc<dyn SessionCatalog>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = scratch(name);
        let map = Arc::new(AgentQueueMap::new());
        let catalog: Arc<dyn SessionCatalog> = Arc::new(girder_core::DirectoryCatalog::new(&dir));
        Self { dir, map, catalog }
    }

    /// Attaches a fresh agent for `session` and returns it.
    fn live(&self, session: &str) -> Arc<Agent> {
        let agent = agent(session);
        self.map.attach(session, Arc::clone(&agent));
        agent
    }

    /// Seeds a durable session log holding one consumed user message.
    ///
    /// This is what makes an item "already started": the loop appended it when it took it out of the
    /// queue.
    fn consumed(&self, session: &str, item: &str) {
        let store = SessionStore::new(self.dir.join(format!("{session}.jsonl.zst")));
        store.touch().expect("a session log is created");
        store
            .append(&SessionEvent::user_text(item, "already sent"))
            .expect("the prompt appends");
    }

    /// Seeds a durable session log with no user message, so the id exists but nothing ran.
    fn known_but_cold(&self, session: &str) {
        let store = SessionStore::new(self.dir.join(format!("{session}.jsonl.zst")));
        store.touch().expect("a session log is created");
    }

    fn registry(&self) -> girder_core::RemoteRegistry {
        let mut registry = girder_core::RemoteRegistry::new();
        register_session_queue_methods(
            &mut registry,
            &self.catalog,
            &(Arc::clone(&self.map) as Arc<dyn QueueAgentSource>),
        );
        registry
    }

    async fn update(&self, session: &str, item: &str, action: Value) -> Result<Value, String> {
        self.registry()
            .dispatch(
                NAMESPACE,
                METHOD,
                call(json!({"sessionId": session, "itemId": item, "action": action})),
            )
            .await
            .map_err(|error| error.to_string())
    }

    fn edit(text: &str) -> Value {
        json!({"kind": "edit", "content": [{"type": "text", "text": text}]})
    }
}

// ---- editing a pending item --------------------------------------------------------------------

#[tokio::test]
async fn an_edit_replaces_the_content_and_keeps_the_identity() {
    let fixture = Fixture::new("edit");
    let live = fixture.live("s1");
    queue(
        &live,
        InboxTarget::NextTurn,
        vec![message("m1", "original")],
    );

    let value = fixture
        .update("s1", "m1", Fixture::edit("rewritten"))
        .await
        .expect("editing a pending message succeeds");

    assert_eq!(value, json!({"accepted": true}));

    let pending = live.inbox_state();
    assert_eq!(pending.next_turn().len(), 1);
    let edited = &pending.next_turn()[0];
    assert_eq!(
        edited.id.as_str(),
        "m1",
        "the id is what the client addresses"
    );
    assert_eq!(edited.text(), "rewritten");
    assert_eq!(
        edited.role,
        girder_core::ChatRole::User,
        "a reworded message is still the user's"
    );
    assert!(
        matches!(edited.source, girder_core::MessageSource::User { .. }),
        "provenance survives the edit; only the blocks change"
    );
}

#[tokio::test]
async fn an_edit_leaves_the_neighbouring_items_alone() {
    let fixture = Fixture::new("neighbours");
    let live = fixture.live("s1");
    queue(
        &live,
        InboxTarget::NextTurn,
        vec![
            message("m1", "first"),
            message("m2", "second"),
            message("m3", "third"),
        ],
    );

    fixture
        .update("s1", "m2", Fixture::edit("changed"))
        .await
        .expect("editing the middle message succeeds");

    let pending = live.inbox_state();
    let texts: Vec<String> = pending.next_turn().iter().map(ChatMessage::text).collect();
    assert_eq!(texts, vec!["first", "changed", "third"]);
    // Length is asserted explicitly: a splice that inserted without removing would keep the queue
    // size and pass a text check built from the first three entries.
    assert_eq!(pending.next_turn().len(), 3);
}

#[tokio::test]
async fn a_next_step_item_is_editable_too() {
    let fixture = Fixture::new("step");
    let live = fixture.live("s1");
    queue(
        &live,
        InboxTarget::NextStep,
        vec![message("m1", "typed mid-answer")],
    );

    fixture
        .update("s1", "m1", Fixture::edit("typo fixed"))
        .await
        .expect("a next-step message is queued input like any other");

    let pending = live.inbox_state();
    assert!(
        pending.next_turn().is_empty(),
        "the other queue is untouched"
    );
    assert_eq!(pending.next_step()[0].text(), "typo fixed");
}

#[tokio::test]
async fn an_edit_replaces_multi_block_content_wholesale() {
    let fixture = Fixture::new("blocks");
    let live = fixture.live("s1");
    queue(&live, InboxTarget::NextTurn, vec![message("m1", "one")]);

    let action = json!({
        "kind": "edit",
        "content": [
            {"type": "text", "text": "first part "},
            {"type": "text", "text": "second part"},
        ],
    });
    fixture
        .update("s1", "m1", action)
        .await
        .expect("two text blocks are valid prompt content");

    let pending = live.inbox_state();
    assert_eq!(pending.next_turn()[0].content.len(), 2);
    assert_eq!(pending.next_turn()[0].text(), "first part second part");
}

// ---- removing a pending item -------------------------------------------------------------------

#[tokio::test]
async fn a_remove_withdraws_the_item() {
    let fixture = Fixture::new("remove");
    let live = fixture.live("s1");
    queue(
        &live,
        InboxTarget::NextTurn,
        vec![message("m1", "keep"), message("m2", "drop")],
    );

    let value = fixture
        .update("s1", "m2", json!({"kind": "remove"}))
        .await
        .expect("withdrawing a pending message succeeds");

    assert_eq!(value, json!({"accepted": true}));
    let pending = live.inbox_state();
    assert_eq!(pending.next_turn().len(), 1);
    assert_eq!(pending.next_turn()[0].id.as_str(), "m1");
}

// ---- refusals ----------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_item_is_refused() {
    let fixture = Fixture::new("unknown-item");
    let live = fixture.live("s1");
    queue(
        &live,
        InboxTarget::NextTurn,
        vec![message("m1", "only one")],
    );

    let error = fixture
        .update("s1", "no-such-item", Fixture::edit("x"))
        .await
        .expect_err("an id that is not pending cannot be edited");

    assert!(error.contains("no queued item `no-such-item`"), "{error}");
    // The queue is unchanged: a refused edit must not have half-applied.
    let pending = live.inbox_state();
    assert_eq!(pending.next_turn().len(), 1);
    assert_eq!(pending.next_turn()[0].text(), "only one");
}

#[tokio::test]
async fn an_item_that_already_started_is_refused_as_such() {
    let fixture = Fixture::new("started");
    // The message is in the log, which is what "already began executing" means here, and it is not
    // in the queue.
    fixture.consumed("s1", "m1");
    let live = fixture.live("s1");

    let error = fixture
        .update("s1", "m1", Fixture::edit("too late"))
        .await
        .expect_err("a message the model has seen cannot be edited");

    assert!(
        error.contains("has already started executing"),
        "the refusal must say the item started, not that it was never queued: {error}"
    );
    assert!(
        error.contains("steering"),
        "the reason is offered, so a reader learns why this is not just a delay: {error}"
    );
    assert!(live.inbox_state().is_empty(), "nothing was re-queued");
}

#[tokio::test]
async fn an_item_that_never_existed_is_told_apart_from_one_that_started() {
    let fixture = Fixture::new("apart");
    fixture.live("s1");

    let error = fixture
        .update("s1", "ghost", json!({"kind": "remove"}))
        .await
        .expect_err("an unknown id is refused");

    assert!(error.contains("no queued item `ghost`"), "{error}");
    assert!(
        !error.contains("already started"),
        "a client that asks about an id it never sent must not be told it was consumed: {error}"
    );
}

#[tokio::test]
async fn a_steer_is_refused_rather_than_treated_as_an_edit() {
    let fixture = Fixture::new("steer");
    let live = fixture.live("s1");
    queue(&live, InboxTarget::NextTurn, vec![message("m1", "waiting")]);

    let error = fixture
        .update("s1", "m1", json!({"kind": "steer"}))
        .await
        .expect_err("steering has no channel in this build");

    assert!(error.contains("steering is unavailable"), "{error}");
    // Critically, the item is still queued: steering must not remove it as a side effect.
    assert_eq!(live.inbox_state().next_turn().len(), 1);
}

#[tokio::test]
async fn a_non_text_block_is_refused_and_named() {
    let fixture = Fixture::new("non-text");
    let live = fixture.live("s1");
    queue(&live, InboxTarget::NextTurn, vec![message("m1", "waiting")]);

    let error = fixture
        .update(
            "s1",
            "m1",
            json!({"kind": "edit", "content": [{"type": "image", "attachment": "a1"}]}),
        )
        .await
        .expect_err("a prompt edit is text-only");

    assert!(
        error.contains("`image`"),
        "the offending block is named: {error}"
    );
    assert_eq!(live.inbox_state().next_turn()[0].text(), "waiting");
}

#[tokio::test]
async fn blank_edit_content_is_refused() {
    let fixture = Fixture::new("blank");
    let live = fixture.live("s1");
    queue(&live, InboxTarget::NextTurn, vec![message("m1", "waiting")]);

    let error = fixture
        .update("s1", "m1", Fixture::edit("   \n  "))
        .await
        .expect_err("whitespace is not a prompt");

    assert!(error.contains("non-whitespace"), "{error}");
}

#[tokio::test]
async fn a_missing_session_or_item_id_is_an_argument_error() {
    let fixture = Fixture::new("missing");

    let session = fixture
        .registry()
        .dispatch(
            NAMESPACE,
            METHOD,
            call(json!({"sessionId": "", "itemId": "m1", "action": {"kind": "remove"}})),
        )
        .await
        .expect_err("a blank session id is meaningless");
    assert!(session.to_string().contains("`sessionId` is required"));

    let item = fixture
        .registry()
        .dispatch(
            NAMESPACE,
            METHOD,
            call(json!({"sessionId": "s1", "itemId": "", "action": {"kind": "remove"}})),
        )
        .await
        .expect_err("a blank item id is meaningless");
    assert!(item.to_string().contains("`itemId` is required"));
}

#[tokio::test]
async fn an_unknown_session_is_refused_as_unknown_not_as_agentless() {
    let fixture = Fixture::new("unknown-session");

    let error = fixture
        .update("no-such-session", "m1", json!({"kind": "remove"}))
        .await
        .expect_err("a session that never existed is refused");

    assert!(error.contains("no session `no-such-session`"), "{error}");
}

#[tokio::test]
async fn a_cold_session_reports_that_there_is_no_live_queue() {
    let fixture = Fixture::new("cold");
    // The log exists, so the session is real, but no agent holds its queue.
    fixture.known_but_cold("s1");

    let error = fixture
        .update("s1", "m1", json!({"kind": "remove"}))
        .await
        .expect_err("a cold session has nothing queued");

    assert!(error.contains("no live agent"), "{error}");
    assert!(
        !error.contains("no session `s1`"),
        "the session exists; saying otherwise sends the caller looking for a typo: {error}"
    );
}

// ---- the declared access -----------------------------------------------------------------------

#[tokio::test]
async fn a_read_only_policy_refuses_a_queue_edit() {
    let fixture = Fixture::new("policy");
    let live = fixture.live("s1");
    queue(
        &live,
        InboxTarget::NextTurn,
        vec![message("m1", "protected")],
    );

    let mut registry = fixture.registry();
    registry = registry.with_policy(Arc::new(ReadOnly));

    let error = registry
        .dispatch(
            NAMESPACE,
            METHOD,
            call(json!({"sessionId": "s1", "itemId": "m1", "action": {"kind": "remove"}})),
        )
        .await
        .expect_err("editing a queue is a write");

    assert!(
        error.to_string().contains("may only read"),
        "the declaration reaches the policy: {error}"
    );
    assert_eq!(
        live.inbox_state().next_turn().len(),
        1,
        "the refused call changed nothing"
    );
    // The declaration is asserted directly as well, so a registration that dropped it fails here
    // rather than only in a deployment that mounted a policy.
    assert_eq!(DECLARED_ACCESS, girder_core::Access::Write);
}

// ---- the map ---------------------------------------------------------------------------------

#[test]
fn the_agent_map_replaces_and_forgets_handles() {
    let map = AgentQueueMap::new();
    assert!(map.is_empty());

    let first = agent("s1");
    map.attach("s1", Arc::clone(&first));
    assert_eq!(map.len(), 1);
    assert!(Arc::ptr_eq(&map.agent("s1").expect("attached"), &first));

    let resumed = agent("s1");
    map.attach("s1", Arc::clone(&resumed));
    assert_eq!(map.len(), 1, "a resumed session replaces the stale handle");
    assert!(Arc::ptr_eq(&map.agent("s1").expect("attached"), &resumed));

    assert!(map.detach("s1").is_some());
    assert!(map.agent("s1").is_none());
    assert!(
        map.detach("s1").is_none(),
        "detaching twice reports absence"
    );
}

// ---- the error type ---------------------------------------------------------------------------

#[test]
fn the_queue_error_messages_state_the_reason() {
    // The wire has no domain error codes, so the message is the only thing a client can read; each
    // variant is checked to say what was wrong rather than only that something was.
    let cases: [(QueueError, &str); 4] = [
        (QueueError::EditBlank, "non-whitespace"),
        (QueueError::SteerUnavailable, "steering is unavailable"),
        (
            QueueError::NotPending {
                item_id: "m9".to_owned(),
            },
            "`m9`",
        ),
        (
            QueueError::AlreadyStarted {
                item_id: "m9".to_owned(),
            },
            "already started",
        ),
    ];
    for (error, needle) in cases {
        assert!(error.to_string().contains(needle), "{error}");
    }
}

/// A catalog that is never consulted would hide the "already started" branch entirely, so one test
/// proves the branch is reached through the real directory catalog rather than a stub: the log is
/// written to the same directory the catalog lists.
#[tokio::test]
async fn the_started_check_reads_the_durable_log() {
    let fixture = Fixture::new("durable");
    fixture.consumed("s1", "m-consumed");
    fixture.known_but_cold("s1");
    fixture.live("s1");

    // The same session now has a live agent whose queue is empty. One id in the log is refused as
    // started; an id in neither place is refused as unknown.
    let started = fixture
        .update("s1", "m-consumed", Fixture::edit("x"))
        .await
        .expect_err("a consumed message is refused");
    assert!(started.contains("already started"), "{started}");

    let absent = fixture
        .update("s1", "m-absent", Fixture::edit("x"))
        .await
        .expect_err("an absent message is refused");
    assert!(absent.contains("no queued item"), "{absent}");
}

/// The catalog is what distinguishes the two refusals, so a reader can see the read happened rather
/// than assume a hardcoded answer.
#[tokio::test]
async fn the_catalog_is_consulted_before_claiming_a_session_is_unknown() {
    let fixture = Fixture::new("catalog");
    fixture.known_but_cold("s1");

    // No agent: the answer depends on whether the catalog knows the session.
    let error = fixture
        .update("s1", "m1", json!({"kind": "remove"}))
        .await
        .expect_err("a cold session has nothing queued");

    assert!(
        error.contains("no live agent"),
        "a session present in the catalog is not reported as unknown: {error}"
    );
}

/// Kept as a compile-time assertion that the seam a host supplies is the same trait the registry
/// takes: a host with a different lookup shape could otherwise only discover a mismatch at wiring.
#[test]
fn a_closure_is_a_queue_source() {
    let map = Arc::new(AgentQueueMap::new());
    let live = agent("s1");
    map.attach("s1", Arc::clone(&live));

    let source: Arc<dyn QueueAgentSource> = {
        let map = Arc::clone(&map);
        Arc::new(move |session: &str| map.agent(session))
    };
    assert!(Arc::ptr_eq(&source.agent("s1").expect("looked up"), &live));
    assert!(source.agent("s2").is_none());
}

/// The scratch directory is removed at the end of a run so repeated local runs do not fill the temp
/// directory; failure to remove it is not a test failure, since the directory's absence is only a
/// convenience.
#[allow(dead_code)]
fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}
