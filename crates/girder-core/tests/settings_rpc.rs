//! Tests for the `settings.*` Remote methods.
//!
//! The properties under test, in the order they matter:
//!
//! - **set → get → list → delete round-trips** — a key handed to `set` must be the key `get` returns
//!   and the key `delete` removes, or the four methods are not one surface
//! - **a deleted key reads as absent** — after `delete`, `get` reports no such setting and the store
//!   holds nothing, because a "deleted" key that still reads back is a delete that did nothing
//! - **an unusable key is refused, and says which rule it broke** — empty, `..`, a path separator,
//!   surrounding whitespace, a control character, and an over-long key; a key is validated before a
//!   store ever sees it, so a file-backed store cannot be aimed at a path the caller did not name
//! - **a read-only mirror can read and cannot write** — `get` and `list` are `read`, the writes are
//!   `write`, `delete` is `destructive`. This is the property a read-only deployment depends on, and
//!   the one a missing declaration would silently break
//! - **the default store refuses rather than pretending** — no storage configured is reported, not
//!   hidden behind an in-memory map the next call could not confirm
//! - **the upstream gaps are declared, not faked** — `describe` and `openSettingsDocument` answer
//!   `NOT_IMPLEMENTED` naming the method, rather than a transport-level miss
//! - **the three write modes behave as upstream describes them** — `update` merges shallowly,
//!   `replace` replaces wholesale, `mutate` applies path edits in order and creates intermediates
//! - **an `expectedRevision` is refused rather than ignored** — this store keeps no revisions, and
//!   silently accepting one would let a client believe a conflict was detected
//!
//! Isolation: every test builds its own store and registry, so tests in one binary cannot see each
//! other's keys.
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

#[path = "../src/settings_rpc.rs"]
mod settings_rpc;

use std::sync::Arc;

use girder_core::{Policy, ReadOnly, RemoteCall, RemoteError, RemoteRegistry};
use serde_json::{Value, json};

use settings_rpc::{
    DELETE_METHOD, DESCRIBE_METHOD, GET_METHOD, LIST_METHOD, MAX_KEY_LEN, MUTATE_METHOD,
    MemorySettingsStore, NAMESPACE, OPEN_DOCUMENT_METHOD, REPLACE_METHOD, SET_METHOD,
    SettingsStore, UNIMPLEMENTED, UPDATE_METHOD, UnavailableSettingsStore,
    register_settings_methods,
};

// ---- fixtures ---------------------------------------------------------------------------------

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// A registry with a fresh in-memory store behind it.
fn registry() -> (RemoteRegistry, Arc<MemorySettingsStore>) {
    let store = Arc::new(MemorySettingsStore::new());
    let dynamic: Arc<dyn SettingsStore> = store.clone();
    let mut registry = RemoteRegistry::new();
    register_settings_methods(&mut registry, &dynamic);
    (registry, store)
}

/// A registry over the same store but behind a read-only policy.
fn read_only(store: &Arc<MemorySettingsStore>) -> RemoteRegistry {
    let dynamic: Arc<dyn SettingsStore> = store.clone();
    let policy: Arc<dyn Policy> = Arc::new(ReadOnly);
    let mut registry = RemoteRegistry::new().with_policy(policy);
    register_settings_methods(&mut registry, &dynamic);
    registry
}

async fn dispatch(
    registry: &RemoteRegistry,
    method: &str,
    args: Value,
) -> Result<Value, RemoteError> {
    registry.dispatch(NAMESPACE, method, call(args)).await
}

/// Sets one key and fails the test if the write is refused.
async fn set(registry: &RemoteRegistry, key: &str, value: Value) -> Value {
    dispatch(registry, SET_METHOD, json!({"key": key, "value": value}))
        .await
        .expect("a valid key is written")
}

// ---- the round trip ---------------------------------------------------------------------------

#[tokio::test]
async fn a_value_set_can_be_read_back() {
    let (registry, _store) = registry();

    let written = set(&registry, "provider.model", json!("deepseek-chat")).await;
    assert_eq!(written["key"], json!("provider.model"));
    assert_eq!(written["value"], json!("deepseek-chat"));

    let read = dispatch(&registry, GET_METHOD, json!({"key": "provider.model"}))
        .await
        .expect("the key that was just written is readable");

    assert_eq!(read["key"], json!("provider.model"));
    assert_eq!(read["value"], json!("deepseek-chat"));
}

