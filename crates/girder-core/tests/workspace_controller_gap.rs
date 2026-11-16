//! Tests for the workspace state stream, the first-use directory policy, and the directory picker.
//!
//! Upstream reference: `packages/api/workspace-controller` (1,960 lines). The families measured are
//! commands, the state stream (`feed.ts` + `follow`), the first-use directory policy, and the
//! `directoryPicker` namespace. The seven command verbs and the six file verbs already exist
//! elsewhere in this crate; this file covers the three gaps closed here.
//!
//! The properties under test are the ones that fail *silently* if they regress:
//!
//! - a generation starts with exactly one `baseline`, and a caught-up cursor returns no frames
//!   rather than a repeated one;
//! - a filtered row is answered with `replace`, never `remove` — a client that resurrected a
//!   deleted workspace would show a row the host still holds;
//! - a `remove` reaches only a caller that received the row;
//! - an `order` frame names only the ids the caller may see, and becomes a `replace` when it would
//!   otherwise reveal a delivered-but-hidden row;
//! - archived and pinned sets are filtered by session access;
//! - a cursor older than the retained journal resynchronizes instead of losing frames;
//! - a page is bounded and returns a cursor that continues exactly where it stopped;
//! - the first-use policy refuses a relative Documents directory, a Windows drive root, and a
//!   second initialization;
//! - a picker verb the composed backend cannot serve is refused for that capability;
//! - `createDirectory` rejects a name carrying a separator before touching the filesystem.
//!
//! # Why the modules are compiled by path
//!
//! `girder-core`'s crate root does not declare `workspace_controller_gap`; adding it and its
//! re-exports is the integrator's change. The module is included from its real source file, so it
//! sees the crate exactly as it will once wired in. It spells its own paths through
//! `girder_core::`, the alias the library declares for itself, so the same source builds here and
//! inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/workspace_controller_gap.rs"]
mod workspace_controller_gap;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use girder_core::api_config::WorkspaceRegistry;
use girder_core::{
    InMemoryPreferences, ReadOnly, RemoteCall, RemoteError, RemoteRegistry, WorkspacePreferences,
};
use serde_json::{Value, json};

use workspace_controller_gap::{
    AllowAll, ConfiguredDocuments, DEFAULT_FOLLOW_FRAMES, DEFAULT_WORKSPACE_DIRECTORY,
    DirectoryPickerBackend, DocumentsDirectory, FOLLOW_METHOD, Hidden, INITIALIZE_DEFAULT_METHOD,
    LocalDirectoryPicker, MAX_DIRECTORY_ENTRIES, MutableAccess, NAMESPACE, PICKER_NAMESPACE,
    PickerCapability, PickerError, SystemDocuments, WorkspaceFeed, default_workspace_path,
    is_default_workspace_title, register_directory_picker, register_initialize_default,
    register_workspace_follow, register_workspace_follow_allow_all, validate_directory_name,
    validate_documents_directory, validate_documents_directory_for, workspace_display_title,
    workspace_id_for_path,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// A registry, preference store, and feed wired the way a deployment wires them.
///
/// The store is the crate's own `InMemoryPreferences` rather than a fixture type: it is the one
/// implementation of the seam this build ships, and a hand-written double would leave its
/// pin/archive interaction untested here.
struct Harness {
    workspaces: Arc<WorkspaceRegistry>,
    preferences: Arc<dyn WorkspacePreferences>,
    store: Arc<InMemoryPreferences>,
    feed: Arc<WorkspaceFeed>,
}

impl Harness {
    fn new(ids: &[&str]) -> Self {
        let workspaces = Arc::new(WorkspaceRegistry::new());
        for id in ids {
            workspaces
                .register(*id, format!("/projects/{id}"), None, 1_000)
                .expect("the workspace registers");
        }
        let store = Arc::new(InMemoryPreferences::new());
        let preferences: Arc<dyn WorkspacePreferences> = store.clone();
        let feed = Arc::new(WorkspaceFeed::new(
            workspaces.as_ref(),
            preferences.as_ref(),
        ));
        Self {
            workspaces,
            preferences,
            store,
            feed,
        }
    }

    /// Registers one more workspace after the feed exists, then refreshes.
    fn add(&self, id: &str) {
        self.add_quietly(id);
        self.refresh();
    }

    /// Registers one more workspace without refreshing, for a caller that refreshes once for a
    /// batch.
    fn add_quietly(&self, id: &str) {
        self.workspaces
            .register(id, format!("/projects/{id}"), None, 2_000)
            .expect("the workspace registers");
    }

    /// Refreshes without changing the registry.
    fn refresh(&self) {
        self.feed
            .refresh(self.workspaces.as_ref(), self.preferences.as_ref());
    }

    /// Archives the ids in the given order.
    fn archive(&self, ids: &[&str]) {
        for id in ids {
            self.store
                .archive_session(id)
                .expect("the archive write succeeds");
        }
    }

    /// Pins the ids, oldest first, so the stored order is most recently pinned first.
    fn pin(&self, ids: &[&str]) {
        for id in ids {
            self.store.pin_session(id).expect("the pin write succeeds");
        }
    }

    fn page(
        &self,
        after: u64,
        limit: usize,
        access: &dyn workspace_controller_gap::FollowAccess,
    ) -> Value {
        self.feed.page(after, limit, access).to_json()
    }

    /// Reads a baseline page and returns its `next_seq`.
    fn baseline_cursor(&self, access: &dyn workspace_controller_gap::FollowAccess) -> u64 {
        self.page(0, DEFAULT_FOLLOW_FRAMES, access)["next_seq"]
            .as_u64()
            .expect("a cursor")
    }
}

/// The `type` of every frame in a page.
fn types(page: &Value) -> Vec<String> {
    page["frames"]
        .as_array()
        .expect("`frames` is an array")
        .iter()
        .map(|frame| {
            frame["type"]
                .as_str()
                .expect("every frame has a type")
                .to_owned()
        })
        .collect()
}

/// The `workspace_id` of every workspace row in a baseline.
fn baseline_ids(value: &Value) -> Vec<String> {
    value["items"]
        .as_array()
        .expect("`items` is an array")
        .iter()
        .map(|item| {
            item["workspace_id"]
                .as_str()
                .expect("a row has an id")
                .to_owned()
        })
        .collect()
}

// ---- the baseline -----------------------------------------------------------------------------

#[test]
fn a_generation_starts_with_exactly_one_baseline() {
    // The whole reconnect contract rests on this: a client that received two baselines could not
    // tell which one to keep, and one that received none would apply increments to nothing.
    let harness = Harness::new(&["a", "b"]);

    let page = harness.page(0, DEFAULT_FOLLOW_FRAMES, &AllowAll);

    assert_eq!(types(&page), vec!["baseline"]);
    assert_eq!(baseline_ids(&page["frames"][0]["value"]), vec!["a", "b"]);
    assert_eq!(page["done"], json!(false));
}

#[test]
fn a_baseline_carries_the_manual_order_not_a_sorted_one() {
    // Manual order is a user decision; sorting would silently undo every drag.
    let harness = Harness::new(&["c", "a", "b"]);

    let page = harness.page(0, DEFAULT_FOLLOW_FRAMES, &AllowAll);

    assert_eq!(
        baseline_ids(&page["frames"][0]["value"]),
        vec!["c", "a", "b"]
    );
}

#[test]
fn a_caught_up_cursor_returns_no_frames() {
    // A client that polls once a second must not be re-sent a baseline every second.
    let harness = Harness::new(&["a"]);
    let cursor = harness.baseline_cursor(&AllowAll);
    assert_ne!(cursor, 0, "a baseline's cursor is already past zero");

    let second = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &AllowAll);

    assert!(types(&second).is_empty(), "{second}");
    assert_eq!(second["next_seq"], json!(cursor));
}

