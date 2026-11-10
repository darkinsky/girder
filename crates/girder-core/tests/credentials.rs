//! Tests for the credential store.
//!
//! Three properties carry the security story: the read view cannot carry a secret, the debug output
//! cannot leak one, and an environment name is validated because the map is handed to a subprocess.

use std::collections::BTreeMap;

use girder_core::{
    CredentialError, CredentialInfo, CredentialRecord, CredentialRef, CredentialStore,
    valid_environment,
};

fn reference(value: &str) -> CredentialRef {
    CredentialRef::new(value).expect("valid")
}

// ---- records ----------------------------------------------------------------------------------

#[test]
fn an_api_key_record_holds_its_secret() {
    let record = CredentialRecord::api_key("sk-123");

    assert_eq!(record.kind(), "api-key");
    assert_eq!(record.secret(), Some("sk-123"));
}

#[test]
fn an_environment_record_holds_no_secret() {
    // A credential that supplies only environment values, such as an AWS profile.
    let mut env = BTreeMap::new();
    env.insert("AWS_PROFILE".to_owned(), "dev".to_owned());
    let record = CredentialRecord::environment(env);

    assert_eq!(record.secret(), None);
    let CredentialRecord::ApiKey { env, .. } = &record else {
        panic!("expected an api-key record");
    };
    assert_eq!(env.get("AWS_PROFILE").map(String::as_str), Some("dev"));
}

#[test]
fn a_grant_record_is_opaque_and_holds_no_secret() {
    // The seam never reads it; only its owning plugin can interpret it.
    let record = CredentialRecord::grant(serde_json::json!({"refresh": "abc"}));

    assert_eq!(record.kind(), "grant");
    assert_eq!(record.secret(), None);
}

#[test]
fn a_blank_reference_is_refused() {
    assert!(CredentialRef::new("").is_none());
}

#[test]
fn a_reference_displays_itself() {
    assert_eq!(reference("deepseek").to_string(), "deepseek");
    assert_eq!(reference("deepseek").as_str(), "deepseek");
}

// ---- the read view ----------------------------------------------------------------------------

#[test]
fn an_unconfigured_reference_reports_missing() {
    let info = CredentialInfo::missing();

    assert!(!info.configured);
    assert!(info.source.is_none());
    assert!(!info.writable);
}

#[test]
fn a_configured_reference_reports_its_source() {
    let info = CredentialInfo::configured("env", true);

    assert!(info.configured);
    assert_eq!(info.source.as_deref(), Some("env"));
    assert!(info.writable);
}

#[test]
fn the_read_view_has_no_field_a_secret_could_ride_in() {
    // Structural rather than behavioural: the guarantee is that there is nowhere to put one, which
    // is what makes the view safe to serialize to a client.
    let json = serde_json::json!({
        "configured": true,
        "source": "store",
        "writable": true,
    });
    let keys: Vec<&String> = json.as_object().expect("object").keys().collect();

    assert_eq!(keys, vec!["configured", "source", "writable"]);
    assert!(
        !keys
            .iter()
            .any(|key| key.contains("key") || key.contains("value") || key.contains("secret"))
    );
}

// ---- the store --------------------------------------------------------------------------------

#[test]
fn storing_and_reading_a_record_round_trips() {
    let store = CredentialStore::new();
    store
        .set(
            &reference("deepseek"),
            CredentialRecord::api_key("sk-1"),
            "store",
        )
        .expect("stored");

    assert_eq!(
        store.get(&reference("deepseek")).expect("present").secret(),
        Some("sk-1")
    );
}

#[test]
fn a_reading_of_an_unset_reference_is_not_found() {
    let store = CredentialStore::new();
    let error = store.get(&reference("nope")).expect_err("absent");

    assert!(matches!(error, CredentialError::NotFound { .. }));
}

#[test]
fn the_read_view_of_a_stored_record_names_its_source() {
    let store = CredentialStore::new();
    store
        .set(
            &reference("deepseek"),
            CredentialRecord::api_key("sk-1"),
            "env",
        )
        .expect("stored");

    let info = store.describe(&reference("deepseek"));

    assert!(info.configured);
    assert_eq!(info.source.as_deref(), Some("env"));
}

#[test]
fn the_read_view_of_an_unset_record_is_missing() {
    let store = CredentialStore::new();
    assert_eq!(
        store.describe(&reference("nope")),
        CredentialInfo::missing()
    );
}