#[tokio::test]
async fn every_json_shape_survives_the_round_trip() {
    // The store keeps `serde_json::Value`, so a number must not come back as a string and a nested
    // object must not be flattened: a client renders each shape differently.
    let (registry, _store) = registry();

    for (key, value) in [
        ("a.string", json!("text")),
        ("a.number", json!(42)),
        ("a.float", json!(1.5)),
        ("a.boolean", json!(true)),
        ("a.null", Value::Null),
        ("a.array", json!([1, "two", null])),
        ("a.object", json!({"nested": {"deep": [1, 2]}})),
    ] {
        let written = set(&registry, key, value.clone()).await;
        assert_eq!(written["value"], value, "the write echoes what it stored");

        let read = dispatch(&registry, GET_METHOD, json!({"key": key}))
            .await
            .expect("a written key is readable");
        assert_eq!(read["value"], value, "`{key}` round-trips unchanged");
    }
}

#[tokio::test]
async fn an_overwrite_returns_the_new_value() {
    let (registry, _store) = registry();

    set(&registry, "flag", json!(false)).await;
    set(&registry, "flag", json!(true)).await;

    let read = dispatch(&registry, GET_METHOD, json!({"key": "flag"}))
        .await
        .expect("the key exists");
    assert_eq!(read["value"], json!(true));
}

// ---- listing ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_listing_contains_a_key_that_was_set() {
    let (registry, _store) = registry();

    set(&registry, "zeta", json!(1)).await;
    set(&registry, "alpha", json!(2)).await;

    let listed = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("a listing needs no arguments");

    assert_eq!(listed["total"], json!(2));
    let items = listed["items"].as_array().expect("items is an array");
    // Sorted by key, because the store promises no order and a listing that reordered between calls
    // would move rows under a reader.
    assert_eq!(items[0]["key"], json!("alpha"));
    assert_eq!(items[1]["key"], json!("zeta"));
    assert_eq!(items[1]["value"], json!(1));
}

#[tokio::test]
async fn an_empty_store_lists_nothing() {
    let (registry, _store) = registry();

    let listed = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("a listing of an empty store succeeds");

    assert_eq!(listed["total"], json!(0));
    assert_eq!(listed["items"], json!([]));
}

// ---- deletion ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_deleted_key_is_gone_from_the_store_and_from_get() {
    let (registry, store) = registry();

    set(&registry, "temp", json!("value")).await;

    let deleted = dispatch(&registry, DELETE_METHOD, json!({"key": "temp"}))
        .await
        .expect("deleting a key that exists succeeds");
    assert_eq!(deleted["key"], json!("temp"));
    assert_eq!(deleted["deleted"], json!(true));

    // Both halves matter: the store no longer holds it, and the RPC surface reports the miss. A
    // delete that only cleared one of the two would leave a value a client could still reach.
    assert!(
        store.get("temp").is_none(),
        "the store dropped the key it deleted"
    );

    let error = dispatch(&registry, GET_METHOD, json!({"key": "temp"}))
        .await
        .expect_err("a deleted key does not read back");
    match error {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(
                message.contains("temp"),
                "the message names the key: {message}"
            );
        }
        other => panic!("reported {other:?} instead of invalid arguments"),
    }

    let listed = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("a listing succeeds");
    assert_eq!(listed["total"], json!(0), "the listing dropped it too");
}

#[tokio::test]
async fn deleting_a_key_nobody_set_is_refused() {
    let (registry, _store) = registry();

    // A miss-shaped success would tell the caller a value is gone when it never existed.
    let error = dispatch(&registry, DELETE_METHOD, json!({"key": "never-set"}))
        .await
        .expect_err("deleting an unknown key is refused");
    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
}

#[tokio::test]
async fn reading_a_key_nobody_set_is_refused_rather_than_null() {
    let (registry, _store) = registry();

    // `null` is a value a setting may legitimately hold, so an absent key must not be reported as
    // one: a client could not tell "unset" from "set to null".
    let error = dispatch(&registry, GET_METHOD, json!({"key": "absent"}))
        .await
        .expect_err("an absent key is refused");
    match error {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(message.contains("absent"), "names the key: {message}");
        }
        other => panic!("reported {other:?} instead of invalid arguments"),
    }
}

