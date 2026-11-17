//! Tests for the `credentials.*` Remote methods.
//!
//! These are the tests for the namespace `packages/api/settings-controller` mounts beside
//! `settings`, and the surface that module's `settings_rpc` counterpart explicitly declined. The
//! properties under test, in the order they matter:
//!
//! - **no read path returns a secret** — `describe` answers four declared fields and nothing else,
//!   and a value written through `set` never appears in any response. This is the property the whole
//!   module exists to keep, and it is asserted by scanning the serialized response for the secret
//!   rather than by trusting the type.
//! - **the reference grammar is upstream's** — `/^[A-Za-z_][A-Za-z0-9_]*$/`, so `AWS_PROFILE` is a
//!   reference and `aws/profile` is not.
//! - **one bad name fails the whole batch** — upstream rejects the entire `describe` call rather
//!   than answering a short object, so a page cannot silently under-render.
//! - **the fan-out is bounded** — more than [`MAX_DESCRIBE_REFS`] references is refused before any
//!   provider work starts.
//! - **a provider refusal is its own code carrying the provider's message** — `FAILED`, not
//!   `INVALID_ARGUMENTS`, because the request was fine.
//! - **`unset` of nothing is refused** rather than reported as a success.
//! - **the declarations match what each method does** — `describe` is a read, `set` a write,
//!   `unset` destructive, so a read-only policy can list and cannot erase.
//! - **the default source refuses rather than pretending** — no storage configured is reported as a
//!   failure, not hidden behind a map the next process could not confirm.
//!
//! Isolation: every test builds its own source and registry.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against the methods as written even before the crate root wires them in. It spells its
//! own paths through `girder_core::`, which is the alias the library declares for itself, so the
//! same source builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/account_gap.rs"]
mod account_gap;

use std::sync::Arc;

use girder_core::{
    Access, AccessDecision, AccessRequest, Policy, RemoteCall, RemoteError, RemoteRegistry,
};
use serde_json::{Value, json};

use account_gap::{
    CredentialInfo, CredentialSource, DESCRIBE_METHOD, MAX_DESCRIBE_REFS, MemoryCredentialSource,
    NAMESPACE, SET_METHOD, UNSET_METHOD, UnavailableCredentialSource, register_credential_methods,
};

// ---- fixtures ---------------------------------------------------------------------------------

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// A registry with a fresh in-memory source behind it.
fn registry() -> (RemoteRegistry, Arc<MemoryCredentialSource>) {
    let source = Arc::new(MemoryCredentialSource::new());
    let dynamic: Arc<dyn CredentialSource> = source.clone();
    let mut registry = RemoteRegistry::new();
    register_credential_methods(&mut registry, &dynamic);
    (registry, source)
}

/// Runs one call and unwraps it.
async fn ok(registry: &RemoteRegistry, method: &str, args: Value) -> Value {
    registry
        .dispatch(NAMESPACE, method, call(args))
        .await
        .unwrap_or_else(|error| panic!("`{NAMESPACE}.{method}` should have succeeded: {error}"))
}

/// Runs one call and expects a failure.
async fn err(registry: &RemoteRegistry, method: &str, args: Value) -> RemoteError {
    match registry.dispatch(NAMESPACE, method, call(args)).await {
        Ok(value) => panic!("`{NAMESPACE}.{method}` should have failed, got {value}"),
        Err(error) => error,
    }
}

/// The string form of one failure, for asserting on the message a caller sees.
fn message(error: &RemoteError) -> String {
    error.to_string()
}

// ---- the grammar ------------------------------------------------------------------------------

#[tokio::test]
async fn describe_prices_every_row_it_was_asked_about() {
    let (registry, source) = registry();
    source.set("OPENAI_API_KEY", "sk-live-1").unwrap();
    source.set("AWS_PROFILE", "prof").unwrap();

    let answer = ok(
        &registry,
        DESCRIBE_METHOD,
        json!({"refs": ["OPENAI_API_KEY", "AWS_PROFILE", "UNSET_ONE"]}),
    )
    .await;

    let items = answer["items"].as_object().expect("an object keyed by ref");
    assert_eq!(items.len(), 3);
    assert_eq!(items["OPENAI_API_KEY"]["configured"], json!(true));
    assert_eq!(items["OPENAI_API_KEY"]["writable"], json!(true));
    assert_eq!(items["AWS_PROFILE"]["configured"], json!(true));
    // A reference nobody set is an ordinary answer, not an error: a fresh install describes every
    // row it renders and most of them are unset.
    assert_eq!(items["UNSET_ONE"]["configured"], json!(false));
    assert_eq!(items["UNSET_ONE"]["writable"], json!(false));
}

