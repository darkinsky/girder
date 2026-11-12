//! Tests for `session.duplicate` and `session.archive`.
//!
//! The properties under test, in the order they matter:
//!
//! - **a copy is a copy** — a new id, and every event of the source in the same order, because a
//!   copy that dropped or reordered one would be a silently different conversation
//! - **a copy does not disturb its source** — the original log and title are untouched, since the
//!   point of duplicating is to keep working on the branch without risking the original
//! - **a copy has its own identity** — a title distinct from the source's, because two identically
//!   named rows are a list nobody can navigate
//! - **an unknown session is refused** by both methods — a copy of nothing and a mark on nothing are
//!   both silent failures otherwise
//! - **archive hides and does not destroy** — the mark leaves the log readable and removes the row
//!   from a listing filtered with `retain_visible`
//! - **archive is idempotent** — a retried call must not be an error, because a retry is the ordinary
//!   reason a request arrives twice
//! - **archive refuses when there is nowhere to write**, and says what is missing — the default
//!   registration is honest rather than pretending a state it cannot record
//!
//! Isolation: every test builds its own catalog and archive over a scratch directory named from the
//! process id and a counter, so two test binaries and two tests in one binary cannot collide.
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

#[path = "../src/session_lifecycle_rpc.rs"]
mod session_lifecycle_rpc;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{
    DirectoryCatalog, RemoteCall, RemoteError, RemoteRegistry, SessionCatalog, SessionEvent,
    SessionStore,
};
use serde_json::{Value, json};

use session_lifecycle_rpc::{
    ARCHIVE_METHOD, ArchiveStore, COPY_SUFFIX, DUPLICATE_METHOD, MAX_COPY_TITLE_CHARS,
    MemoryArchiveStore, NAMESPACE, UnavailableArchiveStore, register_session_lifecycle_methods,
    register_session_lifecycle_methods_with_archive, retain_visible,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated scratch directory: `temp_dir()` plus pid plus a counter.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-lifecycle-{name}-{}-{n}",
        std::process::id()
    ));
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

/// Writes one session holding a user message and an assistant answer.
fn seed(dir: &Path, id: &str, text: &str) {
    let store = SessionStore::new(dir.join(format!("{id}.jsonl.zst")));
    store.touch().expect("a session log is created");
    store
        .append(&SessionEvent::user_text("m1", text))
        .expect("the prompt appends");
    store
        .append(&SessionEvent::assistant_text("m2", "answer", "test"))
        .expect("the answer appends");
}

fn events_of(dir: &Path, id: &str) -> Vec<SessionEvent> {
    girder_core::read_session_file(dir.join(format!("{id}.jsonl.zst")))
        .expect("the log is readable")
}

/// Appends the title a log records, which is where `duplicate` reads a base name from.
fn seed_title(dir: &Path, id: &str, title: &str) {
    let store = SessionStore::new(dir.join(format!("{id}.jsonl.zst")));
    store
        .append(&SessionEvent::SessionTitle {
            title: title.to_owned(),
            message_seqs: vec![1],
            source: "user".to_owned(),
        })
        .expect("the title appends");
}

/// A harness holding one catalog, one archive store, and a registry over both.
struct Harness {
    dir: PathBuf,
    catalog: Arc<dyn SessionCatalog>,
    archive: Arc<MemoryArchiveStore>,
    registry: RemoteRegistry,
}

impl Harness {
    fn new(name: &str) -> Self {
        let dir = scratch(name);
        let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
        let archive = Arc::new(MemoryArchiveStore::new());
        let mut registry = RemoteRegistry::new();
        register_session_lifecycle_methods_with_archive(
            &mut registry,
            &catalog,
            &(Arc::clone(&archive) as Arc<dyn ArchiveStore>),
        );
        Self {
            dir,
            catalog,
            archive,
            registry,
        }
    }

    async fn dispatch(&self, method: &str, args: Value) -> Result<Value, RemoteError> {
        self.registry.dispatch(NAMESPACE, method, call(args)).await
    }

    async fn duplicate(&self, session_id: &str) -> Result<Value, RemoteError> {
        self.dispatch(DUPLICATE_METHOD, json!({"sessionId": session_id}))
            .await
    }

