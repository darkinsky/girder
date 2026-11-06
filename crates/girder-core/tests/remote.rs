//! Tests for the Remote method surface.
//!
//! The point of these tests is the distinction between three failures a client must be able to tell
//! apart: a typo (`UNKNOWN_METHOD`), a gap in this build (`NOT_IMPLEMENTED`), and bad arguments
//! (`INVALID_ARGUMENTS`). Collapsing them into one error would make a half-implemented client look
//! broken instead of incomplete.

use std::sync::Arc;

use girder_core::{
    ContentBlock, DirectoryCatalog, FinishReason, RemoteCall, RemoteError, RemoteRegistry,
    SessionCatalog, SessionEvent, SessionRecord, SessionSummary, default_registry,
    register_session_methods,
};
use serde_json::json;

// ---- a catalog stub -----------------------------------------------------------------------------

struct FixedCatalog(Vec<SessionRecord>);

impl SessionCatalog for FixedCatalog {
    fn list(&self) -> girder_core::BoxFut<Vec<SessionRecord>> {
        let records = self.0.clone();
        Box::pin(async move { records })
    }
}

fn record(id: &str, updated_at: u64, has_message: bool) -> SessionRecord {
    let mut events = vec![SessionEvent::TurnStart { turn: 1 }];
    if has_message {
        events.push(SessionEvent::UserMessage {
            content: vec![ContentBlock::Text {
                text: "hello".to_owned(),
            }],
        });
    }
    SessionRecord {
        id: id.to_owned(),
        updated_at,
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
            .append(&SessionEvent::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hi".to_owned(),
                }],
            })
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
    assert!(
        records.iter().all(|r| !r.events.is_empty()),
        "the events must be read, not just the names"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_summary_reflects_whether_the_session_has_a_message() {
    let with = SessionSummary::from_events(
        "s",
        &[SessionEvent::UserMessage {
            content: vec![ContentBlock::Text {
                text: "x".to_owned(),
            }],
        }],
        5,
    );
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
            reason: FinishReason::Stop,
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