#[tokio::test]
async fn one_name_outside_the_grammar_fails_the_whole_batch() {
    let (registry, source) = registry();
    source.set("GOOD_ONE", "v").unwrap();

    let failure = err(
        &registry,
        DESCRIBE_METHOD,
        json!({"refs": ["GOOD_ONE", "aws/profile"]}),
    )
    .await;

    // The whole call, not a short answer: a page that received one row and no explanation would
    // render eleven rows as unset.
    assert_eq!(failure.code(), "INVALID_ARGUMENTS");
    assert!(message(&failure).contains("aws/profile"), "got: {failure}");
}

#[tokio::test]
async fn the_batch_fan_out_is_bounded_before_any_lookup() {
    let (registry, _source) = registry();
    let refs: Vec<String> = (0..=MAX_DESCRIBE_REFS).map(|i| format!("R{i}")).collect();

    let failure = err(&registry, DESCRIBE_METHOD, json!({"refs": refs})).await;

    assert_eq!(failure.code(), "INVALID_ARGUMENTS");
    assert!(
        message(&failure).contains(&MAX_DESCRIBE_REFS.to_string()),
        "the refusal should name the bound: {failure}"
    );
}

#[tokio::test]
async fn exactly_the_bound_is_accepted() {
    let (registry, _source) = registry();
    let refs: Vec<String> = (0..MAX_DESCRIBE_REFS).map(|i| format!("R{i}")).collect();

    let answer = ok(&registry, DESCRIBE_METHOD, json!({"refs": refs})).await;

    assert_eq!(
        answer["items"].as_object().expect("an object").len(),
        MAX_DESCRIBE_REFS
    );
}

#[tokio::test]
async fn an_absent_refs_field_is_refused_rather_than_read_as_empty() {
    let (registry, _source) = registry();

    let failure = err(&registry, DESCRIBE_METHOD, json!({})).await;

    // Answering `{}` would tell a page every one of its rows is unset, which is a claim about
    // credentials it never asked about.
    assert_eq!(failure.code(), "INVALID_ARGUMENTS");
}

// ---- the secret never crosses ----------------------------------------------------------------

#[tokio::test]
async fn no_response_ever_carries_the_stored_value() {
    const SECRET: &str = "sk-do-not-echo-this-value";

    let (registry, _source) = registry();

    let written = ok(
        &registry,
        SET_METHOD,
        json!({"ref": "OPENAI_API_KEY", "value": SECRET}),
    )
    .await;

    let described = ok(
        &registry,
        DESCRIBE_METHOD,
        json!({"refs": ["OPENAI_API_KEY"]}),
    )
    .await;

    // Asserted against the serialized bytes rather than against a field list: a later edit that
    // added a field would be caught only by the thing that actually crosses.
    for response in [&written, &described] {
        let text = serde_json::to_string(response).expect("serializable");
        assert!(
            !text.contains(SECRET),
            "a response carried the secret: {text}"
        );
    }
    // And the view itself has no field it could have ridden in.
    let row = &described["items"]["OPENAI_API_KEY"];
    let mut fields: Vec<&String> = row.as_object().expect("an object").keys().collect();
    fields.sort();
    assert_eq!(fields, vec!["configured", "source", "writable"]);
}

#[tokio::test]
async fn a_provider_preview_is_carried_verbatim_but_a_missing_one_is_omitted() {
    // The seam's `describe` answers `CredentialInfo`, so a provider that has a masked preview can
    // report it; one that has none reports no such field at all, rather than a null.
    struct Previewing;

    impl CredentialSource for Previewing {
        fn describe(&self, reference: &str) -> CredentialInfo {
            match reference {
                "HAS_PREVIEW" => {
                    CredentialInfo::configured("store", true).with_masked("sk-abcd...")
                }
                _ => CredentialInfo::missing(),
            }
        }

        fn set(&self, _reference: &str, _value: &str) -> Result<(), String> {
            Ok(())
        }

        fn unset(&self, _reference: &str) -> Result<bool, String> {
            Ok(true)
        }
    }

    let source: Arc<dyn CredentialSource> = Arc::new(Previewing);
    let mut registry = RemoteRegistry::new();
    register_credential_methods(&mut registry, &source);

    let answer = ok(
        &registry,
        DESCRIBE_METHOD,
        json!({"refs": ["HAS_PREVIEW", "NO_PREVIEW"]}),
    )
    .await;

    assert_eq!(
        answer["items"]["HAS_PREVIEW"]["masked"],
        json!("sk-abcd...")
    );
    assert!(
        answer["items"]["NO_PREVIEW"].get("masked").is_none(),
        "an absent preview must be omitted, not null: {}",
        answer["items"]["NO_PREVIEW"]
    );
    assert!(
        answer["items"]["NO_PREVIEW"].get("source").is_none(),
        "an unconfigured row has no source: {}",
        answer["items"]["NO_PREVIEW"]
    );
}

