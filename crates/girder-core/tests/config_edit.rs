//! Tests for the declared-schema registry behind `settings.describe`.
//!
//! The properties under test, in the order they matter:
//!
//! - **a secret is never echoed** — the stored value, and any declared default, must not appear
//!   anywhere in the serialized response; a redaction applied afterwards would be one forgotten
//!   field away from leaking, so this checks the whole document, not one field
//! - **`describe` renders what was declared, with the value the store resolves to** — stored value
//!   first, declared default otherwise, and `configured` distinguishing the two
//! - **duplicate declarations are refused** — a second declaration of one key would silently make
//!   the rendered form depend on declaration order
//! - **`validate` checks the JSON type and refuses what it cannot vouch for** — a wrong type is
//!   refused with a message naming the kind and not the value; an undeclared key is refused
//!   because no declaration says what a write there means
//! - **undeclared stored keys are counted, not published** — the store is a shared flat map, and a
//!   key no declaration covers may hold material this surface never owned
//! - **an empty registry is a valid answer** — `namespaces: []` plus the count of what it hides
//!
//! Isolation: every test builds its own registry and store, so tests in one binary cannot see each
//! other's declarations.
//!
//! # Why the modules are compiled by path
//!
//! `config_edit` is not wired into the crate root yet; that change belongs to the integrator, and
//! it is the same `#[path]` arrangement the neighbouring tests use. The real source file is
//! compiled — nothing is duplicated — and `config_edit`'s `crate::settings_rpc` import resolves
//! against the sibling module included just below.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

// Included for its `SettingsStore` seam and in-process store. The rest of its surface is not under
// test here, so unused-code warnings are silenced rather than left to hide a real one.
#[allow(
    dead_code,
    reason = "only the storage seam of this module is used by these tests"
)]
#[path = "../src/settings_rpc.rs"]
mod settings_rpc;

#[path = "../src/config_edit.rs"]
mod config_edit;

use config_edit::{REDACTED, SchemaError, SchemaRegistry, SettingSpec};
use serde_json::{Value, json};
use settings_rpc::{MemorySettingsStore, SettingsStore};

// ---- fixtures ---------------------------------------------------------------------------------

/// A non-secret declaration of `namespace.key` with `schema` and no default.
fn spec(namespace: &str, key: &str, schema: Value) -> SettingSpec {
    SettingSpec {
        namespace: namespace.to_owned(),
        key: key.to_owned(),
        schema,
        default: None,
        secret: false,
        description: String::new(),
    }
}

/// A registry with one string setting declared at `namespace.key`.
fn registry_with(namespace: &str, key: &str) -> SchemaRegistry {
    let mut registry = SchemaRegistry::new();
    registry
        .declare(spec(namespace, key, json!({"type": "string"})))
        .expect("a plain name is declarable");
    registry
}

/// The rendered entry for `namespace.key`, or a panic naming what was rendered instead.
fn find<'a>(described: &'a Value, namespace: &str, key: &str) -> &'a Value {
    let rendered = described
        .get("namespaces")
        .and_then(Value::as_array)
        .expect("describe always renders a namespaces array");
    let settings = rendered
        .iter()
        .find(|entry| entry.get("namespace").and_then(Value::as_str) == Some(namespace))
        .and_then(|entry| entry.get("settings"))
        .and_then(Value::as_array);
    settings
        .and_then(|settings| {
            settings
                .iter()
                .find(|setting| setting.get("key").and_then(Value::as_str) == Some(key))
        })
        .unwrap_or_else(|| panic!("`{namespace}.{key}` was not rendered"))
}

/// The namespace names `describe` rendered, in order.
fn namespace_names(described: &Value) -> Vec<&str> {
    described["namespaces"]
        .as_array()
        .expect("describe always renders a namespaces array")
        .iter()
        .filter_map(|entry| entry.get("namespace").and_then(Value::as_str))
        .collect()
}

// ---- declaration and describe ----------------------------------------------------------------

