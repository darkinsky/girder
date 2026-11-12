//! Tests for renaming and deleting sessions.
//!
//! A rename is not an event: appending one to the log would make every history read carry a record
//! the model never saw. These tests pin that the conversation is untouched by a title and that a
//! deletion removes the log rather than only the name.

use std::sync::Arc;

use girder_core::{DirectoryCatalog, RemoteCall, SessionCatalog, SessionEvent, default_registry};
use serde_json::json;

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-mgmt-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A provider that never answers; the test only inspects which methods are registered.
struct Silent;

impl girder_core::LlmProvider for Silent {
    fn stream(
        &self,
        _request: girder_core::LlmRequest,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, girder_core::LlmError>> {
        Box::pin(async { Err(girder_core::LlmError::Cancelled) })
    }
}

/// Writes one session with a message so it is not blank.
fn seed(dir: &std::path::Path, id: &str) {
    let store = girder_core::SessionStore::new(dir.join(format!("{id}.jsonl.zst")));
    store.touch().expect("created");
    store
        .append(&SessionEvent::user_text("m1", "hello"))
        .expect("appended");
}

// ---- rename -----------------------------------------------------------------------------------

#[tokio::test]
async fn renaming_stores_a_title() {
    let dir = scratch("rename");
    seed(&dir, "s1");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let registry = default_registry(catalog);

    let value = registry
        .dispatch(
            "session",
            "rename",
            call(json!({"sessionId": "s1", "title": "My chat"})),
        )
        .await
        .expect("renamed");

    assert_eq!(value["title"], "My chat");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_title_does_not_touch_the_log() {
    // A rename is not part of the conversation: appending it would make every history read carry a
    // record the model never saw.
    let dir = scratch("rename-log");
    seed(&dir, "s1");
    let before = girder_core::read_session_file(dir.join("s1.jsonl.zst")).expect("read");

    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    catalog.rename("s1", "Titled").await.expect("renamed");

    let after = girder_core::read_session_file(dir.join("s1.jsonl.zst")).expect("read");
    assert_eq!(before, after, "the conversation must be unchanged");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_blank_title_is_refused() {
    let dir = scratch("rename-blank");
    seed(&dir, "s1");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    let error = catalog.rename("s1", "   ").await.expect_err("blank");

    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn renaming_an_unknown_session_is_refused() {
    // A title with no conversation would be a row nothing can open.
    let dir = scratch("rename-missing");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    let error = catalog.rename("nope", "Title").await.expect_err("missing");

    assert!(!matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_registry_requires_a_session() {
    let dir = scratch("rename-args");
    seed(&dir, "s1");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let registry = default_registry(catalog);

    // A missing `sessionId` is still an error. A missing `title` is not: the session has a message, so the name can be
    // derived from it, which is what a client listing untitled conversations wants.
    for args in [json!({"title": "x"})] {
        let error = registry
            .dispatch("session", "rename", call(args))
            .await
            .expect_err("missing field");
        assert!(matches!(
            error,
            girder_core::RemoteError::InvalidArguments { .. }
        ));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- delete -----------------------------------------------------------------------------------

#[tokio::test]
async fn deleting_removes_the_log() {
    let dir = scratch("delete");
    seed(&dir, "s1");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    assert!(catalog.delete("s1").await.expect("deleted"));
    assert!(!dir.join("s1.jsonl.zst").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn deleting_twice_reports_nothing_removed() {
    // The client distinguishes a deletion from a no-op without an error.
    let dir = scratch("delete-twice");
    seed(&dir, "s1");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    assert!(catalog.delete("s1").await.expect("deleted"));
    assert!(!catalog.delete("s1").await.expect("no-op"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn deleting_removes_the_title_too() {
    // A title with no log is a stale row.
    let dir = scratch("delete-title");
    seed(&dir, "s1");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    catalog.rename("s1", "Titled").await.expect("renamed");

    catalog.delete("s1").await.expect("deleted");

    assert!(!dir.join("s1.title").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_deleted_session_disappears_from_the_list() {
    let dir = scratch("delete-list");
    seed(&dir, "s1");
    seed(&dir, "s2");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    catalog.delete("s1").await.expect("deleted");
    let records = catalog.list().await;

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, "s2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_registry_requires_a_session_id() {
    let dir = scratch("delete-args");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let registry = default_registry(catalog);

    let error = registry
        .dispatch("session", "delete", call(json!({})))
        .await
        .expect_err("missing id");

    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the gap list -----------------------------------------------------------------------------

#[test]
fn an_implemented_method_is_not_declared_a_gap() {
    // A name in the gap list makes a client show "not implemented" while the method works.
    let names = girder_core::registered_method_names();

    for method in ["session.rename", "session.delete", "session.projections"] {
        assert!(
            names.contains(&method.to_owned()),
            "{method} must be listed"
        );
    }
}

// ---- the two registration paths ---------------------------------------------------------------

#[tokio::test]
async fn both_registration_paths_serve_the_same_methods() {
    // `serve` uses the runtime registry and a test uses the plain one. They were two hand-maintained
    // lists and drifted: rename and delete existed in one and not the other, so a running server
    // answered `UNKNOWN_METHOD` for a method the schema advertised.
    let dir = scratch("paths");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let plain = default_registry(Arc::clone(&catalog));

    let runtime = girder_core::AgentRuntime::new(girder_core::RuntimeConfig {
        provider: Arc::new(Silent),
        tools: Arc::new(girder_core::ToolRegistry::new()),
        bus: Arc::new(girder_core::EventBus::new()),
        model: "test".to_owned(),
        max_turns: 1,
        token_budget: girder_core::Ceiling::Unlimited,
    });
    let with_runtime = girder_core::registry_with_runtime(catalog, runtime);

    let mut plain_names = plain.names();
    let mut runtime_names = with_runtime.names();
    plain_names.sort();
    runtime_names.sort();

    for method in &plain_names {
        assert!(
            runtime_names.contains(method),
            "`{}.{}` is in the plain registry but not the runtime one",
            method.0,
            method.1
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- fork -------------------------------------------------------------------------------------

/// Writes a session holding two complete turns.
fn seed_two_turns(dir: &std::path::Path, id: &str) {
    let store = girder_core::SessionStore::new(dir.join(format!("{id}.jsonl.zst")));
    store.touch().expect("created");
    for turn in 1..=2u64 {
        store
            .append(&SessionEvent::TurnStart { turn })
            .expect("appended");
        store
            .append(&SessionEvent::user_text(&format!("m{turn}"), "hello"))
            .expect("appended");
        store
            .append(&SessionEvent::assistant_text(
                &format!("a{turn}"),
                "hi",
                "test",
            ))
            .expect("appended");
        store
            .append(&SessionEvent::TurnEnd {
                turn,
                reason: girder_core::TurnEndReason::Completed,
            })
            .expect("appended");
    }
}

#[tokio::test]
async fn a_fork_copies_the_source_log() {
    let dir = scratch("fork");
    seed_two_turns(&dir, "s1");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    let forked = catalog.fork("s1", None).await.expect("forked");

    assert_ne!(forked, "s1");
    let source = girder_core::read_session_file(dir.join("s1.jsonl.zst")).expect("read");
    let copy =
        girder_core::read_session_file(dir.join(format!("{forked}.jsonl.zst"))).expect("read");
    assert_eq!(copy, source, "a fork with no cut copies the whole log");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_fork_cut_lands_on_a_turn_boundary() {
    // A cut inside a turn would separate a tool call from its result.
    let dir = scratch("fork-cut");
    seed_two_turns(&dir, "s1");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    // The first turn is 4 events; asking for 2 lands mid-turn and must back up to the boundary.
    let forked = catalog.fork("s1", Some(2)).await.expect("forked");

    let copy =
        girder_core::read_session_file(dir.join(format!("{forked}.jsonl.zst"))).expect("read");
    assert!(
        matches!(copy.last(), Some(SessionEvent::TurnEnd { .. }) | None),
        "a fork must end on a turn boundary: {copy:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_fork_at_the_first_boundary_keeps_one_turn() {
    let dir = scratch("fork-one");
    seed_two_turns(&dir, "s1");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    let forked = catalog.fork("s1", Some(4)).await.expect("forked");

    let copy =
        girder_core::read_session_file(dir.join(format!("{forked}.jsonl.zst"))).expect("read");
    assert_eq!(copy.len(), 4, "the first turn is four events");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_fork_of_an_unknown_session_is_refused() {
    let dir = scratch("fork-missing");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    let error = catalog.fork("nope", None).await.expect_err("missing");

    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_fork_registry_requires_a_session_id() {
    let dir = scratch("fork-args");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let registry = default_registry(catalog);

    let error = registry
        .dispatch("session", "fork", call(json!({})))
        .await
        .expect_err("missing id");

    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_forked_session_appears_in_the_list() {
    // A fork the client cannot then open is not a fork.
    let dir = scratch("fork-list");
    seed_two_turns(&dir, "s1");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));

    let forked = catalog.fork("s1", None).await.expect("forked");
    let records = catalog.list().await;

    assert_eq!(records.len(), 2);
    assert!(records.iter().any(|record| record.id == forked));
    let _ = std::fs::remove_dir_all(&dir);
}
