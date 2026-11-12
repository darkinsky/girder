//! Tests for the extra `workspace` methods: `rename`, `insertBefore`, `insertSessionBefore`,
//! `archiveSession`, `unarchiveSession`, `pinSession`, and `unpinSession`.
//!
//! The properties under test, in the order they matter:
//!
//! - **every implemented method has a working normal path** — a rename reaches `workspace.list`, an
//!   order change reaches the registry, and a pin or an archive reaches the store it was given
//! - **an unknown id is refused, not silently accepted** — for a workspace, a session, and an anchor
//!   alike, because a success for something that did not happen is the failure mode this module
//!   exists to avoid
//! - **a read-only policy refuses every one of them** — each declares a write, and a mirror that
//!   could reorder or hide sessions would not be a mirror
//! - **the in-memory store does not persist** — a second store built over the same process starts
//!   empty, which is the documented limitation expressed as a test rather than left to a doc comment
//! - **pin and archive stay disjoint** — archiving drops a pin, and pinning an archived id is
//!   refused, because a hidden session must not appear on a surface reserved for visible ones
//! - **the `stopActivity` flag is refused rather than ignored** — this build cannot stop work, so a
//!   success would be a lie about what happened
//!
//! Isolation: every test builds its own registry, preference store, and session catalog, and every
//! session id is unique within its test, so tests do not share state.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the methods as written before the crate root wires them in. It spells its own paths
//! through `girder_core::`, the alias the library declares for itself, so the same source builds
//! here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/workspace_rpc_extra.rs"]
mod workspace_rpc_extra;

use std::sync::Arc;

use girder_core::api_config::WorkspaceRegistry;
use girder_core::{
    BoxFut, ReadOnly, RemoteCall, RemoteError, RemoteRegistry, SessionCatalog, SessionRecord,
};

