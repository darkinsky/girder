//! Tests for the Remote method surface.
//!
//! The point of these tests is the distinction between three failures a client must be able to tell
//! apart: a typo (`UNKNOWN_METHOD`), a gap in this build (`NOT_IMPLEMENTED`), and bad arguments
//! (`INVALID_ARGUMENTS`). Collapsing them into one error would make a half-implemented client look
//! broken instead of incomplete.

use std::sync::Arc;

use girder_core::{
    DirectoryCatalog, FinishReason, RemoteCall, RemoteError, RemoteRegistry, SessionCatalog,
    SessionEvent, SessionRecord, SessionSummary, default_registry, register_session_methods,
};
use serde_json::json;

// ---- a catalog stub -----------------------------------------------------------------------------

struct FixedCatalog(Vec<SessionRecord>);

impl SessionCatalog for FixedCatalog {
    fn list(&self) -> girder_core::BoxFut<Vec<SessionRecord>> {
        let records = self.0.clone();
        Box::pin(async move { records })
    }

    fn read(&self, id: &str) -> girder_core::BoxFut<Option<SessionRecord>> {
        let found = self.0.iter().find(|record| record.id == id).cloned();
        Box::pin(async move { found })
    }

    fn create(&self, _id: &str) -> girder_core::BoxFut<Result<SessionRecord, RemoteError>> {
        Box::pin(async {
            Err(RemoteError::Failed(
                "FixedCatalog does not create".to_owned(),
            ))
        })
    }
}

fn record(id: &str, updated_at: u64, has_message: bool) -> SessionRecord {
    let mut events = vec![SessionEvent::TurnStart { turn: 1 }];
    if has_message {
        events.push(SessionEvent::user_text("msg-1", "hello".to_owned()));
    }
    SessionRecord {
        id: id.to_owned(),
        updated_at,
        blank: !has_message,
        events,
    }
}

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test-peer".to_owned(),
    }
}

// ---- session.list -------------------------------------------------------------------------------

#[tokio::test]
async fn list_returns_one_row_per_session() {
    let catalog = Arc::new(FixedCatalog(vec![
        record("s1", 100, true),
        record("s2", 200, false),
    ]));
    let registry = default_registry(catalog);

    let value = registry
        .dispatch("session", "list", call(json!({})))
        .await
        .expect("list must succeed");

    let items = value["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["sessionId"], "s1");
}

#[tokio::test]
async fn list_marks_a_session_without_messages_as_blank() {
    let catalog = Arc::new(FixedCatalog(vec![
        record("with-message", 1, true),
        record("blank", 2, false),
    ]));
    let registry = default_registry(catalog);

    let value = registry
        .dispatch("session", "list", call(json!({})))
        .await
        .unwrap();
    let items = value["items"].as_array().unwrap();
    // Ordering belongs to the catalog; this test is about the blank flag, so rows are looked up by
    // id rather than by position.
    let by_id = |id: &str| {
        items
            .iter()
            .find(|item| item["sessionId"] == id)
            .unwrap_or_else(|| panic!("row {id} missing"))
    };

    assert_eq!(by_id("blank")["blank"], true);
    assert_eq!(by_id("with-message")["blank"], false);
}

#[tokio::test]
async fn list_accepts_a_null_body() {
    // Upstream's `session.list` takes an empty request; a client may omit the body entirely.
    let registry = default_registry(Arc::new(FixedCatalog(vec![])));

    let value = registry
        .dispatch("session", "list", call(serde_json::Value::Null))
        .await
        .expect("a null body must be accepted");

    assert_eq!(value["items"], json!([]));
}

#[tokio::test]
async fn an_empty_catalog_lists_nothing() {
    let registry = default_registry(Arc::new(FixedCatalog(vec![])));

    let value = registry
        .dispatch("session", "list", call(json!({})))
        .await
        .unwrap();

    assert_eq!(value["items"], json!([]));
}

#[tokio::test]
async fn an_empty_catalog_is_ordered_by_activity() {
    let catalog = Arc::new(FixedCatalog(vec![
        record("old", 10, true),
        record("new", 30, true),
        record("mid", 20, true),
    ]));
    let registry = default_registry(catalog);

    let value = registry
        .dispatch("session", "list", call(json!({})))
        .await
        .unwrap();
    let ids: Vec<&str> = value["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["sessionId"].as_str().unwrap())
        .collect();

    // The catalog sorts; the method preserves its order rather than re-sorting, so one place owns
    // ordering.
    assert_eq!(ids, vec!["old", "new", "mid"]);
}

// ---- three distinct failures --------------------------------------------------------------------

#[tokio::test]
async fn a_typo_reports_unknown_method() {
    let registry = default_registry(Arc::new(FixedCatalog(vec![])));

    let error = registry
        .dispatch("session", "lst", call(json!({})))
        .await
        .expect_err("a typo must not resolve");

    assert!(
        matches!(error, RemoteError::UnknownMethod { .. }),
        "{error:?}"
    );
    assert_eq!(error.code(), "UNKNOWN_METHOD");
}

#[tokio::test]
async fn a_declared_gap_reports_not_implemented() {
    // The distinction that makes a half-implemented client legible: `search` exists upstream, so a
    // client author should see a gap, not a typo.
    let registry = default_registry(Arc::new(FixedCatalog(vec![])));

    let error = registry
        .dispatch("session", "search", call(json!({"query": "x"})))
        .await
        .expect_err("search is declared but absent");

    assert!(
        matches!(error, RemoteError::NotImplemented { .. }),
        "{error:?}"
    );
    assert_eq!(error.code(), "NOT_IMPLEMENTED");
    assert!(error.to_string().contains("session.search"), "{error}");
}

#[tokio::test]
async fn an_unknown_namespace_reports_unknown_method() {
    let registry = default_registry(Arc::new(FixedCatalog(vec![])));

    let error = registry
        .dispatch("workspace", "list", call(json!({})))
        .await
        .expect_err("an unimplemented namespace must not resolve");

    assert_eq!(error.code(), "UNKNOWN_METHOD");
}

#[tokio::test]
async fn bad_arguments_name_the_method() {
    let registry = default_registry(Arc::new(FixedCatalog(vec![])));

    let error = registry
        .dispatch("session", "prompt", call(json!({"sessionId": 42})))
        .await
        .expect_err("a wrong type must be refused");

    let RemoteError::InvalidArguments {
        namespace, method, ..
    } = &error
    else {
        panic!("expected InvalidArguments, got {error:?}");
    };
    assert_eq!(namespace, "session");
    assert_eq!(method, "prompt");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
}

// ---- session.prompt -----------------------------------------------------------------------------

#[tokio::test]
async fn prompt_accepts_a_well_formed_request() {
    let registry = default_registry(Arc::new(FixedCatalog(vec![])));

    let value = registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "s1", "text": "hello"})),
        )
        .await
        .expect("a valid prompt must be accepted");

    assert_eq!(value["accepted"], true);
    assert_eq!(value["turn"], 1);
}