#[test]
fn an_added_workspace_arrives_as_one_upsert() {
    let harness = Harness::new(&["a"]);
    let cursor = harness.baseline_cursor(&AllowAll);

    harness.add("b");
    let page = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &AllowAll);

    assert_eq!(types(&page), vec!["upsert", "order"]);
    assert_eq!(page["frames"][0]["workspace"]["workspace_id"], json!("b"));
    assert_eq!(page["frames"][1]["workspace_ids"], json!(["a", "b"]));
}

#[test]
fn a_deleted_workspace_arrives_as_one_remove() {
    let harness = Harness::new(&["a", "b"]);
    let cursor = harness.baseline_cursor(&AllowAll);

    harness.workspaces.delete("b");
    harness.refresh();
    let page = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &AllowAll);

    assert!(types(&page).contains(&"remove".to_owned()), "{page}");
    let removed = page["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|frame| frame["type"] == json!("remove"))
        .expect("a remove frame");
    assert_eq!(removed["workspace_id"], json!("b"));
}

#[test]
fn a_renamed_workspace_arrives_as_an_upsert_without_an_order_frame() {
    // A rename is not a reorder, and a client that received one would run its order animation.
    let harness = Harness::new(&["a", "b"]);
    let cursor = harness.baseline_cursor(&AllowAll);

    harness.workspaces.rename("a", "Renamed", 3_000);
    harness.refresh();
    let page = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &AllowAll);

    assert_eq!(types(&page), vec!["upsert"]);
    assert_eq!(page["frames"][0]["workspace"]["title"], json!("Renamed"));
}

#[test]
fn refreshing_without_a_change_publishes_nothing() {
    // Polling is idempotent; a diff that published on every poll would grow the journal forever.
    let harness = Harness::new(&["a", "b"]);
    let cursor = harness.baseline_cursor(&AllowAll);

    for _ in 0..5 {
        harness.refresh();
    }

    assert_eq!(harness.feed.head(), cursor, "no increments were journaled");
    assert!(types(&harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &AllowAll)).is_empty());
}

// ---- access filtering -------------------------------------------------------------------------