    async fn archive(&self, session_id: &str) -> Result<Value, RemoteError> {
        self.dispatch(ARCHIVE_METHOD, json!({"sessionId": session_id}))
            .await
    }

    /// The listing a client sees: the catalog's rows, without the archived ones.
    async fn visible(&self) -> Vec<String> {
        let mut records = self.catalog.list().await;
        retain_visible(&mut records, self.archive.as_ref());
        let mut ids: Vec<String> = records.into_iter().map(|record| record.id).collect();
        ids.sort();
        ids
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---- duplicate --------------------------------------------------------------------------------

#[tokio::test]
async fn a_duplicate_has_a_new_id_and_the_same_conversation() {
    let harness = Harness::new("dup");
    seed(&harness.dir, "s1", "hello");
    let source_events = events_of(&harness.dir, "s1");

    let value = harness.duplicate("s1").await.expect("duplicated");
    let new_id = value["sessionId"].as_str().expect("an id");

    assert_ne!(new_id, "s1", "a duplicate is a different session");
    assert_eq!(
        events_of(&harness.dir, new_id),
        source_events,
        "the copy holds every event, in order"
    );
}

#[tokio::test]
async fn a_duplicate_leaves_the_source_untouched() {
    let harness = Harness::new("dup-source");
    seed(&harness.dir, "s1", "hello");
    harness
        .catalog
        .rename("s1", "Original")
        .await
        .expect("the source is titled");
    let before = events_of(&harness.dir, "s1");
    let title_before =
        std::fs::read_to_string(harness.dir.join("s1.title")).expect("a title file exists");

    harness.duplicate("s1").await.expect("duplicated");

    assert_eq!(
        events_of(&harness.dir, "s1"),
        before,
        "the log is unchanged"
    );
    assert_eq!(
        std::fs::read_to_string(harness.dir.join("s1.title")).expect("a title file exists"),
        title_before,
        "the source keeps its own title"
    );
}

#[tokio::test]
async fn a_duplicate_is_titled_after_the_title_recorded_in_its_log() {
    // Two identically named rows are a list a user cannot navigate, so the copy has to say it is one.
    let harness = Harness::new("dup-title");
    seed(&harness.dir, "s1", "hello");
    seed_title(&harness.dir, "s1", "Original");

    let value = harness.duplicate("s1").await.expect("duplicated");
    let new_id = value["sessionId"].as_str().expect("an id");
    let title = value["title"].as_str().expect("a stored title");

    assert_eq!(title, format!("Original{COPY_SUFFIX}"));
    assert_eq!(
        std::fs::read_to_string(harness.dir.join(format!("{new_id}.title")))
            .expect("a copy title exists"),
        title,
        "the title reported is the title stored"
    );
}

#[tokio::test]
async fn a_duplicate_derives_a_title_from_the_conversation_when_the_source_has_none() {
    // The catalog cannot be asked for a title, so the log is the source of the base name.
    let harness = Harness::new("dup-derived");
    seed(&harness.dir, "s1", "how do I sort a vector");

    let value = harness.duplicate("s1").await.expect("duplicated");
    let title = value["title"].as_str().expect("a stored title");

    assert_eq!(title, format!("how do I sort a vector{COPY_SUFFIX}"));
}

#[tokio::test]
async fn a_duplicate_of_a_long_conversation_stays_within_the_title_bound() {
    // The base is user text with no length of its own, and the suffix still has to fit.
    let harness = Harness::new("dup-long");
    seed(&harness.dir, "s1", &"x".repeat(500));

    let value = harness.duplicate("s1").await.expect("duplicated");
    let title = value["title"].as_str().expect("a stored title");

    assert!(
        title.chars().count() <= MAX_COPY_TITLE_CHARS,
        "a title of {} characters exceeds the bound",
        title.chars().count()
    );
    assert!(title.ends_with(COPY_SUFFIX), "{title}");
}

#[tokio::test]
async fn a_duplicate_of_a_blank_session_is_blank_and_empty() {
    // A session that was created but never prompted is a legitimate thing to copy.
    let harness = Harness::new("dup-blank");
    let store = SessionStore::new(harness.dir.join("s1.jsonl.zst"));
    store.touch().expect("created");

    let value = harness.duplicate("s1").await.expect("duplicated");
    let new_id = value["sessionId"].as_str().expect("an id");

    assert!(events_of(&harness.dir, new_id).is_empty(), "no events");
}

#[tokio::test]
async fn two_duplicates_of_one_session_get_different_ids() {
    // Uniqueness is manufactured in this module, because `create` reuses an id that exists.
    let harness = Harness::new("dup-ids");
    seed(&harness.dir, "s1", "hello");

    let first = harness.duplicate("s1").await.expect("duplicated");
    let second = harness.duplicate("s1").await.expect("duplicated");

    let first = first["sessionId"].as_str().expect("an id");
    let second = second["sessionId"].as_str().expect("an id");
    assert_ne!(first, second);
    assert_eq!(events_of(&harness.dir, first).len(), 2);
    assert_eq!(events_of(&harness.dir, second).len(), 2);
}

#[tokio::test]
async fn duplicating_an_unknown_session_is_refused() {
    let harness = Harness::new("dup-missing");

    let error = harness
        .duplicate("nope")
        .await
        .expect_err("no such session");

    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("nope"), "{error}");
}

#[tokio::test]
async fn duplicating_without_a_session_id_is_refused() {
    let harness = Harness::new("dup-args");

    let error = harness
        .dispatch(DUPLICATE_METHOD, json!({}))
        .await
        .expect_err("no id");

    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error}"
    );
}

