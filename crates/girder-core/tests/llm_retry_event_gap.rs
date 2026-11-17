//! Tests for the two durable retry events: their payloads, their validation, and their chain rules.
//!
//! The reference vectors come from three upstream suites, which is what makes this a port rather
//! than an interpretation:
//!
//! - `packages/session/session-format-v0-to-v1/tests/validation.spec.ts` — the released payload
//!   fixture for `llm/retry` and `llm/retry-started`, reproduced member for member.
//! - `packages/llm/llm-retry/tests/retry.spec.ts` — the recorded `event.data` object the executor
//!   writes, including the exact `policyKey` a two-code normal policy produces.
//! - `packages/llm/llm-retry/tests/persistence.spec.ts` — the `always` payload with
//!   `policyKey: '["always",500,10000,0.1]'`, and the rule that `maxRetries` is absent there.
//!
//! # Why the module is compiled by path
//!
//! `girder-core`'s crate root does not declare `llm_retry_event_gap`; adding `pub mod` and its
//! `pub use` belongs to the integrator. The module is therefore compiled into this test binary
//! directly. It touches the crate root only through `girder_core::…`, which is how the same source
//! builds in both places.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    reason = "tests assert by panicking; a failed unwrap is a failed test, and these vectors are \
              literals compared for exact wire fidelity, which is the thing under test"
)]

#[path = "../src/llm_retry_event_gap.rs"]
mod llm_retry_event_gap;

use girder_core::llm_extra_gap::{IMAGE_OFFLOAD_REQUIRED, ProviderError};

use llm_retry_event_gap::{
    Chain, ChainKey, MAX_DELAY_MS, PayloadError, ResolvedPolicy, RetryEvent, RetryId, RetryMode,
    RetryStarted, js_number, policy_key, state_key,
};

/// The chain every fixture in this file advances unless it deliberately says otherwise.
fn key() -> ChainKey {
    ChainKey::new(
        1,
        1,
        "mock",
        r#"["normal",2,["RATE_LIMIT","SERVER"],500,10000,0]"#,
    )
}

// ---- fixtures ---------------------------------------------------------------------------------

fn failure() -> ProviderError {
    ProviderError {
        message: "busy".to_owned(),
        code: "RATE_LIMIT".to_owned(),
        status: Some(429),
        ..ProviderError::default()
    }
}

fn normal_policy() -> ResolvedPolicy {
    ResolvedPolicy {
        mode: RetryMode::Normal,
        retryable_codes: vec!["SERVER".to_owned(), "RATE_LIMIT".to_owned()],
        max_retries: Some(2),
        initial_delay_ms: 500.0,
        max_delay_ms: 10_000.0,
        jitter_ratio: 0.0,
    }
}

fn always_policy() -> ResolvedPolicy {
    ResolvedPolicy {
        mode: RetryMode::Always,
        retryable_codes: Vec::new(),
        max_retries: None,
        initial_delay_ms: 500.0,
        max_delay_ms: 10_000.0,
        jitter_ratio: 0.1,
    }
}

/// Upstream's `retry.spec.ts` expected object, field for field.
fn scheduled_normal() -> RetryEvent {
    RetryEvent::normal(RetryId::new("retry-1"), key(), 1, 2, 500.0, failure())
}