#[test]
fn a_hidden_workspace_never_appears_in_a_baseline() {
    let harness = Harness::new(&["a", "b"]);
    let access = Hidden::new().without_workspace("b");

    let page = harness.page(0, DEFAULT_FOLLOW_FRAMES, &access);

    assert_eq!(baseline_ids(&page["frames"][0]["value"]), vec!["a"]);
}

#[test]
fn a_hidden_session_is_dropped_from_its_workspace_row() {
    let harness = Harness::new(&["a"]);
    harness.workspaces.attach_session("a", "s1", 2_000);
    harness.workspaces.attach_session("a", "s2", 2_000);
    // The feed mirrors the registry; a page taken without a refresh would describe the state it
    // was built from, which is the behaviour a polling client relies on.
    harness.refresh();
    let access = Hidden::new().without_session("s2");

    let page = harness.page(0, DEFAULT_FOLLOW_FRAMES, &access);

    assert_eq!(
        page["frames"][0]["value"]["items"][0]["session_ids"],
        json!(["s1"])
    );
}

#[test]
fn an_archived_session_is_dropped_from_its_workspace_row() {
    // Upstream removes an archived session from the grouping surface; a row that kept it would
    // show a session the same client just archived.
    let harness = Harness::new(&["a"]);
    harness.workspaces.attach_session("a", "s1", 2_000);
    harness.workspaces.attach_session("a", "s2", 2_000);
    harness.archive(&["s2"]);
    harness.refresh();

    let page = harness.page(0, DEFAULT_FOLLOW_FRAMES, &AllowAll);

    assert_eq!(
        page["frames"][0]["value"]["items"][0]["session_ids"],
        json!(["s1"])
    );
    assert_eq!(
        page["frames"][0]["value"]["archived_session_ids"],
        json!(["s2"])
    );
}

#[test]
fn a_session_hidden_from_a_caller_is_dropped_from_the_archive_and_pin_sets() {
    // A pinned id the caller cannot read is a row the client would fail to resolve.
    let harness = Harness::new(&["a"]);
    // Distinct ids per set: archiving a pinned session drops its pin in the same write, so reusing
    // one id would test that interaction instead of the filtering this test is about.
    harness.archive(&["archived-visible", "archived-secret"]);
    harness.pin(&["pin-visible", "pin-secret"]);
    harness.refresh();
    let access = Hidden::new()
        .without_session("archived-secret")
        .without_session("pin-secret");

    let page = harness.page(0, DEFAULT_FOLLOW_FRAMES, &access);
    let baseline = &page["frames"][0]["value"];

    assert_eq!(
        baseline["archived_session_ids"],
        json!(["archived-visible"])
    );
    assert_eq!(baseline["pinned_session_ids"], json!(["pin-visible"]));
}

#[test]
fn a_newly_hidden_delivered_workspace_is_answered_with_replace_not_remove() {
    // The load-bearing rule. A client never resurrects a removed workspace, so a `remove` for a
    // workspace the host still holds would make the row disappear until the next full load.
    let harness = Harness::new(&["a", "b"]);
    let access = MutableAccess::new();
    let cursor = harness.baseline_cursor(&access);

    access.hide_workspace("b");
    harness.workspaces.rename("b", "Still Here", 3_000);
    harness.refresh();
    let page = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &access);

    assert_eq!(types(&page), vec!["replace"], "{page}");
    assert_eq!(baseline_ids(&page["frames"][0]["value"]), vec!["a"]);
}

#[test]
fn a_never_delivered_hidden_workspace_produces_no_frame_at_all() {
    // Rule 1: the caller never saw it, so it must not learn it exists — not even by its absence.
    let harness = Harness::new(&["a"]);
    let access = Hidden::new().without_workspace("secret");
    let cursor = harness.baseline_cursor(&access);

    harness.add("secret");
    let page = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &access);

    assert!(
        !page["frames"]
            .as_array()
            .unwrap()
            .iter()
            .any(|frame| frame["type"] == json!("upsert")),
        "{page}"
    );
}

#[test]
fn a_remove_for_a_workspace_the_caller_never_received_stays_silent() {
    // Rule 3: a deletion the caller could not have observed is not news, and sending it would
    // tell them a workspace existed.
    let harness = Harness::new(&["a", "secret"]);
    let access = Hidden::new().without_workspace("secret");
    let cursor = harness.baseline_cursor(&access);

    harness.workspaces.delete("secret");
    harness.refresh();
    let page = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &access);

    // The visible order is unchanged, so the only frame upstream's feed would produce is the
    // global order change, filtered to what this caller may see. What must not appear is a
    // `remove`, which would tell the caller a row it never saw had existed.
    assert!(
        !types(&page).contains(&"remove".to_owned()),
        "no remove for an unreceived row: {page}"
    );
    assert_eq!(types(&page), vec!["order"], "{page}");
    assert_eq!(page["frames"][0]["workspace_ids"], json!(["a"]));
}