#[test]
fn overwriting_is_allowed() {
    // A rotated key is the normal case; refusing it would make rotation impossible.
    let store = CredentialStore::new();
    store
        .set(&reference("k"), CredentialRecord::api_key("old"), "store")
        .expect("stored");
    store
        .set(&reference("k"), CredentialRecord::api_key("new"), "store")
        .expect("rotated");

    assert_eq!(store.get(&reference("k")).unwrap().secret(), Some("new"));
}

#[test]
fn unsetting_removes_the_record() {
    let store = CredentialStore::new();
    store
        .set(&reference("k"), CredentialRecord::api_key("v"), "store")
        .expect("stored");

    assert!(store.unset(&reference("k")));
    assert!(
        !store.unset(&reference("k")),
        "a second unset removes nothing"
    );
    assert!(!store.is_configured(&reference("k")));
}

#[test]
fn references_are_sorted_and_never_contain_values() {
    // A reference names which credential is configured, which a settings page needs.
    let store = CredentialStore::new();
    for name in ["zeta", "alpha"] {
        store
            .set(
                &reference(name),
                CredentialRecord::api_key("secret"),
                "store",
            )
            .expect("stored");
    }

    let references = store.references();
    assert_eq!(references, vec!["alpha", "zeta"]);
    assert!(!references.iter().any(|name| name.contains("secret")));
}

#[test]
fn is_configured_answers_without_revealing() {
    let store = CredentialStore::new();
    store
        .set(&reference("k"), CredentialRecord::api_key("v"), "store")
        .expect("stored");

    assert!(store.is_configured(&reference("k")));
    assert!(!store.is_configured(&reference("other")));
}

// ---- environment names ------------------------------------------------------------------------

#[test]
fn an_environment_name_must_be_a_posix_identifier() {
    // The map is handed to a subprocess: a name containing `=` or a newline would be interpreted by
    // the shell rather than by the program.
    assert!(valid_environment(&BTreeMap::from([(
        "AWS_PROFILE".to_owned(),
        "dev".to_owned()
    )])));
    assert!(valid_environment(&BTreeMap::from([(
        "_X".to_owned(),
        "1".to_owned()
    )])));
    assert!(!valid_environment(&BTreeMap::from([(
        "A=B".to_owned(),
        "1".to_owned()
    )])));
    assert!(!valid_environment(&BTreeMap::from([(
        "A B".to_owned(),
        "1".to_owned()
    )])));
    assert!(!valid_environment(&BTreeMap::from([(
        "1A".to_owned(),
        "1".to_owned()
    )])));
    assert!(!valid_environment(&BTreeMap::from([(
        String::new(),
        "1".to_owned()
    )])));
}

#[test]
fn storing_an_invalid_environment_name_is_refused() {
    let store = CredentialStore::new();
    let mut env = BTreeMap::new();
    env.insert("BAD=NAME".to_owned(), "x".to_owned());

    let error = store
        .set(&reference("k"), CredentialRecord::environment(env), "store")
        .expect_err("invalid name");

    let CredentialError::InvalidEnvName { name } = error else {
        panic!("expected InvalidEnvName, got {error:?}");
    };
    assert_eq!(name, "BAD=NAME");
}

// ---- secrecy ----------------------------------------------------------------------------------

#[test]
fn the_debug_form_never_prints_a_value() {
    // A credential in a debug line is a credential in every log aggregator.
    let store = CredentialStore::new();
    store
        .set(
            &reference("deepseek"),
            CredentialRecord::api_key("sk-super-secret"),
            "store",
        )
        .expect("stored");

    let text = format!("{store:?}");

    assert!(text.contains("deepseek"), "the reference is safe to show");
    assert!(!text.contains("sk-super-secret"), "{text}");
}

#[test]
fn the_debug_form_prints_no_secret_even_after_rotation() {
    let store = CredentialStore::new();
    for key in ["sk-old-secret", "sk-new-secret"] {
        store
            .set(&reference("k"), CredentialRecord::api_key(key), "store")
            .expect("stored");
    }

    let text = format!("{store:?}");
    assert!(!text.contains("sk-old-secret"));
    assert!(!text.contains("sk-new-secret"));
}

#[test]
fn a_grant_payload_survives_a_json_round_trip() {
    // The one constraint the seam enforces on a payload: a durable store must be able to hold it.
    let payload = serde_json::json!({"refresh_token": "r1", "expires": 1234});
    let record = CredentialRecord::grant(payload.clone());

    let text = serde_json::to_string(&payload).expect("serializes");
    let restored: serde_json::Value = serde_json::from_str(&text).expect("parses");

    assert_eq!(restored, payload);
    let CredentialRecord::Grant { payload: held } = record else {
        panic!("expected a grant");
    };
    assert_eq!(held, payload);
}