fn scheduled_always() -> RetryEvent {
    RetryEvent::always(
        RetryId::new("retry-jsonl-chain"),
        ChainKey::new(1, 1, "mock", r#"["always",500,10000,0.1]"#),
        1,
        750.0,
        ProviderError {
            message: "provider busy".to_owned(),
            code: "RATE_LIMIT".to_owned(),
            status: Some(429),
            ..ProviderError::default()
        },
    )
}

// ---- the policy key ---------------------------------------------------------------------------

/// Upstream's `retryPolicyKey` output for the recorded normal policy, character for character.
///
/// This is the vector `retry.spec.ts` asserts on `event.data.policyKey`, so a key that differed by
/// one byte would be a real divergence rather than a formatting preference.
#[test]
fn a_normal_policy_key_matches_the_recorded_vector() {
    assert_eq!(
        policy_key(&normal_policy()),
        r#"["normal",2,["RATE_LIMIT","SERVER"],500,10000,0]"#
    );
}

/// The `always` arm carries neither the bound nor the codes, because the mode already says both.
#[test]
fn an_always_policy_key_omits_the_bound_and_the_codes() {
    assert_eq!(policy_key(&always_policy()), r#"["always",500,10000,0.1]"#);
}

/// Upstream sorts `[...retryableCodes].sort()`, so a caller's ordering cannot split one chain in two.
#[test]
fn the_code_list_is_sorted_so_the_key_is_canonical() {
    let mut reordered = normal_policy();
    reordered.retryable_codes = vec!["RATE_LIMIT".to_owned(), "SERVER".to_owned()];
    assert_eq!(policy_key(&reordered), policy_key(&normal_policy()));
}

/// A different bound, delay, or jitter is a different chain, which is what makes a mid-session policy
/// replacement open a new one — the behaviour upstream's replacement test pins.
#[test]
fn every_resolved_member_takes_part_in_the_key() {
    let base = policy_key(&normal_policy());
    let variants = [
        ResolvedPolicy {
            max_retries: Some(3),
            ..normal_policy()
        },
        ResolvedPolicy {
            initial_delay_ms: 1_000.0,
            ..normal_policy()
        },
        ResolvedPolicy {
            max_delay_ms: 20_000.0,
            ..normal_policy()
        },
        ResolvedPolicy {
            jitter_ratio: 0.5,
            ..normal_policy()
        },
        ResolvedPolicy {
            retryable_codes: vec!["SERVER".to_owned()],
            ..normal_policy()
        },
    ];
    for variant in variants {
        assert_ne!(policy_key(&variant), base, "{variant:?}");
    }
}

/// The resolved defaults are `resolveRetryPolicy(undefined, …)`'s, and its key is the one the
/// default policy produces — which is the shape most sessions will actually contain.
#[test]
fn the_default_policy_is_the_resolved_default_and_keys_accordingly() {
    let policy = ResolvedPolicy::default();
    assert_eq!(policy.mode, RetryMode::Normal);
    assert_eq!(policy.max_retries, Some(5));
    assert_eq!(
        policy_key(&policy),
        r#"["normal",5,["EMPTY_RESPONSE","RATE_LIMIT","SERVER","TIMEOUT","TRANSPORT"],500,10000,0.1]"#
    );
}

/// The projection key names `(provider, policyKey)`, and a policy key containing quotes is escaped
/// rather than spliced in raw — the nested quotes are what makes this worth a vector.
#[test]
fn the_state_key_nests_an_escaped_policy_key() {
    assert_eq!(
        state_key("mock", r#"["always",500,10000,0.1]"#),
        r#"["mock","[\"always\",500,10000,0.1]"]"#
    );
}

// ---- JavaScript number formatting ---------------------------------------------------------------

/// `JSON.stringify`'s spelling, which is not Rust's `Display`'s.
///
/// Each vector was produced by `node -e 'console.log(JSON.stringify(v))'`, so these are recorded
/// outputs rather than roundings of one. The interesting cases are the three regime boundaries:
/// `1e-6` is written plainly while `1e-7` is not, `1e21` switches to exponent form, and `-0` becomes
/// `0`.
#[test]
fn numbers_are_spelled_the_way_javascript_spells_them() {
    let cases: [(f64, &str); 21] = [
        (0.0, "0"),
        (-0.0, "0"),
        (0.1, "0.1"),
        (0.5, "0.5"),
        (1.0, "1"),
        (2.0, "2"),
        (10.0, "10"),
        (100.0, "100"),
        (500.0, "500"),
        (10_000.0, "10000"),
        (2_147_483_647.0, "2147483647"),
        (9_007_199_254_740_991.0, "9007199254740991"),
        (1e21, "1e+21"),
        (1.5e21, "1.5e+21"),
        (1e-6, "0.000001"),
        (1e-7, "1e-7"),
        (5e-7, "5e-7"),
        (1.2345e-7, "1.2345e-7"),
        (123.456, "123.456"),
        (-1.0, "-1"),
        (0.300_000_000_000_000_04, "0.30000000000000004"),
    ];
    for (value, expected) in cases {
        assert_eq!(js_number(value), expected, "for {value}");
    }
}

/// A non-finite value has no JSON spelling, so it becomes `null` rather than an invalid token.
#[test]
fn a_non_finite_number_becomes_null() {
    assert_eq!(js_number(f64::NAN), "null");
    assert_eq!(js_number(f64::INFINITY), "null");
    assert_eq!(js_number(f64::NEG_INFINITY), "null");
}

// ---- the scheduled payload ----------------------------------------------------------------------

/// The released fixture's members, and upstream's recorded values for them.
///
/// Each member's *serialized token* is pinned rather than the whole object's text, because a `JSON`
/// object is unordered and `serde_json` happens to sort it. The token is where the interesting
/// fidelity lives: `serde_json` renders an `f64` `500.0` as `500.0` while JavaScript writes `500`,
/// so the delay is asserted as the text a JavaScript reader would have written.
#[test]
fn a_normal_payload_round_trips_through_its_wire_form() {
    let event = scheduled_normal();
    let value = event.to_value().unwrap();
    let token = |key: &str| serde_json::to_string(&value[key]).unwrap();
    assert_eq!(token("retryId"), r#""retry-1""#);
    assert_eq!(token("turn"), "1");
    assert_eq!(token("step"), "1");
    assert_eq!(token("provider"), r#""mock""#);
    assert_eq!(token("mode"), r#""normal""#);
    assert_eq!(
        token("policyKey"),
        r#""[\"normal\",2,[\"RATE_LIMIT\",\"SERVER\"],500,10000,0]""#
    );
    assert_eq!(token("retry"), "1");
    assert_eq!(token("maxRetries"), "2");
    assert_eq!(token("delayMs"), "500");
    assert_eq!(
        token("failure"),
        r#"{"code":"RATE_LIMIT","message":"busy","status":429}"#
    );
    assert_eq!(
        value,
        serde_json::json!({
            "retryId": "retry-1",
            "turn": 1,
            "step": 1,
            "provider": "mock",
            "mode": "normal",
            "policyKey": r#"["normal",2,["RATE_LIMIT","SERVER"],500,10000,0]"#,
            "retry": 1,
            "maxRetries": 2,
            "delayMs": 500,
            "failure": {"message": "busy", "code": "RATE_LIMIT", "status": 429},
        })
    );
    assert_eq!(RetryEvent::from_value(&value).unwrap(), event);
}

/// The `always` payload's members, with the optional one absent entirely rather than `null`.
#[test]
fn an_always_payload_serializes_without_max_retries() {
    let value = scheduled_always().to_value().unwrap();
    assert!(value.get("maxRetries").is_none(), "{value}");
    assert_eq!(
        serde_json::to_string(&value["policyKey"]).unwrap(),
        r#""[\"always\",500,10000,0.1]""#
    );
    assert_eq!(serde_json::to_string(&value["delayMs"]).unwrap(), "750");
}

/// The `always` arm omits `maxRetries` entirely rather than writing `null`, which is what the
/// released validator's union requires.
#[test]
fn an_always_payload_omits_max_retries() {
    let value = scheduled_always().to_value().unwrap();
    assert!(value.get("maxRetries").is_none(), "{value}");
    assert_eq!(value["mode"], serde_json::json!("always"));
    assert_eq!(
        value["policyKey"],
        serde_json::json!(r#"["always",500,10000,0.1]"#)
    );
}

/// A delay is a JSON number, so a jittered wait keeps its fractional part instead of being rounded
/// into a wait that never happened.
#[test]
fn a_fractional_delay_survives_the_wire_form() {
    let mut event = scheduled_normal();
    event.delay_ms = 499.999_999_9;
    let value = event.to_value().unwrap();
    assert_eq!(value["delayMs"], serde_json::json!(499.999_999_9));
    assert_eq!(
        RetryEvent::from_value(&value).unwrap().delay_ms,
        499.999_999_9
    );
}

/// The failure bundle is exactly `{message, code}` plus three optional members; an absent one is
/// absent on the wire rather than `null`.
#[test]
fn every_optional_failure_member_appears_exactly_when_present() {
    let mut event = scheduled_normal();
    event.failure = ProviderError {
        message: "slow down".to_owned(),
        code: "RATE_LIMIT".to_owned(),
        status: Some(429),
        provider_retry_after_ms: Some(3_000),
        request_id: Some("req-1".to_owned()),
        offload_images: None,
    };
    let value = event.to_value().unwrap();
    assert_eq!(
        value["failure"],
        serde_json::json!({
            "message": "slow down",
            "code": "RATE_LIMIT",
            "status": 429,
            "providerRetryAfterMs": 3000,
            "requestId": "req-1",
        })
    );
    assert_eq!(
        RetryEvent::from_value(&value).unwrap().failure,
        event.failure
    );

    let bare = scheduled_normal();
    let bare_value = bare.to_value().unwrap();
    assert_eq!(
        bare_value["failure"],
        serde_json::json!({"message": "busy", "code": "RATE_LIMIT", "status": 429})
    );
}

// ---- the scheduled payload's refusals -----------------------------------------------------------

/// `normal` without a bound is the union violation the released validator names, and the addressable
/// form of it is the enum member that exists for exactly this case.
#[test]
fn a_normal_payload_without_a_bound_is_refused() {
    let mut event = scheduled_normal();
    event.max_retries = None;
    assert_eq!(event.validate(), Err(PayloadError::MaxRetriesMissing));
    assert!(event.to_value().is_err());
}

/// `always` with a bound is refused rather than ignored, because a writer that emitted it would be
/// describing a bound the mode does not honour.
#[test]
fn an_always_payload_with_a_bound_is_refused() {
    let mut event = scheduled_always();
    event.max_retries = Some(3);
    assert_eq!(event.validate(), Err(PayloadError::MaxRetriesInAlways));
}

/// A retry number above its own bound describes an attempt the policy would never have scheduled.
#[test]
fn a_retry_above_its_bound_is_refused() {
    let mut event = scheduled_normal();
    event.retry = 3;
    assert_eq!(
        event.validate(),
        Err(PayloadError::RetryExceedsMax {
            retry: 3,
            max_retries: 2
        })
    );
}

/// `maxRetries: 0` is not a bound of zero retries at the payload layer: `resolveRetryPolicy` rejects
/// it, and the payload validator requires a *positive* integer. A policy that retries nothing simply
/// never writes this event.
#[test]
fn a_zero_bound_is_refused() {
    let mut event = scheduled_normal();
    event.max_retries = Some(0);
    assert_eq!(
        event.validate(),
        Err(PayloadError::NotPositive { key: "maxRetries" })
    );
}

/// Every string member is non-empty, and `retry` starts at 1.
#[test]
fn empty_identities_and_a_zero_attempt_are_refused() {
    let mut empty_id = scheduled_normal();
    empty_id.retry_id = RetryId::new("");
    assert_eq!(
        empty_id.validate(),
        Err(PayloadError::EmptyString { key: "retryId" })
    );

    let mut empty_provider = scheduled_normal();
    empty_provider.chain.provider = String::new();
    assert_eq!(
        empty_provider.validate(),
        Err(PayloadError::EmptyString { key: "provider" })
    );

    let mut empty_key = scheduled_normal();
    empty_key.chain.policy_key = String::new();
    assert_eq!(
        empty_key.validate(),
        Err(PayloadError::EmptyString { key: "policyKey" })
    );

    let mut zero = scheduled_normal();
    zero.retry = 0;
    assert_eq!(
        zero.validate(),
        Err(PayloadError::NotPositive { key: "retry" })
    );
}

/// The delay bound is the timer range, and both ends are closed.
#[test]
fn the_delay_range_is_closed_at_both_ends() {
    assert_eq!(MAX_DELAY_MS, 2_147_483_647.0);

    let mut zero = scheduled_normal();
    zero.delay_ms = 0.0;
    assert!(zero.validate().is_ok(), "a zero delay is a real delay");

    let mut at_ceiling = scheduled_normal();
    at_ceiling.delay_ms = MAX_DELAY_MS;
    assert!(at_ceiling.validate().is_ok());

    for bad in [-1.0, -0.0, MAX_DELAY_MS + 1.0, f64::NAN, f64::INFINITY] {
        let mut event = scheduled_normal();
        event.delay_ms = bad;
        assert!(
            matches!(event.validate(), Err(PayloadError::BadDelay { .. })),
            "{bad} should be refused"
        );
    }
}

/// The failure bundle is validated on write, so a hostile or buggy provider payload cannot become a
/// durable fact.
#[test]
fn an_invalid_failure_is_refused() {
    let mut empty_message = scheduled_normal();
    empty_message.failure.message = String::new();
    assert!(matches!(
        empty_message.validate(),
        Err(PayloadError::BadFailure { .. })
    ));

    let mut bad_status = scheduled_normal();
    bad_status.failure.status = Some(999);
    assert!(matches!(
        bad_status.validate(),
        Err(PayloadError::BadFailure { .. })
    ));
}

/// `offloadImages` is a real member of the failure type this crate already carries, but the retry
/// payload's released member set does not have it: it belongs to the `image/offload` flow that reads
/// the count. Carrying it here would put a member on the wire that the validator refuses.
#[test]
fn an_offload_count_is_refused_in_a_retry_payload() {
    let mut event = scheduled_normal();
    event.failure.code = IMAGE_OFFLOAD_REQUIRED.to_owned();
    event.failure.offload_images = Some(2);
    assert_eq!(
        event.validate(),
        Err(PayloadError::UnknownFailureKey {
            key: "offloadImages".to_owned()
        })
    );
}

/// The member sets are exact in both directions, which is what stops an unknown extra member from
/// riding along unnoticed.
#[test]
fn the_reader_refuses_missing_and_unknown_members() {
    let missing = serde_json::json!({"retryId": "r", "turn": 1, "step": 1});
    assert!(matches!(
        RetryEvent::from_value(&missing),
        Err(PayloadError::MissingKey { .. })
    ));

    let mut extra = scheduled_normal().to_value().unwrap();
    extra["attempt"] = serde_json::json!(1);
    assert_eq!(
        RetryEvent::from_value(&extra),
        Err(PayloadError::UnknownKey {
            label: "llm/retry".to_owned(),
            key: "attempt".to_owned()
        })
    );

    let not_an_object = serde_json::json!([]);
    assert!(matches!(
        RetryEvent::from_value(&not_an_object),
        Err(PayloadError::NotAnObject { .. })
    ));
}

/// The local [`girder_core::SessionEvent::LlmRetry`] variant spells the same tag `{attempt, reason}`,
/// and this reader must not accept that shape: the two cannot both be right, and the upstream
/// payload is the one a harness writes.
#[test]
fn the_local_attempt_and_reason_shape_is_not_the_upstream_payload() {
    let local = serde_json::json!({"attempt": 2, "reason": "RATE_LIMIT"});
    assert!(RetryEvent::from_value(&local).is_err());
}

/// `mode` has two spellings and a third is a refusal rather than a default.
#[test]
fn an_unknown_mode_is_refused() {
    let mut value = scheduled_normal().to_value().unwrap();
    value["mode"] = serde_json::json!("auto");
    assert_eq!(
        RetryEvent::from_value(&value),
        Err(PayloadError::BadMode {
            label: "llm/retry".to_owned(),
            found: "auto".to_owned()
        })
    );
    assert_eq!(RetryMode::parse("normal"), Some(RetryMode::Normal));
    assert_eq!(RetryMode::parse("always"), Some(RetryMode::Always));
    assert_eq!(RetryMode::parse("Normal"), None);
}

// ---- the started payload ------------------------------------------------------------------------

/// Four members and no policy, exactly as the released fixture has it.
#[test]
fn a_started_payload_carries_only_identity_and_coordinates() {
    let started = RetryStarted::after(&scheduled_normal());
    assert_eq!(
        started.to_value().unwrap(),
        serde_json::json!({"retryId": "retry-1", "turn": 1, "step": 1, "retry": 1})
    );
    assert_eq!(
        RetryStarted::from_value(&started.to_value().unwrap()).unwrap(),
        started
    );
}

/// The pairing constructor is the only way in, so a started event cannot disagree with the schedule
/// it was built from.
#[test]
fn the_started_event_inherits_its_schedules_coordinates() {
    let mut scheduled = scheduled_normal();
    scheduled.chain.turn = 7;
    scheduled.chain.step = 3;
    scheduled.retry = 2;
    scheduled.retry_id = RetryId::new("chain-9");
    let started = RetryStarted::after(&scheduled);
    assert_eq!(started.retry_id, scheduled.retry_id);
    assert_eq!((started.turn, started.step, started.retry), (7, 3, 2));
}

/// The started payload's refusals are the same field rules, applied to its own member set.
#[test]
fn a_started_payload_is_validated_like_any_other() {
    let mut zero = RetryStarted::after(&scheduled_normal());
    zero.retry = 0;
    assert_eq!(
        zero.validate(),
        Err(PayloadError::NotPositive { key: "retry" })
    );

    let mut empty = RetryStarted::after(&scheduled_normal());
    empty.retry_id = RetryId::new("");
    assert_eq!(
        empty.validate(),
        Err(PayloadError::EmptyString { key: "retryId" })
    );

    let mut extra = RetryStarted::after(&scheduled_normal()).to_value().unwrap();
    extra["policyKey"] = serde_json::json!("p");
    assert!(matches!(
        RetryStarted::from_value(&extra),
        Err(PayloadError::UnknownKey { .. })
    ));
}

// ---- chain arithmetic ----------------------------------------------------------------------------

/// The first attempt of a chain takes the freshly minted id; later ones keep it.
#[test]
fn a_chain_reuses_the_id_it_minted() {
    let chain = Chain::new();
    let first = scheduled_normal();
    assert_eq!(
        chain.retry_id(&first, RetryId::new("minted")).unwrap(),
        RetryId::new("minted")
    );

    let mut chain = Chain::new();
    chain.admit(&scheduled_normal()).unwrap();
    let mut second = scheduled_normal();
    second.retry = 2;
    second.retry_id = RetryId::new("ignored-mint");
    assert_eq!(
        chain.retry_id(&second, RetryId::new("minted-2")).unwrap(),
        RetryId::new("retry-1")
    );
}

/// Opening a second chain with an id the first one used is refused: two chains sharing an id would
/// make `llm/retry-started` unable to say which schedule it pairs with.
#[test]
fn a_minted_id_already_in_use_is_refused() {
    let mut chain = Chain::new();
    chain.admit(&scheduled_normal()).unwrap();
    let mut other = scheduled_normal();
    other.chain.policy_key = "another-policy".to_owned();
    assert_eq!(
        chain.retry_id(&other, RetryId::new("retry-1")),
        Err(PayloadError::RetryIdReused {
            retry_id: RetryId::new("retry-1")
        })
    );
}

/// The counter is per `(turn, step, provider, policyKey)`: changing any one of them starts a fresh
/// chain at 1 rather than continuing the old count.
#[test]
fn each_coordinate_starts_its_own_chain() {
    let mut chain = Chain::new();
    chain.admit(&scheduled_normal()).unwrap();

    for varied in [
        RetryEvent {
            chain: ChainKey::new(2, 1, "mock", key().policy_key),
            ..scheduled_normal()
        },
        RetryEvent {
            chain: ChainKey::new(1, 2, "mock", key().policy_key),
            ..scheduled_normal()
        },
        RetryEvent {
            chain: ChainKey::new(1, 1, "other", key().policy_key),
            ..scheduled_normal()
        },
        RetryEvent {
            chain: ChainKey::new(1, 1, "mock", "other"),
            ..scheduled_normal()
        },
    ] {
        assert_eq!(chain.count(&varied), 0, "{varied:?}");
        assert!(chain.prior(&varied).is_none());
    }

    let same = scheduled_normal();
    assert_eq!(chain.count(&same), 1);
}

/// `always` is unbounded; `normal` stops at its bound, counted over the chain.
#[test]
fn the_bound_is_the_chains_counter_not_the_codes_opinion() {
    let mut chain = Chain::new();
    let mut event = scheduled_always();
    event.retry = 9;
    assert!(chain.should_schedule(&event), "always has no bound");

    let bounded = scheduled_normal();
    assert!(chain.should_schedule(&bounded));

    chain.admit(&bounded).unwrap();
    let mut second = scheduled_normal();
    second.retry = 2;
    assert!(chain.should_schedule(&second));

    chain.admit(&second).unwrap();
    let mut third = scheduled_normal();
    third.retry = 3;
    assert!(!chain.should_schedule(&third), "two retries is the bound");
}

/// The attempt number must continue the chain, and a gap is refused rather than renumbered.
///
/// The gap is checked on an `always` chain because a bounded one would refuse `retry: 3` against its
/// own `maxRetries` first, and the two refusals are different claims: the bound says the policy would
/// never have scheduled it, the sequence says the chain disagrees with its history.
#[test]
fn a_skipped_attempt_number_is_refused() {
    let mut chain = Chain::new();
    let mut first = scheduled_always();
    first.retry = 1;
    chain.admit(&first).unwrap();

    let mut skipping = scheduled_always();
    skipping.retry = 3;
    assert_eq!(
        chain.admit(&skipping),
        Err(PayloadError::RetrySkipsSequence {
            expected: 2,
            found: 3
        })
    );

    let mut second = scheduled_always();
    second.retry = 2;
    assert!(chain.admit(&second).is_ok());
}

/// A continuing attempt that changes its id is refused, because the chain's identity is what pairs
/// the started transition with the schedule.
#[test]
fn a_changed_id_mid_chain_is_refused() {
    let mut chain = Chain::new();
    chain.admit(&scheduled_normal()).unwrap();
    let mut second = scheduled_normal();
    second.retry = 2;
    second.retry_id = RetryId::new("different");
    assert_eq!(
        chain.admit(&second),
        Err(PayloadError::RetryIdChanged {
            expected: RetryId::new("retry-1"),
            found: RetryId::new("different")
        })
    );
}

/// A second chain that borrows the first one's id is refused at admission too, not only at minting.
#[test]
fn a_borrowed_id_is_refused_on_a_new_chain() {
    let mut chain = Chain::new();
    chain.admit(&scheduled_normal()).unwrap();
    let mut other = scheduled_normal();
    other.chain.policy_key = "another-policy".to_owned();
    assert_eq!(
        chain.admit(&other),
        Err(PayloadError::RetryIdReused {
            retry_id: RetryId::new("retry-1")
        })
    );
}

/// An invalid payload never enters the chain, because a fold over invalid rows proves nothing.
#[test]
fn an_invalid_payload_is_refused_before_it_is_folded() {
    let mut chain = Chain::new();
    let mut event = scheduled_normal();
    event.delay_ms = -1.0;
    assert!(matches!(
        chain.admit(&event),
        Err(PayloadError::BadDelay { .. })
    ));
    assert_eq!(chain.count(&scheduled_normal()), 0);
}

// ---- the started transition's chain rules ---------------------------------------------------------

/// A start pairs with a scheduled attempt; an orphan is refused.
#[test]
fn an_orphan_start_is_refused() {
    let mut chain = Chain::new();
    let started = RetryStarted::after(&scheduled_normal());
    assert_eq!(
        chain.admit_started(&started),
        Err(PayloadError::OrphanStarted {
            retry_id: RetryId::new("retry-1"),
            retry: 1
        })
    );
}

/// A start whose coordinates drifted from its schedule is refused: the two rows must agree, or a
/// reader cannot tell which step was retried.
#[test]
fn a_start_that_moves_its_coordinates_is_refused() {
    let mut chain = Chain::new();
    chain.admit(&scheduled_normal()).unwrap();
    let mut started = RetryStarted::after(&scheduled_normal());
    started.step = 4;
    assert_eq!(
        chain.admit_started(&started),
        Err(PayloadError::StartedCoordinatesChanged {
            scheduled_turn: 1,
            scheduled_step: 1,
            turn: 1,
            step: 4
        })
    );
}

/// The wait completes once, so the transition is admitted once.
#[test]
fn a_repeated_start_is_refused() {
    let mut chain = Chain::new();
    chain.admit(&scheduled_normal()).unwrap();
    let started = RetryStarted::after(&scheduled_normal());
    chain.admit_started(&started).unwrap();
    assert_eq!(
        chain.admit_started(&started),
        Err(PayloadError::StartedRepeats {
            retry_id: RetryId::new("retry-1"),
            retry: 1
        })
    );
}

/// The whole lifecycle upstream writes — schedule attempt 1, wait, start, schedule attempt 2 — is
/// admitted in order, which is the only shape a real log has.
#[test]
fn the_full_lifecycle_is_admitted() {
    let mut chain = Chain::new();
    let first = scheduled_normal();
    chain.admit(&first).unwrap();
    chain.admit_started(&RetryStarted::after(&first)).unwrap();

    let mut second = scheduled_normal();
    second.retry = 2;
    chain.admit(&second).unwrap();
    chain.admit_started(&RetryStarted::after(&second)).unwrap();

    assert_eq!(chain.count(&second), 2);
    // A third attempt is past the bound of two, so it is not scheduled even though it would fold.
    let mut third = scheduled_normal();
    third.retry = 3;
    assert!(!chain.should_schedule(&third));
    // And the payload itself refuses `retry: 3` against `maxRetries: 2`.
    assert!(matches!(
        third.validate(),
        Err(PayloadError::RetryExceedsMax { .. })
    ));
}

/// An `always` chain is admitted without a bound and never becomes unschedulable.
#[test]
fn an_always_chain_stays_open() {
    let mut chain = Chain::new();
    let mut event = scheduled_always();
    chain.admit(&event).unwrap();
    chain.admit_started(&RetryStarted::after(&event)).unwrap();
    for retry in 2..=5 {
        event.retry = retry;
        assert!(chain.should_schedule(&event));
        chain.admit(&event).unwrap();
        chain.admit_started(&RetryStarted::after(&event)).unwrap();
    }
    assert_eq!(chain.count(&event), 5);
}

// ---- number-token edge cases ---------------------------------------------------------------------

/// `1e3` is an integer to JavaScript — `Number.isSafeInteger(1e3)` is `true` — so a count written in
/// exponent form must be read as `1000`, not refused for not being an integer token.
#[test]
fn a_count_in_exponent_form_is_read_as_its_integer() {
    let mut value = scheduled_normal().to_value().unwrap();
    value["turn"] = serde_json::from_str("1e3").unwrap();
    let event = RetryEvent::from_value(&value).unwrap();
    assert_eq!(event.chain.turn, 1000);
}

/// `-0` is refused as a count and as a delay, even though `as_u64` would report `Some(0)`.
///
/// Upstream's clause is `!Object.is(value, -0)`, which exists because both `Number.isSafeInteger(-0)`
/// and `-0 >= 0` are `true`: without it, a producer writing `-0` would pass unnoticed.
#[test]
fn a_negative_zero_count_or_delay_is_refused() {
    let mut count = scheduled_normal().to_value().unwrap();
    count["turn"] = serde_json::from_str("-0").unwrap();
    assert!(matches!(
        RetryEvent::from_value(&count),
        Err(PayloadError::NotCount { .. })
    ));

    let mut delay = scheduled_normal().to_value().unwrap();
    delay["delayMs"] = serde_json::from_str("-0").unwrap();
    assert!(matches!(
        RetryEvent::from_value(&delay),
        Err(PayloadError::BadDelay { .. })
    ));
}

/// A fractional count is refused: `1.5` attempts is not an attempt number.
#[test]
fn a_fractional_count_is_refused() {
    let mut value = scheduled_normal().to_value().unwrap();
    value["retry"] = serde_json::json!(1.5);
    assert!(matches!(
        RetryEvent::from_value(&value),
        Err(PayloadError::NotCount { .. })
    ));
}

/// A count past `2^53` is refused, because a JavaScript reader could not distinguish it from its
/// neighbour.
#[test]
fn a_count_past_the_safe_integer_bound_is_refused() {
    let mut value = scheduled_normal().to_value().unwrap();
    value["turn"] = serde_json::json!(9_007_199_254_740_992u64);
    assert!(matches!(
        RetryEvent::from_value(&value),
        Err(PayloadError::NotCount { .. })
    ));
}