#[test]
fn declare_then_describe_reports_the_declared_setting() {
    let mut registry = SchemaRegistry::new();
    let mut declared = spec("provider", "model", json!({"type": "string"}));
    declared.description = "The model to call.".to_owned();
    registry
        .declare(declared)
        .expect("a plain name is declarable");

    let store = MemorySettingsStore::new();
    let described = registry.describe(&store);
    let entry = find(&described, "provider", "model");

    assert_eq!(entry["store_key"], json!("provider.model"));
    assert_eq!(entry["schema"], json!({"type": "string"}));
    assert_eq!(entry["description"], json!("The model to call."));
    assert_eq!(entry["secret"], json!(false));
    assert_eq!(entry["configured"], json!(false));
    assert_eq!(entry["value"], Value::Null);
}

#[test]
fn describe_resolves_a_declared_default_when_the_store_is_empty() {
    let mut registry = SchemaRegistry::new();
    let mut declared = spec("provider", "model", json!({"type": "string"}));
    declared.default = Some(json!("deepseek-chat"));
    registry
        .declare(declared)
        .expect("a plain name is declarable");

    let described = registry.describe(&MemorySettingsStore::new());
    let entry = find(&described, "provider", "model");

    assert_eq!(entry["value"], json!("deepseek-chat"));
    assert_eq!(entry["default"], json!("deepseek-chat"));
    assert_eq!(entry["configured"], json!(false));
}

#[test]
fn describe_prefers_the_stored_value_over_the_default() {
    let mut registry = SchemaRegistry::new();
    let mut declared = spec("provider", "model", json!({"type": "string"}));
    declared.default = Some(json!("deepseek-chat"));
    registry
        .declare(declared)
        .expect("a plain name is declarable");

    let store = MemorySettingsStore::new();
    store
        .set("provider.model", json!("deepseek-reasoner"))
        .expect("the in-process store accepts a write");

    let described = registry.describe(&store);
    let entry = find(&described, "provider", "model");

    assert_eq!(entry["value"], json!("deepseek-reasoner"));
    assert_eq!(entry["configured"], json!(true));
}

#[test]
fn an_unavailable_store_describes_the_declared_defaults() {
    let mut registry = SchemaRegistry::new();
    let mut declared = spec("provider", "model", json!({"type": "string"}));
    declared.default = Some(json!("deepseek-chat"));
    registry
        .declare(declared)
        .expect("a plain name is declarable");

    // A deployment with no storage still has a describable form; it simply has nothing stored.
    let store = settings_rpc::UnavailableSettingsStore::default_reason();
    let described = registry.describe(&store);
    let entry = find(&described, "provider", "model");

    assert_eq!(entry["value"], json!("deepseek-chat"));
    assert_eq!(entry["configured"], json!(false));
}

// ---- duplicates ------------------------------------------------------------------------------

#[test]
fn duplicate_declaration_is_refused() {
    let mut registry = registry_with("provider", "model");
    let error = registry
        .declare(spec("provider", "model", json!({"type": "number"})))
        .expect_err("the second declaration of one key is refused");

    assert_eq!(
        error,
        SchemaError::DuplicateDeclaration {
            namespace: "provider".to_owned(),
            key: "model".to_owned(),
        }
    );
    // The refused declaration must not have replaced the first one.
    let entry = find(
        &registry.describe(&MemorySettingsStore::new()),
        "provider",
        "model",
    )
    .clone();
    assert_eq!(entry["schema"], json!({"type": "string"}));
    assert_eq!(registry.len(), 1);
}

#[test]
fn the_same_key_in_two_namespaces_is_not_a_duplicate() {
    let mut registry = registry_with("provider", "model");
    registry
        .declare(spec("search", "model", json!({"type": "string"})))
        .expect("two namespaces are two settings");

    assert_eq!(registry.len(), 2);
    assert!(registry.get("provider", "model").is_some());
    assert!(registry.get("search", "model").is_some());
}

// ---- secrets: the property that matters ------------------------------------------------------