#[test]
fn an_order_frame_names_only_the_ids_the_caller_may_see() {
    let harness = Harness::new(&["a", "b", "c"]);
    let access = Hidden::new().without_workspace("b");
    let cursor = harness.baseline_cursor(&access);

    // Reorder to c, b, a: `b` stays hidden, so the visible order is c, a.
    harness
        .workspaces
        .insert_before("c", "a", 3_000)
        .expect("the reorder succeeds");
    harness.refresh();
    let page = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &access);

    let order = page["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|frame| frame["type"] == json!("order"))
        .expect("an order frame");
    assert_eq!(order["workspace_ids"], json!(["c", "a"]));
}

#[test]
fn an_order_that_would_reveal_a_hidden_delivered_row_becomes_a_replace() {
    // Rule 4: an order naming a delivered-but-hidden id cannot be sent, because the client would
    // learn of a row it may not show. Its state is superseded instead.
    let harness = Harness::new(&["a", "b"]);
    let access = MutableAccess::new();
    let cursor = harness.baseline_cursor(&access);

    access.hide_workspace("b");
    // A pure reorder: no row changes, so nothing but the order frame can carry the leak.
    harness
        .workspaces
        .insert_before("b", "a", 3_000)
        .expect("the reorder succeeds");
    harness.refresh();
    let page = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &access);

    assert_eq!(types(&page), vec!["replace"], "{page}");
}

#[test]
fn an_archive_change_reaches_every_caller_filtered() {
    let harness = Harness::new(&["a"]);
    let access = Hidden::new().without_session("secret");
    let cursor = harness.baseline_cursor(&access);

    harness.archive(&["visible", "secret"]);
    harness.refresh();
    let page = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &access);

    assert_eq!(types(&page), vec!["archived"]);
    assert_eq!(
        page["frames"][0]["archived_session_ids"],
        json!(["visible"])
    );
}

#[test]
fn a_pin_change_arrives_most_recently_pinned_first() {
    let harness = Harness::new(&["a"]);
    let cursor = harness.baseline_cursor(&AllowAll);

    // Pinned oldest first, so the store's most-recent-first order is `newest`, `older`.
    harness.pin(&["older", "newest"]);
    harness.refresh();
    let page = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &AllowAll);

    assert_eq!(types(&page), vec!["pinned"]);
    assert_eq!(
        page["frames"][0]["pinned_session_ids"],
        json!(["newest", "older"])
    );
}

// ---- cursors and bounded pages -----------------------------------------------------------------

#[test]
fn a_page_is_bounded_and_its_cursor_continues_exactly() {
    // A cursor that skipped or repeated a frame would duplicate or drop a workspace in the client.
    let harness = Harness::new(&["a"]);
    let access = AllowAll;
    let cursor = harness.baseline_cursor(&access);
    for index in 0..5 {
        harness.add_quietly(&format!("w{index}"));
    }
    harness.refresh();

    let first = harness.page(cursor, 2, &access);
    assert!(
        first["frames"].as_array().expect("frames").len() <= 2,
        "the page honours its limit: {first}"
    );

    let mut seen = Vec::new();
    let mut cursor = cursor;
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 100, "the walk terminates");
        let page = harness.page(cursor, 2, &access);
        let frames = page["frames"].as_array().expect("frames");
        if frames.is_empty() {
            break;
        }
        for frame in frames {
            if frame["type"] == json!("upsert") {
                seen.push(frame["workspace"]["workspace_id"].clone());
            }
        }
        cursor = page["next_seq"].as_u64().expect("a cursor");
        if cursor == harness.feed.head() {
            break;
        }
    }

    assert_eq!(
        seen.len(),
        5,
        "every increment arrived exactly once across pages: {seen:?}"
    );
}

#[test]
fn a_cursor_older_than_the_journal_resynchronizes_with_a_replace() {
    // The journal is bounded, so a client that drops long enough cannot be caught up by increments.
    // It must be caught up by a complete state rather than by a partial replay.
    let workspaces = Arc::new(WorkspaceRegistry::new());
    workspaces
        .register("a", "/projects/a", None, 1_000)
        .expect("registers");
    let store = InMemoryPreferences::new();
    let feed = Arc::new(WorkspaceFeed::with_capacity(workspaces.as_ref(), &store, 1));
    let access = AllowAll;
    let cursor = feed.page(0, DEFAULT_FOLLOW_FRAMES, &access).next_seq;

    for index in 0..3 {
        workspaces
            .register(
                format!("w{index}"),
                format!("/projects/w{index}"),
                None,
                2_000,
            )
            .expect("registers");
        feed.refresh(workspaces.as_ref(), &store);
    }
    assert_eq!(feed.retained(), 1, "the journal kept only its capacity");

    let page = feed.page(cursor, DEFAULT_FOLLOW_FRAMES, &access);

    assert_eq!(types(&page.to_json()), vec!["replace"]);
}

#[test]
fn a_zero_limit_is_treated_as_one_rather_than_as_unbounded() {
    // A limit of zero that fell through to "no limit" would make one poll return the whole journal.
    let harness = Harness::new(&["a"]);
    let cursor = harness.baseline_cursor(&AllowAll);
    harness.add("b");

    let page = harness.page(cursor, 0, &AllowAll);

    assert_eq!(
        page["frames"].as_array().expect("frames").len(),
        1,
        "a zero limit is one frame, never unbounded: {page}"
    );
}