#[tokio::test]
async fn duplicating_a_blank_id_is_refused_rather_than_looked_up() {
    let harness = Harness::new("dup-blank-id");
    seed(&harness.dir, "s1", "hello");

    let error = harness
        .dispatch(DUPLICATE_METHOD, json!({"sessionId": "   "}))
        .await
        .expect_err("blank id");

    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error}"
    );
}

// ---- archive ----------------------------------------------------------------------------------

#[tokio::test]
async fn an_archived_session_leaves_the_listing_but_keeps_its_log() {
    let harness = Harness::new("arch");
    seed(&harness.dir, "s1", "hello");
    seed(&harness.dir, "s2", "other");
    assert_eq!(harness.visible().await, vec!["s1", "s2"]);

    let value = harness.archive("s1").await.expect("archived");

    assert_eq!(value["archived"], true);
    assert_eq!(value["sessionId"], "s1");
    assert_eq!(harness.visible().await, vec!["s2"], "the row is hidden");
    assert_eq!(
        events_of(&harness.dir, "s1").len(),
        2,
        "archiving is not a deletion"
    );
}

#[tokio::test]
async fn archiving_the_same_session_twice_succeeds() {
    // A retry is the ordinary reason a request arrives twice, and the state asked for is already
    // true, so a failure here would report a problem the caller does not have.
    let harness = Harness::new("arch-idempotent");
    seed(&harness.dir, "s1", "hello");

    let first = harness.archive("s1").await.expect("archived");
    let second = harness.archive("s1").await.expect("archived again");

    assert_eq!(first["archived"], true);
    assert!(
        second["archived"].as_bool().expect("a bool"),
        "the retry is acknowledged"
    );
    // The store says "nothing changed" the second time, which is how a host logs a transition
    // rather than a request.
    assert!(
        !harness.archive.mark_archived("s1").expect("already marked"),
        "the second mark is a no-op"
    );
    assert_eq!(harness.archive.archived_ids(), vec!["s1".to_owned()]);
    assert_eq!(harness.visible().await, Vec::<String>::new());
}

#[tokio::test]
async fn archiving_an_unknown_session_is_refused_and_marks_nothing() {
    // A mark on an id nobody holds would hide whatever later took that id, and a caller who
    // mistyped would be told the wrong thing succeeded.
    let harness = Harness::new("arch-missing");

    let error = harness.archive("nope").await.expect_err("no such session");

    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("nope"), "{error}");
    assert!(harness.archive.archived_ids().is_empty(), "nothing marked");
}

#[tokio::test]
async fn archiving_without_a_session_id_is_refused() {
    let harness = Harness::new("arch-args");

    let error = harness
        .dispatch(ARCHIVE_METHOD, json!({}))
        .await
        .expect_err("no id");

    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error}"
    );
}