use workspace_rpc_extra::{
    ARCHIVE_SESSION_METHOD, DECLARED_ACCESS, INSERT_BEFORE_METHOD, INSERT_SESSION_BEFORE_METHOD,
    InMemoryPreferences, NAMESPACE, PIN_SESSION_METHOD, PreferenceError, RENAME_METHOD,
    UNARCHIVE_SESSION_METHOD, UNPIN_SESSION_METHOD, UnavailablePreferences, WorkspacePreferences,
    register_workspace_extra_methods, register_workspace_extra_methods_without_preferences,
    retain_visible,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// A catalog holding a fixed set of session ids.
///
/// Only `read` and `list` matter here: the methods under test ask whether an id exists, and
/// returning `None` for everything would make the "unknown session" tests pass for the wrong reason.
struct FixedCatalog {
    ids: Vec<String>,
}

impl FixedCatalog {
    fn new(ids: &[&str]) -> Self {
        Self {
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        }
    }
}

impl SessionCatalog for FixedCatalog {
    fn list(&self) -> BoxFut<Vec<SessionRecord>> {
        let records = self
            .ids
            .iter()
            .map(|id| SessionRecord {
                id: id.clone(),
                updated_at: 0,
                blank: true,
                events: Vec::new(),
            })
            .collect();
        Box::pin(async move { records })
    }

    fn read(&self, id: &str) -> BoxFut<Option<SessionRecord>> {
        let found = self
            .ids
            .iter()
            .find(|known| *known == id)
            .map(|id| SessionRecord {
                id: id.clone(),
                updated_at: 0,
                blank: true,
                events: Vec::new(),
            });
        Box::pin(async move { found })
    }

    fn create(&self, id: &str) -> BoxFut<Result<SessionRecord, RemoteError>> {
        let id = id.to_owned();
        Box::pin(async move {
            Err(RemoteError::Failed(format!(
                "FixedCatalog does not create sessions (`{id}`)"
            )))
        })
    }
}

/// A store whose writes always fail, for the "retry" arm of the error mapping.
///
/// The two shipped stores fail only as "unavailable", so without this one the `Failed` variant of
/// [`PreferenceError`] — the one a disk-backed store produces — would never be exercised.
struct FailingPreferences;

impl WorkspacePreferences for FailingPreferences {
    fn pinned_session_ids(&self) -> Vec<String> {
        Vec::new()
    }

    fn archived_session_ids(&self) -> Vec<String> {
        Vec::new()
    }

    fn pin_session(&self, _session_id: &str) -> Result<bool, PreferenceError> {
        Err(PreferenceError::failed("the disk is full"))
    }

    fn unpin_session(&self, _session_id: &str) -> Result<bool, PreferenceError> {
        Err(PreferenceError::failed("the disk is full"))
    }

    fn archive_session(&self, _session_id: &str) -> Result<bool, PreferenceError> {
        Err(PreferenceError::failed("the disk is full"))
    }

    fn unarchive_session(&self, _session_id: &str) -> Result<bool, PreferenceError> {
        Err(PreferenceError::failed("the disk is full"))
    }
}

/// A harness holding one registry, one preference store, and one registry of methods.
struct Harness {
    workspaces: Arc<WorkspaceRegistry>,
    preferences: Arc<InMemoryPreferences>,
    registry: RemoteRegistry,
}

impl Harness {
    /// A harness over the given sessions, with the sessions registered before the methods so the
    /// paths below can name them.
    fn new(sessions: &[&str]) -> Self {
        let workspaces = Arc::new(WorkspaceRegistry::new());
        let preferences = Arc::new(InMemoryPreferences::new());
        let catalog: Arc<dyn SessionCatalog> = Arc::new(FixedCatalog::new(sessions));
        let mut registry = RemoteRegistry::new();
        register_workspace_extra_methods(
            &mut registry,
            &workspaces,
            &(Arc::clone(&preferences) as Arc<dyn WorkspacePreferences>),
            &catalog,
        );
        Self {
            workspaces,
            preferences,
            registry,
        }
    }

    /// A harness with three registered workspaces — `a`, `b`, `c` — in that order.
    fn with_workspaces(sessions: &[&str]) -> Self {
        let harness = Self::new(sessions);
        for id in ["a", "b", "c"] {
            harness
                .workspaces
                .register(id, format!("/projects/{id}"), None, 1_000)
                .expect("the workspace registers");
        }
        harness
    }

    async fn dispatch(
        &self,
        method: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, RemoteError> {
        self.registry
            .dispatch(
                NAMESPACE,
                method,
                RemoteCall {
                    args,
                    peer: "test".to_owned(),
                },
            )
            .await
    }

    fn order(&self) -> Vec<String> {
        self.workspaces
            .list()
            .into_iter()
            .map(|workspace| workspace.id)
            .collect()
    }
}

/// The `workspace_ids` of an order response.
fn ids_of(value: &serde_json::Value) -> Vec<String> {
    value["workspace_ids"]
        .as_array()
        .expect("`workspace_ids` is an array")
        .iter()
        .map(|id| id.as_str().expect("an id is a string").to_owned())
        .collect()
}

/// The `workspace.session_ids` of a `WorkspaceValue`.
fn session_ids_of(value: &serde_json::Value) -> Vec<String> {
    value["workspace"]["session_ids"]
        .as_array()
        .expect("`session_ids` is an array")
        .iter()
        .map(|id| id.as_str().expect("an id is a string").to_owned())
        .collect()
}

/// The `pinned_session_ids` of a `WorkspacePinValue`.
fn pinned_of(value: &serde_json::Value) -> Vec<String> {
    value["pinned_session_ids"]
        .as_array()
        .expect("`pinned_session_ids` is an array")
        .iter()
        .map(|id| id.as_str().expect("an id is a string").to_owned())
        .collect()
}

/// The `archived_session_ids` of a `WorkspaceArchiveValue`.
fn archived_of(value: &serde_json::Value) -> Vec<String> {
    value["archived_session_ids"]
        .as_array()
        .expect("`archived_session_ids` is an array")
        .iter()
        .map(|id| id.as_str().expect("an id is a string").to_owned())
        .collect()
}

// ---- rename -----------------------------------------------------------------------------------

#[tokio::test]
async fn rename_changes_the_title_a_listing_reports() {
    let harness = Harness::with_workspaces(&[]);

    let renamed = harness
        .dispatch(
            RENAME_METHOD,
            serde_json::json!({
                "workspaceId": "a",
                "title": "Project Alpha",
            }),
        )
        .await
        .expect("the rename succeeds");

    assert_eq!(renamed["workspace"]["title"], "Project Alpha");
    assert_eq!(
        harness.workspaces.get("a").expect("still registered").title,
        "Project Alpha",
        "the registry is what `workspace.list` reads, so the change must land there"
    );
}

#[tokio::test]
async fn rename_trims_and_refuses_a_blank_title() {
    let harness = Harness::with_workspaces(&[]);

    let padded = harness
        .dispatch(
            RENAME_METHOD,
            serde_json::json!({
                "workspaceId": "a",
                "title": "  Trimmed  ",
            }),
        )
        .await
        .expect("the rename succeeds");
    assert_eq!(padded["workspace"]["title"], "Trimmed");

    let error = harness
        .dispatch(
            RENAME_METHOD,
            serde_json::json!({
                "workspaceId": "a",
                "title": "   ",
            }),
        )
        .await
        .expect_err("a blank title is refused");
    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error}"
    );
}