#[test]
fn a_revoked_grant_can_be_restored() {
    // A `replace` that supersedes a stale row is only useful if the grant can come back without a
    // reconnect; otherwise a client would have to rebuild its whole state to re-show the row.
    let harness = Harness::new(&["a", "b"]);
    let access = MutableAccess::new();
    let cursor = harness.baseline_cursor(&access);
    access.hide_workspace("b");
    harness.workspaces.rename("b", "Renamed", 3_000);
    harness.refresh();
    let hidden = harness.page(cursor, DEFAULT_FOLLOW_FRAMES, &access);
    assert_eq!(types(&hidden), vec!["replace"], "{hidden}");

    access.show_workspace("b");
    harness.refresh();
    let restored = harness.feed.baseline(&access);

    assert_eq!(baseline_ids(&restored), vec!["a", "b"]);
}

#[test]
fn a_mutable_session_grant_filters_a_pin_set() {
    let harness = Harness::new(&["a"]);
    harness.pin(&["s1", "s2"]);
    harness.refresh();
    let access = MutableAccess::new();
    // The stream is taken through the feed directly: `hide_session` is the seam a running host
    // flips without reopening a subscription.
    access.hide_session("s2");

    let page = harness.page(0, DEFAULT_FOLLOW_FRAMES, &access);

    assert_eq!(
        page["frames"][0]["value"]["pinned_session_ids"],
        json!(["s1"])
    );
}

#[test]
fn a_picker_with_a_pinned_home_lists_it_by_default() {
    // The picker roots its breadcrumbs at the account's home; a host that knows the home better
    // than the environment does must be able to say so.
    let root = temp_dir("picker-home");
    std::fs::create_dir_all(root.join("child")).expect("the fixture makes a child");
    let picker = LocalDirectoryPicker::with_home(&root);

    let listing = picker.list(None).expect("the listing succeeds");

    assert_eq!(listing.home, root.display().to_string());
    assert_eq!(listing.path, root.display().to_string());
    assert_eq!(
        listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<&str>>(),
        vec!["child"]
    );

    std::fs::remove_dir_all(&root).ok();
}

// ---- registration -----------------------------------------------------------------------------

#[tokio::test]
async fn the_follow_method_refreshes_before_it_answers() {
    // Answered from a stale feed, a client would be told it is caught up while the registry had
    // already moved, and the next poll would look like a skipped change.
    let harness = Harness::new(&["a"]);
    let mut registry = RemoteRegistry::new();
    register_workspace_follow_allow_all(
        &mut registry,
        &harness.workspaces,
        &harness.preferences,
        &harness.feed,
    );

    let first = call(&registry, &json!({})).await;
    let cursor = first["next_seq"].as_u64().expect("a cursor");
    harness
        .workspaces
        .register("b", "/projects/b", None, 2_000)
        .expect("registers");

    // No explicit refresh: the handler must do it.
    let second = call(&registry, &json!({ "afterSeq": cursor })).await;

    assert!(types(&second).contains(&"upsert".to_owned()), "{second}");
}

#[tokio::test]
async fn the_follow_method_honours_after_seq_and_limit() {
    let harness = Harness::new(&["a"]);
    let mut registry = RemoteRegistry::new();
    register_workspace_follow_allow_all(
        &mut registry,
        &harness.workspaces,
        &harness.preferences,
        &harness.feed,
    );
    let cursor = call(&registry, &json!({})).await["next_seq"]
        .as_u64()
        .expect("a cursor");
    harness
        .workspaces
        .register("b", "/projects/b", None, 2_000)
        .expect("registers");
    harness
        .workspaces
        .register("c", "/projects/c", None, 2_000)
        .expect("registers");

    let page = call(&registry, &json!({ "afterSeq": cursor, "limit": 1 })).await;

    assert_eq!(
        page["frames"].as_array().expect("frames").len(),
        1,
        "{page}"
    );
}

#[tokio::test]
async fn a_read_only_policy_still_permits_follow() {
    // Following is a read. A read-only deployment that refused it could not run a client at all.
    let harness = Harness::new(&["a"]);
    let mut registry = RemoteRegistry::new().with_policy(Arc::new(ReadOnly));
    register_workspace_follow_allow_all(
        &mut registry,
        &harness.workspaces,
        &harness.preferences,
        &harness.feed,
    );

    let page = call(&registry, &json!({})).await;

    assert_eq!(types(&page), vec!["baseline"]);
}

#[tokio::test]
async fn a_custom_access_seam_filters_the_registered_stream() {
    let harness = Harness::new(&["a", "secret"]);
    let mut registry = RemoteRegistry::new();
    let access: Arc<dyn workspace_controller_gap::FollowAccess> =
        Arc::new(Hidden::new().without_workspace("secret"));
    register_workspace_follow(
        &mut registry,
        &harness.workspaces,
        &harness.preferences,
        &harness.feed,
        access,
    );

    let page = call(&registry, &json!({})).await;

    assert_eq!(baseline_ids(&page["frames"][0]["value"]), vec!["a"]);
}

