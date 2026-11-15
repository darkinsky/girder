//! End-to-end tests for configuration *reload*: does a written setting actually come back?
//!
//! A settings surface is only worth having if a value written at one moment is the value a reader
//! sees at the next one — across a `get`, across a fresh reader, and across a restart of the
//! process. Those are three different claims, and each can fail while the other two pass:
//!
//! - **write → read** can pass on a shared in-memory map and prove nothing about durability;
//! - **write → restart** can pass for a durable store while a long-lived reader keeps serving a
//!   snapshot it took at construction;
//! - **write → next call** can pass in a test that rebuilds its reader while the shipped runtime
//!   holds one instance for the life of the process.
//!
//! So the tests below are grouped by which of those claims they pin down, and every one asserts a
//! concrete value rather than "no error".
//!
//! # The durable store here is a fixture, and that is one of the findings
//!
//! `girder-core` ships exactly two `SettingsStore` implementations — `MemorySettingsStore`, a
//! process-local map, and `UnavailableSettingsStore`, which refuses every write. Neither keeps a
//! byte on disk, and `register_settings_methods` has no reload or watch path. The file-backed store
//! below is therefore written *in this test*, on top of the crate's own `write_text_atomic`, to
//! answer the one question the crate cannot answer for itself: **is the `SettingsStore` seam
//! sufficient for a deployment to keep and reload settings?** It is. What is missing is the
//! implementation, not the seam — and that gap is pinned by
//! [`the_shipped_memory_store_loses_every_setting_on_restart`] and
//! [`a_stored_setting_does_not_reach_a_tracker_that_was_built_once`] so it cannot drift silently.
//!
//! # Isolation
//!
//! Every test that touches a file builds its own scratch directory under the system temporary
//! directory, named with the process id and a per-process counter, so two test binaries running at
//! once cannot share one. Tests that need no file build their own in-memory store. No test reads
//! state another test wrote.
//!
//! # Why method names are literals
//!
//! This file exercises the *wire* names a client sends, so `"get"` and `"set"` are spelled out
//! rather than imported from `settings_rpc`. Importing them would let a rename break every client
//! while the test kept compiling.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting, and \
              the indexes are over documents the test itself built"
)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use girder_core::{
    ConfigSchema, ConfigValue, FieldRule, FieldSchema, MemorySettingsStore, Mode, RemoteCall,
    RemoteError, RemoteRegistry, SchemaRegistry, SettingSpec, SettingsStore, ToolLimit,
    ToolTracker, ToolVerdict, register_settings_methods, write_text_atomic,
};
use serde_json::{Value, json};

/// The namespace the settings methods are served under.
const NAMESPACE: &str = "settings";

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated scratch directory: `temp_dir()` plus pid plus a per-process counter.
fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-config-reload-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory is creatable");
    dir
}

/// Builds the `ConfigValue` a schema validates, from a JSON object literal.
fn map_of(value: &Value) -> ConfigValue {
    ConfigValue::Map(value.as_object().cloned().unwrap_or_default())
}

/// A `SettingsStore` that keeps every value in one JSON object on disk.
///
/// Written here rather than shipped, because the crate ships no durable store; see the module
/// documentation. It is a fixture only in *location* — every durability property the tests assert
/// comes from the crate's own [`write_text_atomic`], which stages a sibling file and renames it
/// over the target. That rename is what makes the concurrent test's claim true: a reader observes
/// the complete old document or the complete new one, never a half-written one.
struct JsonFileStore {
    path: PathBuf,
    /// Serialises the read-modify-write cycle inside this process.
    ///
    /// `write_text_atomic` makes one *replacement* atomic; it does not make "read the document,
    /// insert one key, write the document back" atomic, so two writers would each read the same old
    /// document and the second would drop the first's key. Cross-process writers would need a lock
    /// file on top ([`girder_core::with_lock`]); these tests are one process, and this comment is
    /// here so a reader does not mistake it for the cross-process guarantee.
    gate: Mutex<()>,
}