// ---- writes -----------------------------------------------------------------------------------

#[tokio::test]
async fn set_then_unset_round_trips_through_the_registry() {
    let (registry, source) = registry();

    ok(
        &registry,
        SET_METHOD,
        json!({"ref": "DEEPSEEK_API_KEY", "value": "sk-1"}),
    )
    .await;
    assert_eq!(source.len(), 1);

    ok(&registry, UNSET_METHOD, json!({"ref": "DEEPSEEK_API_KEY"})).await;
    assert!(source.is_empty());
}

#[tokio::test]
async fn a_blank_value_is_refused_and_the_refusal_points_at_unset() {
    let (registry, source) = registry();

    let failure = err(&registry, SET_METHOD, json!({"ref": "K", "value": ""})).await;

    // An empty credential authenticates nothing, and a page that cleared a field meant to remove
    // the entry; saying so is more useful than storing an empty string.
    assert_eq!(failure.code(), "INVALID_ARGUMENTS");
    assert!(message(&failure).contains(UNSET_METHOD), "got: {failure}");
    assert!(source.is_empty());
}

#[tokio::test]
async fn a_missing_value_field_is_refused_by_name() {
    let (registry, _source) = registry();

    let failure = err(&registry, SET_METHOD, json!({"ref": "K"})).await;

    assert_eq!(failure.code(), "INVALID_ARGUMENTS");
    assert!(message(&failure).contains("value"), "got: {failure}");
}

#[tokio::test]
async fn a_bad_reference_is_refused_on_the_write_path_too() {
    let (registry, source) = registry();

    let failure = err(
        &registry,
        SET_METHOD,
        json!({"ref": "../escape", "value": "v"}),
    )
    .await;

    // The grammar is checked on every method, not only on the read: a name that could not be
    // described must not be storable either.
    assert_eq!(failure.code(), "INVALID_ARGUMENTS");
    assert!(source.is_empty());
}

#[tokio::test]
async fn unset_of_a_reference_that_holds_nothing_is_refused() {
    let (registry, _source) = registry();

    let failure = err(&registry, UNSET_METHOD, json!({"ref": "NEVER_SET"})).await;

    // Not reported as a success: the caller meant to remove something, and "done" would hide that
    // its own listing was stale.
    assert_eq!(failure.code(), "INVALID_ARGUMENTS");
    assert!(message(&failure).contains("NEVER_SET"), "got: {failure}");
}

#[tokio::test]
async fn a_provider_refusal_is_a_failed_call_carrying_the_provider_message() {
    struct Refusing;

    impl CredentialSource for Refusing {
        fn describe(&self, _reference: &str) -> CredentialInfo {
            CredentialInfo::missing()
        }

        fn set(&self, _reference: &str, _value: &str) -> Result<(), String> {
            Err("the stored document no longer parses".to_owned())
        }

        fn unset(&self, _reference: &str) -> Result<bool, String> {
            Err("the stored document no longer parses".to_owned())
        }
    }

    let source: Arc<dyn CredentialSource> = Arc::new(Refusing);
    let mut registry = RemoteRegistry::new();
    register_credential_methods(&mut registry, &source);

    let failure = err(
        &registry,
        SET_METHOD,
        json!({"ref": "K", "value": "sk-very-secret"}),
    )
    .await;

    // `FAILED`, not `INVALID_ARGUMENTS`: the request was fine and the provider refused it, which is
    // the distinction a client needs to stop retrying for the first case and keep trying for the
    // second.
    assert_eq!(failure.code(), "FAILED");
    assert!(
        message(&failure).contains("no longer parses"),
        "the provider's own message must reach the user: {failure}"
    );
    // And the refusal names the reference, never the value.
    assert!(message(&failure).contains("`K`"), "got: {failure}");
    assert!(
        !message(&failure).contains("sk-very-secret"),
        "a refusal carried the value: {failure}"
    );
}