#[tokio::test]
async fn rename_refuses_a_title_another_workspace_holds() {
    let harness = Harness::with_workspaces(&[]);

    let error = harness
        .dispatch(
            RENAME_METHOD,
            serde_json::json!({
                "workspaceId": "a",
                "title": "b",
            }),
        )
        .await
        .expect_err("the name is taken by registration `b`");

    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error}"
    );
    assert_eq!(
        harness.workspaces.get("a").expect("registered").title,
        "a",
        "a refused rename must not change the title"
    );
}

#[tokio::test]
async fn rename_of_an_unchanged_title_does_not_move_updated_at() {
    let harness = Harness::with_workspaces(&[]);

    let value = harness
        .dispatch(
            RENAME_METHOD,
            serde_json::json!({
                "workspaceId": "a",
                "title": "a",
            }),
        )
        .await
        .expect("the rename succeeds");

    assert_eq!(
        value["workspace"]["updated_at"], 1_000,
        "a no-op must not report that the workspace changed"
    );
}

#[tokio::test]
async fn rename_of_an_unknown_workspace_is_refused() {
    let harness = Harness::with_workspaces(&[]);

    let error = harness
        .dispatch(
            RENAME_METHOD,
            serde_json::json!({
                "workspaceId": "nope",
                "title": "x",
            }),
        )
        .await
        .expect_err("the id is unknown");

    assert!(
        error.to_string().contains("unknown workspace `nope`"),
        "{error}"
    );
}

// ---- insertBefore -----------------------------------------------------------------------------

#[tokio::test]
async fn insert_before_places_a_workspace_ahead_of_its_anchor() {
    let harness = Harness::with_workspaces(&[]);

    let value = harness
        .dispatch(
            INSERT_BEFORE_METHOD,
            serde_json::json!({
                "workspaceId": "c",
                "beforeWorkspaceId": "a",
            }),
        )
        .await
        .expect("the move succeeds");

    assert_eq!(ids_of(&value), vec!["c", "a", "b"]);
    assert_eq!(harness.order(), vec!["c", "a", "b"]);
}

#[tokio::test]
async fn insert_before_without_an_anchor_appends() {
    let harness = Harness::with_workspaces(&[]);

    let value = harness
        .dispatch(
            INSERT_BEFORE_METHOD,
            serde_json::json!({ "workspaceId": "a" }),
        )
        .await
        .expect("the move succeeds");

    assert_eq!(ids_of(&value), vec!["b", "c", "a"]);
    assert_eq!(harness.order(), vec!["b", "c", "a"]);
}

#[tokio::test]
async fn insert_before_an_unknown_workspace_is_refused() {
    let harness = Harness::with_workspaces(&[]);

    let error = harness
        .dispatch(
            INSERT_BEFORE_METHOD,
            serde_json::json!({
                "workspaceId": "nope",
                "beforeWorkspaceId": "a",
            }),
        )
        .await
        .expect_err("the moved id is unknown");

    assert!(
        error.to_string().contains("unknown workspace `nope`"),
        "{error}"
    );
}

#[tokio::test]
async fn insert_before_an_unknown_anchor_is_refused_and_leaves_the_order_alone() {
    let harness = Harness::with_workspaces(&[]);

    let error = harness
        .dispatch(
            INSERT_BEFORE_METHOD,
            serde_json::json!({
                "workspaceId": "c",
                "beforeWorkspaceId": "nope",
            }),
        )
        .await
        .expect_err("the anchor is unknown");

    assert!(
        error.to_string().contains("unknown workspace `nope`"),
        "{error}"
    );
    assert_eq!(harness.order(), vec!["a", "b", "c"], "nothing moved");
}

// ---- insertSessionBefore ----------------------------------------------------------------------