#[tokio::test]
async fn prompt_refuses_empty_text() {
    let registry = default_registry(Arc::new(FixedCatalog(vec![])));

    let error = registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "s1", "text": "   "})),
        )
        .await
        .expect_err("whitespace is not a prompt");

    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(error.to_string().contains("text"));
}

#[tokio::test]
async fn prompt_requires_a_session_id() {
    let registry = default_registry(Arc::new(FixedCatalog(vec![])));

    let error = registry
        .dispatch("session", "prompt", call(json!({"text": "hello"})))
        .await
        .expect_err("a prompt without a session is ambiguous");

    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(error.to_string().contains("sessionId"));
}

// ---- the registry itself ------------------------------------------------------------------------

#[test]
fn registering_a_method_replaces_the_previous_one() {
    let mut registry = RemoteRegistry::new();
    registry.register("ns", "m", |_| Box::pin(async { Ok(json!(1)) }));
    registry.register("ns", "m", |_| Box::pin(async { Ok(json!(2)) }));

    assert_eq!(registry.len(), 1, "one name, one method");
}

#[test]
fn names_lists_what_is_registered() {
    let mut registry = RemoteRegistry::new();
    registry.register("session", "list", |_| Box::pin(async { Ok(json!(null)) }));
    registry.register("session", "prompt", |_| Box::pin(async { Ok(json!(null)) }));

    let mut names = registry.names();
    names.sort();
    assert_eq!(
        names,
        vec![
            ("session".to_owned(), "list".to_owned()),
            ("session".to_owned(), "prompt".to_owned())
        ]
    );
}

#[test]
fn the_default_registry_implements_the_two_methods_a_list_view_needs() {
    let registry = default_registry(Arc::new(FixedCatalog(vec![])));
    let names = registry.names();

    assert!(names.contains(&("session".to_owned(), "list".to_owned())));
    assert!(names.contains(&("session".to_owned(), "prompt".to_owned())));
}

#[test]
fn registering_only_session_methods_leaves_other_namespaces_absent() {
    let mut registry = RemoteRegistry::new();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(FixedCatalog(vec![]));
    register_session_methods(&mut registry, &catalog);

    assert!(
        !registry.names().iter().any(|(ns, _)| ns == "workspace"),
        "this build implements no workspace method"
    );
}

// ---- the directory catalog ----------------------------------------------------------------------

#[tokio::test]
async fn a_missing_directory_lists_nothing() {
    // A fresh install has no sessions; the client should render an empty list, not an error.
    let catalog = DirectoryCatalog::new("/nonexistent/girder/sessions");

    let records = catalog.list().await;

    assert!(records.is_empty());
}