#[tokio::test]
async fn the_default_registration_refuses_to_archive_and_says_what_is_missing() {
    // `SessionCatalog` has no place to record the mark, and inventing one would either guess a
    // catalog's layout or report a state a restart loses. The refusal has to name the seam.
    let dir = scratch("arch-unavailable");
    seed(&dir, "s1", "hello");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let mut registry = RemoteRegistry::new();
    register_session_lifecycle_methods(&mut registry, &catalog);

    let error = registry
        .dispatch(NAMESPACE, ARCHIVE_METHOD, call(json!({"sessionId": "s1"})))
        .await
        .expect_err("no storage");

    assert!(matches!(error, RemoteError::Failed(_)), "{error}");
    assert!(
        error.to_string().contains("ArchiveStore"),
        "the message must name what a host has to supply: {error}"
    );

    // Duplicating needs no storage, so the same default registration must still provide it.
    let value = registry
        .dispatch(
            NAMESPACE,
            DUPLICATE_METHOD,
            call(json!({"sessionId": "s1"})),
        )
        .await
        .expect("duplicate works without an archive store");
    assert!(value["sessionId"].as_str().is_some_and(|id| id != "s1"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_unavailable_store_reports_nothing_archived() {
    // Reporting a mark this store never accepted would make a listing hide a session for no
    // recorded reason.
    let store = UnavailableArchiveStore::default_reason();

    assert!(!store.is_archived("anything"));
    assert!(store.archived_ids().is_empty());
    assert!(store.mark_archived("anything").is_err());
}

/// A store whose write always fails, standing in for a full disk or a read-only metadata directory.
struct BrokenStore;

impl ArchiveStore for BrokenStore {
    fn is_archived(&self, _session_id: &str) -> bool {
        false
    }

    fn mark_archived(
        &self,
        _session_id: &str,
    ) -> Result<bool, session_lifecycle_rpc::ArchiveError> {
        Err(session_lifecycle_rpc::ArchiveError::failed(
            "the metadata directory is read-only",
        ))
    }

    fn archived_ids(&self) -> Vec<String> {
        Vec::new()
    }
}

#[tokio::test]
async fn a_store_that_cannot_write_reports_a_failure_rather_than_success() {
    // The two failure shapes must stay distinguishable: no storage is a deployment mistake, and a
    // failed write is a condition the caller retries. Both are failures, and neither may be a
    // success.
    let dir = scratch("arch-broken");
    seed(&dir, "s1", "hello");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let archive: Arc<dyn ArchiveStore> = Arc::new(BrokenStore);
    let mut registry = RemoteRegistry::new();
    register_session_lifecycle_methods_with_archive(&mut registry, &catalog, &archive);

    let error = registry
        .dispatch(NAMESPACE, ARCHIVE_METHOD, call(json!({"sessionId": "s1"})))
        .await
        .expect_err("the write failed");

    assert!(matches!(error, RemoteError::Failed(_)), "{error}");
    assert!(error.to_string().contains("read-only"), "{error}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- registration -----------------------------------------------------------------------------

#[tokio::test]
async fn both_methods_are_registered_under_the_session_namespace() {
    let harness = Harness::new("registered");

    let names = harness.registry.names();
    assert!(
        names.contains(&(NAMESPACE.to_owned(), DUPLICATE_METHOD.to_owned())),
        "{names:?}"
    );
    assert!(
        names.contains(&(NAMESPACE.to_owned(), ARCHIVE_METHOD.to_owned())),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_read_only_policy_cannot_duplicate_or_archive() {
    // Both change a session, so both declare a write; without the declaration a read-only mirror
    // could copy or hide a user's conversations.
    let dir = scratch("policy");
    seed(&dir, "s1", "hello");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
    let archive: Arc<dyn ArchiveStore> = Arc::new(MemoryArchiveStore::new());
    let mut registry = RemoteRegistry::new();
    register_session_lifecycle_methods_with_archive(&mut registry, &catalog, &archive);
    let registry = registry.with_policy(Arc::new(girder_core::ReadOnly));

    for method in [DUPLICATE_METHOD, ARCHIVE_METHOD] {
        let error = registry
            .dispatch(NAMESPACE, method, call(json!({"sessionId": "s1"})))
            .await
            .expect_err("a read-only policy refuses a write");
        assert!(matches!(error, RemoteError::Refused { .. }), "{error}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