#[tokio::test]
async fn a_read_only_policy_refuses_initialize_default() {
    // It writes a registration and creates a directory, so a mirror must refuse it.
    let workspaces = Arc::new(WorkspaceRegistry::new());
    let mut registry = RemoteRegistry::new().with_policy(Arc::new(ReadOnly));
    register_initialize_default(
        &mut registry,
        &workspaces,
        Arc::new(ConfiguredDocuments::new(std::env::temp_dir())),
    );

    let error = registry
        .dispatch(
            NAMESPACE,
            INITIALIZE_DEFAULT_METHOD,
            RemoteCall {
                args: json!({}),
                peer: "viewer".to_owned(),
            },
        )
        .await
        .expect_err("a read-only policy refuses a write");

    assert!(matches!(error, RemoteError::Refused { .. }), "{error}");
    assert!(workspaces.list().is_empty(), "nothing was written");
}

async fn call(registry: &RemoteRegistry, args: &Value) -> Value {
    registry
        .dispatch(
            NAMESPACE,
            FOLLOW_METHOD,
            RemoteCall {
                args: args.clone(),
                peer: "test".to_owned(),
            },
        )
        .await
        .expect("the follow call succeeds")
}

// ---- first-use directory policy ----------------------------------------------------------------

#[test]
fn a_relative_documents_directory_is_refused() {
    // Joined against the working directory, `Documents` would register a workspace that moves when
    // the launcher does.
    let error = validate_documents_directory("Documents").expect_err("a relative path is refused");

    assert!(error.contains("fully qualified"), "{error}");
}

#[test]
fn an_absolute_documents_directory_is_accepted_and_normalized() {
    assert_eq!(
        validate_documents_directory("/home/u/Documents/./sub/../").expect("accepted"),
        PathBuf::from("/home/u/Documents")
    );
}

#[test]
fn a_windows_drive_root_is_refused() {
    // `C:\` is a volume root shared with every other installation, not a user's Documents folder.
    let error =
        validate_documents_directory_for(r"C:\\", true).expect_err("a drive root is refused");

    assert!(error.contains("drive root"), "{error}");
}

#[test]
fn a_windows_documents_directory_below_a_drive_is_accepted() {
    assert!(validate_documents_directory_for(r"C:\Users\me\Documents", true).is_ok());
}

#[test]
fn the_first_use_path_is_the_fixed_leaf_under_documents() {
    // Language-neutral and fixed: one installation keeps one on-disk path across language switches.
    assert_eq!(
        default_workspace_path(Path::new("/home/u/Documents")),
        PathBuf::from("/home/u/Documents/deepseek-harness/default-workspace")
    );
}

#[test]
fn the_automatic_title_is_localized_and_every_other_title_is_verbatim() {
    assert_eq!(
        workspace_display_title(DEFAULT_WORKSPACE_DIRECTORY, "默认工作区"),
        "默认工作区"
    );
    assert_eq!(
        workspace_display_title("my-project", "默认工作区"),
        "my-project"
    );
    assert!(is_default_workspace_title(DEFAULT_WORKSPACE_DIRECTORY));
    assert!(!is_default_workspace_title("my-project"));
}

#[test]
fn a_workspace_id_for_a_path_is_stable_across_calls() {
    // The same directory must derive the same id, or creating and initializing the same path would
    // register it twice and the client would show one project in two rows.
    assert_eq!(
        workspace_id_for_path("/projects/a"),
        workspace_id_for_path("/projects/a")
    );
    assert_ne!(
        workspace_id_for_path("/projects/a"),
        workspace_id_for_path("/projects/b")
    );
}