// ---- key validation ---------------------------------------------------------------------------

/// Asserts that `key` is refused by both `set` and `get`.
async fn assert_key_refused(registry: &RemoteRegistry, key: &str) {
    let set_error = dispatch(registry, SET_METHOD, json!({"key": key, "value": 1}))
        .await
        .expect_err("an unusable key is refused by set");
    assert!(
        matches!(set_error, RemoteError::InvalidArguments { .. }),
        "`{key}` was refused for the wrong reason: {set_error:?}"
    );

    let get_error = dispatch(registry, GET_METHOD, json!({"key": key}))
        .await
        .expect_err("an unusable key is refused by get");
    assert!(
        matches!(get_error, RemoteError::InvalidArguments { .. }),
        "`{key}` was refused for the wrong reason: {get_error:?}"
    );
}

#[tokio::test]
async fn an_empty_key_is_refused() {
    let (registry, _store) = registry();

    assert_key_refused(&registry, "").await;
    // Whitespace only addresses nothing, and is the shape a missing field takes after a trim.
    assert_key_refused(&registry, "   ").await;
}

#[tokio::test]
async fn a_key_containing_a_parent_directory_is_refused() {
    let (registry, _store) = registry();

    // The case that matters: a file-backed store would resolve this outside the directory it owns.
    assert_key_refused(&registry, "../../etc/passwd").await;
    assert_key_refused(&registry, "..").await;
    assert_key_refused(&registry, "a..b").await;
}

#[tokio::test]
async fn a_key_containing_a_path_separator_is_refused() {
    let (registry, _store) = registry();

    assert_key_refused(&registry, "a/b").await;
    assert_key_refused(&registry, "/absolute").await;
    assert_key_refused(&registry, "trailing/").await;
    assert_key_refused(&registry, "windows\\path").await;
}

#[tokio::test]
async fn degenerate_keys_are_refused_with_a_reason() {
    let (registry, _store) = registry();

    // Surrounding whitespace would make `"a "` and `"a"` two keys nothing distinguishes.
    assert_key_refused(&registry, " padded ").await;
    // A control character cannot be shown in any log that has to name the key.
    assert_key_refused(&registry, "line\nbreak").await;
    // `.` is a directory reference, not a name.
    assert_key_refused(&registry, ".").await;

    let too_long = "k".repeat(MAX_KEY_LEN + 1);
    assert_key_refused(&registry, &too_long).await;
}

#[tokio::test]
async fn a_refusal_names_the_rule_that_was_broken() {
    let (registry, _store) = registry();

    let error = dispatch(
        &registry,
        SET_METHOD,
        json!({"key": "../escape", "value": 1}),
    )
    .await
    .expect_err("a traversal key is refused");

    match error {
        RemoteError::InvalidArguments { message, .. } => {
            // The message has to say why, or a caller can only guess which rule it broke.
            assert!(
                message.contains("..") && message.contains("parent directory"),
                "the message explains the traversal rule: {message}"
            );
        }
        other => panic!("reported {other:?} instead of invalid arguments"),
    }
}

#[tokio::test]
async fn a_dotted_key_is_a_name_and_is_allowed() {
    let (registry, _store) = registry();

    // `provider.model` is this crate's own convention for a flat namespace; a dot inside a segment
    // is not a traversal, and refusing it would break the existing settings catalogue.
    set(&registry, "provider.model", json!("x")).await;
    set(&registry, "llm.deepseek.endpoint", json!("y")).await;

    let listed = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("a listing succeeds");
    assert_eq!(listed["total"], json!(2));
}

#[tokio::test]
async fn a_missing_key_field_is_reported_as_a_field_not_as_a_miss() {
    let (registry, _store) = registry();

    let error = dispatch(&registry, GET_METHOD, json!({}))
        .await
        .expect_err("a body without a key is refused");
    match error {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(
                message.contains("`key` is required"),
                "the message names the missing field: {message}"
            );
        }
        other => panic!("reported {other:?} instead of invalid arguments"),
    }
}

// ---- access policy ----------------------------------------------------------------------------