#[tokio::test]
async fn a_directory_of_logs_lists_one_row_per_file() {
    let dir = std::env::temp_dir().join(format!("girder-catalog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for id in ["alpha", "beta"] {
        let store = girder_core::SessionStore::new(dir.join(format!("{id}.jsonl.zst")));
        store.append(&SessionEvent::TurnStart { turn: 1 }).unwrap();
        store
            .append(&SessionEvent::user_text("msg-1", "hi".to_owned()))
            .unwrap();
    }
    // A non-log file must be ignored rather than listed as a session.
    std::fs::write(dir.join("notes.txt"), "not a session").unwrap();

    let catalog = DirectoryCatalog::new(&dir);
    let records = catalog.list().await;

    assert_eq!(records.len(), 2, "only .zst logs are sessions");
    let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"alpha"), "{ids:?}");
    assert!(ids.contains(&"beta"), "{ids:?}");
    // `list` deliberately does not carry events: decoding every event of every session to decide
    // `blank` is O(sessions x events) for one bit. It reads until the first user message instead,
    // so the flag is right and the payload is empty.
    assert!(
        records.iter().all(|r| r.events.is_empty()),
        "list must not decode whole logs"
    );
    assert!(
        records.iter().all(|r| !r.blank),
        "both fixtures have a user message, so neither is blank"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_summary_reflects_whether_the_session_has_a_message() {
    let with =
        SessionSummary::from_events("s", &[SessionEvent::user_text("msg-1", "x".to_owned())], 5);
    assert!(!with.blank);

    let without = SessionSummary::from_events("s", &[SessionEvent::TurnStart { turn: 1 }], 5);
    assert!(without.blank);
}

#[test]
fn a_summary_is_not_attached_to_a_live_agent() {
    // This build keeps no agent registry, so claiming otherwise would make a client render a
    // "running" indicator that can never update.
    let summary = SessionSummary::from_events(
        "s",
        &[SessionEvent::TurnEnd {
            turn: 1,
            reason: girder_core::TurnEndReason::Completed,
        }],
        1,
    );
    assert!(!summary.agent_available);
    assert!(!summary.running);
}

#[test]
fn a_summary_omits_unknown_optional_fields() {
    // A client that distinguishes "absent" from "null" would mis-render a nulled cwd.
    let summary = SessionSummary::from_events("s", &[], 1);
    let json = serde_json::to_value(&summary).unwrap();

    assert!(
        !json.as_object().unwrap().contains_key("cwd"),
        "an unknown cwd must be omitted: {json}"
    );
}

// ---- session.create -----------------------------------------------------------------------------

/// A catalog with no sessions that records what was created.
struct EmptyCatalog {
    created: std::sync::Mutex<Vec<String>>,
}

impl EmptyCatalog {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            created: std::sync::Mutex::new(Vec::new()),
        })
    }
}

impl SessionCatalog for EmptyCatalog {
    fn list(&self) -> girder_core::BoxFut<Vec<SessionRecord>> {
        Box::pin(async { Vec::new() })
    }

    fn read(&self, _id: &str) -> girder_core::BoxFut<Option<SessionRecord>> {
        Box::pin(async { None })
    }

    fn create(&self, id: &str) -> girder_core::BoxFut<Result<SessionRecord, RemoteError>> {
        self.created.lock().unwrap().push(id.to_owned());
        let id = id.to_owned();
        Box::pin(async move {
            Ok(SessionRecord {
                id,
                updated_at: 1,
                blank: true,
                events: vec![SessionEvent::TurnStart { turn: 1 }],
            })
        })
    }
}

#[tokio::test]
async fn create_returns_the_requested_id() {
    let registry = default_registry(EmptyCatalog::new());

    let value = registry
        .dispatch(
            "session",
            "create",
            call(json!({"sessionId": "new-session"})),
        )
        .await
        .expect("create must succeed");

    assert_eq!(value["sessionId"], "new-session");
}

#[tokio::test]
async fn create_mints_an_id_when_none_is_given() {
    let registry = default_registry(EmptyCatalog::new());

    let value = registry
        .dispatch("session", "create", call(json!({})))
        .await
        .expect("create must succeed");

    let id = value["sessionId"].as_str().unwrap();
    assert!(!id.is_empty(), "a minted id must be non-empty");
    assert!(id.starts_with("session-"), "got {id}");
}

#[tokio::test]
async fn create_is_idempotent_per_id() {
    // A client that re-sends create must not fork history; the catalog decides reuse, and the
    // method must pass the same id through rather than minting a second one.
    let catalog = EmptyCatalog::new();
    let registry = default_registry(Arc::clone(&catalog) as Arc<dyn SessionCatalog>);

    registry
        .dispatch("session", "create", call(json!({"sessionId": "same"})))
        .await
        .unwrap();
    registry
        .dispatch("session", "create", call(json!({"sessionId": "same"})))
        .await
        .unwrap();

    let created = catalog.created.lock().unwrap().clone();
    assert_eq!(
        created,
        vec!["same", "same"],
        "the id is passed through unchanged"
    );
}