// ---- storage default --------------------------------------------------------------------------

#[tokio::test]
async fn the_default_source_refuses_writes_and_reports_why() {
    let source: Arc<dyn CredentialSource> = Arc::new(UnavailableCredentialSource::default_reason());
    let mut registry = RemoteRegistry::new();
    register_credential_methods(&mut registry, &source);

    let failure = err(&registry, SET_METHOD, json!({"ref": "K", "value": "v"})).await;
    assert_eq!(failure.code(), "FAILED");
    // The reason names the call a host makes to fix it; "not supported" would leave an operator
    // guessing which half to supply.
    assert!(
        message(&failure).contains("register_credential_methods"),
        "got: {failure}"
    );

    // Its read half reports nothing configured, which is true of a source that holds nothing.
    let answer = ok(&registry, DESCRIBE_METHOD, json!({"refs": ["K"]})).await;
    assert_eq!(answer["items"]["K"]["configured"], json!(false));
}

// ---- access -----------------------------------------------------------------------------------

struct ReadOnly;

impl Policy for ReadOnly {
    fn decide(&self, request: &AccessRequest) -> AccessDecision {
        match request.access {
            Access::Read => AccessDecision::Allow,
            _ => AccessDecision::Refuse {
                reason: "read-only".to_owned(),
            },
        }
    }
}

#[tokio::test]
async fn a_read_only_policy_can_describe_and_cannot_write_or_erase() {
    let source = Arc::new(MemoryCredentialSource::new());
    source.set("EXISTING", "v").unwrap();
    let dynamic: Arc<dyn CredentialSource> = source;
    let mut registry = RemoteRegistry::new().with_policy(Arc::new(ReadOnly));
    register_credential_methods(&mut registry, &dynamic);

    // The read is allowed.
    ok(&registry, DESCRIBE_METHOD, json!({"refs": ["EXISTING"]})).await;

    // Both writes are refused, `unset` as destructive rather than as an ordinary write — which is
    // why one policy can allow rotation and deny erasure.
    for (method, args) in [
        (SET_METHOD, json!({"ref": "NEW", "value": "v"})),
        (UNSET_METHOD, json!({"ref": "EXISTING"})),
    ] {
        let failure = err(&registry, method, args).await;
        assert_eq!(failure.code(), "REFUSED", "`{method}` should be refused");
    }

    // The destructive one is distinguishable in the declaration the policy sees: a policy that
    // allows rotation and denies erasure reads the two as different accesses.
    assert_eq!(
        recording_access(UNSET_METHOD).await,
        Some(Access::Destructive)
    );
    assert_eq!(recording_access(SET_METHOD).await, Some(Access::Write));
    assert_eq!(recording_access(DESCRIBE_METHOD).await, Some(Access::Read));
}

/// The access a policy is asked about for `method`, observed by mounting a recording policy.
///
/// Read through a policy rather than through a registry getter because the correspondence that
/// matters is the one the registry *acts on*: a declaration table read directly could agree with
/// itself while `authorise` consulted something else.
async fn recording_access(method: &str) -> Option<Access> {
    #[derive(Debug)]
    struct Record(Arc<std::sync::Mutex<Option<Access>>>);

    impl Policy for Record {
        fn decide(&self, request: &AccessRequest) -> AccessDecision {
            *self.0.lock().expect("the recorder is not poisoned") = Some(request.access);
            AccessDecision::Allow
        }
    }

    let seen = Arc::new(std::sync::Mutex::new(None));
    let source: Arc<dyn CredentialSource> = Arc::new(MemoryCredentialSource::new());
    let mut registry = RemoteRegistry::new().with_policy(Arc::new(Record(Arc::clone(&seen))));
    register_credential_methods(&mut registry, &source);
    // The arguments may be malformed; the policy is consulted before the handler runs, which is the
    // ordering under test here and the reason a malformed body still records an access.
    let _ = registry.dispatch(NAMESPACE, method, call(json!({}))).await;
    *seen.lock().expect("the recorder is not poisoned")
}

#[tokio::test]
async fn every_declared_method_dispatches_rather_than_reporting_a_gap() {
    let (registry, _source) = registry();

    for method in account_gap::METHODS {
        let failure = err(&registry, method, json!({})).await;
        assert_ne!(
            failure.code(),
            "UNKNOWN_METHOD",
            "`{method}` is not registered"
        );
        assert_ne!(
            failure.code(),
            "NOT_IMPLEMENTED",
            "`{method}` is declared as a gap"
        );
    }
}