#[tokio::test]
async fn a_read_only_peer_can_read_settings_and_cannot_write_them() {
    let store = Arc::new(MemorySettingsStore::new());
    let writable = {
        let dynamic: Arc<dyn SettingsStore> = store.clone();
        let mut registry = RemoteRegistry::new();
        register_settings_methods(&mut registry, &dynamic);
        registry
    };
    set(&writable, "existing", json!("before")).await;

    let mirror = read_only(&store);

    // Reads are admitted.
    let read = dispatch(&mirror, GET_METHOD, json!({"key": "existing"}))
        .await
        .expect("a read-only peer may read a setting");
    assert_eq!(read["value"], json!("before"));

    let listed = dispatch(&mirror, LIST_METHOD, json!({}))
        .await
        .expect("a read-only peer may list settings");
    assert_eq!(listed["total"], json!(1));

    // Writes are refused, and the refusal says so rather than looking like a missing key.
    for (method, args) in [
        (SET_METHOD, json!({"key": "new", "value": 1})),
        (SET_METHOD, json!({"key": "existing", "value": 2})),
        (UPDATE_METHOD, json!({"ns": "existing", "patch": {"a": 1}})),
        (
            REPLACE_METHOD,
            json!({"ns": "existing", "section": {"a": 1}}),
        ),
        (
            MUTATE_METHOD,
            json!({"ns": "existing", "ops": [{"op": "set", "path": ["a"], "value": 1}]}),
        ),
        (DELETE_METHOD, json!({"key": "existing"})),
    ] {
        let error = dispatch(&mirror, method, args)
            .await
            .expect_err("a read-only peer may not change settings");
        assert!(
            matches!(error, RemoteError::Refused { .. }),
            "`{method}` was refused by something other than the policy: {error:?}"
        );
    }

    // And the value is untouched: a refusal that still wrote would be the worst outcome.
    assert_eq!(
        store.get("existing"),
        Some(json!("before")),
        "a refused write changed nothing"
    );
    assert!(store.get("new").is_none(), "no new key was created");
}

#[tokio::test]
async fn the_declarations_match_what_each_method_does() {
    // The declarations are invisible from a response, so they are checked through the policy that
    // reads them: if `delete` were declared a write instead of `destructive`, a deployment that
    // allowed writes and refused destructive calls would permit an irreversible removal.
    struct DenyDestructive;

    impl Policy for DenyDestructive {
        fn decide(&self, request: &girder_core::AccessRequest) -> girder_core::AccessDecision {
            match request.access {
                girder_core::Access::Destructive => girder_core::AccessDecision::Refuse {
                    reason: "destructive calls are refused here".to_owned(),
                },
                _ => girder_core::AccessDecision::Allow,
            }
        }
    }

    let store = Arc::new(MemorySettingsStore::new());
    let dynamic: Arc<dyn SettingsStore> = store.clone();
    let policy: Arc<dyn Policy> = Arc::new(DenyDestructive);
    let mut registry = RemoteRegistry::new().with_policy(policy);
    register_settings_methods(&mut registry, &dynamic);

    set(&registry, "kept", json!(1)).await;

    let error = dispatch(&registry, DELETE_METHOD, json!({"key": "kept"}))
        .await
        .expect_err("delete declares itself destructive");
    assert!(matches!(error, RemoteError::Refused { .. }));
    assert_eq!(store.get("kept"), Some(json!(1)), "nothing was removed");
}

#[tokio::test]
async fn a_read_declaration_lets_a_read_only_peer_past_the_policy() {
    // The failure this rules out: a read declared as a write would be refused to every read-only
    // peer, which is exactly the deployment that needs reads most.
    let store = Arc::new(MemorySettingsStore::new());
    store.set("visible", json!("yes")).expect("seed the store");

    let mirror = read_only(&store);
    dispatch(&mirror, GET_METHOD, json!({"key": "visible"}))
        .await
        .expect("a read-only peer reads");
}

// ---- the upstream write modes ----------------------------------------------------------------