/// A harness whose workspace `a` accounts for `s1`, `s2`, `s3` in that order.
fn harness_with_sessions() -> Harness {
    let harness = Harness::with_workspaces(&["s1", "s2", "s3"]);
    for id in ["s1", "s2", "s3"] {
        harness
            .workspaces
            .attach_session("a", id, 1_000)
            .expect("the session attaches");
    }
    harness
}

#[tokio::test]
async fn insert_session_before_moves_a_session_ahead_of_its_anchor() {
    let harness = harness_with_sessions();

    let value = harness
        .dispatch(
            INSERT_SESSION_BEFORE_METHOD,
            serde_json::json!({
                "workspaceId": "a",
                "sessionId": "s3",
                "beforeSessionId": "s1",
            }),
        )
        .await
        .expect("the move succeeds");

    assert_eq!(session_ids_of(&value), vec!["s3", "s1", "s2"]);
    assert_eq!(
        harness.workspaces.get("a").expect("registered").session_ids,
        vec!["s3", "s1", "s2"]
    );
}

#[tokio::test]
async fn insert_session_before_without_an_anchor_appends() {
    let harness = harness_with_sessions();

    let value = harness
        .dispatch(
            INSERT_SESSION_BEFORE_METHOD,
            serde_json::json!({
                "workspaceId": "a",
                "sessionId": "s1",
            }),
        )
        .await
        .expect("the move succeeds");

    assert_eq!(session_ids_of(&value), vec!["s2", "s3", "s1"]);
}

#[tokio::test]
async fn insert_session_before_an_unaccounted_session_is_refused() {
    let harness = harness_with_sessions();

    let error = harness
        .dispatch(
            INSERT_SESSION_BEFORE_METHOD,
            serde_json::json!({
                "workspaceId": "a",
                "sessionId": "s2",
                "beforeSessionId": "stranger",
            }),
        )
        .await
        .expect_err("the anchor is not accounted by this workspace");
    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error}"
    );

    let error = harness
        .dispatch(
            INSERT_SESSION_BEFORE_METHOD,
            serde_json::json!({
                "workspaceId": "a",
                "sessionId": "stranger",
            }),
        )
        .await
        .expect_err("the moved session is not accounted");
    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error}"
    );

    assert_eq!(
        harness.workspaces.get("a").expect("registered").session_ids,
        vec!["s1", "s2", "s3"],
        "a refused move must leave the order untouched"
    );
}

#[tokio::test]
async fn insert_session_before_an_unknown_workspace_is_refused() {
    let harness = harness_with_sessions();

    let error = harness
        .dispatch(
            INSERT_SESSION_BEFORE_METHOD,
            serde_json::json!({
                "workspaceId": "nope",
                "sessionId": "s1",
            }),
        )
        .await
        .expect_err("the workspace is unknown");

    assert!(
        error.to_string().contains("unknown workspace `nope`"),
        "{error}"
    );
}

#[tokio::test]
async fn insert_session_before_its_own_id_is_a_no_op() {
    let harness = harness_with_sessions();

    let value = harness
        .dispatch(
            INSERT_SESSION_BEFORE_METHOD,
            serde_json::json!({
                "workspaceId": "a",
                "sessionId": "s2",
                "beforeSessionId": "s2",
            }),
        )
        .await
        .expect("the no-op succeeds");

    assert_eq!(session_ids_of(&value), vec!["s1", "s2", "s3"]);
}

// ---- pinSession / unpinSession ----------------------------------------------------------------

#[tokio::test]
async fn pinning_puts_the_most_recent_pin_first() {
    let harness = Harness::new(&["s1", "s2"]);

    harness
        .dispatch(PIN_SESSION_METHOD, serde_json::json!({ "sessionId": "s1" }))
        .await
        .expect("the pin succeeds");
    let value = harness
        .dispatch(PIN_SESSION_METHOD, serde_json::json!({ "sessionId": "s2" }))
        .await
        .expect("the pin succeeds");

    assert_eq!(pinned_of(&value), vec!["s2", "s1"]);
}

#[tokio::test]
async fn pinning_twice_does_not_reorder() {
    let harness = Harness::new(&["s1", "s2"]);

    harness
        .dispatch(PIN_SESSION_METHOD, serde_json::json!({ "sessionId": "s1" }))
        .await
        .expect("the pin succeeds");
    harness
        .dispatch(PIN_SESSION_METHOD, serde_json::json!({ "sessionId": "s2" }))
        .await
        .expect("the pin succeeds");
    let value = harness
        .dispatch(PIN_SESSION_METHOD, serde_json::json!({ "sessionId": "s1" }))
        .await
        .expect("a repeat succeeds");

    assert_eq!(
        pinned_of(&value),
        vec!["s2", "s1"],
        "a retry is not a second pin"
    );
}