impl JsonFileStore {
    /// Opens the store at `path`. The file is created by the first write.
    fn open(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            gate: Mutex::new(()),
        }
    }

    /// Reads the whole document.
    ///
    /// A missing or empty file is an empty store, which is the state a deployment starts in. An
    /// unparsable one is an *error*: reporting it as empty would turn every stored setting into
    /// "unset", and the next write would silently destroy the file it could not read.
    fn load(&self) -> Result<BTreeMap<String, Value>, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BTreeMap::new());
            }
            Err(error) => {
                return Err(format!(
                    "`{}` could not be read: {error}",
                    self.path.display()
                ));
            }
        };
        if text.trim().is_empty() {
            return Ok(BTreeMap::new());
        }
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|error| format!("`{}` is not valid JSON: {error}", self.path.display()))?;
        let Value::Object(map) = parsed else {
            return Err(format!(
                "`{}` does not hold a JSON object, so it is not a settings document",
                self.path.display()
            ));
        };
        Ok(map.into_iter().collect())
    }

    /// Replaces the whole document atomically.
    fn store(&self, values: &BTreeMap<String, Value>) -> Result<(), String> {
        let text = serde_json::to_string_pretty(values)
            .map_err(|error| format!("settings could not be serialized: {error}"))?;
        write_text_atomic(&self.path, &text, Mode::PRIVATE).map_err(|error| error.to_string())
    }

    /// The document as a reader on disk sees it, for the assertions that check the file itself
    /// rather than the store's view of it.
    fn document_on_disk(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.path).expect("settings file exists"))
            .expect("settings file holds valid JSON")
    }
}

impl SettingsStore for JsonFileStore {
    fn get(&self, key: &str) -> Option<Value> {
        // `None` on a damaged document because this trait has no error channel here; the refusal a
        // caller can act on comes from the write path, which answers the parse error instead of
        // overwriting what it could not read.
        self.load().ok()?.get(key).cloned()
    }

    fn set(&self, key: &str, value: Value) -> Result<(), String> {
        let _guard = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut values = self.load()?;
        values.insert(key.to_owned(), value);
        self.store(&values)
    }

    fn list(&self) -> Vec<(String, Value)> {
        self.load()
            .map(|values| values.into_iter().collect())
            .unwrap_or_default()
    }

    fn remove(&self, key: &str) -> Result<bool, String> {
        let _guard = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut values = self.load()?;
        let present = values.remove(key).is_some();
        if present {
            self.store(&values)?;
        }
        Ok(present)
    }
}

/// A registry over `store`, with no access policy mounted.
fn registry(store: &Arc<dyn SettingsStore>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    register_settings_methods(&mut registry, store);
    registry
}

/// Dispatches one settings call.
async fn dispatch(
    registry: &RemoteRegistry,
    method: &str,
    args: Value,
) -> Result<Value, RemoteError> {
    registry
        .dispatch(
            NAMESPACE,
            method,
            RemoteCall {
                args,
                peer: "config-reload-e2e".to_owned(),
            },
        )
        .await
}

/// The seven methods this build registers under `settings`, as a client spells them.
const SEVEN_METHODS: [&str; 7] = [
    "get", "set", "list", "delete", "update", "replace", "mutate",
];

// ---- 1. a written value is the value that is read back ----------------------------------------

#[tokio::test]
async fn a_value_written_through_settings_set_is_returned_unchanged_by_get() {
    // The base round trip, asserted at wire level so the stored *type* is covered too: a store that
    // stringified a number or dropped a null would answer a `get` that no longer matches what the
    // writer sent, and a client would render the wrong field.
    let store: Arc<dyn SettingsStore> = Arc::new(MemorySettingsStore::new());
    let registry = registry(&store);

    for (key, value) in [
        ("provider.model", json!("deepseek-reasoner")),
        ("tools.maxCalls", json!(7)),
        ("tools.enabled", json!(true)),
        ("compaction.policy", json!({"at": 0.75, "keep": 4})),
        ("provider.temperature", json!(null)),
    ] {
        let written = dispatch(&registry, "set", json!({"key": key, "value": value}))
            .await
            .expect("a valid key and any JSON value are accepted");
        assert_eq!(written["key"], json!(key));
        assert_eq!(written["value"], value);

        let read = dispatch(&registry, "get", json!({"key": key}))
            .await
            .expect("the key just written is readable");
        assert_eq!(read["key"], json!(key), "the read names the key it read");
        assert_eq!(
            read["value"], value,
            "`{key}` must read back exactly what was written, including its type"
        );
    }

    // Five distinct keys were written, so five are stored. A store that silently deduplicated or
    // dropped the `null` would answer four, and the `null` case above would still have passed
    // because it is read immediately after its own write.
    assert_eq!(store.list().len(), 5);
}