#[tokio::test]
async fn update_merges_a_patch_into_the_stored_section() {
    let (registry, _store) = registry();

    dispatch(
        &registry,
        REPLACE_METHOD,
        json!({"ns": "llm", "section": {"model": "a", "temperature": 1}}),
    )
    .await
    .expect("a replace of a new namespace succeeds");

    let updated = dispatch(
        &registry,
        UPDATE_METHOD,
        json!({"ns": "llm", "patch": {"temperature": 2, "extra": true}}),
    )
    .await
    .expect("a patch merges");

    // Merge, not replacement: `model` is still there.
    assert_eq!(
        updated["value"],
        json!({"model": "a", "temperature": 2, "extra": true})
    );
    assert_eq!(updated["ns"], json!("llm"));
}

#[tokio::test]
async fn update_creates_the_namespace_when_it_is_absent() {
    let (registry, _store) = registry();

    let updated = dispatch(
        &registry,
        UPDATE_METHOD,
        json!({"ns": "fresh", "patch": {"a": 1}}),
    )
    .await
    .expect("a patch on an absent namespace creates it");
    assert_eq!(updated["value"], json!({"a": 1}));
}

#[tokio::test]
async fn update_is_shallow_so_a_nested_patch_replaces_the_nested_object() {
    let (registry, _store) = registry();

    dispatch(
        &registry,
        REPLACE_METHOD,
        json!({"ns": "n", "section": {"outer": {"keep": 1, "drop": 2}}}),
    )
    .await
    .expect("replace seeds the section");

    let updated = dispatch(
        &registry,
        UPDATE_METHOD,
        json!({"ns": "n", "patch": {"outer": {"fresh": 3}}}),
    )
    .await
    .expect("a nested patch merges one level");

    // Upstream's `update` merges the patch into the section; a deep merge would be a second,
    // unstated rule about arrays and nulls.
    assert_eq!(updated["value"], json!({"outer": {"fresh": 3}}));
}

#[tokio::test]
async fn replace_swaps_the_whole_section() {
    let (registry, _store) = registry();

    dispatch(
        &registry,
        REPLACE_METHOD,
        json!({"ns": "s", "section": {"a": 1, "b": 2}}),
    )
    .await
    .expect("the first replace succeeds");

    let replaced = dispatch(
        &registry,
        REPLACE_METHOD,
        json!({"ns": "s", "section": {"c": 3}}),
    )
    .await
    .expect("the second replace succeeds");

    // Wholesale: `a` and `b` are gone, which is the difference from `update`.
    assert_eq!(replaced["value"], json!({"c": 3}));
}

#[tokio::test]
async fn mutate_applies_path_edits_in_order_and_creates_intermediates() {
    let (registry, _store) = registry();

    dispatch(
        &registry,
        REPLACE_METHOD,
        json!({"ns": "m", "section": {"keep": 1}}),
    )
    .await
    .expect("replace seeds the section");

    let mutated = dispatch(
        &registry,
        MUTATE_METHOD,
        json!({
            "ns": "m",
            "ops": [
                {"op": "set", "path": ["deep", "nested", "value"], "value": 7},
                // Applied after the set, so the final value is the one the later op wrote: the
                // edits are a sequence, not a set.
                {"op": "set", "path": ["deep", "nested", "value"], "value": 8},
                {"op": "unset", "path": ["keep"]},
            ],
        }),
    )
    .await
    .expect("path edits apply");

    assert_eq!(mutated["value"], json!({"deep": {"nested": {"value": 8}}}));
}

#[tokio::test]
async fn mutate_unset_of_an_absent_path_is_not_an_error() {
    let (registry, _store) = registry();

    // `unset` asks for a state, and that state already holds. Reporting a miss would force every
    // caller to read the section before removing from it.
    let mutated = dispatch(
        &registry,
        MUTATE_METHOD,
        json!({"ns": "n", "ops": [{"op": "unset", "path": ["nothing", "here"]}]}),
    )
    .await
    .expect("unsetting an absent path is a no-op");
    assert_eq!(mutated["value"], json!({}));
}

#[tokio::test]
async fn mutate_refuses_a_path_through_a_non_object_with_the_position() {
    let (registry, _store) = registry();

    dispatch(
        &registry,
        REPLACE_METHOD,
        json!({"ns": "scalar", "section": {"a": 1}}),
    )
    .await
    .expect("replace seeds the section");

    let error = dispatch(
        &registry,
        MUTATE_METHOD,
        json!({"ns": "scalar", "ops": [{"op": "set", "path": ["a", "b"], "value": 1}]}),
    )
    .await
    .expect_err("a path through a number cannot be walked");

    match error {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(message.contains("ops[0]"), "names the op: {message}");
            assert!(message.contains('a'), "names the path: {message}");
        }
        other => panic!("reported {other:?} instead of invalid arguments"),
    }
}