/// The stored value, asserted absent from every byte of the rendered document.
const SECRET_VALUE: &str = "sk-live-9f3a-secret-do-not-echo";

#[test]
fn a_secret_value_is_redacted() {
    let mut registry = SchemaRegistry::new();
    let mut declared = spec("provider", "api_key", json!({"type": "string"}));
    declared.secret = true;
    declared.description = "The provider API key.".to_owned();
    registry
        .declare(declared)
        .expect("a plain name is declarable");

    let store = MemorySettingsStore::new();
    store
        .set("provider.api_key", json!(SECRET_VALUE))
        .expect("the in-process store accepts a write");

    let described = registry.describe(&store);
    let entry = find(&described, "provider", "api_key");

    assert_eq!(entry["value"], json!(REDACTED));
    assert_eq!(entry["secret"], json!(true));
    // Presence is reported, because a form needs it to offer "replace" rather than "add".
    assert_eq!(entry["configured"], json!(true));

    // The whole document, not one field: a redaction that leaves the value in `default`, in a
    // sibling field, or in a nested copy is still a leak, and only this check sees it.
    let serialized = described.to_string();
    assert!(
        !serialized.contains(SECRET_VALUE),
        "the secret value appeared in the describe response: {serialized}"
    );
}

#[test]
fn a_secret_default_is_not_reported() {
    let mut registry = SchemaRegistry::new();
    let mut declared = spec("provider", "api_key", json!({"type": "string"}));
    declared.secret = true;
    declared.default = Some(json!(SECRET_VALUE));
    registry
        .declare(declared)
        .expect("a plain name is declarable");

    // An unset secret has no value to show, and its default must not stand in for one: a default
    // is written down in the deployment's source and is as sensitive as a stored value.
    let described = registry.describe(&MemorySettingsStore::new());
    let entry = find(&described, "provider", "api_key");

    assert_eq!(entry["value"], Value::Null);
    assert_eq!(entry["configured"], json!(false));
    assert_eq!(entry.get("default"), None);
    assert!(!described.to_string().contains(SECRET_VALUE));
}

#[test]
fn a_secret_does_not_leak_through_a_type_mismatch_error() {
    let mut registry = SchemaRegistry::new();
    let mut declared = spec("provider", "api_key", json!({"type": "number"}));
    declared.secret = true;
    registry
        .declare(declared)
        .expect("a plain name is declarable");

    let error = registry
        .validate("provider", "api_key", &json!(SECRET_VALUE))
        .expect_err("a string is not a number");
    let rendered = error.to_string();

    assert!(rendered.contains("number"), "the message names the type");
    assert!(rendered.contains("string"), "the message names the kind");
    assert!(
        !rendered.contains(SECRET_VALUE),
        "a refusal may be logged, so it must not repeat the value: {rendered}"
    );
}

// ---- validate ---------------------------------------------------------------------------------

#[test]
fn validate_accepts_the_declared_type() {
    let registry = registry_with("provider", "model");
    registry
        .validate("provider", "model", &json!("deepseek-chat"))
        .expect("a string matches a string declaration");
}

#[test]
fn validate_refuses_the_wrong_type() {
    let registry = registry_with("provider", "model");
    let error = registry
        .validate("provider", "model", &json!(42))
        .expect_err("a number is not a string");

    assert_eq!(
        error,
        SchemaError::TypeMismatch {
            namespace: "provider".to_owned(),
            key: "model".to_owned(),
            expected: "string".to_owned(),
            actual: "number",
        }
    );
    let rendered = error.to_string();
    assert!(rendered.contains("provider.model"));
    assert!(
        !rendered.contains("42"),
        "the message names the kind, never the value: {rendered}"
    );
}

