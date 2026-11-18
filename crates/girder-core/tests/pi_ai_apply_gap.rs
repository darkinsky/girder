//! Tests for the pi-ai `apply` gap: profile resolution, serviceability, and the pure half of the mount.
//!
//! The module is included by `#[path]` because the crate-root wiring belongs to the integrator: it is
//! written and reviewed, and a test that waited for the declaration could not run. That is also why
//! the imports below reach for `girder_core::` rather than `crate::` — inside a `#[path]` module
//! `crate::` names *this* test crate.
//!
//! # What is worth pinning
//!
//! Every assertion here is a decision nobody watching the system can see being made:
//!
//! - **The `resolveProfiles` defaults are the adapter's, not the schema's.** `streamIdleTimeoutMs`
//!   300000, the five image bounds, and a quantum *capped at* `maxRequestImages` rather than merely
//!   defaulted to 10. A route configured with 4 images must offload in steps of 4, or the first
//!   overflow would try to remove more occurrences than it has.
//! - **`assertServiceable` only re-resolves what changed.** A stored route broken by a catalog upgrade
//!   must stay editable; if it were re-resolved, one stale route would make the settings page
//!   unopenable. This is the single most consequential line in `config.ts`.
//! - **A deferred read keeps the route; a strict write refuses it.** Same input, opposite outcome, and
//!   the difference is which of the two is allowed to destroy configuration.
//! - **A named-but-missing credential fails loud.** Handing pi-ai nothing would let it pick up an
//!   unrelated ambient key and bill another tenant.
//! - **A catalog route reuses its catalog provider.** Rebuilding one from parts would drop API
//!   implementations this build cannot reconstruct.
//! - **The registration plan is a decision, not a call.** Facts compared before routes, so a naked
//!   mount stays dormant and a route reorder does not churn the registry.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::panic
)]

#[path = "../src/pi_ai_apply_gap.rs"]
mod pi_ai_apply_gap;

use std::collections::BTreeMap;

use serde_json::json;

use girder_core::provider_catalog_gap::{
    CatalogModel, DEFAULT_INPUT, InstalledCatalog, Modality, ModelFields, ReasoningEfforts,
    ThinkingLevel, Validation,
};

use pi_ai_apply_gap::{
    DEFAULT_IMAGE_OFFLOAD_COUNT_QUANTUM, DEFAULT_MAX_REQUEST_IMAGE_BYTES,
    DEFAULT_MAX_REQUEST_IMAGES, DEFAULT_REQUEST_IMAGE_MAX_BYTES,
    DEFAULT_REQUEST_IMAGE_PIXEL_BUDGET, DEFAULT_STREAM_IDLE_TIMEOUT_MS, DirectoryEntry,
    INVALID_CREDENTIAL, InstalledRoute, InstalledRoutes, MISSING_CREDENTIAL, NS,
    PiAiProviderProfile, Portability, RegistrationAction, RegistrationState, assert_serviceable,
    assert_valid_headers, build_provider, can_address_credential_record, config_hooks,
    directory_entries, is_header_name, is_header_value, is_legal_api_key_char,
    login_offerable_providers, plan_registration, portability_of, providers_from_json,
    record_key_for, registration_facts, reject_removed_fields, resolve_profiles,
    resolve_profiles_from_json, resolve_route_credential, stored_discovery_profile,
};

// ---- fixtures -----------------------------------------------------------------------------------

fn model(id: &str) -> CatalogModel {
    CatalogModel {
        id: id.to_owned(),
        name: id.to_owned(),
        api: "openai-completions".to_owned(),
        provider: "acme".to_owned(),
        base_url: "https://acme.example/v1".to_owned(),
        input: DEFAULT_INPUT.to_vec(),
        context_window: 128_000,
        max_tokens: 8_192,
        reasoning: false,
        thinking_level_map: None,
        compat: None,
    }
}

/// An installed catalog with one two-model provider, which is enough to be a catalog route.
fn installed_acme() -> InstalledRoutes {
    let mut models = InstalledCatalog::new();
    models.insert("acme-large".to_owned(), model("acme-large"));
    models.insert("acme-think".to_owned(), model("acme-think"));
    let mut routes = InstalledRoutes::new();
    routes.insert(
        "acme".to_owned(),
        InstalledRoute {
            models,
            base_url: Some("https://acme.example/v1".to_owned()),
            api: "openai-completions".to_owned(),
            oauth: false,
        },
    );
    routes
}

fn profiles(entries: &[(&str, PiAiProviderProfile)]) -> BTreeMap<String, PiAiProviderProfile> {
    entries
        .iter()
        .map(|(key, profile)| ((*key).to_owned(), profile.clone()))
        .collect()
}

fn catalog_profile() -> PiAiProviderProfile {
    PiAiProviderProfile {
        api_key_env: Some("ACME_API_KEY".to_owned()),
        ..PiAiProviderProfile::default()
    }
}

fn resolved_acme() -> BTreeMap<String, pi_ai_apply_gap::ResolvedPiAiProviderProfile> {
    resolve_profiles(
        &profiles(&[("acme", catalog_profile())]),
        &installed_acme(),
        Validation::Strict,
    )
    .expect("a bare catalog route resolves")
}