// ---- 2. a value survives the store that wrote it ----------------------------------------------

#[test]
fn settings_survive_a_store_restart_when_the_store_is_durable() {
    // The core claim: a deployment writes a setting, the process exits, a new process reads it.
    // Modelled as "drop the store, construct a new one over the same path", which is what a restart
    // looks like to a file-backed store and what a reload looks like to a reader that rebuilds one.
    let dir = scratch("restart");
    let path = dir.join("settings.json");

    {
        let writer = JsonFileStore::open(&path);
        writer
            .set("provider.model", json!("deepseek-reasoner"))
            .expect("the first write creates the file");
        writer
            .set("tools.maxCalls", json!(3))
            .expect("a second key is added to the same document");
    } // The writer is dropped here; nothing left in it can serve the reads below.

    let reader = JsonFileStore::open(&path);
    assert_eq!(
        reader.get("provider.model"),
        Some(json!("deepseek-reasoner"))
    );
    assert_eq!(reader.get("tools.maxCalls"), Some(json!(3)));
    assert_eq!(
        reader.get("never.written"),
        None,
        "a key that was never written stays absent after a restart"
    );

    // The file itself, not only the store's view of it: a store with an in-memory cache would
    // satisfy the assertions above while the disk held nothing.
    assert_eq!(
        reader.document_on_disk(),
        json!({"provider.model": "deepseek-reasoner", "tools.maxCalls": 3}),
        "both keys must be in the document on disk"
    );

    // A removal is durable too, otherwise a value a user deleted comes back on the next start.
    assert_eq!(reader.remove("tools.maxCalls"), Ok(true));
    let after_restart = JsonFileStore::open(&path);
    assert_eq!(after_restart.get("tools.maxCalls"), None);
    assert_eq!(
        after_restart.get("provider.model"),
        Some(json!("deepseek-reasoner"))
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_store_opened_before_a_write_sees_the_new_value_on_its_next_read() {
    // The long-lived-reader half: the instance that reads is *older* than the write. Nothing may be
    // cached when the store is built, or a process that started before the edit keeps serving the
    // value the operator already changed — the failure that looks like "my setting is ignored".
    let dir = scratch("hot-read");
    let path = dir.join("settings.json");

    let writer = JsonFileStore::open(&path);
    let long_lived_reader = JsonFileStore::open(&path);
    writer
        .set("provider.model", json!("deepseek-chat"))
        .expect("written");

    assert_eq!(
        long_lived_reader.get("provider.model"),
        Some(json!("deepseek-chat")),
        "a reader built before the write must observe it, not a snapshot from construction"
    );

    // The change, then the *same* reader again: this is what "hot" means.
    writer
        .set("provider.model", json!("deepseek-reasoner"))
        .expect("rewritten");
    assert_eq!(
        long_lived_reader.get("provider.model"),
        Some(json!("deepseek-reasoner")),
        "the next read after a change must return the new value"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_shipped_memory_store_loses_every_setting_on_restart() {
    // FINDING, pinned rather than hidden. `MemorySettingsStore` is what the CLI wires (see
    // `register_settings_surface` in `crates/girder-cli/src/main.rs`), so this is the behaviour a
    // shipped deployment has today: a setting written through `settings.set` is gone on restart,
    // and no path exists for it to have reached the disk.
    //
    // The test asserts the *current* behaviour so that adding a durable store changes this test
    // deliberately instead of leaving a silent gap. It is not a specification.
    let dir = scratch("memory-only");
    let settings_path = dir.join("settings.json");

    let store = MemorySettingsStore::new();
    store
        .set("provider.model", json!("deepseek-reasoner"))
        .expect("the in-memory store accepts the write");
    assert_eq!(
        store.get("provider.model"),
        Some(json!("deepseek-reasoner"))
    );

    // "A new process" over the same path: the memory store wrote nothing, so there is nothing to
    // reload from.
    let reloaded = JsonFileStore::open(&settings_path);
    assert_eq!(
        reloaded.get("provider.model"),
        None,
        "the in-memory store must not be mistaken for persistence"
    );
    assert!(
        !settings_path.exists(),
        "no file may appear: the memory store does not write one"
    );

    // A second memory store is a second empty map, which is the shape a restart has.
    let restarted = MemorySettingsStore::new();
    assert_eq!(restarted.get("provider.model"), None);
    assert!(restarted.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- 3. two writers, one key, no corruption ---------------------------------------------------

#[test]
fn two_threads_writing_one_key_leave_one_whole_value_and_a_readable_file() {
    // A settings write is a whole-document replacement, so the danger is not a merged value but a
    // *torn* one: a reader that parsed the document mid-write would see half of it. `write_atomic`
    // stages a sibling and renames, so the only admissible observations are "the old document" and
    // "the new document".
    //
    // Both halves are asserted: every read taken *during* the race parses (a torn write would not),
    // and the final value is one the writers chose (an interleaving that merged or invented a value
    // would not).
    const ROUNDS: usize = 25;

    for round in 0..ROUNDS {
        let dir = scratch(&format!("race-{round}"));
        let path = dir.join("settings.json");
        let store = Arc::new(JsonFileStore::open(&path));
        // The document has to exist before the reader's "no torn read" claim means anything.
        store
            .set("tools.maxCalls", json!(1))
            .expect("the initial document is written");

        let barrier = Arc::new(Barrier::new(3));
        let done = Arc::new(AtomicBool::new(false));

        let mut handles = Vec::with_capacity(2);
        for value in [2, 3] {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                store
                    .set("tools.maxCalls", json!(value))
                    .expect("each write replaces the whole document");
            }));
        }

        let reader_store = Arc::clone(&store);
        let reader_done = Arc::clone(&done);
        let reader = std::thread::spawn(move || {
            barrier.wait();
            let mut reads = 0usize;
            while !reader_done.load(Ordering::Acquire) {
                // A read taken while a writer is mid-write must still be a complete document.
                let document = reader_store
                    .load()
                    .expect("a reader never observes a half-written document");
                if let Some(value) = document.get("tools.maxCalls") {
                    assert!(
                        value.as_u64().is_some(),
                        "a torn write would surface here as a non-number: {value}"
                    );
                }
                reads += 1;
                std::thread::yield_now();
            }
            reads
        });

        for handle in handles {
            handle.join().expect("a writer thread panicked");
        }
        done.store(true, Ordering::Release);
        let reads = reader.join().expect("the reader thread panicked");
        assert!(
            reads > 0,
            "round {round}: the reader must have observed the document at least once"
        );

        // The end state: one of the two values, and a document that parses as a whole.
        let final_value = store
            .get("tools.maxCalls")
            .expect("the key is still present");
        assert!(
            final_value == json!(2) || final_value == json!(3),
            "round {round}: the final value must be one a writer wrote, got {final_value}"
        );
        assert_eq!(
            store.document_on_disk()["tools.maxCalls"],
            final_value,
            "round {round}: the store's view and the file on disk must agree"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn two_threads_writing_same_and_different_keys_drop_nothing() {
    // The in-memory half of the same question, plus the hazard the file store's own gate protects:
    // two writers touching *different* keys of one document must not drop each other's key. A store
    // that answered "one of the two writes won" would be correct about the contended key and wrong
    // about the rest of the document.
    const WRITERS: usize = 8;

    let store = Arc::new(MemorySettingsStore::new());
    let barrier = Arc::new(Barrier::new(WRITERS + 1));

    let mut handles = Vec::with_capacity(WRITERS);
    for index in 0..WRITERS {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            // Two writers per key: even indices share `contended`, so the final value is one of the
            // values written to it, and every writer also owns a key of its own.
            store
                .set(&format!("writer.{index}"), json!(index))
                .expect("the in-memory store accepts the write");
            store
                .set("contended", json!(index))
                .expect("the in-memory store accepts the write");
        }));
    }
    barrier.wait();
    for handle in handles {
        handle.join().expect("a writer thread panicked");
    }

    // Every writer's own key survived: distinct keys never contend, so all eight must be present
    // with the value that writer chose.
    for index in 0..WRITERS {
        assert_eq!(
            store.get(&format!("writer.{index}")),
            Some(json!(index)),
            "writer {index}'s own key was lost by a concurrent writer"
        );
    }

    // The contended key holds exactly one writer's value, whole.
    let contended = store.get("contended").expect("the contended key is set");
    assert!(
        contended
            .as_u64()
            .is_some_and(|value| value < WRITERS as u64),
        "the contended key must hold one writer's value, got {contended}"
    );

    assert_eq!(store.list().len(), WRITERS + 1);
}

// ---- 4. an out-of-range value is refused, and the refusal says the range ----------------------

#[test]
fn an_out_of_range_value_is_refused_with_a_message_naming_the_range() {
    // The crate's only range validator is `ConfigSchema`; `settings.set` checks nothing (see the
    // test below). A refusal a user can act on has to name the bound *and* the field, or the next
    // thing they do is guess a number.
    let schema = ConfigSchema::new("dsh-llm-deepseek").field(FieldSchema {
        name: "maxTokens",
        rule: FieldRule::Integer {
            min: Some(1),
            max: Some(8192),
            step: Some(1),
        },
        required: false,
        default: Some(json!(8192)),
    });

    let above = schema
        .validate(&map_of(&json!({"maxTokens": 999_999})))
        .expect_err("999999 is above the maximum");
    assert_eq!(
        above.len(),
        1,
        "one field is wrong, so one error is reported"
    );
    assert_eq!(above[0].path, "dsh-llm-deepseek.maxTokens");
    assert_eq!(
        above[0].message, "must be at most 8192",
        "the message must state the bound, not merely that the value is wrong"
    );

    let below = schema
        .validate(&map_of(&json!({"maxTokens": 0})))
        .expect_err("0 is below the minimum");
    assert_eq!(below[0].path, "dsh-llm-deepseek.maxTokens");
    assert_eq!(below[0].message, "must be at least 1");

    // The bounds are inclusive, so the test above is about the range and not a schema that rejects
    // every value.
    let accepted = schema
        .validate(&map_of(&json!({"maxTokens": 8192})))
        .expect("the maximum itself is allowed");
    let ConfigValue::Map(map) = accepted else {
        panic!("validation answers a map for a map input");
    };
    assert_eq!(map["maxTokens"], json!(8192));
}

#[tokio::test]
async fn settings_set_stores_an_out_of_range_value_because_the_write_path_checks_no_range() {
    // FINDING, pinned. `settings.set` validates the *key* (path separators, length, control
    // characters) and stores the *value* verbatim. There is no schema lookup on the write path, so
    // `SchemaRegistry::validate` — the crate's documented pre-write check — is never called by any
    // registered method. A client that writes 999999 is told it succeeded.
    //
    // Reported, not fixed: making the RPC consult a schema is a wiring decision that belongs to the
    // integrator, and the honest description of today's behaviour is this test.
    let store: Arc<dyn SettingsStore> = Arc::new(MemorySettingsStore::new());
    let registry = registry(&store);

    let written = dispatch(
        &registry,
        "set",
        json!({"key": "provider.maxTokens", "value": 999_999}),
    )
    .await
    .expect("the write path performs no range check, so it accepts the value");
    assert_eq!(written["value"], json!(999_999));
    assert_eq!(
        store.get("provider.maxTokens"),
        Some(json!(999_999)),
        "the out-of-range value is stored exactly as sent"
    );

    // A negative count is accepted for the same reason, which makes the point sharper than one
    // example could: nothing on this path knows what field it is writing.
    dispatch(
        &registry,
        "set",
        json!({"key": "tools.maxCalls", "value": -1}),
    )
    .await
    .expect("a negative value is stored too");
    assert_eq!(store.get("tools.maxCalls"), Some(json!(-1)));
}

// ---- 5. an unknown key is absent, and the two layers say so differently -----------------------

#[tokio::test]
async fn reading_an_unknown_key_is_absent_at_the_store_and_a_named_error_on_the_wire() {
    // Two layers, two honest answers, and a test that pins both so neither drifts into the other.
    //
    // - At the **seam** a read is `Option`: `None` means nothing is stored, and the caller that has
    //   an error channel decides what to say. That is the `None`-not-error behaviour.
    // - At the **wire** the answer is an error naming the key, by deliberate choice (see
    //   `settings_rpc`'s module docs): a `{value: null}` success would be indistinguishable from a
    //   setting whose value is legitimately `null`, and a client that forgot to check a "found" flag
    //   would render an absent setting as a null one.
    let store = MemorySettingsStore::new();
    assert_eq!(store.get("never.set"), None);
    assert!(store.is_empty());
    assert_eq!(store.list(), Vec::new());
    assert_eq!(
        store.remove("never.set"),
        Ok(false),
        "removing an absent key reports that nothing was there"
    );

    let dynamic: Arc<dyn SettingsStore> = Arc::new(MemorySettingsStore::new());
    let registry = registry(&dynamic);
    let error = dispatch(&registry, "get", json!({"key": "never.set"}))
        .await
        .expect_err("an unset key is not a successful read");
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    let RemoteError::InvalidArguments {
        namespace,
        method,
        message,
    } = &error
    else {
        panic!("a miss must be reported as an invalid argument, got {error:?}");
    };
    assert_eq!(namespace, NAMESPACE);
    assert_eq!(method, "get");
    assert_eq!(
        message, "no setting `never.set`",
        "the refusal names the key that is missing"
    );

    // A `null` that *was* written reads back as a success holding null, which is the distinction
    // the error above exists to preserve.
    dispatch(
        &registry,
        "set",
        json!({"key": "provider.temperature", "value": null}),
    )
    .await
    .expect("null is a value, not an absence");
    let read = dispatch(&registry, "get", json!({"key": "provider.temperature"}))
        .await
        .expect("a stored null is readable");
    assert_eq!(read["value"], Value::Null);
}

// ---- 6. a changed setting takes effect on the next call ---------------------------------------

/// The budget a call runs under, resolved from the store at call time.
///
/// Deliberately a free function rather than a cached value: whether a setting takes effect on the
/// *next* call is entirely a question of when it is read, and a reader that resolves it once at
/// construction is the exact shape that produces "I changed it and nothing happened".
fn tool_budget(store: &dyn SettingsStore) -> ToolLimit {
    let calls = store
        .get("tools.maxCalls")
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(3);
    ToolLimit::new(calls, calls)
}

#[test]
fn a_tool_budget_setting_takes_effect_on_the_next_call() {
    // The property: change a setting that governs tool behaviour, and the *next* call is judged by
    // the new number. Asserted on the verdict, so the value does not merely reach a struct — it
    // changes what happens.
    let store = MemorySettingsStore::new();

    // Nothing stored: the documented default of three calls applies.
    let mut default_tracker = ToolTracker::new(tool_budget(&store));
    assert_eq!(default_tracker.record("bash", true), ToolVerdict::Allow);
    assert_eq!(default_tracker.record("bash", true), ToolVerdict::Allow);
    assert_eq!(default_tracker.record("bash", true), ToolVerdict::Allow);
    assert_eq!(
        default_tracker.record("bash", true),
        ToolVerdict::TooManyCalls {
            tool: "bash".to_owned(),
            limit: 3,
        },
        "the default budget allows three calls and refuses the fourth"
    );

    // The change.
    store.set("tools.maxCalls", json!(1)).expect("written");

    // The next call, through a budget re-read from the store: one call is now the whole budget.
    let mut tightened = ToolTracker::new(tool_budget(&store));
    assert_eq!(tightened.record("bash", true), ToolVerdict::Allow);
    assert_eq!(
        tightened.record("bash", true),
        ToolVerdict::TooManyCalls {
            tool: "bash".to_owned(),
            limit: 1,
        },
        "the setting changed the next call's verdict, not a later one"
    );

    // Loosening works the same way, so the assertion is about reading the store rather than about
    // moving in one direction.
    store.set("tools.maxCalls", json!(5)).expect("written");
    let mut relaxed = ToolTracker::new(tool_budget(&store));
    for call in 1..=5 {
        assert_eq!(
            relaxed.record("bash", true),
            ToolVerdict::Allow,
            "call {call} is within the relaxed budget"
        );
    }
    assert_eq!(
        relaxed.record("bash", true),
        ToolVerdict::TooManyCalls {
            tool: "bash".to_owned(),
            limit: 5,
        }
    );
}

#[test]
fn a_stored_setting_does_not_reach_a_tracker_that_was_built_once() {
    // FINDING, pinned. `ToolTracker` copies its `Limit` at construction, and nothing in this crate
    // reads a setting in order to build one: `register_settings_methods` writes to a store, and no
    // module consults that store for tool behaviour. A deployment that writes `tools.maxCalls`
    // therefore gets a store entry and no behaviour change at all — the exact "written but not
    // effective" failure, and the reason
    // [`a_tool_budget_setting_takes_effect_on_the_next_call`] has to build its reader itself.
    //
    // Reported, not fixed: wiring a store into the tool loop is the integrator's call.
    let store = MemorySettingsStore::new();
    let mut tracker = ToolTracker::new(tool_budget(&store)); // resolves the default: 3

    store
        .set("tools.maxCalls", json!(1))
        .expect("the write succeeds");

    // The same tracker keeps its original ceiling, because it holds a copy rather than a reference.
    assert_eq!(tracker.record("bash", true), ToolVerdict::Allow);
    assert_eq!(tracker.record("bash", true), ToolVerdict::Allow);
    assert_eq!(tracker.record("bash", true), ToolVerdict::Allow);
    assert_eq!(
        tracker.record("bash", true),
        ToolVerdict::TooManyCalls {
            tool: "bash".to_owned(),
            limit: 3,
        },
        "the stored value of 1 changed nothing: the tracker was built before the write"
    );

    // The store really did keep the value, so the failure is the missing reader — not a lost write.
    // That distinction is the whole reason this is reported as a separate test.
    assert_eq!(store.get("tools.maxCalls"), Some(json!(1)));
}

// ---- 7. the second consumer of the store resolves the same value ------------------------------

#[test]
fn a_store_write_is_visible_to_the_next_schema_describe() {
    // A store with one writer is only half a configuration system. This asserts that the reader the
    // crate actually ships (`SchemaRegistry::describe`) resolves what was written, on the call
    // after the write, and reports whether the value came from the store or from a default.
    let store = MemorySettingsStore::new();
    let mut schema = SchemaRegistry::new();
    schema
        .declare(SettingSpec {
            namespace: "provider".to_owned(),
            key: "model".to_owned(),
            schema: json!({"type": "string"}),
            default: Some(json!("deepseek-chat")),
            secret: false,
            description: "The model to call.".to_owned(),
        })
        .expect("the declaration is valid");

    let before = schema.describe(&store);
    assert_eq!(before["namespaces"][0]["namespace"], json!("provider"));
    assert_eq!(
        before["namespaces"][0]["settings"][0]["key"],
        json!("model")
    );
    assert_eq!(
        before["namespaces"][0]["settings"][0]["value"],
        json!("deepseek-chat"),
        "with nothing stored, the declared default is what a form renders"
    );
    assert_eq!(
        before["namespaces"][0]["settings"][0]["configured"],
        json!(false)
    );

    store
        .set("provider.model", json!("deepseek-reasoner"))
        .expect("written");

    let after = schema.describe(&store);
    assert_eq!(
        after["namespaces"][0]["settings"][0]["value"],
        json!("deepseek-reasoner"),
        "the next describe resolves the stored value, not the default"
    );
    assert_eq!(
        after["namespaces"][0]["settings"][0]["configured"],
        json!(true)
    );
    assert_eq!(
        after["undeclared"],
        json!(0),
        "the key is declared, so it is not counted as undeclared"
    );
}

// ---- 8. the surface is the seven methods, and describe is the declared gap --------------------

#[tokio::test]
async fn the_settings_namespace_serves_seven_methods_and_reports_describe_as_a_gap() {
    // The premise of the whole file, pinned at wire level: seven methods are registered, and
    // `describe` — the method `api_config::settings_methods()` advertises to a form-rendering
    // client — answers NOT_IMPLEMENTED naming itself rather than a transport-level 404 a client
    // would read as a typo.
    let store: Arc<dyn SettingsStore> = Arc::new(MemorySettingsStore::new());
    let registry = registry(&store);

    for method in SEVEN_METHODS {
        // Each must be reachable: an unknown-method error would mean the name is wrong, and any
        // other error is the method's own answer to an empty body.
        let result = dispatch(&registry, method, json!({})).await;
        assert!(
            !matches!(result, Err(RemoteError::UnknownMethod { .. })),
            "`{NAMESPACE}.{method}` must be registered, got {result:?}"
        );
    }

    let gap = dispatch(&registry, "describe", json!({}))
        .await
        .expect_err("describe is a declared gap, not an implemented method");
    assert_eq!(gap.code(), "NOT_IMPLEMENTED");
    let RemoteError::NotImplemented { namespace, method } = &gap else {
        panic!("the gap must be reported as not-implemented, got {gap:?}");
    };
    assert_eq!(namespace, NAMESPACE);
    assert_eq!(method, "describe");
}