#[test]
fn validate_checks_every_scalar_type_it_declares() {
    let types = [
        ("flag", json!({"type": "boolean"}), json!(true)),
        ("count", json!({"type": "number"}), json!(2.5)),
        ("size", json!({"type": "integer"}), json!(3)),
        ("nothing", json!({"type": "null"}), Value::Null),
        ("tags", json!({"type": "array"}), json!(["a"])),
        ("map", json!({"type": "object"}), json!({"a": 1})),
    ];
    let mut registry = SchemaRegistry::new();
    for (key, schema, _) in &types {
        registry
            .declare(spec("ui", key, schema.clone()))
            .expect("a plain name is declarable");
    }

    for (key, _, value) in &types {
        registry
            .validate("ui", key, value)
            .unwrap_or_else(|error| panic!("`{key}` rejected its own type: {error}"));
    }

    // The complementary half: each declaration refuses a value of another kind.
    for (key, _, _) in &types {
        let wrong = json!("not-of-that-type");
        if *key == "map" {
            // An array is the one non-object JSON value the map's own type test would confuse.
            assert!(registry.validate("ui", key, &json!(["a"])).is_err());
        } else {
            assert!(
                registry.validate("ui", key, &wrong).is_err(),
                "`{key}` accepted a string it does not declare"
            );
        }
    }
}

#[test]
fn validate_accepts_a_whole_float_for_an_integer() {
    let mut registry = SchemaRegistry::new();
    registry
        .declare(spec("ui", "size", json!({"type": "integer"})))
        .expect("a plain name is declarable");

    // A client that serialises an integer as `3.0` chose a wire form, not a different value.
    registry
        .validate("ui", "size", &json!(3.0))
        .expect("a whole float is an integer under JSON Schema");
    assert!(registry.validate("ui", "size", &json!(3.5)).is_err());
}

#[test]
fn validate_accepts_a_list_of_types() {
    let mut registry = SchemaRegistry::new();
    registry
        .declare(spec("ui", "either", json!({"type": ["string", "null"]})))
        .expect("a plain name is declarable");

    registry
        .validate("ui", "either", &json!("a string"))
        .expect("the list accepts its first type");
    registry
        .validate("ui", "either", &Value::Null)
        .expect("the list accepts its second type");
    assert!(registry.validate("ui", "either", &json!(1)).is_err());
}

#[test]
fn validate_skips_a_schema_that_makes_no_type_claim() {
    let mut registry = SchemaRegistry::new();
    registry
        .declare(spec("ui", "free", json!({"description": "anything"})))
        .expect("a plain name is declarable");
    registry
        .declare(spec("ui", "colour", json!({"type": "color"})))
        .expect("a plain name is declarable");

    // Neither schema names a type this build implements, so neither refuses a value: rejecting
    // writes the deployment's own reader accepts would be worse than the missing check.
    registry
        .validate("ui", "free", &json!(42))
        .expect("a schema without a type checks nothing");
    registry
        .validate("ui", "colour", &json!("#ff0000"))
        .expect("an unknown type name checks nothing");
}

#[test]
fn validate_refuses_an_undeclared_key() {
    let registry = registry_with("provider", "model");
    let error = registry
        .validate("provider", "modle", &json!("typo"))
        .expect_err("no declaration covers this key");

    assert_eq!(
        error,
        SchemaError::Undeclared {
            namespace: "provider".to_owned(),
            key: "modle".to_owned(),
        }
    );
}

// ---- undeclared stored keys -------------------------------------------------------------------

#[test]
fn undeclared_stored_keys_are_counted_but_not_shown() {
    let registry = registry_with("provider", "model");
    let store = MemorySettingsStore::new();
    store
        .set("provider.model", json!("deepseek-chat"))
        .expect("the in-process store accepts a write");
    store
        .set("legacy.hostname", json!("internal.example"))
        .expect("the in-process store accepts a write");
    store
        .set("credentials.token", json!(SECRET_VALUE))
        .expect("the in-process store accepts a write");

    let described = registry.describe(&store);

    // The count tells an operator the store holds more than this registry describes.
    assert_eq!(described["undeclared"], json!(2));
    // Neither the names nor the values of the hidden keys are published: no declaration marked
    // them secret, so the only safe reading is that this surface does not own them.
    let serialized = described.to_string();
    for hidden in [
        "legacy.hostname",
        "internal.example",
        "credentials.token",
        SECRET_VALUE,
    ] {
        assert!(
            !serialized.contains(hidden),
            "an undeclared key leaked into the response: {hidden}"
        );
    }
    assert_eq!(
        find(&described, "provider", "model")["configured"],
        json!(true)
    );
}