// ---- capability classification ------------------------------------------------------------------

#[test]
fn config_ts_is_almost_entirely_portable() {
    // The premise of this module: the earlier report called the 510 missing lines of `config.ts` an
    // unportable plugin concern. Six of its seven capabilities are pure functions.
    let hooks = config_hooks();
    let pure = hooks
        .iter()
        .filter(|(_, portability)| portability.is_portable())
        .count();
    // Six of seven: everything but the schema, which is the one row a port reproduces as behaviour
    // inside resolution rather than as itself.
    assert_eq!(pure, hooks.len() - 1);
    assert_eq!(portability_of("resolveProfiles"), Some(Portability::Pure));
    assert_eq!(portability_of("assertServiceable"), Some(Portability::Pure));
    // The one non-portable row is the schema, and it is non-portable in name only: its contribution is
    // defaults and bounds, which resolution re-asserts.
    assert_eq!(
        portability_of("schemastery schemas"),
        Some(Portability::Schema)
    );
    assert!(!portability_of("schemastery schemas").unwrap().is_portable());
}

#[test]
fn index_ts_is_mostly_the_plugin_host() {
    let hooks = pi_ai_apply_gap::apply_hooks();
    let cordis = hooks
        .iter()
        .filter(|(_, portability)| *portability == Portability::Cordis)
        .count();
    // Nine of fifteen: `apply` itself plus the eight behaviours it wraps around host calls.
    assert_eq!(cordis, 9);
    assert_eq!(
        portability_of("registerPiAiFlows injection"),
        Some(Portability::Cordis)
    );
    assert_eq!(
        portability_of("ensureRegistrationFacts"),
        Some(Portability::Cordis)
    );
    // The three pure ones are the ones this module implements as functions.
    for hook in [
        "registrationFacts",
        "directoryEntries",
        "storedDiscoveryProfile",
    ] {
        assert_eq!(portability_of(hook), Some(Portability::Pure), "{hook}");
    }
    assert_eq!(
        portability_of("PiAiAdapter construction"),
        Some(Portability::PiAiRuntime)
    );
    assert_eq!(portability_of("not a hook"), None);
}

// ---- profile reading ----------------------------------------------------------------------------

#[test]
fn providers_are_a_dict_not_an_array() {
    let error = providers_from_json(&json!([{"provider": "acme"}])).unwrap_err();
    assert!(
        error.to_string().contains("not an array of profiles"),
        "{error}"
    );
}

#[test]
fn removed_pre_release_fields_are_refused_by_name() {
    let error = providers_from_json(&json!({
        "acme": {"provider": "acme"}
    }))
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("moved to the providers dict key"),
        "{error}"
    );

    let error = providers_from_json(&json!({
        "acme": {"maxRetries": 3}
    }))
    .unwrap_err();
    assert!(
        error.to_string().contains("compose agent recovery"),
        "{error}"
    );

    // The three-key form names both.
    let error = reject_removed_fields("acme", json!({"maxRetryDelayMs": 10}).as_object().unwrap())
        .unwrap_err();
    assert!(
        error.to_string().contains("maxRetries or maxRetryDelayMs"),
        "{error}"
    );
}

#[test]
fn an_unknown_profile_key_is_refused_rather_than_ignored() {
    // A `streamIdleTimoutMs` typo must not resolve to the default while looking configured.
    let error = providers_from_json(&json!({
        "acme": {"streamIdleTimoutMs": 1000}
    }))
    .unwrap_err();
    assert!(error.to_string().contains("unknown key"), "{error}");
    assert!(error.to_string().contains("streamIdleTimeoutMs"), "{error}");
}

#[test]
fn a_profile_reads_every_member_it_declares() {
    let value = json!({
        "acme": {
            "apiKeyEnv": "ACME_API_KEY",
            "displayName": "Acme",
            "api": "openai-completions",
            "baseURL": "https://acme.example/v1",
            "models": [{"id": "acme-large", "name": "Large", "contextWindow": 200_000,
                        "maxTokens": 4_096, "input": ["text", "image"]}],
            "modelOverrides": {"acme-think": {"maxTokens": 1_024}},
            "compat": {"supportsStore": true},
            "defaultContextWindow": 4_096,
            "defaultMaxTokens": 1_024,
            "defaultInput": ["text"],
            "headers": {"X-Acme": "on"},
            "reasoning": "high",
            "thinkingBudgets": {"high": 8_192},
            "cacheRetention": "long",
            "transport": "sse",
            "timeoutMs": 30_000,
            "websocketConnectTimeoutMs": 5_000,
            "streamIdleTimeoutMs": 1_000,
            "maxRequestImageBytes": 1_048_576,
            "maxRequestImages": 4,
            "imageOffloadCountQuantum": 2,
            "requestImagePixelBudget": 1_048_576,
            "requestImageMaxBytes": 524_288,
            "retryPolicy": {"maxRetries": 2}
        }
    });
    let read = providers_from_json(&value).unwrap();
    let profile = &read["acme"];
    assert_eq!(profile.api_key_env.as_deref(), Some("ACME_API_KEY"));
    assert_eq!(profile.display_name.as_deref(), Some("Acme"));
    assert_eq!(profile.models.as_ref().unwrap().len(), 1);
    assert_eq!(
        profile.models.as_ref().unwrap()[0].fields.input,
        Some(vec![Modality::Text, Modality::Image])
    );
    assert_eq!(
        profile.model_overrides["acme-think"].max_tokens,
        Some(1_024)
    );
    assert_eq!(profile.reasoning, Some(ThinkingLevel::High));
    assert_eq!(profile.thinking_budgets.unwrap().high, Some(8_192));
    assert_eq!(profile.headers["X-Acme"], "on");
    assert_eq!(profile.max_request_images, Some(4));
    // `mode` is absent, and upstream's schema defaults it to `normal` — see the retry test below.
    assert!(profile.retry_policy.is_some());
}