#[tokio::test]
async fn a_write_mode_over_a_non_object_value_is_refused() {
    let (registry, _store) = registry();

    set(&registry, "scalar", json!("just a string")).await;

    let error = dispatch(
        &registry,
        UPDATE_METHOD,
        json!({"ns": "scalar", "patch": {"a": 1}}),
    )
    .await
    .expect_err("a string cannot be patched");

    match error {
        RemoteError::InvalidArguments { message, .. } => {
            // The message names the type and the method that can replace it, which is what the
            // caller needs to fix the call.
            assert!(message.contains("string"), "names the type: {message}");
            assert!(message.contains("set"), "names the way out: {message}");
        }
        other => panic!("reported {other:?} instead of invalid arguments"),
    }
}

#[tokio::test]
async fn a_write_mode_requires_its_payload() {
    let (registry, _store) = registry();

    for (method, body) in [
        (UPDATE_METHOD, json!({"ns": "n"})),
        (REPLACE_METHOD, json!({"ns": "n"})),
        (MUTATE_METHOD, json!({"ns": "n"})),
    ] {
        let error = dispatch(&registry, method, body)
            .await
            .expect_err("a write with no payload is refused");
        assert!(
            matches!(error, RemoteError::InvalidArguments { .. }),
            "`{method}` reported the wrong error: {error:?}"
        );
    }
}

#[tokio::test]
async fn a_write_mode_requires_a_namespace_and_validates_it() {
    let (registry, _store) = registry();

    let error = dispatch(&registry, REPLACE_METHOD, json!({"section": {"a": 1}}))
        .await
        .expect_err("a missing ns is refused");
    match error {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(message.contains("`ns` is required"), "{message}");
        }
        other => panic!("reported {other:?} instead of invalid arguments"),
    }

    // The namespace becomes a key, so it obeys the same rules as one.
    let error = dispatch(
        &registry,
        REPLACE_METHOD,
        json!({"ns": "../escape", "section": {"a": 1}}),
    )
    .await
    .expect_err("a traversal namespace is refused");
    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
}

// ---- optimistic concurrency -------------------------------------------------------------------

#[tokio::test]
async fn a_supplied_expected_revision_is_refused_rather_than_ignored() {
    let (registry, store) = registry();

    for method in [UPDATE_METHOD, REPLACE_METHOD, MUTATE_METHOD] {
        let body = match method {
            UPDATE_METHOD => json!({"ns": "n", "patch": {"a": 1}, "expectedRevision": 3}),
            REPLACE_METHOD => json!({"ns": "n", "section": {"a": 1}, "expectedRevision": 3}),
            _ => json!({"ns": "n", "ops": [], "expectedRevision": 3}),
        };
        let error = dispatch(&registry, method, body)
            .await
            .expect_err("an unhonourable revision is refused");
        match error {
            RemoteError::InvalidArguments { message, .. } => {
                assert!(
                    message.contains("expectedRevision") && message.contains('3'),
                    "`{method}` names the field and the value: {message}"
                );
            }
            other => panic!("`{method}` reported {other:?}"),
        }
    }

    // Nothing was written by any of the three: a refusal that still wrote would be the one failure
    // a client could not see.
    assert!(store.is_empty(), "no refused call wrote anything");
}

// ---- the store seam ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fresh_in_memory_store_starts_empty() {
    // The limitation the documentation states, as a checkable property: the map lives in the
    // process, so a new one — what a restart produces — holds nothing.
    let store = MemorySettingsStore::new();
    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
    assert!(store.list().is_empty());
    assert!(store.get("anything").is_none());
}