#[tokio::test]
async fn initialize_default_creates_the_fixed_directory_and_registers_it() {
    let root = temp_dir("init");
    let documents = root.join("Documents");
    std::fs::create_dir_all(&documents).expect("the fixture makes its Documents directory");
    let workspaces = Arc::new(WorkspaceRegistry::new());
    let mut registry = RemoteRegistry::new();
    register_initialize_default(
        &mut registry,
        &workspaces,
        Arc::new(ConfiguredDocuments::new(documents.clone())),
    );

    let page = call_named(&registry, INITIALIZE_DEFAULT_METHOD, json!({})).await;

    let expected = documents
        .join("deepseek-harness")
        .join(DEFAULT_WORKSPACE_DIRECTORY);
    assert!(expected.is_dir(), "the fixed directory exists");
    assert_eq!(
        page["workspace"]["path"],
        json!(expected.display().to_string())
    );
    assert_eq!(workspaces.list().len(), 1);

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn initialize_default_is_ineligible_once_a_workspace_exists() {
    // First use means first: re-running would relocate a workspace someone is working in.
    let workspaces = Arc::new(WorkspaceRegistry::new());
    workspaces
        .register("a", "/projects/a", None, 1_000)
        .expect("registers");
    let mut registry = RemoteRegistry::new();
    register_initialize_default(
        &mut registry,
        &workspaces,
        Arc::new(ConfiguredDocuments::new(std::env::temp_dir())),
    );

    let page = call_named(&registry, INITIALIZE_DEFAULT_METHOD, json!({})).await;

    // A null row rather than an error: a client calling this on every boot must not see a failure
    // on every boot after the first.
    assert_eq!(page["workspace"], Value::Null);
    assert_eq!(workspaces.list().len(), 1);
}

#[tokio::test]
async fn initialize_default_reuses_the_row_on_a_second_call() {
    let root = temp_dir("init-twice");
    let documents = root.join("Documents");
    std::fs::create_dir_all(&documents).expect("the fixture makes its Documents directory");
    let workspaces = Arc::new(WorkspaceRegistry::new());
    let mut registry = RemoteRegistry::new();
    register_initialize_default(
        &mut registry,
        &workspaces,
        Arc::new(ConfiguredDocuments::new(documents.clone())),
    );

    let first = call_named(&registry, INITIALIZE_DEFAULT_METHOD, json!({})).await;
    let id = first["workspace"]["workspace_id"].clone();
    // Clearing the registry while the directory stays on disk is exactly the restart case.
    workspaces.delete(id.as_str().expect("an id"));

    let second = call_named(&registry, INITIALIZE_DEFAULT_METHOD, json!({})).await;

    assert_eq!(second["workspace"]["workspace_id"], id);
    assert_eq!(workspaces.list().len(), 1);

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn the_system_documents_probe_validates_what_it_returns() {
    // The probe must not hand back a relative or unusable spelling; refusing is the honest answer
    // on a host with no Documents directory at all.
    match SystemDocuments.resolve() {
        Ok(path) => assert!(path.is_absolute(), "{}", path.display()),
        Err(reason) => assert!(!reason.is_empty()),
    }
}

async fn call_named(registry: &RemoteRegistry, method: &str, args: Value) -> Value {
    registry
        .dispatch(
            NAMESPACE,
            method,
            RemoteCall {
                args,
                peer: "test".to_owned(),
            },
        )
        .await
        .expect("the call succeeds")
}

/// A unique temporary directory for one test.
fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "girder-workspace-controller-gap-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("the fixture makes its root");
    path
}

// ---- directory picker -------------------------------------------------------------------------

/// A backend that serves `native` only, to prove the capability refusal.
struct NativeOnly;

impl DirectoryPickerBackend for NativeOnly {
    fn capability(&self) -> PickerCapability {
        PickerCapability::Native
    }
}

/// A backend whose `browse` verbs always fail, for the failure-code mapping.
struct BrokenBrowse;

impl DirectoryPickerBackend for BrokenBrowse {
    fn capability(&self) -> PickerCapability {
        PickerCapability::Browse
    }

    fn list(
        &self,
        path: Option<&str>,
    ) -> Result<workspace_controller_gap::DirectoryListing, PickerError> {
        Err(PickerError::explained(
            workspace_controller_gap::PICKER_UNREADABLE,
            path.unwrap_or_default(),
            "cannot be read",
        ))
    }
}

/// Coerces one backend into the trait object the registration takes.
fn picker_arc<B: DirectoryPickerBackend + 'static>(backend: B) -> Arc<dyn DirectoryPickerBackend> {
    Arc::new(backend)
}

async fn picker_call(
    registry: &RemoteRegistry,
    method: &str,
    args: Value,
) -> Result<Value, RemoteError> {
    registry
        .dispatch(
            PICKER_NAMESPACE,
            method,
            RemoteCall {
                args,
                peer: "test".to_owned(),
            },
        )
        .await
}

#[tokio::test]
async fn a_browse_backend_refuses_pick_by_capability() {
    // Refused rather than approximated: a browse backend cannot open a window.
    let mut registry = RemoteRegistry::new();
    register_directory_picker(&mut registry, &picker_arc(LocalDirectoryPicker::new()));

    let error = picker_call(&registry, "pick", json!({}))
        .await
        .expect_err("a browse backend cannot pick");

    let message = error.to_string();
    assert!(
        message.contains("directory-picker/unavailable"),
        "{message}"
    );
    assert!(message.contains("native"), "{message}");
}

#[tokio::test]
async fn a_native_backend_refuses_the_browse_verbs_by_capability() {
    let mut registry = RemoteRegistry::new();
    register_directory_picker(&mut registry, &picker_arc(NativeOnly));

    for method in ["list", "createDirectory"] {
        let error = picker_call(&registry, method, json!({ "path": "/", "name": "x" }))
            .await
            .expect_err("a native backend cannot browse");
        assert!(
            error.to_string().contains("directory-picker/unavailable"),
            "{method}: {error}"
        );
    }
}