#[test]
fn a_model_entry_states_its_id_apart_from_its_fields() {
    let error = providers_from_json(&json!({
        "acme": {"models": [{"id": "acme-large", "context": 1}]}
    }))
    .unwrap_err();
    assert!(error.to_string().contains("unknown key"), "{error}");

    let error = providers_from_json(&json!({
        "acme": {"modelOverrides": {"acme-large": {"id": "other"}}}
    }))
    .unwrap_err();
    assert!(
        error.to_string().contains("which is the dict key"),
        "{error}"
    );
}

#[test]
fn reasoning_efforts_only_lets_off_omit_its_wire_value() {
    let read = providers_from_json(&json!({
        "acme": {"modelOverrides": {"acme-think": {"reasoningEfforts": {"off": null, "high": "high"}}}}
    }))
    .unwrap();
    let efforts = read["acme"].model_overrides["acme-think"]
        .reasoning_efforts
        .clone()
        .unwrap();
    let ReasoningEfforts::Levels(levels) = efforts else {
        panic!("a level map is not a disable");
    };
    assert_eq!(levels[&ThinkingLevel::Off], None);
    assert_eq!(levels[&ThinkingLevel::High], Some("high".to_owned()));

    // A non-`off` level with no spelling would be written but not applied.
    let error = providers_from_json(&json!({
        "acme": {"modelOverrides": {"acme-think": {"reasoningEfforts": {"high": null}}}}
    }))
    .unwrap_err();
    assert!(error.to_string().contains("only off may leave"), "{error}");

    // An empty map declares nothing, and is neither a spelling of inherit nor of disable.
    let error = providers_from_json(&json!({
        "acme": {"modelOverrides": {"acme-think": {"reasoningEfforts": {}}}}
    }))
    .unwrap_err();
    assert!(error.to_string().contains("declares nothing"), "{error}");

    let read = providers_from_json(&json!({
        "acme": {"modelOverrides": {"acme-think": {"reasoningEfforts": false}}}
    }))
    .unwrap();
    assert_eq!(
        read["acme"].model_overrides["acme-think"].reasoning_efforts,
        Some(ReasoningEfforts::Disabled)
    );
}

#[test]
fn headers_are_judged_by_what_fetch_can_send() {
    assert!(is_header_name("X-Acme"));
    assert!(is_header_name("x.y_z~1"));
    assert!(!is_header_name(""));
    assert!(!is_header_name("X Acme"));
    assert!(!is_header_name("X-Acme:"));

    assert!(is_header_value("plain"));
    assert!(is_header_value("tab\there"));
    assert!(!is_header_value("line\nbreak"));
    assert!(!is_header_value("null\0byte"));
    // Latin-1 is representable as bytes, which is `Headers`' limit; refusing it is a different
    // decision from refusing a control character.
    assert!(is_header_value("caf\u{e9}"));
    assert!(!is_header_value("emoji \u{1f600}"));

    let mut headers = BTreeMap::new();
    headers.insert("X-Good".to_owned(), "ok".to_owned());
    assert!(assert_valid_headers("acme", &headers).is_ok());

    headers.insert("X-Bad".to_owned(), "two\nlines".to_owned());
    let error = assert_valid_headers("acme", &headers).unwrap_err();
    assert!(error.to_string().contains("not valid for Fetch"), "{error}");
}

// ---- resolution defaults ------------------------------------------------------------------------