#[tokio::test]
async fn pinning_an_unknown_session_is_refused() {
    let harness = Harness::new(&["s1"]);

    let error = harness
        .dispatch(
            PIN_SESSION_METHOD,
            serde_json::json!({ "sessionId": "nope" }),
        )
        .await
        .expect_err("the session is unknown");

    assert!(error.to_string().contains("no session `nope`"), "{error}");
    assert!(
        harness.preferences.pinned_session_ids().is_empty(),
        "a refused pin writes nothing"
    );
}

#[tokio::test]
async fn unpinning_removes_the_id_and_repeats_are_successful() {
    let harness = Harness::new(&["s1", "s2"]);
    harness
        .dispatch(PIN_SESSION_METHOD, serde_json::json!({ "sessionId": "s1" }))
        .await
        .expect("the pin succeeds");

    let value = harness
        .dispatch(
            UNPIN_SESSION_METHOD,
            serde_json::json!({ "sessionId": "s1" }),
        )
        .await
        .expect("the unpin succeeds");
    assert!(pinned_of(&value).is_empty());

    let repeated = harness
        .dispatch(
            UNPIN_SESSION_METHOD,
            serde_json::json!({ "sessionId": "s1" }),
        )
        .await
        .expect("unpinning an unpinned id is idempotent");
    assert!(pinned_of(&repeated).is_empty());
}

#[tokio::test]
async fn unpinning_an_id_no_session_holds_still_succeeds() {
    // Dropping an id cannot introduce an unknown one, so the removal must work even after the
    // session it named has gone; the alternative strands the entry forever.
    let harness = Harness::new(&["s1"]);

    let value = harness
        .dispatch(
            UNPIN_SESSION_METHOD,
            serde_json::json!({ "sessionId": "gone" }),
        )
        .await
        .expect("unpinning an absent id is a no-op");

    assert!(pinned_of(&value).is_empty());
}

// ---- archiveSession / unarchiveSession --------------------------------------------------------

#[tokio::test]
async fn archiving_hides_a_session_without_touching_its_workspace_slot() {
    let harness = harness_with_sessions();

    let value = harness
        .dispatch(
            ARCHIVE_SESSION_METHOD,
            serde_json::json!({ "sessionId": "s2" }),
        )
        .await
        .expect("the archive succeeds");

    assert_eq!(archived_of(&value), vec!["s2"]);
    assert_eq!(
        harness.workspaces.get("a").expect("registered").session_ids,
        vec!["s1", "s2", "s3"],
        "the account keeps the slot so unarchiving restores the position"
    );
}

#[tokio::test]
async fn archiving_drops_a_pin() {
    let harness = Harness::new(&["s1"]);
    harness
        .dispatch(PIN_SESSION_METHOD, serde_json::json!({ "sessionId": "s1" }))
        .await
        .expect("the pin succeeds");

    harness
        .dispatch(
            ARCHIVE_SESSION_METHOD,
            serde_json::json!({ "sessionId": "s1" }),
        )
        .await
        .expect("the archive succeeds");

    assert!(
        harness.preferences.pinned_session_ids().is_empty(),
        "an id must not be both hidden and pinned"
    );
}

#[tokio::test]
async fn pinning_an_archived_session_is_refused() {
    let harness = Harness::new(&["s1"]);
    harness
        .dispatch(
            ARCHIVE_SESSION_METHOD,
            serde_json::json!({ "sessionId": "s1" }),
        )
        .await
        .expect("the archive succeeds");

    let error = harness
        .dispatch(PIN_SESSION_METHOD, serde_json::json!({ "sessionId": "s1" }))
        .await
        .expect_err("an archived session cannot be pinned");

    assert!(error.to_string().contains("is archived"), "{error}");
}

#[tokio::test]
async fn archiving_an_unknown_session_is_refused() {
    let harness = Harness::new(&["s1"]);

    let error = harness
        .dispatch(
            ARCHIVE_SESSION_METHOD,
            serde_json::json!({ "sessionId": "nope" }),
        )
        .await
        .expect_err("the session is unknown");

    assert!(error.to_string().contains("no session `nope`"), "{error}");
    assert!(
        harness.preferences.archived_session_ids().is_empty(),
        "a refused archive writes nothing"
    );
}