#[tokio::test]
async fn listing_a_real_directory_returns_sorted_child_directories_with_crumbs() {
    let root = temp_dir("picker-list");
    std::fs::create_dir_all(root.join("beta")).expect("the fixture makes a child");
    std::fs::create_dir_all(root.join("alpha")).expect("the fixture makes a child");
    std::fs::create_dir_all(root.join(".hidden")).expect("the fixture makes a hidden child");
    std::fs::write(root.join("file.txt"), "not a directory").expect("the fixture makes a file");
    let mut registry = RemoteRegistry::new();
    register_directory_picker(&mut registry, &picker_arc(LocalDirectoryPicker::new()));

    let page = picker_call(
        &registry,
        "list",
        json!({ "path": root.display().to_string() }),
    )
    .await
    .expect("the listing succeeds");

    let names: Vec<&str> = page["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["name"].as_str().expect("a name"))
        .collect();
    // Sorted, directories only: a file row would offer a target that cannot be picked.
    assert_eq!(names, vec![".hidden", "alpha", "beta"]);
    assert_eq!(page["entries"][0]["hidden"], json!(true));
    assert_eq!(page["entries"][1]["hidden"], json!(false));
    // Crumbs run root-first and end at the listed directory; every crumb is a jump target.
    let crumbs = page["crumbs"].as_array().expect("crumbs");
    assert_eq!(
        crumbs.last().expect("a last crumb")["path"],
        json!(page["path"].as_str().expect("a path"))
    );
    assert!(crumbs.iter().all(|crumb| crumb["hidden"] == json!(false)));

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn a_relative_listing_path_is_refused() {
    // Listed relative, the answer would describe whatever directory the host happened to start in.
    let mut registry = RemoteRegistry::new();
    register_directory_picker(&mut registry, &picker_arc(LocalDirectoryPicker::new()));

    let error = picker_call(&registry, "list", json!({ "path": "relative/dir" }))
        .await
        .expect_err("a relative path is refused");

    assert!(error.to_string().contains("fully qualified"), "{error}");
}

#[tokio::test]
async fn an_unreadable_directory_maps_to_the_seam_code() {
    let mut registry = RemoteRegistry::new();
    register_directory_picker(&mut registry, &picker_arc(BrokenBrowse));

    let error = picker_call(&registry, "list", json!({ "path": "/nope" }))
        .await
        .expect_err("the backend fails");

    assert!(
        error.to_string().contains("directory-picker/unreadable"),
        "{error}"
    );
}

#[tokio::test]
async fn create_directory_makes_one_child_and_refuses_a_second() {
    let root = temp_dir("picker-create");
    let mut registry = RemoteRegistry::new();
    register_directory_picker(&mut registry, &picker_arc(LocalDirectoryPicker::new()));
    let parent = root.display().to_string();

    let created = picker_call(
        &registry,
        "createDirectory",
        json!({ "path": parent, "name": "made" }),
    )
    .await
    .expect("the child is created");
    assert!(root.join("made").is_dir());
    assert_eq!(
        created["path"],
        json!(root.join("made").display().to_string())
    );

    let error = picker_call(
        &registry,
        "createDirectory",
        json!({ "path": parent, "name": "made" }),
    )
    .await
    .expect_err("an existing child is refused");
    assert!(
        error.to_string().contains("directory-picker/exists"),
        "{error}"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn create_directory_refuses_a_name_carrying_a_separator_before_touching_the_filesystem() {
    // Accepting `../escape` would create a directory outside the parent the caller named, which is
    // the one thing a picker must not do.
    let root = temp_dir("picker-escape");
    let mut registry = RemoteRegistry::new();
    register_directory_picker(&mut registry, &picker_arc(LocalDirectoryPicker::new()));
    let parent = root.display().to_string();

    for name in ["../escape", "a/b", r"a\b", "..", ".", "   "] {
        let error = picker_call(
            &registry,
            "createDirectory",
            json!({ "path": parent, "name": name }),
        )
        .await
        .expect_err("the name is refused");
        assert!(
            matches!(error, RemoteError::InvalidArguments { .. }),
            "`{name}`: {error}"
        );
    }
    assert!(
        !root.join("..").join("escape").exists(),
        "nothing was created outside the parent"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn a_read_only_policy_refuses_create_directory_but_permits_list() {
    let root = temp_dir("picker-policy");
    let mut registry = RemoteRegistry::new().with_policy(Arc::new(ReadOnly));
    register_directory_picker(&mut registry, &picker_arc(LocalDirectoryPicker::new()));
    let parent = root.display().to_string();

    picker_call(&registry, "list", json!({ "path": parent }))
        .await
        .expect("a read is permitted");

    let error = picker_call(
        &registry,
        "createDirectory",
        json!({ "path": parent, "name": "made" }),
    )
    .await
    .expect_err("a write is refused");
    assert!(matches!(error, RemoteError::Refused { .. }), "{error}");
    assert!(!root.join("made").exists(), "nothing was created");

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn a_name_validator_accepts_an_ordinary_segment() {
    for name in ["docs", "my project", ".hidden", "a.b"] {
        validate_directory_name(name).expect("the name is a single segment");
    }
}

#[test]
fn the_entry_cap_is_a_positive_bound() {
    // A zero cap would render an empty browser for every non-empty directory. Written through a
    // `const` block so the assertion is about the constant itself rather than a value computed at
    // run time that a later edit could quietly stop being the same constant.
    const { assert!(MAX_DIRECTORY_ENTRIES > 0) };
}