#[test]
fn a_bare_catalog_route_takes_every_adapter_default() {
    let resolved = resolved_acme();
    let profile = &resolved["acme"];
    assert_eq!(profile.display_name, "acme");
    // Compared as a difference rather than for equality: the interval is an `f64` because upstream
    // admits a fractional one, and an exact `==` on a float is a lint for a reason.
    assert!(
        (profile.stream_idle_timeout_ms - 300_000.0).abs() < f64::EPSILON,
        "{}",
        profile.stream_idle_timeout_ms
    );
    assert_eq!(
        profile.max_request_image_bytes,
        DEFAULT_MAX_REQUEST_IMAGE_BYTES
    );
    assert_eq!(profile.max_request_images, DEFAULT_MAX_REQUEST_IMAGES);
    assert_eq!(
        profile.image_offload_count_quantum,
        DEFAULT_IMAGE_OFFLOAD_COUNT_QUANTUM
    );
    assert_eq!(
        profile.request_image_pixel_budget,
        DEFAULT_REQUEST_IMAGE_PIXEL_BUDGET
    );
    assert_eq!(
        profile.request_image_max_bytes,
        DEFAULT_REQUEST_IMAGE_MAX_BYTES
    );
    assert_eq!(profile.default_input, DEFAULT_INPUT.to_vec());
    assert_eq!(profile.default_context_window, 1_000_000);
    assert_eq!(profile.default_max_tokens, 128_000);
    assert!(profile.is_serviceable());
    // The catalog route's models are materialized, in configuration order.
    let models: Vec<&str> = profile
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(models, ["acme-large", "acme-think"]);
}

#[test]
fn a_small_image_budget_caps_the_offload_quantum() {
    // `min(10, maxRequestImages)`, not the flat 10: a route capped at four images cannot offload ten
    // occurrences in one step.
    let profile = PiAiProviderProfile {
        max_request_images: Some(4),
        ..PiAiProviderProfile::default()
    };
    let resolved = resolve_profiles(
        &profiles(&[("acme", profile)]),
        &installed_acme(),
        Validation::Strict,
    )
    .unwrap();
    assert_eq!(resolved["acme"].image_offload_count_quantum, 4);
}

#[test]
fn a_quantum_above_the_image_cap_is_refused() {
    let profile = PiAiProviderProfile {
        max_request_images: Some(4),
        image_offload_count_quantum: Some(5),
        ..PiAiProviderProfile::default()
    };
    let error = resolve_profiles(
        &profiles(&[("acme", profile)]),
        &installed_acme(),
        Validation::Strict,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no greater than maxRequestImages"),
        "{error}"
    );
}

#[test]
fn structural_faults_are_refused_before_any_catalog_work() {
    let cases: Vec<(PiAiProviderProfile, &str)> = vec![
        (
            PiAiProviderProfile {
                base_url: Some(String::new()),
                ..PiAiProviderProfile::default()
            },
            "empty baseURL",
        ),
        (
            PiAiProviderProfile {
                display_name: Some(String::new()),
                ..PiAiProviderProfile::default()
            },
            "empty displayName",
        ),
        (
            PiAiProviderProfile {
                default_input: Some(Vec::new()),
                ..PiAiProviderProfile::default()
            },
            "at least one modality",
        ),
        (
            PiAiProviderProfile {
                api_key_env: Some("NOT-A-NAME".to_owned()),
                ..PiAiProviderProfile::default()
            },
            "environment-variable name",
        ),
        (
            PiAiProviderProfile {
                stream_idle_timeout_ms: Some(0.0),
                ..PiAiProviderProfile::default()
            },
            "positive finite number",
        ),
        (
            PiAiProviderProfile {
                stream_idle_timeout_ms: Some(-1.0),
                ..PiAiProviderProfile::default()
            },
            "positive finite number",
        ),
        (
            PiAiProviderProfile {
                max_request_image_bytes: Some(0),
                ..PiAiProviderProfile::default()
            },
            "positive integer",
        ),
        (
            PiAiProviderProfile {
                max_request_images: Some(0),
                ..PiAiProviderProfile::default()
            },
            "positive safe integer",
        ),
        (
            PiAiProviderProfile {
                request_image_pixel_budget: Some(0),
                ..PiAiProviderProfile::default()
            },
            "positive safe integer",
        ),
        (
            PiAiProviderProfile {
                request_image_max_bytes: Some(0),
                ..PiAiProviderProfile::default()
            },
            "positive safe integer",
        ),
    ];
    for (profile, expected) in cases {
        let error = resolve_profiles(
            &profiles(&[("acme", profile)]),
            &installed_acme(),
            Validation::Strict,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?} in {error}"
        );
    }
}

#[test]
fn an_empty_provider_key_is_refused() {
    let error = resolve_profiles(
        &profiles(&[("", PiAiProviderProfile::default())]),
        &installed_acme(),
        Validation::Strict,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("keyed by provider route"),
        "{error}"
    );
}

#[test]
fn an_omitted_provider_map_resolves_to_the_dormant_set() {
    let resolved =
        resolve_profiles(&BTreeMap::new(), &installed_acme(), Validation::Strict).unwrap();
    assert!(resolved.is_empty());
    assert_eq!(
        plan_registration(&resolved, None).action,
        RegistrationAction::Dormant
    );
}

#[test]
fn a_retry_policy_without_a_mode_is_normal_mode() {
    // Upstream's `normalPolicySchema` declares `mode` with a constant, so a block stating only
    // `maxRetries` is accepted and means normal mode. The strict parser refuses that shape, which is
    // why the profile reader supplies the default.
    let read = providers_from_json(&json!({
        "acme": {"retryPolicy": {"maxRetries": 2}}
    }))
    .unwrap();
    assert!(read["acme"].retry_policy.is_some());
    let resolved = resolve_profiles(&read, &installed_acme(), Validation::Strict).unwrap();
    assert_eq!(resolved["acme"].retry_policy.max_retries, Some(2));

    // An explicit `always` is still honoured, and carries no retry count.
    let read = providers_from_json(&json!({
        "acme": {"retryPolicy": {"mode": "always"}}
    }))
    .unwrap();
    let resolved = resolve_profiles(&read, &installed_acme(), Validation::Strict).unwrap();
    assert_eq!(resolved["acme"].retry_policy.max_retries, None);
}