#[tokio::test]
async fn archiving_with_stop_activity_is_refused_rather_than_ignored() {
    let harness = Harness::new(&["s1"]);

    let error = harness
        .dispatch(
            ARCHIVE_SESSION_METHOD,
            serde_json::json!({
                "sessionId": "s1",
                "stopActivity": true,
            }),
        )
        .await
        .expect_err("this build cannot stop a session's work");

    assert!(error.to_string().contains("stopActivity"), "{error}");
    assert!(
        harness.preferences.archived_session_ids().is_empty(),
        "a refused archive writes nothing"
    );
}

#[tokio::test]
async fn archiving_twice_reports_the_same_set() {
    let harness = Harness::new(&["s1"]);

    harness
        .dispatch(
            ARCHIVE_SESSION_METHOD,
            serde_json::json!({ "sessionId": "s1" }),
        )
        .await
        .expect("the archive succeeds");
    let value = harness
        .dispatch(
            ARCHIVE_SESSION_METHOD,
            serde_json::json!({ "sessionId": "s1" }),
        )
        .await
        .expect("a repeat succeeds");

    assert_eq!(archived_of(&value), vec!["s1"]);
}

#[tokio::test]
async fn unarchiving_restores_the_session_to_its_recorded_position() {
    let harness = harness_with_sessions();
    harness
        .dispatch(
            ARCHIVE_SESSION_METHOD,
            serde_json::json!({ "sessionId": "s2" }),
        )
        .await
        .expect("the archive succeeds");

    let value = harness
        .dispatch(
            UNARCHIVE_SESSION_METHOD,
            serde_json::json!({ "sessionId": "s2" }),
        )
        .await
        .expect("the unarchive succeeds");

    assert!(archived_of(&value).is_empty());
    assert_eq!(
        harness.workspaces.get("a").expect("registered").session_ids,
        vec!["s1", "s2", "s3"],
        "the slot was never moved, so the position came back unchanged"
    );
}

#[tokio::test]
async fn unarchiving_an_id_that_was_not_archived_is_idempotent() {
    let harness = Harness::new(&["s1"]);

    let value = harness
        .dispatch(
            UNARCHIVE_SESSION_METHOD,
            serde_json::json!({ "sessionId": "s1" }),
        )
        .await
        .expect("unarchiving an unarchived id is a no-op");

    assert!(archived_of(&value).is_empty());
}

// ---- the storage seam -------------------------------------------------------------------------

#[test]
fn the_in_memory_store_does_not_survive_its_process_state() {
    // The documented limitation, as a test: a fresh store over the same session ids knows nothing
    // about what another store recorded. This is what a restart looks like to this module.
    let first = InMemoryPreferences::new();
    first.pin_session("s1").expect("the pin is recorded");
    first
        .archive_session("s2")
        .expect("the archive is recorded");
    assert_eq!(first.pinned_session_ids(), vec!["s1"]);

    let second = InMemoryPreferences::new();
    assert!(
        second.pinned_session_ids().is_empty(),
        "pins are lost when the process state is rebuilt"
    );
    assert!(
        second.archived_session_ids().is_empty(),
        "archives are lost when the process state is rebuilt"
    );
}

#[test]
fn the_unavailable_store_refuses_writes_and_reads_empty() {
    let store = UnavailablePreferences::default_reason();

    assert!(store.pinned_session_ids().is_empty());
    assert!(store.archived_session_ids().is_empty());
    assert!(matches!(
        store.pin_session("s1"),
        Err(PreferenceError::Unavailable { .. })
    ));
    assert!(matches!(
        store.archive_session("s1"),
        Err(PreferenceError::Unavailable { .. })
    ));
}

#[tokio::test]
async fn a_registration_without_a_store_refuses_pins_but_still_renames() {
    let workspaces = Arc::new(WorkspaceRegistry::new());
    workspaces
        .register("a", "/projects/a", None, 1_000)
        .expect("the workspace registers");
    let catalog: Arc<dyn SessionCatalog> = Arc::new(FixedCatalog::new(&["s1"]));
    let mut registry = RemoteRegistry::new();
    register_workspace_extra_methods_without_preferences(&mut registry, &workspaces, &catalog);

    let error = registry
        .dispatch(
            NAMESPACE,
            PIN_SESSION_METHOD,
            RemoteCall {
                args: serde_json::json!({ "sessionId": "s1" }),
                peer: "test".to_owned(),
            },
        )
        .await
        .expect_err("there is nowhere to record the pin");
    assert!(
        error.to_string().contains("nowhere to record its change"),
        "{error}"
    );

    let renamed = registry
        .dispatch(
            NAMESPACE,
            RENAME_METHOD,
            RemoteCall {
                args: serde_json::json!({ "workspaceId": "a", "title": "Renamed" }),
                peer: "test".to_owned(),
            },
        )
        .await
        .expect("the registry-backed method still works");
    assert_eq!(renamed["workspace"]["title"], "Renamed");
}