#[tokio::test]
async fn create_echoes_the_requested_preset() {
    let registry = default_registry(EmptyCatalog::new());

    let value = registry
        .dispatch(
            "session",
            "create",
            call(json!({"sessionId": "s", "agentPreset": "coding"})),
        )
        .await
        .unwrap();

    assert_eq!(value["agentPreset"], "coding");
}

#[tokio::test]
async fn create_on_a_directory_catalog_writes_a_log() {
    let dir = std::env::temp_dir().join(format!("girder-create-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let registry = default_registry(catalog);

    registry
        .dispatch("session", "create", call(json!({"sessionId": "fresh"})))
        .await
        .expect("create must write a log");

    let path = dir.join("fresh.jsonl.zst");
    assert!(path.is_file(), "the session must exist on disk");
    // It must already be visible to `list`, or a client would create and then not see it.
    let listed = registry
        .dispatch("session", "list", call(json!({})))
        .await
        .unwrap();
    assert_eq!(listed["items"][0]["sessionId"], "fresh");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn create_twice_on_a_directory_reuses_the_existing_log() {
    let dir = std::env::temp_dir().join(format!("girder-create2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let registry = default_registry(catalog);

    registry
        .dispatch("session", "create", call(json!({"sessionId": "reuse"})))
        .await
        .unwrap();
    registry
        .dispatch("session", "create", call(json!({"sessionId": "reuse"})))
        .await
        .unwrap();

    let events = girder_core::read_session_file(dir.join("reuse.jsonl.zst")).unwrap();
    // A created session holds no events: it has not been prompted, so it has no turn. A placeholder
    // event here made a fresh session look like it already had one.
    assert!(
        events.is_empty(),
        "a create must not record a turn: {events:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- session.page / session.events --------------------------------------------------------------

fn catalog_with_events(id: &str, count: usize) -> Arc<dyn SessionCatalog> {
    let events: Vec<SessionEvent> = (1..=count)
        .map(|turn| SessionEvent::TurnStart {
            turn: u64::try_from(turn).unwrap(),
        })
        .collect();
    Arc::new(FixedCatalog(vec![SessionRecord {
        id: id.to_owned(),
        updated_at: 1,
        blank: false,
        events,
    }]))
}

#[tokio::test]
async fn page_returns_every_record_by_default() {
    let registry = default_registry(catalog_with_events("s", 5));

    let value = registry
        .dispatch(
            "session",
            "page",
            call(json!({"address": {"kind": "session", "sessionId": "s"}})),
        )
        .await
        .unwrap();

    assert_eq!(value["records"].as_array().unwrap().len(), 5);
    assert_eq!(value["hasMore"], false);
    assert_eq!(value["cursor"], 5);
}

#[tokio::test]
async fn page_honours_the_message_budget_from_the_end() {
    // A history view opens at the newest records, so the budget applies backwards from the cut.
    let registry = default_registry(catalog_with_events("s", 10));

    let value = registry
        .dispatch(
            "session",
            "page",
            call(json!({"address": {"kind": "session", "sessionId": "s"}, "maxMessages": 3})),
        )
        .await
        .unwrap();

    let records = value["records"].as_array().unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(value["hasMore"], true, "older records remain");
    assert_eq!(value["cursor"], 10, "the cut is still the log end");
}

#[tokio::test]
async fn page_before_a_sequence_excludes_that_record() {
    let registry = default_registry(catalog_with_events("s", 5));

    let value = registry
        .dispatch(
            "session",
            "page",
            call(json!({"address": {"kind": "session", "sessionId": "s"}, "beforeSeq": 3})),
        )
        .await
        .unwrap();

    let records = value["records"].as_array().unwrap();
    assert_eq!(records.len(), 2, "records 1 and 2 precede 3");
    assert_eq!(value["cursor"], 2);
}

#[tokio::test]
async fn page_through_a_sequence_stops_there() {
    let registry = default_registry(catalog_with_events("s", 5));

    let value = registry
        .dispatch(
            "session",
            "page",
            call(json!({"address": {"kind": "session", "sessionId": "s"}, "throughSeq": 2})),
        )
        .await
        .unwrap();

    assert_eq!(value["records"].as_array().unwrap().len(), 2);
    assert_eq!(value["cursor"], 2);
}

#[tokio::test]
async fn page_past_the_end_clamps_rather_than_failing() {
    // A client pinning a sequence beyond a log another host truncated should get what exists.
    let registry = default_registry(catalog_with_events("s", 3));

    let value = registry
        .dispatch(
            "session",
            "page",
            call(json!({"address": {"kind": "session", "sessionId": "s"}, "throughSeq": 999})),
        )
        .await
        .unwrap();

    assert_eq!(value["records"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn page_of_an_unknown_session_is_empty_not_an_error() {
    // A session deleted by another host should render as nothing, not as a failure banner.
    let registry = default_registry(Arc::new(FixedCatalog(vec![])));

    let value = registry
        .dispatch(
            "session",
            "page",
            call(json!({"address": {"kind": "session", "sessionId": "missing"}})),
        )
        .await
        .expect("an unknown session is an empty page");

    assert_eq!(value["records"], json!([]));
    assert_eq!(value["hasMore"], false);
    assert_eq!(value["cursor"], 0);
}

#[tokio::test]
async fn events_is_the_same_read_as_page() {
    // Upstream keeps both names because one is deprecated but still called.
    let registry = default_registry(catalog_with_events("s", 4));
    let request = json!({"address": {"kind": "session", "sessionId": "s"}, "maxMessages": 2});

    let from_page = registry
        .dispatch("session", "page", call(request.clone()))
        .await
        .unwrap();
    let from_events = registry
        .dispatch("session", "events", call(request))
        .await
        .unwrap();

    assert_eq!(from_page, from_events);
}

#[tokio::test]
async fn page_rejects_a_subagent_address() {
    // This build has no subagent registry, so reading one by parent/child id would read the wrong
    // log. Refusing is better than returning an empty page that looks like an empty session.
    let registry = default_registry(catalog_with_events("s", 3));

    let error = registry
        .dispatch(
            "session",
            "page",
            call(json!({"address": {"kind": "subagent", "sessionId": "s"}})),
        )
        .await
        .expect_err("an unsupported address kind must be refused");

    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(error.to_string().contains("subagent"));
}

#[tokio::test]
async fn page_requires_a_session_id() {
    let registry = default_registry(catalog_with_events("s", 3));

    let error = registry
        .dispatch(
            "session",
            "page",
            call(json!({"address": {"kind": "session"}})),
        )
        .await
        .expect_err("an address without an id cannot select a log");

    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(error.to_string().contains("sessionId"));
}

#[tokio::test]
async fn page_of_an_empty_log_is_empty() {
    let registry = default_registry(Arc::new(FixedCatalog(vec![SessionRecord {
        id: "s".to_owned(),
        updated_at: 1,
        blank: false,
        events: Vec::new(),
    }])));

    let value = registry
        .dispatch(
            "session",
            "page",
            call(json!({"address": {"kind": "session", "sessionId": "s"}})),
        )
        .await
        .unwrap();

    assert_eq!(value["records"], json!([]));
    assert_eq!(value["cursor"], 0);
}

// ---- prompt actually runs a turn ----------------------------------------------------------------

use girder_core::{
    AgentRuntime, EventBus, LlmError, LlmProvider, LlmRequest, RuntimeConfig, StreamChunk,
    ToolRegistry, ToolRegistryView,
};

/// A provider that answers with a fixed text, recording that it was called.
struct EchoProvider {
    calls: std::sync::atomic::AtomicUsize,
}

impl LlmProvider for EchoProvider {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, LlmError>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        Box::pin(
            async move { Ok(Box::pin(futures::stream::iter(chunks)) as girder_core::ChunkStream) },
        )
    }
}

fn runtime_with(provider: Arc<dyn LlmProvider>, bus: Arc<EventBus>) -> Arc<AgentRuntime> {
    let tools: Arc<dyn ToolRegistryView> = Arc::new(ToolRegistry::new());
    AgentRuntime::new(RuntimeConfig {
        provider,
        tools,
        bus,
        model: "test".to_owned(),
        max_turns: 1,
    })
}

#[tokio::test]
async fn a_prompt_actually_calls_the_model() {
    // The point of the whole commit: before this, prompt returned accepted:true and did nothing.
    let dir = std::env::temp_dir().join(format!("girder-prompt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    catalog.create("s1").await.unwrap();

    let provider = Arc::new(EchoProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let bus = Arc::new(EventBus::new());
    let mut observed = bus.observe(None);
    let runtime = runtime_with(
        Arc::clone(&provider) as Arc<dyn LlmProvider>,
        Arc::clone(&bus),
    );
    let registry = girder_core::registry_with_runtime(Arc::clone(&catalog), runtime);

    let value = registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "s1", "text": "hello"})),
        )
        .await
        .expect("prompt must be accepted");

    assert_eq!(value["accepted"], true);
    // Turn numbers count prompts, not log records. `create` writes no user message, so this is the
    // session's first exchange.
    assert_eq!(value["turn"], 1);

    // The model runs asynchronously; wait for the event rather than sleeping a fixed time.
    let mut saw_text = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(200), observed.recv()).await {
            Ok(Some(event)) => {
                if event.args.to_string().contains("echoed") {
                    saw_text = true;
                    break;
                }
            }
            Ok(None) => break,
            // A timeout just means no event arrived yet; keep waiting until the deadline.
            Err(_) => {}
        }
    }
    assert!(saw_text, "the model's text must reach the event stream");
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the provider must have been called exactly once"
    );

    // The prompt is durable before the model answers.
    let events = girder_core::read_session_file(dir.join("s1.jsonl.zst")).unwrap();
    assert!(
        events.iter().any(|event| matches!(
            event,
            SessionEvent::UserMessage { message }
                if message.text() == "hello"
        )),
        "the user message must be in the log: {events:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn prompting_an_unknown_session_fails() {
    // Minting a log on demand would hide a client bug behind a new session.
    let dir = std::env::temp_dir().join(format!("girder-prompt2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let provider = Arc::new(EchoProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = runtime_with(provider, Arc::new(EventBus::new()));
    let registry = girder_core::registry_with_runtime(catalog, runtime);

    let error = registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "nope", "text": "hello"})),
        )
        .await
        .expect_err("an unknown session must not be prompted");

    assert_eq!(error.code(), "FAILED");
    assert!(error.to_string().contains("no session named"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_second_prompt_while_running_is_refused() {
    // Two concurrent turns in one session would append messages out of order.
    let dir = std::env::temp_dir().join(format!("girder-prompt3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    catalog.create("s").await.unwrap();

    let provider = Arc::new(EchoProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = runtime_with(provider, Arc::new(EventBus::new()));
    let registry = girder_core::registry_with_runtime(Arc::clone(&catalog), Arc::clone(&runtime));

    registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "s", "text": "one"})),
        )
        .await
        .unwrap();
    // The first turn is in flight; a second prompt to the same session must be refused.
    let error = registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "s", "text": "two"})),
        )
        .await
        .expect_err("a running turn must block a second prompt");

    assert!(error.to_string().contains("already running"), "{error}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_runtime_clears_a_session_when_the_turn_ends() {
    // A failed or finished turn must not wedge the session.
    let dir = std::env::temp_dir().join(format!("girder-prompt4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    catalog.create("s").await.unwrap();

    let provider = Arc::new(EchoProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = runtime_with(provider, Arc::new(EventBus::new()));
    let registry = girder_core::registry_with_runtime(Arc::clone(&catalog), Arc::clone(&runtime));

    registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "s", "text": "x"})),
        )
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while runtime.is_running("s") && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!runtime.is_running("s"), "the session must be released");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_prompt_is_recorded_exactly_once() {
    // The runtime writes the prompt before spawning so it is durable even if the model never
    // answers. The loop must therefore not write it again: a duplicate user message makes the
    // replayed conversation show the same turn twice.
    let dir = std::env::temp_dir().join(format!("girder-once-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    catalog.create("once").await.unwrap();

    let provider = Arc::new(EchoProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = runtime_with(provider, Arc::new(EventBus::new()));
    let registry = girder_core::registry_with_runtime(Arc::clone(&catalog), Arc::clone(&runtime));

    registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "once", "text": "unique-prompt"})),
        )
        .await
        .unwrap();

    // Let the turn settle, then count.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while runtime.is_running("once") && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let events = girder_core::read_session_file(dir.join("once.jsonl.zst")).unwrap();
    let prompts = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                SessionEvent::UserMessage { message }
                    if message.text() == "unique-prompt"
            )
        })
        .count();

    assert_eq!(
        prompts, 1,
        "the prompt must appear once, got {prompts}: {events:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- session.cancel -----------------------------------------------------------------------------

/// A provider that emits slowly, so a cancel has something to interrupt.
struct SlowProvider {
    started: std::sync::atomic::AtomicUsize,
}

impl LlmProvider for SlowProvider {
    fn stream(
        &self,
        _request: LlmRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, LlmError>> {
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Ten chunks, 100 ms apart: long enough that a cancel lands mid-stream.
        let stream = async_stream::stream! {
            for index in 0..10 {
                if cancel.is_cancelled() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                yield StreamChunk::TextDelta {
                    index: 0,
                    text: format!("{index}"),
                };
            }
            yield StreamChunk::Finish {
                reason: FinishReason::Stop,
            };
        };
        Box::pin(async move { Ok(Box::pin(stream) as girder_core::ChunkStream) })
    }
}

#[tokio::test]
async fn cancel_stops_a_running_turn() {
    let dir = std::env::temp_dir().join(format!("girder-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    catalog.create("c").await.unwrap();

    let provider = Arc::new(SlowProvider {
        started: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = runtime_with(
        Arc::clone(&provider) as Arc<dyn LlmProvider>,
        Arc::new(EventBus::new()),
    );
    let registry = girder_core::registry_with_runtime(Arc::clone(&catalog), Arc::clone(&runtime));

    registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "c", "text": "go"})),
        )
        .await
        .unwrap();

    // Let it start producing, then cancel.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(runtime.is_running("c"), "the turn must still be running");

    let value = registry
        .dispatch("session", "cancel", call(json!({"sessionId": "c"})))
        .await
        .expect("cancel must be accepted");
    assert_eq!(value["accepted"], true);

    // The turn must stop well before its natural end (ten 100 ms chunks).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while runtime.is_running("c") && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        !runtime.is_running("c"),
        "the cancelled turn must release the session"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cancel_with_nothing_running_is_accepted() {
    // A client that cancels a turn which just finished should not see an error for winning a race.
    let dir = std::env::temp_dir().join(format!("girder-cancel2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let provider = Arc::new(EchoProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = runtime_with(provider, Arc::new(EventBus::new()));
    let registry = girder_core::registry_with_runtime(catalog, runtime);

    let value = registry
        .dispatch("session", "cancel", call(json!({"sessionId": "idle"})))
        .await
        .expect("cancel of an idle session must be accepted");

    assert_eq!(value["accepted"], true);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cancel_requires_a_session_id() {
    let dir = std::env::temp_dir().join(format!("girder-cancel3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let provider = Arc::new(EchoProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = runtime_with(provider, Arc::new(EventBus::new()));
    let registry = girder_core::registry_with_runtime(catalog, runtime);

    let error = registry
        .dispatch("session", "cancel", call(json!({})))
        .await
        .expect_err("cancel without a session cannot select one");

    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(error.to_string().contains("sessionId"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_cancelled_turn_can_be_prompted_again() {
    // Cancellation must not wedge the session: a client that stops a turn and retries expects to
    // be able to.
    let dir = std::env::temp_dir().join(format!("girder-cancel4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    catalog.create("r").await.unwrap();

    let provider = Arc::new(SlowProvider {
        started: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = runtime_with(
        Arc::clone(&provider) as Arc<dyn LlmProvider>,
        Arc::new(EventBus::new()),
    );
    let registry = girder_core::registry_with_runtime(Arc::clone(&catalog), Arc::clone(&runtime));

    registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "r", "text": "first"})),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    registry
        .dispatch("session", "cancel", call(json!({"sessionId": "r"})))
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while runtime.is_running("r") && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let value = registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "r", "text": "second"})),
        )
        .await
        .expect("a cancelled session must accept a new prompt");
    assert_eq!(value["accepted"], true);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn turn_numbers_count_prompts_not_log_records() {
    // Counting `TurnStart` records would skip ahead: the loop writes one per model round, so a
    // two-round turn leaves two and the next prompt would report turn 4 instead of 2.
    let dir = std::env::temp_dir().join(format!("girder-turnnum-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    catalog.create("t").await.unwrap();

    let provider = Arc::new(EchoProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = runtime_with(provider, Arc::new(EventBus::new()));
    let registry = girder_core::registry_with_runtime(Arc::clone(&catalog), Arc::clone(&runtime));

    let first = registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "t", "text": "one"})),
        )
        .await
        .unwrap();
    assert_eq!(first["turn"], 1, "the first prompt is turn 1");

    // Let the turn finish so its own TurnStart records land in the log.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while runtime.is_running("t") && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let second = registry
        .dispatch(
            "session",
            "prompt",
            call(json!({"sessionId": "t", "text": "two"})),
        )
        .await
        .unwrap();
    assert_eq!(
        second["turn"], 2,
        "the second prompt is turn 2 regardless of how many model rounds the first ran"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn concurrent_creates_of_one_id_leave_one_session() {
    // Existence check and creation must not race: two creates both seeing "absent" would each append
    // a `TurnStart`, leaving a fresh session holding two turns. Measured before the guard: eight
    // concurrent creates left eight events instead of one.
    let dir = std::env::temp_dir().join(format!("girder-toctou-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let catalog = Arc::clone(&catalog);
        handles.push(tokio::spawn(async move { catalog.create("dup").await }));
    }
    for handle in handles {
        handle
            .await
            .expect("the task must not panic")
            .expect("every create must succeed: reusing is success, not failure");
    }

    let events = girder_core::read_session_file(dir.join("dup.jsonl.zst")).unwrap();
    // The property: one session, not eight. A created session holds no turn because it has not been
    // prompted; what must not happen is eight separate files or eight creation records.
    assert!(
        events.is_empty(),
        "a created session holds no turn: {events:?}"
    );
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "eight creates must leave one file: {files:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn concurrent_creates_all_return_the_same_id() {
    // Every caller must see the session, including the ones that lost the race to create it.
    let dir = std::env::temp_dir().join(format!("girder-toctou2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    let mut handles = Vec::new();
    for _ in 0..6 {
        let catalog = Arc::clone(&catalog);
        handles.push(tokio::spawn(async move { catalog.create("same").await }));
    }
    for handle in handles {
        let record = handle.await.unwrap().unwrap();
        // The property: every racer sees the same session. Whether it holds events is not this
        // test's business -- a created session has none -- but a racer that returned a different
        // identity would have forked history.
        assert_eq!(record.id, "same");
    }
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(files.len(), 1, "six racers must leave one log: {files:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- registry consistency -----------------------------------------------------------------------

#[tokio::test]
async fn every_method_in_one_registry_exists_in_the_other() {
    // Two constructors exist: one for tests (no runtime) and one for production. They must agree on
    // which names resolve, or a client's behaviour would depend on which the host happened to build.
    let dir = std::env::temp_dir().join(format!("girder-registry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    let provider = Arc::new(EchoProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = runtime_with(provider, Arc::new(EventBus::new()));
    let with_runtime = girder_core::registry_with_runtime(Arc::clone(&catalog), runtime);
    let plain = default_registry(Arc::clone(&catalog));

    // The two constructors differ deliberately: `cancel` needs a runtime, and the model methods need a
    // route table. What must not differ is how an unregistered name is reported -- a client's error
    // handling cannot depend on which constructor the host chose.
    let runtime_methods: Vec<String> = with_runtime.names().into_iter().map(|(_, m)| m).collect();
    for method in &runtime_methods {
        // A method both serve resolves the same way, so there is nothing to compare. This is the
        // common case now: the two constructors differ only in whether a turn can actually run.
        if plain.names().iter().any(|(_, m)| m == method) {
            continue;
        }
        // Registered by the runtime constructor only: the plain one must call it a gap, not a typo.
        let error = plain
            .dispatch("session", method, call(json!({})))
            .await
            .expect_err("a method absent from the plain registry must not resolve");
        assert_eq!(
            error.code(),
            "NOT_IMPLEMENTED",
            "`{method}` must be a declared gap in the plain registry"
        );
    }

    // And every name outside that set must report a gap rather than a typo.
    //
    // `projections`, `rename`, `delete`, `selectModel`, `modelCatalog`, and `fork` were all on this
    // list until they were implemented. This assertion is the one that notices: probing a name that
    // works returns its handler's own error, not NOT_IMPLEMENTED.
    for probe in ["search", "stream", "resume", "archive"] {
        let error = with_runtime
            .dispatch("session", probe, call(json!({})))
            .await
            .expect_err("an unimplemented method must not resolve");
        assert_eq!(
            error.code(),
            "NOT_IMPLEMENTED",
            "`{probe}` must be a declared gap, not an unknown name"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_unimplemented_list_does_not_name_an_implemented_method() {
    // A method present in both lists would answer NOT_IMPLEMENTED after being registered, because
    // the registry checks the method map first — but the entry would still be a stale claim.
    let dir = std::env::temp_dir().join(format!("girder-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let provider = Arc::new(EchoProvider {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let runtime = runtime_with(provider, Arc::new(EventBus::new()));
    let registry = girder_core::registry_with_runtime(catalog, runtime);

    let implemented: Vec<String> = registry.names().into_iter().map(|(_, m)| m).collect();
    for method in &implemented {
        // Dispatching an implemented method must not answer NOT_IMPLEMENTED.
        let result = registry.dispatch("session", method, call(json!({}))).await;
        if let Err(error) = result {
            assert_ne!(
                error.code(),
                "NOT_IMPLEMENTED",
                "`{method}` is registered but also listed as unimplemented"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn every_constructor_serves_the_same_session_methods() {
    // The structural guarantee the refactor bought: three constructors, one shared registration. Before
    // it, seven methods were added to one path and forgotten in another, and a test caught each one.
    // This asserts the property directly so a future hand-written list breaks it immediately.
    let dir = std::env::temp_dir().join(format!("girder-constructors-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    let mut routes = girder_core::RouteRegistry::new();
    routes
        .register(
            girder_core::Route::new("ds", "deepseek")
                .with_model(girder_core::ModelInfo::text("m", 100, 10)),
        )
        .expect("registered");
    let routes = Arc::new(routes);

    let runtime = runtime_with(
        Arc::new(EchoProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Arc::new(EventBus::new()),
    );

    let with_routes = girder_core::registry_with_routes(&Arc::clone(&catalog), &runtime, &routes);
    let with_runtime =
        girder_core::registry_with_runtime(Arc::clone(&catalog), Arc::clone(&runtime));
    let plain = default_registry(Arc::clone(&catalog));

    let session_methods = |registry: &girder_core::RemoteRegistry| -> Vec<String> {
        let mut names: Vec<String> = registry
            .names()
            .into_iter()
            .filter(|(namespace, _)| namespace == "session")
            .map(|(_, method)| method)
            .collect();
        names.sort();
        names
    };

    let a = session_methods(&with_routes);
    let b = session_methods(&with_runtime);
    let c = session_methods(&plain);

    // The runtime path serves everything except the two model methods, which need a route table.
    let mut expected = a.clone();
    expected.retain(|method| method != "modelCatalog" && method != "selectModel");
    assert_eq!(
        b, expected,
        "the runtime path must match the routes path minus the model methods"
    );

    // The plain path additionally lacks `cancel`, which needs a runtime.
    let mut expected_plain = b.clone();
    expected_plain.retain(|method| method != "cancel");
    assert_eq!(
        c, expected_plain,
        "the plain path must match the runtime path minus cancel"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