// ---- deferred vs strict -------------------------------------------------------------------------

#[test]
fn a_deferred_read_keeps_a_route_a_strict_write_refuses() {
    // A route the installed catalog does not describe and that lists no models resolves nothing. A
    // write must refuse it; a stored read must keep it visible and editable.
    let orphan = PiAiProviderProfile {
        api: Some("openai-completions".to_owned()),
        ..PiAiProviderProfile::default()
    };
    let map = profiles(&[("orphan", orphan)]);

    let error = resolve_profiles(&map, &installed_acme(), Validation::Strict).unwrap_err();
    assert!(error.to_string().contains("resolves no models"), "{error}");

    let resolved = resolve_profiles(&map, &installed_acme(), Validation::Deferred).unwrap();
    let profile = &resolved["orphan"];
    assert!(!profile.is_serviceable());
    assert!(profile.pi_provider.is_none());
    let diagnostic = profile.catalog_error.clone().unwrap();
    assert!(diagnostic.contains("resolves no models"), "{diagnostic}");
}

#[test]
fn a_deferred_read_never_keeps_a_scalar_fault() {
    // The strict/deferred split covers catalog faults only. A scalar fault is a configuration that
    // cannot be interpreted at all, and a settings form cannot show it either way.
    let profile = PiAiProviderProfile {
        base_url: Some(String::new()),
        ..PiAiProviderProfile::default()
    };
    let error = resolve_profiles(
        &profiles(&[("acme", profile)]),
        &installed_acme(),
        Validation::Deferred,
    )
    .unwrap_err();
    assert!(error.to_string().contains("empty baseURL"), "{error}");
}

#[test]
fn a_stored_route_keeps_its_model_diagnostics() {
    // An *override* naming a model the catalog no longer describes: deferred mode keeps the route
    // serving the rest of the catalog and records the diagnostic beside it, which is what makes the
    // product of a catalog upgrade repairable rather than deleted.
    let mut routes = installed_acme();
    let route = routes.get_mut("acme").unwrap();
    route.models.remove("acme-think");
    let mut configured = PiAiProviderProfile::default();
    configured
        .model_overrides
        .insert("acme-think".to_owned(), ModelFields::default());

    let resolved = resolve_profiles(
        &profiles(&[("acme", configured.clone())]),
        &routes,
        Validation::Deferred,
    )
    .unwrap();
    let profile = &resolved["acme"];
    assert_eq!(profile.models.len(), 1);
    assert_eq!(profile.models[0].id, "acme-large");
    assert!(profile.model_errors.contains_key("acme-think"));
    assert!(profile.catalog_error.is_some());
    // The route still serves, so it is still registered.
    assert!(profile.is_serviceable());

    // A write refuses the same configuration, because the override has no referent.
    let error = resolve_profiles(
        &profiles(&[("acme", configured)]),
        &routes,
        Validation::Strict,
    )
    .unwrap_err();
    assert!(error.to_string().contains("acme-think"), "{error}");
}

// ---- provider construction ----------------------------------------------------------------------

#[test]
fn a_catalog_route_reuses_its_catalog_provider() {
    let built = build_provider(
        "acme",
        "Acme",
        None,
        None,
        true,
        installed_acme().get("acme"),
        vec![model("acme-large")],
    )
    .unwrap();
    assert!(built.reuses_catalog_provider);
    assert_eq!(built.api, "openai-completions");
    // Provider-level `baseUrl` is display metadata, so the catalog endpoint prefills it.
    assert_eq!(built.base_url.as_deref(), Some("https://acme.example/v1"));
    assert!(built.names_credential);
}

#[test]
fn an_explicit_protocol_builds_a_provider_from_the_table() {
    let built = build_provider(
        "acme",
        "Acme",
        Some("anthropic-messages"),
        Some("https://proxy.example"),
        false,
        installed_acme().get("acme"),
        vec![model("acme-large")],
    )
    .unwrap();
    assert!(!built.reuses_catalog_provider);
    assert_eq!(built.api, "anthropic-messages");
    assert_eq!(built.base_url.as_deref(), Some("https://proxy.example"));
    assert!(!built.names_credential);
}