#[tokio::test]
async fn a_failing_store_surfaces_as_a_failure_the_caller_can_retry() {
    let workspaces = Arc::new(WorkspaceRegistry::new());
    let preferences: Arc<dyn WorkspacePreferences> = Arc::new(FailingPreferences);
    let catalog: Arc<dyn SessionCatalog> = Arc::new(FixedCatalog::new(&["s1"]));
    let mut registry = RemoteRegistry::new();
    register_workspace_extra_methods(&mut registry, &workspaces, &preferences, &catalog);

    let error = registry
        .dispatch(
            NAMESPACE,
            PIN_SESSION_METHOD,
            RemoteCall {
                args: serde_json::json!({ "sessionId": "s1" }),
                peer: "test".to_owned(),
            },
        )
        .await
        .expect_err("the store refused the write");

    assert!(matches!(error, RemoteError::Failed(_)), "{error}");
    assert!(error.to_string().contains("the disk is full"), "{error}");
}

#[test]
fn retain_visible_drops_archived_sessions_and_keeps_the_rest() {
    let preferences = InMemoryPreferences::new();
    preferences.archive_session("s2").expect("archived");
    let mut ids = vec!["s1".to_owned(), "s2".to_owned(), "s3".to_owned()];

    retain_visible(&mut ids, &preferences);

    assert_eq!(ids, vec!["s1", "s3"]);
}

// ---- access policy ----------------------------------------------------------------------------

#[tokio::test]
async fn every_method_declares_a_write_so_a_read_only_policy_refuses_it() {
    let mut registry = RemoteRegistry::new().with_policy(Arc::new(ReadOnly));
    let workspaces = Arc::new(WorkspaceRegistry::new());
    workspaces
        .register("a", "/projects/a", None, 1_000)
        .expect("the workspace registers");
    let preferences = Arc::new(InMemoryPreferences::new());
    let catalog: Arc<dyn SessionCatalog> = Arc::new(FixedCatalog::new(&["s1"]));
    register_workspace_extra_methods(
        &mut registry,
        &workspaces,
        &(Arc::clone(&preferences) as Arc<dyn WorkspacePreferences>),
        &catalog,
    );

    for (method, args) in [
        (
            RENAME_METHOD,
            serde_json::json!({"workspaceId": "a", "title": "x"}),
        ),
        (
            INSERT_BEFORE_METHOD,
            serde_json::json!({"workspaceId": "a"}),
        ),
        (
            INSERT_SESSION_BEFORE_METHOD,
            serde_json::json!({"workspaceId": "a", "sessionId": "s1"}),
        ),
        (
            ARCHIVE_SESSION_METHOD,
            serde_json::json!({"sessionId": "s1"}),
        ),
        (
            UNARCHIVE_SESSION_METHOD,
            serde_json::json!({"sessionId": "s1"}),
        ),
        (PIN_SESSION_METHOD, serde_json::json!({"sessionId": "s1"})),
        (UNPIN_SESSION_METHOD, serde_json::json!({"sessionId": "s1"})),
    ] {
        let error = registry
            .dispatch(
                NAMESPACE,
                method,
                RemoteCall {
                    args,
                    peer: "viewer".to_owned(),
                },
            )
            .await
            .expect_err("a read-only policy refuses a write");
        assert!(
            matches!(error, RemoteError::Refused { .. }),
            "{method}: {error}"
        );
    }

    assert_eq!(
        workspaces.get("a").expect("registered").title,
        "a",
        "nothing was written through the refused calls"
    );
    assert!(preferences.pinned_session_ids().is_empty());
    assert!(preferences.archived_session_ids().is_empty());
}

#[test]
fn the_declared_access_is_a_write() {
    assert_eq!(DECLARED_ACCESS, girder_core::Access::Write);
}