#[test]
fn an_empty_registry_describes_nothing_and_counts_what_it_hides() {
    let registry = SchemaRegistry::new();
    let store = MemorySettingsStore::new();
    store
        .set("anything.at_all", json!(1))
        .expect("the in-process store accepts a write");

    let described = registry.describe(&store);

    assert_eq!(described["namespaces"], json!([]));
    assert_eq!(described["undeclared"], json!(1));
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert_eq!(registry.get("anything", "at_all"), None);
    assert!(
        registry.validate("anything", "at_all", &json!(1)).is_err(),
        "an empty registry vouches for nothing"
    );
}

// ---- declaration hygiene and ordering --------------------------------------------------------

#[test]
fn declaration_names_are_checked() {
    let mut registry = SchemaRegistry::new();

    for (namespace, key) in [
        ("", "model"),
        ("provider", ""),
        (" provider", "model"),
        ("provider", "model "),
        ("..", "model"),
        ("provider", "a/b"),
        ("provider", "a\\b"),
        ("pro\x07vider", "model"),
    ] {
        let error = registry
            .declare(spec(namespace, key, json!({"type": "string"})))
            .expect_err("an unusable name is refused");
        assert!(
            matches!(error, SchemaError::InvalidDeclaration { .. }),
            "`{namespace}.{key}` was refused with {error}"
        );
    }
    assert!(registry.is_empty());
}

#[test]
fn dotted_names_are_allowed_because_they_are_this_crates_convention() {
    let mut registry = SchemaRegistry::new();
    registry
        .declare(spec(
            "provider.deepseek",
            "model.name",
            json!({"type": "string"}),
        ))
        .expect("a dot inside a segment is not a path traversal");

    let described = registry.describe(&MemorySettingsStore::new());
    let entry = find(&described, "provider.deepseek", "model.name");
    assert_eq!(entry["store_key"], json!("provider.deepseek.model.name"));
}

#[test]
fn namespaces_and_keys_are_rendered_in_name_order() {
    let mut registry = SchemaRegistry::new();
    for (namespace, key) in [("zeta", "b"), ("alpha", "z"), ("zeta", "a"), ("alpha", "a")] {
        registry
            .declare(spec(namespace, key, json!({"type": "string"})))
            .expect("a plain name is declarable");
    }

    let described = registry.describe(&MemorySettingsStore::new());

    // Two runs over the same registry must serialize identically, or a diff of the response is
    // unreadable and a client sees rows move for no reason.
    assert_eq!(described, registry.describe(&MemorySettingsStore::new()));
    assert_eq!(namespace_names(&described), vec!["alpha", "zeta"]);
    let alpha: Vec<&str> = described["namespaces"][0]["settings"]
        .as_array()
        .expect("a declared namespace renders a settings array")
        .iter()
        .filter_map(|setting| setting.get("key").and_then(Value::as_str))
        .collect();
    assert_eq!(alpha, vec!["a", "z"]);
}

#[test]
fn get_len_and_is_empty_track_the_declarations() {
    let mut registry = SchemaRegistry::new();
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);

    registry
        .declare(spec("provider", "model", json!({"type": "string"})))
        .expect("a plain name is declarable");
    registry
        .declare(spec("provider", "temperature", json!({"type": "number"})))
        .expect("a plain name is declarable");

    assert!(!registry.is_empty());
    assert_eq!(registry.len(), 2);
    let declared = registry
        .get("provider", "model")
        .expect("the declaration that was just made is present");
    assert_eq!(declared.store_key(), "provider.model");
    assert_eq!(registry.get("provider", "describable"), None);
}