#[test]
fn a_protocol_this_build_cannot_serve_is_refused() {
    let error = build_provider(
        "acme",
        "Acme",
        Some("bedrock-sigv4"),
        None,
        false,
        None,
        Vec::new(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("cannot serve"), "{error}");
    // The supported set is named so the operator can pick one.
    assert!(error.to_string().contains("openai-completions"), "{error}");

    // A route naming no protocol and matching no catalog entry interpolates `undefined`, as upstream
    // does; the spelling is kept so the two diagnostics are recognizable as the same fault.
    let error = build_provider("acme", "Acme", None, None, false, None, Vec::new()).unwrap_err();
    assert!(error.to_string().contains("\"undefined\""), "{error}");
}

// ---- assertServiceable --------------------------------------------------------------------------

#[test]
fn only_changed_profiles_are_re_resolved() {
    // The stored profile is broken (it names a route the catalog lost), but it is unchanged, so an edit
    // to a different provider must still pass. Without the narrowing, one stale route would make the
    // settings page unopenable.
    let broken = PiAiProviderProfile {
        api: Some("openai-completions".to_owned()),
        ..PiAiProviderProfile::default()
    };
    let previous = profiles(&[("orphan", broken.clone()), ("acme", catalog_profile())]);

    let mut candidate = previous.clone();
    candidate.insert(
        "acme".to_owned(),
        PiAiProviderProfile {
            display_name: Some("Acme Renamed".to_owned()),
            ..catalog_profile()
        },
    );
    assert!(assert_serviceable(&candidate, Some(&previous), &installed_acme()).is_ok());

    // The same broken profile, now changed, is refused and named.
    let mut candidate = previous.clone();
    candidate.insert(
        "orphan".to_owned(),
        PiAiProviderProfile {
            api: Some("anthropic-messages".to_owned()),
            ..broken
        },
    );
    let error = assert_serviceable(&candidate, Some(&previous), &installed_acme()).unwrap_err();
    assert_eq!(error.provider(), "orphan");
    assert!(error.to_string().contains("resolves no models"), "{error}");
}

#[test]
fn a_removed_profile_needs_no_validation() {
    let previous = profiles(&[
        (
            "orphan",
            PiAiProviderProfile {
                api: Some("openai-completions".to_owned()),
                ..PiAiProviderProfile::default()
            },
        ),
        ("acme", catalog_profile()),
    ]);
    let candidate = profiles(&[("acme", catalog_profile())]);
    assert!(assert_serviceable(&candidate, Some(&previous), &installed_acme()).is_ok());
}

#[test]
fn without_a_previous_section_every_profile_is_checked() {
    let broken = profiles(&[(
        "orphan",
        PiAiProviderProfile {
            api: Some("openai-completions".to_owned()),
            ..PiAiProviderProfile::default()
        },
    )]);
    let error = assert_serviceable(&broken, None, &installed_acme()).unwrap_err();
    assert!(error.to_string().contains("resolves no models"), "{error}");
}

#[test]
fn a_resolved_config_value_round_trips_through_the_json_reader() {
    let resolved = resolve_profiles_from_json(
        &json!({"acme": {"apiKeyEnv": "ACME_API_KEY"}}),
        &installed_acme(),
        Validation::Strict,
    )
    .unwrap();
    assert_eq!(
        resolved["acme"].api_key_env.as_deref(),
        Some("ACME_API_KEY")
    );
}

// ---- registration decisions ---------------------------------------------------------------------

#[test]
fn registration_facts_are_sorted_so_a_reorder_is_not_a_change() {
    let pairs = [("acme", catalog_profile()), ("acme-two", catalog_profile())];
    let map = resolve_profiles(&profiles(&pairs), &installed_acme(), Validation::Deferred).unwrap();
    let facts = registration_facts(&map);
    let order: Vec<&str> = facts.iter().map(|fact| fact.provider.as_str()).collect();
    assert_eq!(order, ["acme", "acme-two"]);

    // Reversing the configuration order yields the same facts, which is what keeps a settings document
    // that merely reorders its keys from churning the registry.
    let reversed: BTreeMap<String, PiAiProviderProfile> = pairs
        .iter()
        .rev()
        .map(|(key, profile)| ((*key).to_owned(), profile.clone()))
        .collect();
    let rebuilt = resolve_profiles(&reversed, &installed_acme(), Validation::Deferred).unwrap();
    assert_eq!(registration_facts(&rebuilt), facts);
    assert_eq!(registration_facts(&map), facts);
}

#[test]
fn a_bare_mount_is_dormant_and_a_route_set_registers() {
    let empty = BTreeMap::new();
    assert_eq!(
        plan_registration(&empty, None).action,
        RegistrationAction::Dormant
    );

    let resolved = resolved_acme();
    let plan = plan_registration(&resolved, None);
    assert_eq!(plan.action, RegistrationAction::Register);
    assert_eq!(plan.routes, ["acme"]);

    // The same facts again change nothing.
    let state = RegistrationState {
        routes: plan.routes.clone(),
        facts: plan.facts.clone(),
    };
    assert_eq!(
        plan_registration(&resolved, Some(&state)).action,
        RegistrationAction::Unchanged
    );

    // A changed retry policy must re-register: the registry captured the policy at registration.
    let mut changed = resolved.clone();
    changed.get_mut("acme").unwrap().retry_policy.max_retries = Some(0);
    assert_eq!(
        plan_registration(&changed, Some(&state)).action,
        RegistrationAction::Replace
    );

    // A renamed route changes the label every selector shows, so it must re-register too.
    let mut renamed = resolved.clone();
    renamed.get_mut("acme").unwrap().display_name = "Acme Renamed".to_owned();
    assert_eq!(
        plan_registration(&renamed, Some(&state)).action,
        RegistrationAction::Replace
    );

    // An emptied route set on an existing registration replaces itself with nothing rather than
    // leaving the old routes serving.
    assert_eq!(
        plan_registration(&empty, Some(&state)).action,
        RegistrationAction::Replace
    );
    assert!(plan_registration(&empty, Some(&state)).routes.is_empty());
}

// ---- directory ----------------------------------------------------------------------------------

#[test]
fn the_directory_is_the_installed_catalog_plus_declared_routes() {
    let mut routes = installed_acme();
    // A second installed provider, so "catalog order" is observable.
    routes.insert(
        "zzz".to_owned(),
        InstalledRoute {
            models: InstalledCatalog::new(),
            base_url: Some("https://zzz.example".to_owned()),
            api: "openai-completions".to_owned(),
            oauth: true,
        },
    );
    let mut profiles = resolved_acme();
    profiles.insert(
        "declared".to_owned(),
        pi_ai_apply_gap::ResolvedPiAiProviderProfile {
            provider: "declared".to_owned(),
            display_name: "Declared Gateway".to_owned(),
            ..resolved_acme()["acme"].clone()
        },
    );
    let entries = directory_entries(&profiles, &routes, NS);

    let keys: Vec<&str> = entries
        .iter()
        .map(|entry| entry.provider.as_str())
        .collect();
    // Every installed provider first, the declared route last, and the profile for an installed route
    // does not duplicate or reorder it.
    let acme = keys.iter().position(|key| *key == "acme").unwrap();
    let zzz = keys.iter().position(|key| *key == "zzz").unwrap();
    let declared = keys.iter().position(|key| *key == "declared").unwrap();
    assert!(acme < zzz, "{keys:?}");
    assert!(zzz < declared, "{keys:?}");
    assert_eq!(keys.len(), routes.len() + 1);
    // The directory is the whole installed catalog, not only the configured routes: `zzz` has no
    // profile and still appears.
    assert!(keys.contains(&"zzz"));
}

#[test]
fn a_directory_entry_carries_its_defaults_path_and_diagnostic() {
    let routes = installed_acme();
    let mut profiles = resolved_acme();
    let mut orphan = profiles["acme"].clone();
    orphan.provider = "orphan".to_owned();
    orphan.display_name = "Orphan".to_owned();
    orphan.catalog_error = Some("llm-pi-ai: provider \"orphan\" resolves no models".to_owned());
    profiles.insert("orphan".to_owned(), orphan);

    let entries: Vec<DirectoryEntry> = directory_entries(&profiles, &routes, NS);
    let entry = entries
        .iter()
        .find(|entry| entry.provider == "orphan")
        .unwrap();
    assert!(entry.declared);
    assert_eq!(entry.display_name, "Orphan");
    assert_eq!(entry.settings_ns, NS);
    assert_eq!(
        entry.settings_path,
        ("providers".to_owned(), "orphan".to_owned())
    );
    assert!(
        entry
            .error
            .as_deref()
            .unwrap()
            .contains("resolves no models")
    );

    // A shipped provider is not "declared" even when a profile exists for it.
    let acme = entries
        .iter()
        .find(|entry| entry.provider == "acme")
        .unwrap();
    assert!(!acme.declared);
    assert_eq!(acme.display_name, "acme");

    // A recorded provider's native auth and endpoint come from the directory table rather than from the
    // installed catalog, which is what lets a settings editor offer the variable name before any key is
    // entered. This is also the fallback's job: a consumer can install a newer catalog than this table
    // was generated from, and such a provider must stay configurable.
    let mut routes = routes;
    routes.insert(
        "groq".to_owned(),
        InstalledRoute {
            base_url: None,
            ..InstalledRoute::default()
        },
    );
    let entries = directory_entries(&profiles, &routes, NS);
    let groq = entries
        .iter()
        .find(|entry| entry.provider == "groq")
        .unwrap();
    let native = groq.defaults.native_auth.clone().unwrap();
    assert!(native.env.iter().any(|name| name == "GROQ_API_KEY"));
    assert_eq!(
        groq.defaults.default_base_url.as_deref(),
        Some("https://api.groq.com/openai/v1")
    );
}

#[test]
fn a_stored_discovery_profile_supplies_headers_and_the_reference() {
    let mut profile = catalog_profile();
    profile.headers.insert("X-Acme".to_owned(), "on".to_owned());
    let resolved = resolve_profiles(
        &profiles(&[("acme", profile)]),
        &installed_acme(),
        Validation::Strict,
    )
    .unwrap();
    let stored = stored_discovery_profile(&resolved, Some("acme")).unwrap();
    assert_eq!(stored.headers["X-Acme"], "on");
    assert_eq!(stored.api_key_env.as_deref(), Some("ACME_API_KEY"));

    // A route that does not exist, or no route named at all, has nothing to supply.
    assert!(stored_discovery_profile(&resolved, Some("nope")).is_none());
    assert!(stored_discovery_profile(&resolved, None).is_none());
}

// ---- credentials --------------------------------------------------------------------------------

#[test]
fn a_profile_naming_no_credential_defers_to_provider_discovery() {
    // `None` here is not a miss: a keyless route authenticates through pi-ai's own ambient discovery.
    assert_eq!(resolve_route_credential(None, None, "acme").unwrap(), None);
    assert_eq!(
        resolve_route_credential(None, Some("ignored"), "acme").unwrap(),
        None
    );
}

#[test]
fn a_named_credential_that_did_not_resolve_fails_loud() {
    // The load-bearing refusal: handing pi-ai `None` would let it pick up an unrelated ambient key and
    // bill another tenant.
    let fault = resolve_route_credential(Some("ACME_API_KEY"), None, "acme").unwrap_err();
    assert_eq!(fault.code, MISSING_CREDENTIAL);
    assert!(
        fault
            .message
            .contains("no credential for provider route \"acme\"")
    );
    assert!(fault.message.contains("ACME_API_KEY"));
    assert!(fault.message.contains("MISSING") || fault.message.contains("remove apiKeyEnv"));

    // An empty hit is the same fault as a miss.
    let fault = resolve_route_credential(Some("ACME_API_KEY"), Some(""), "acme").unwrap_err();
    assert_eq!(fault.code, MISSING_CREDENTIAL);
}

#[test]
fn a_hit_is_trimmed_and_a_defective_one_is_named() {
    let key =
        resolve_route_credential(Some("ACME_API_KEY"), Some("  sk-acme-key  "), "acme").unwrap();
    assert_eq!(key.as_deref(), Some("sk-acme-key"));

    // Whitespace only is blank, which is not a key.
    let fault = resolve_route_credential(Some("ACME_API_KEY"), Some("   "), "acme").unwrap_err();
    assert_eq!(fault.code, INVALID_CREDENTIAL);
    assert!(fault.message.contains("is blank"), "{}", fault.message);

    // A character no header can carry is refused locally rather than as an opaque 401.
    let fault =
        resolve_route_credential(Some("ACME_API_KEY"), Some("sk-acme\nkey"), "acme").unwrap_err();
    assert_eq!(fault.code, INVALID_CREDENTIAL);
    assert!(
        fault.message.contains("no HTTP header can carry"),
        "{}",
        fault.message
    );

    // The message must never echo the secret.
    assert!(!fault.message.contains("sk-acme"));

    assert!(is_legal_api_key_char('!'));
    assert!(is_legal_api_key_char('~'));
    assert!(!is_legal_api_key_char(' '));
    assert!(!is_legal_api_key_char('\u{7f}'));
    assert!(!is_legal_api_key_char('\u{e9}'));
}

// ---- sign-in gate (the portable half of registerPiAiFlows) --------------------------------------

#[test]
fn only_provider_ids_that_can_address_a_credential_record_are_offerable() {
    // The gate `registerPiAiFlows` applies: a credential key is `<scope>/<id>` with `^[a-z][a-z0-9-]*$`
    // on both halves, so an id outside that grammar would throw while addressing its record and take
    // the whole mount down.
    assert!(can_address_credential_record("acme"));
    assert!(can_address_credential_record("acme-2"));
    assert!(!can_address_credential_record("Acme"));
    assert!(!can_address_credential_record("acme.gateway"));
    assert!(!can_address_credential_record("-acme"));
    assert!(!can_address_credential_record(""));

    assert_eq!(record_key_for("acme").as_deref(), Some("llm-pi-ai/acme"));
    assert_eq!(record_key_for("Acme"), None);

    // The domain of `registerPiAiFlows`: providers offering OAuth, whose ids can address a record.
    let mut routes = installed_acme();
    routes.insert(
        "oauth-ok".to_owned(),
        InstalledRoute {
            oauth: true,
            ..InstalledRoute::default()
        },
    );
    routes.insert(
        "Oauth.Bad".to_owned(),
        InstalledRoute {
            oauth: true,
            ..InstalledRoute::default()
        },
    );
    let offerable = login_offerable_providers(&routes);
    assert_eq!(offerable, ["oauth-ok"]);
}

// ---- defaults the module publishes ---------------------------------------------------------------

#[test]
fn the_published_defaults_are_upstreams_numbers() {
    assert_eq!(DEFAULT_STREAM_IDLE_TIMEOUT_MS, 300_000);
    assert_eq!(DEFAULT_MAX_REQUEST_IMAGE_BYTES, 20 * 1024 * 1024);
    assert_eq!(DEFAULT_MAX_REQUEST_IMAGES, 50);
    assert_eq!(DEFAULT_IMAGE_OFFLOAD_COUNT_QUANTUM, 10);
    assert_eq!(DEFAULT_REQUEST_IMAGE_PIXEL_BUDGET, 2048 * 2048);
    assert_eq!(DEFAULT_REQUEST_IMAGE_MAX_BYTES, 1024 * 1024);
    assert_eq!(NS, "llm-pi-ai");
}