#[tokio::test]
async fn the_default_store_refuses_every_write_and_says_why() {
    let store: Arc<dyn SettingsStore> = Arc::new(UnavailableSettingsStore::default_reason());
    let mut registry = RemoteRegistry::new();
    register_settings_methods(&mut registry, &store);

    let error = dispatch(&registry, SET_METHOD, json!({"key": "k", "value": 1}))
        .await
        .expect_err("a store with no storage refuses");
    match error {
        // `Failed`, not `InvalidArguments`: the request was fine and the deployment is what is
        // missing, which is the distinction that tells a client not to retry on the wrong case.
        RemoteError::Failed(message) => {
            assert!(
                message.contains("SettingsStore"),
                "the message names what a host must supply: {message}"
            );
        }
        other => panic!("reported {other:?} instead of a store failure"),
    }

    // Reads answer with nothing rather than a refusal, because the trait's read has no error
    // channel; a miss on an unavailable store is still a reported miss.
    let error = dispatch(&registry, GET_METHOD, json!({"key": "k"}))
        .await
        .expect_err("nothing is stored");
    assert!(matches!(error, RemoteError::InvalidArguments { .. }));

    let listed = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("a listing of an unavailable store is empty");
    assert_eq!(listed["total"], json!(0));
}

#[tokio::test]
async fn a_store_that_cannot_remove_reports_the_gap() {
    // The trait's default `remove` refuses instead of doing nothing, so a store written against an
    // earlier version of the trait keeps compiling and its delete fails loudly.
    struct NoRemoval;

    impl SettingsStore for NoRemoval {
        fn get(&self, _key: &str) -> Option<Value> {
            Some(json!("present"))
        }

        fn set(&self, _key: &str, _value: Value) -> Result<(), String> {
            Ok(())
        }

        fn list(&self) -> Vec<(String, Value)> {
            vec![("k".to_owned(), json!("present"))]
        }
    }

    let store: Arc<dyn SettingsStore> = Arc::new(NoRemoval);
    let mut registry = RemoteRegistry::new();
    register_settings_methods(&mut registry, &store);

    let error = dispatch(&registry, DELETE_METHOD, json!({"key": "k"}))
        .await
        .expect_err("a store with no removal refuses the delete");
    match error {
        RemoteError::Failed(message) => {
            assert!(message.contains('k'), "names the key: {message}");
        }
        other => panic!("reported {other:?} instead of a store failure"),
    }
}

// ---- the declared gaps ------------------------------------------------------------------------

#[tokio::test]
async fn the_upstream_methods_this_build_lacks_are_declared_not_missing() {
    let (registry, _store) = registry();

    for method in UNIMPLEMENTED {
        let error = dispatch(&registry, method, json!({}))
            .await
            .expect_err("an unimplemented method is not served");
        match error {
            // `NOT_IMPLEMENTED` rather than an unknown method: the two mean different things to a
            // client author, and only this one says "the surface exists upstream".
            RemoteError::NotImplemented { method: name, .. } => {
                assert_eq!(&name, method, "the gap names the method that hit it");
            }
            other => panic!("`{method}` reported {other:?} instead of not-implemented"),
        }
    }

    // Exactly the two documented gaps, no more: a method that quietly stopped being implemented
    // would otherwise vanish from this list without a test noticing.
    assert_eq!(UNIMPLEMENTED, &[DESCRIBE_METHOD, OPEN_DOCUMENT_METHOD]);
    let declared = registry.unimplemented();
    let names: Vec<&str> = declared
        .iter()
        .filter(|(namespace, _)| namespace == NAMESPACE)
        .map(|(_, method)| method.as_str())
        .collect();
    assert_eq!(names, UNIMPLEMENTED);
}

#[tokio::test]
async fn the_implemented_methods_are_all_reachable() {
    let (registry, _store) = registry();

    let names: Vec<String> = registry
        .names()
        .into_iter()
        .filter(|(namespace, _)| namespace == NAMESPACE)
        .map(|(_, method)| method)
        .collect();

    for method in [
        GET_METHOD,
        SET_METHOD,
        LIST_METHOD,
        DELETE_METHOD,
        UPDATE_METHOD,
        REPLACE_METHOD,
        MUTATE_METHOD,
    ] {
        assert!(
            names.iter().any(|name| name == method),
            "`{method}` is registered; names are {names:?}"
        );
    }

    // The gap names are not registered as handlers: a declared gap and a handler for the same name
    // would mean the dispatch order decides what a client gets.
    for method in UNIMPLEMENTED {
        assert!(
            !names.iter().any(|name| name == method),
            "`{method}` must not have a handler"
        );
    }
}
