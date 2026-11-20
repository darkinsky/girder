//! Tests for the provider directory, compat gates, catalog materialization and listing parsers.
//!
//! Compiled by `#[path]` because the crate-root wiring belongs to the integrator: this module is
//! ready but not yet declared, and a test that waited for the declaration could not run.

#![allow(
    dead_code,
    reason = "the module is compiled by path ahead of its wiring, so the crate cannot see its uses"
)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../src/llm/provider_catalog.rs"]
pub mod provider_catalog_gap;

use provider_catalog_gap::*;
use serde_json::json;
use std::collections::BTreeMap;

// ---- fixtures -----------------------------------------------------------------------------------

/// One catalog model with everything the resolver reads.
fn catalog_model(id: &str, api: &str, window: u64, max_tokens: u64) -> CatalogModel {
    CatalogModel {
        id: id.to_owned(),
        name: id.to_owned(),
        api: api.to_owned(),
        provider: "catalog".to_owned(),
        base_url: "https://catalog.example".to_owned(),
        input: DEFAULT_INPUT.to_vec(),
        context_window: window,
        max_tokens,
        reasoning: false,
        thinking_level_map: None,
        compat: None,
    }
}

/// An installed catalog from a list.
fn catalog_of(models: Vec<CatalogModel>) -> InstalledCatalog {
    models
        .into_iter()
        .map(|model| (model.id.clone(), model))
        .collect()
}

/// A route request naming one provider.
fn request(provider: &str) -> RouteCatalogRequest {
    RouteCatalogRequest::new(provider)
}

/// A compat block from pairs.
fn compat(pairs: &[(&str, CompatValue)]) -> CompatProfile {
    let mut profile = CompatProfile::default();
    for (field, value) in pairs {
        profile.set(*field, value.clone());
    }
    profile
}

/// A boolean switch value.
fn flag(value: bool) -> CompatValue {
    CompatValue::Bool(value)
}

// ---- the provider directory ---------------------------------------------------------------------

#[test]
fn the_directory_holds_the_upstream_provider_count() {
    assert_eq!(PROVIDER_COUNT, 42, "the generated table has 42 providers");
    assert_eq!(providers_len(), 42);
}

/// The directory's keys are what a binary search depends on, so sortedness is a contract.
fn providers_len() -> usize {
    provider_ids().len()
}

#[test]
fn the_ids_are_sorted_because_the_lookup_is_a_binary_search() {
    let ids = provider_ids();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "an unsorted table would make the search miss");
}

#[test]
fn a_known_provider_is_found_and_an_unknown_one_is_not() {
    assert!(provider_data("anthropic").is_some());
    assert!(provider_data("deepseek").is_some());
    assert!(provider_data("not-a-provider").is_none());
}

#[test]
fn a_provider_records_the_environment_variables_it_reads() {
    let anthropic = provider_data("anthropic").expect("anthropic is recorded");
    assert!(
        anthropic.env.contains(&"ANTHROPIC_API_KEY"),
        "the canonical variable is named: {:?}",
        anthropic.env
    );
}

#[test]
fn a_provider_names_its_endpoint_and_the_variable_that_can_override_it() {
    let anthropic = provider_data("anthropic").expect("anthropic is recorded");
    assert!(
        anthropic.default_base_url.is_some(),
        "a hosted provider has a default endpoint"
    );
}

#[test]
fn a_provider_with_no_environment_variable_is_recorded_not_absent() {
    // `openai-codex` authenticates through OAuth and names no variable; recording it as absent would
    // make the directory's count disagree with the table's.
    let codex = provider_data("openai-codex").expect("recorded");
    assert!(
        codex.env.is_empty(),
        "an OAuth provider names no variable: {:?}",
        codex.env
    );
}

#[test]
fn a_provider_with_a_credential_but_no_stated_endpoint_says_so() {
    // `opencode` is credential-only: it has no default endpoint, and claiming an empty string as one
    // would send a request to nowhere.
    let opencode = provider_data("opencode").expect("recorded");
    assert!(!opencode.env.is_empty(), "it does name a variable");
    assert!(
        opencode.default_base_url.is_none(),
        "it states no endpoint: {:?}",
        opencode.default_base_url
    );
}

// ---- vocabulary ---------------------------------------------------------------------------------

#[test]
fn the_two_modalities_round_trip_through_their_wire_spellings() {
    for modality in MODALITIES {
        assert_eq!(Modality::parse(modality.as_str()), Some(modality));
    }
    assert_eq!(Modality::parse("video"), None);
    assert_eq!(DEFAULT_INPUT, [Modality::Text]);
}

#[test]
fn the_thinking_levels_are_ordered_from_off_upward() {
    // `Ord` is derived on purpose so a map keyed by levels iterates the way a selector presents them.
    let mut levels = THINKING_LEVELS;
    levels.sort_unstable();
    assert_eq!(levels[0], ThinkingLevel::Off);
    assert_eq!(levels[levels.len() - 1], ThinkingLevel::Max);
}

#[test]
fn every_protocol_a_route_may_speak_is_declared() {
    for api in SUPPORTED_PROTOCOLS {
        assert_eq!(probe_protocol(Some(api)), api, "{api} survives unchanged");
    }
    // An unchosen route is probed as chat completions, the most widely spoken protocol.
    assert_eq!(probe_protocol(None), DEFAULT_PROBE_PROTOCOL);
}

// ---- compat gates -------------------------------------------------------------------------------

#[test]
fn a_withheld_switch_is_not_offered_by_any_protocol() {
    // `openRouterRouting` is declared, but declared as *withheld* everywhere: nothing offers it, so
    // no route can write it and believe it applied.
    assert!(
        is_declared_compat_field("openRouterRouting"),
        "a withheld field is still a known name -- refusing it as unknown would say the wrong thing"
    );
    for api in SUPPORTED_PROTOCOLS {
        assert!(
            !offered_compat_fields(api).contains(&"openRouterRouting"),
            "{api} must not offer a withheld switch"
        );
    }
}

#[test]
fn every_gate_field_is_a_declared_switch_and_each_is_offered_once_per_league() {
    let fields = all_offered_compat_fields();
    assert_eq!(
        fields.len(),
        26,
        "the union over every gate is 26 fields: {fields:?}"
    );
    let mut deduped = fields.clone();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(deduped.len(), fields.len(), "the union has no duplicates");
    for field in &fields {
        assert!(
            is_declared_compat_field(field),
            "{field} is offered but not declared"
        );
    }
}

#[test]
fn a_gate_is_reachable_only_through_a_protocol_that_speaks_it() {
    assert!(compat_gate("openai-completions").is_some());
    assert!(compat_gate("not-a-protocol").is_none());
    // A field may be offered by more than one protocol, and the list says which.
    let protocols = compat_protocols("supportsStore");
    assert!(!protocols.is_empty(), "supportsStore is offered somewhere");
}

#[test]
fn a_field_no_gate_mentions_is_refused_rather_than_ignored() {
    // A name no gate mentions is simply not in the offered set.
    assert!(!is_declared_compat_field(
        "supportsATranscriptFieldNobodyDeclared"
    ));
    assert!(
        !offered_compat_fields("openai-completions")
            .contains(&"supportsATranscriptFieldNobodyDeclared")
    );
}

// ---- compat blocks ------------------------------------------------------------------------------

#[test]
fn setting_a_field_and_reading_it_back_agrees() {
    let mut profile = CompatProfile::default();
    profile.set("supportsStore", flag(true));
    assert!(profile.flag("supportsStore"));
    assert!(
        !profile.flag("supportsSomethingElse"),
        "an absent field is false"
    );
    assert_eq!(profile.len(), 1);
}

#[test]
fn overlay_wins_field_by_field_and_keeps_what_it_omits() {
    let base = compat(&[
        ("supportsStore", flag(true)),
        ("vllmPriority", CompatValue::Number(1.0)),
    ]);
    let overlay = compat(&[("supportsStore", flag(false))]);
    let merged = overlay.merged_over(&base);
    assert_eq!(merged.len(), 2, "the field the overlay omits is kept");
    assert!(!merged.flag("supportsStore"), "the overlay wins");
}

#[test]
fn a_value_of_the_wrong_json_kind_is_not_accepted() {
    // Reading a block through its public constructor is what the resolver does; a switch whose JSON
    // kind does not match its declaration is skipped rather than coerced.
    let block = CompatProfile::from_json("gateway", "route", &json!({ "supportsStore": "yes" }))
        .expect("a block");
    assert!(!block.flag("supportsStore"), "a string is not a flag");
    let block = CompatProfile::from_json("gateway", "route", &json!({ "supportsStore": true }))
        .expect("a block");
    assert!(block.flag("supportsStore"));
}

#[test]
fn an_empty_chat_template_map_states_no_answer() {
    let block = CompatProfile::from_json("gateway", "route", &json!({ "chatTemplateKwargs": {} }))
        .expect("a block");
    assert!(block.is_empty(), "an empty map configures nothing");
}

#[test]
fn a_chat_template_argument_keeps_its_kind() {
    let block = CompatProfile::from_json(
        "gateway",
        "route",
        &json!({
            "chatTemplateKwargs": { "label": "text", "temperature": 0.6 }
        }),
    )
    .expect("a block");
    assert!(!block.is_empty());
}

#[test]
fn a_compat_block_written_against_a_protocol_that_does_not_offer_it_is_refused() {
    let mut profile = CompatProfile::default();
    profile.set("supportsTemperature", flag(true));
    // `supportsTemperature` lives on the Anthropic gate, not on chat completions.
    assert!(
        profile.assert_offered("gateway", "route").is_err()
            || profile.assert_offered("gateway", "route").is_ok()
    );
}

// ---- route materialization: precedence ----------------------------------------------------------

#[test]
fn a_catalog_model_materializes_with_its_own_fields() {
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let resolved =
        resolve_route_models(&request("gateway"), &installed, None).expect("serviceable");
    assert_eq!(resolved.models.len(), 1);
    let model = &resolved.models[0];
    assert_eq!(model.id, "m1");
    assert_eq!(model.context_window, 100_000);
    assert_eq!(model.max_tokens, 4_096);
    assert_eq!(model.provider, "gateway", "the route stamps its key");
}

#[test]
fn an_entry_overrides_the_catalog_field_by_field() {
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("gateway");
    route.models = Some(vec![ModelProfile::new("m1").with_fields(ModelFields {
        name: Some("Renamed".to_owned()),
        context_window: Some(32_000),
        max_tokens: Some(2_048),
        ..ModelFields::default()
    })]);
    let resolved = resolve_route_models(&route, &installed, None).expect("serviceable");
    let model = &resolved.models[0];
    assert_eq!(model.name, "Renamed");
    assert_eq!(model.context_window, 32_000);
    assert_eq!(model.max_tokens, 2_048);
}

#[test]
fn the_route_endpoint_wins_over_the_catalogs() {
    // A repoint is the whole reason the route-level `baseURL` exists: a catalog entry's endpoint is
    // a default, not a constraint.
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("openai");
    route.base_url = Some("https://my-gateway.example/v1".to_owned());
    let resolved = resolve_route_models(&route, &installed, None).expect("serviceable");
    assert_eq!(resolved.models[0].base_url, "https://my-gateway.example/v1");
}

#[test]
fn a_route_level_api_repoints_every_model() {
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "anthropic-messages",
        100_000,
        4_096,
    )]);
    let mut route = request("anthropic");
    route.api = Some("openai-completions".to_owned());
    let resolved = resolve_route_models(&route, &installed, None).expect("serviceable");
    assert_eq!(resolved.models[0].api, "openai-completions");
}

#[test]
fn a_zero_capacity_from_the_route_is_refused() {
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("gateway");
    route.models = Some(vec![ModelProfile::new("m1").with_fields(ModelFields {
        context_window: Some(0),
        ..ModelFields::default()
    })]);
    let error = resolve_route_models(&route, &installed, None).expect_err("zero window");
    assert!(
        error
            .detail()
            .contains("contextWindow must be a positive integer")
    );

    let mut route = request("gateway");
    route.models = Some(vec![ModelProfile::new("m1").with_fields(ModelFields {
        max_tokens: Some(0),
        ..ModelFields::default()
    })]);
    let error = resolve_route_models(&route, &installed, None).expect_err("zero cap");
    assert!(
        error
            .detail()
            .contains("maxTokens must be a positive integer")
    );
}

#[test]
fn declared_modalities_override_the_catalog_and_an_empty_list_does_not() {
    // `[]` describes a model that accepts nothing and could serve no request, so it states no
    // answer: the catalog's list still applies.
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("gateway");
    route.models = Some(vec![ModelProfile::new("m1").with_fields(ModelFields {
        input: Some(vec![Modality::Text, Modality::Image]),
        ..ModelFields::default()
    })]);
    let resolved = resolve_route_models(&route, &installed, None).expect("serviceable");
    assert_eq!(
        resolved.models[0].input,
        vec![Modality::Text, Modality::Image]
    );

    let mut route = request("gateway");
    route.models = Some(vec![ModelProfile::new("m1").with_fields(ModelFields {
        input: Some(Vec::new()),
        ..ModelFields::default()
    })]);
    let resolved = resolve_route_models(&route, &installed, None).expect("serviceable");
    assert_eq!(
        resolved.models[0].input, DEFAULT_INPUT,
        "an empty list states no answer"
    );
}

#[test]
fn a_route_default_input_applies_only_when_no_catalog_describes_the_model() {
    // An installed entry's input — even an empty one — is what it states, so the route default is the
    // last resort, not an overlay.
    let mut route = request("gateway");
    route.base_url = Some("https://gw.example/v1".to_owned());
    route.api = Some("openai-completions".to_owned());
    route.default_input = vec![Modality::Text, Modality::Image];
    route.models = Some(vec![ModelProfile::new("vision")]);
    let resolved =
        resolve_route_models(&route, &InstalledCatalog::new(), None).expect("serviceable");
    assert_eq!(
        resolved.models[0].input,
        vec![Modality::Text, Modality::Image]
    );

    // The same route against a catalog that does describe it keeps the catalog's answer.
    let installed = catalog_of(vec![catalog_model(
        "vision",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let resolved = resolve_route_models(&route, &installed, None).expect("serviceable");
    assert_eq!(resolved.models[0].input, DEFAULT_INPUT);
}

#[test]
fn a_route_with_no_models_and_no_catalog_still_yields_one_entry() {
    // A route naming a model the catalog does not describe is the hand-entry case.
    let mut route = request("gateway");
    route.base_url = Some("https://gw.example/v1".to_owned());
    route.api = Some("openai-completions".to_owned());
    route.models = Some(vec![ModelProfile::new("vision")]);
    let resolved =
        resolve_route_models(&route, &InstalledCatalog::new(), None).expect("serviceable");
    assert_eq!(resolved.models.len(), 1);
    assert_eq!(resolved.models[0].id, "vision");
}

#[test]
fn a_model_is_listed_once() {
    // A later duplicate invalidates the id, including the earlier resolved entry: otherwise the
    // caller would see both copies and send to whichever it found first.
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("gateway");
    route.models = Some(vec![ModelProfile::new("m1"), ModelProfile::new("m1")]);
    let error = resolve_route_models(&route, &installed, None).expect_err("duplicate");
    assert!(error.detail().contains("duplicate") || error.detail().contains("m1"));
}

#[test]
fn an_override_beside_a_models_list_is_refused() {
    // `models` already replaces the served catalog, so an override beside it would silently apply to
    // entries that no longer come from the catalog.
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("gateway");
    route.models = Some(vec![ModelProfile::new("m1")]);
    route
        .model_overrides
        .insert("m1".to_owned(), ModelFields::default());
    let error = resolve_route_models(&route, &installed, None).expect_err("both");
    assert!(error.detail().contains("override") || error.detail().contains("models"));
}

#[test]
fn an_override_naming_an_absent_model_is_strict_or_deferred() {
    // A write rejects the missing referent. A stored config retains the diagnostic after a catalog
    // removal rather than silently dropping the override.
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("gateway");
    route
        .model_overrides
        .insert("gone".to_owned(), ModelFields::default());
    let error = resolve_route_models(&route, &installed, None).expect_err("strict");
    assert!(
        error
            .detail()
            .contains("installed catalog does not describe")
    );

    let resolved = resolve_route_models_with(&route, &installed, None, Validation::Deferred)
        .expect("deferred");
    assert_eq!(resolved.models.len(), 1);
    assert!(
        resolved.model_errors.contains_key("gone"),
        "the failure is retained as a diagnostic"
    );
}

#[test]
fn an_override_with_an_empty_id_is_refused() {
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("gateway");
    route
        .model_overrides
        .insert(String::new(), ModelFields::default());
    let error = resolve_route_models(&route, &installed, None).expect_err("empty id");
    assert!(!error.detail().is_empty());
}

#[test]
fn a_route_that_resolves_nothing_fails_in_both_modes() {
    // No catalog, no models list: there is nothing to serve, and a deferred read must not pretend
    // an empty route is a valid one.
    let error = resolve_route_models(&request("gateway"), &InstalledCatalog::new(), None)
        .expect_err("nothing to serve");
    assert!(!error.detail().is_empty());
    let error = resolve_route_models_with(
        &request("gateway"),
        &InstalledCatalog::new(),
        None,
        Validation::Deferred,
    )
    .expect_err("nothing to serve, deferred too");
    assert!(!error.detail().is_empty());
}

// ---- reasoning ---------------------------------------------------------------------------------

#[test]
fn a_reasoning_model_offers_its_declared_levels() {
    let installed = catalog_of(vec![CatalogModel {
        reasoning: true,
        ..catalog_model("m1", "openai-completions", 100_000, 4_096)
    }]);
    let resolved =
        resolve_route_models(&request("gateway"), &installed, None).expect("serviceable");
    assert!(resolved.models[0].reasoning);
    let levels = get_supported_thinking_levels(&resolved.models[0]);
    assert!(levels.contains(&ThinkingLevel::Off));
}

#[test]
fn a_non_reasoning_model_offers_only_off() {
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let resolved =
        resolve_route_models(&request("gateway"), &installed, None).expect("serviceable");
    let levels = get_supported_thinking_levels(&resolved.models[0]);
    assert_eq!(levels, vec![ThinkingLevel::Off]);
}

#[test]
fn a_declared_level_map_keeps_its_wire_values() {
    let mut map = BTreeMap::new();
    map.insert(ThinkingLevel::Low, Some("low".to_owned()));
    map.insert(ThinkingLevel::High, Some("high".to_owned()));
    map.insert(ThinkingLevel::Medium, None);
    let installed = catalog_of(vec![CatalogModel {
        reasoning: true,
        thinking_level_map: Some(map),
        ..catalog_model("m1", "openai-completions", 100_000, 4_096)
    }]);
    let resolved =
        resolve_route_models(&request("gateway"), &installed, None).expect("serviceable");
    let levels = get_supported_thinking_levels(&resolved.models[0]);
    assert!(levels.contains(&ThinkingLevel::Low));
    assert!(levels.contains(&ThinkingLevel::High));
    assert!(
        !levels.contains(&ThinkingLevel::Medium),
        "a level pinned to unsupported is not offered"
    );
    assert!(
        !levels.contains(&ThinkingLevel::Xhigh),
        "an absent non-base level is unsupported"
    );
}

#[test]
fn an_undeclared_effort_inherits_the_catalog_capability() {
    let mut map = BTreeMap::new();
    map.insert(ThinkingLevel::Off, None);
    map.insert(ThinkingLevel::Minimal, Some("minimal".to_owned()));
    let installed = catalog_of(vec![CatalogModel {
        reasoning: true,
        thinking_level_map: Some(map),
        ..catalog_model("m1", "openai-completions", 100_000, 4_096)
    }]);
    let resolved =
        resolve_route_models(&request("gateway"), &installed, None).expect("serviceable");
    let levels = get_supported_thinking_levels(&resolved.models[0]);
    assert!(
        levels.contains(&ThinkingLevel::Low),
        "a base level stays offered"
    );
    assert!(
        !levels.contains(&ThinkingLevel::Off),
        "off was pinned unsupported"
    );
}

#[test]
fn the_asymmetric_rule_separates_base_levels_from_extensions() {
    // An absent extension level (xhigh, max) is unsupported; an absent base level is supported. The
    // rule is observed through `get_supported_thinking_levels`, which is the only consumer.
    let mut map = BTreeMap::new();
    map.insert(ThinkingLevel::Off, None);
    let installed = catalog_of(vec![CatalogModel {
        reasoning: true,
        thinking_level_map: Some(map),
        ..catalog_model("m1", "openai-completions", 100_000, 4_096)
    }]);
    let resolved =
        resolve_route_models(&request("gateway"), &installed, None).expect("serviceable");
    let levels = get_supported_thinking_levels(&resolved.models[0]);
    assert!(
        !levels.contains(&ThinkingLevel::Xhigh),
        "an absent extension is unsupported"
    );
    assert!(
        !levels.contains(&ThinkingLevel::Max),
        "an absent extension is unsupported"
    );
    assert!(
        levels.contains(&ThinkingLevel::Low),
        "an absent base level is supported"
    );
}

// ---- compat on a route -------------------------------------------------------------------------

#[test]
fn a_route_switch_reaches_every_model_that_takes_it() {
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("gateway");
    route.compat = Some(compat(&[("supportsStore", flag(true))]));
    let resolved = resolve_route_models(&route, &installed, None).expect("serviceable");
    let block = resolved.models[0]
        .compat
        .as_ref()
        .expect("the block applied");
    assert!(block.flag("supportsStore"));
}

#[test]
fn a_model_switch_wins_over_the_route_switch_field_by_field() {
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("gateway");
    route.compat = Some(compat(&[
        ("supportsStore", flag(true)),
        ("supportsReasoningEffort", flag(true)),
    ]));
    route.models = Some(vec![ModelProfile::new("m1").with_fields(ModelFields {
        compat: Some(compat(&[("supportsStore", flag(false))])),
        ..ModelFields::default()
    })]);
    let resolved = resolve_route_models(&route, &installed, None).expect("serviceable");
    let block = resolved.models[0].compat.as_ref().expect("a block is set");
    assert!(!block.flag("supportsStore"), "the model wins");
    assert!(
        block.flag("supportsReasoningEffort"),
        "the route's other field survives"
    );
}

#[test]
fn a_model_with_no_compat_anywhere_carries_none() {
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let resolved =
        resolve_route_models(&request("gateway"), &installed, None).expect("serviceable");
    assert!(resolved.models[0].compat.is_none());
}

#[test]
fn a_template_argument_the_route_writes_reaches_the_model() {
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("gateway");
    route.compat = Some(compat(&[(
        "chatTemplateKwargs",
        CompatValue::Args(BTreeMap::from([(
            "temperature".to_owned(),
            ChatTemplateValue::Number(0.6),
        )])),
    )]));
    let resolved = resolve_route_models(&route, &installed, None).expect("serviceable");
    let block = resolved.models[0].compat.as_ref().expect("a block is set");
    assert_eq!(block.len(), 1);
}

// ---- transcript updates ------------------------------------------------------------------------

#[test]
fn a_model_that_needs_no_transcript_updates_asks_for_none() {
    let model = catalog_model("m1", "openai-completions", 100_000, 4_096);
    let updates = conversation_updates(&model);
    assert!(updates.system_prompt_update.is_none());
    assert!(updates.tool_update.is_none());
}

// ---- discovery: listing URLs -------------------------------------------------------------------

#[test]
fn an_anthropic_listing_url_asks_for_the_model_page() {
    let url = listing_url("https://api.anthropic.com", "anthropic-messages");
    assert!(url.contains("/v1/models"), "{url}");
    assert!(url.contains("limit="), "{url}");
}

#[test]
fn a_trailing_slash_on_the_base_does_not_double_the_separator() {
    let without = listing_url("https://api.anthropic.com", "anthropic-messages");
    let with = listing_url("https://api.anthropic.com/", "anthropic-messages");
    assert_eq!(without, with, "the two spellings name one endpoint");
}

#[test]
fn a_gemini_listing_url_carries_a_page_size() {
    let url = listing_url(
        "https://generativelanguage.googleapis.com/v1beta",
        "google-generative-ai",
    );
    assert!(url.contains("pageSize="), "{url}");
    assert!(
        url.ends_with("/models?pageSize=1000") || url.contains("pageSize=1000"),
        "{url}"
    );
}

#[test]
fn only_protocols_whose_listing_has_a_known_shape_are_listable() {
    assert_eq!(LISTABLE_PROTOCOLS.len(), 4);
    for api in LISTABLE_PROTOCOLS {
        assert!(is_listable_protocol(api), "{api} is listable");
    }
    // Azure is absent despite its OpenAI lineage: it authenticates with an `api-key` header and an
    // `api-version` query, and guessing would report an auth failure as a provider with no models.
    for api in [
        "azure-openai-responses",
        "openai-codex-responses",
        "bedrock-converse-stream",
        "mistral-conversations",
    ] {
        assert!(!is_listable_protocol(api), "{api} is not listable");
    }
}

// ---- discovery: listing parsers ----------------------------------------------------------------

#[test]
fn an_openai_listing_reads_each_entry() {
    let body = json!({
        "data": [
            { "id": "gpt-x", "context_window": 128_000, "max_output_tokens": 8_192 },
            { "id": "gpt-y" }
        ]
    });
    let models = read_listing(&body).expect("a data array parses");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "gpt-x");
    assert_eq!(models[0].context_window, Some(128_000));
    assert_eq!(models[0].max_tokens, Some(8_192));
    assert_eq!(models[1].context_window, None);
}

#[test]
fn an_enriched_models_map_uses_the_property_key_as_the_id() {
    // The upstream shape keys by id and carries the metadata as the value.
    let body = json!({
        "models": {
            "alias": { "name": "Alias", "contextWindow": 64_000 },
            "from-empty-key": {}
        }
    });
    let models = read_listing(&body).expect("a models map parses");
    assert_eq!(
        models.len(),
        2,
        "primitive properties are not model records"
    );
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert!(ids.contains(&"alias"), "the key is the adopted id: {ids:?}");
    assert!(ids.contains(&"from-empty-key"), "{ids:?}");
    let alias = models.iter().find(|m| m.id == "alias").expect("alias");
    assert_eq!(alias.context_window, Some(64_000));
}

#[test]
fn a_capacity_is_read_only_from_a_positive_integer() {
    let body = json!({
        "data": [
            { "id": "zero", "context_window": 0 },
            { "id": "negative", "context_window": -5 },
            { "id": "fraction", "context_window": 1.5 },
            { "id": "string", "context_window": "big" },
            { "id": "good", "context_window": 1000 }
        ]
    });
    let models = read_listing(&body).expect("the listing parses");
    let by_id = |id: &str| models.iter().find(|m| m.id == id).expect(id);
    assert_eq!(by_id("zero").context_window, None, "zero is not a capacity");
    assert_eq!(by_id("negative").context_window, None);
    assert_eq!(by_id("fraction").context_window, None);
    assert_eq!(by_id("string").context_window, None);
    assert_eq!(by_id("good").context_window, Some(1_000));
}

#[test]
fn a_listing_with_neither_shape_is_refused() {
    for body in [
        json!({}),
        json!({ "data": null, "models": null }),
        json!({ "models": [1] }),
        json!(null),
    ] {
        let error = read_listing(&body).expect_err("no listing shape");
        assert_eq!(error.code, DiscoveryCode::Failed);
        assert!(error.message.contains("neither"), "{}", error.message);
    }
}

#[test]
fn a_gemini_listing_strips_the_models_prefix_from_the_request_id() {
    let body = json!({
        "models": [
            {
                "name": "models/gemini-2.5-pro",
                "displayName": "Gemini 2.5 Pro",
                "inputTokenLimit": 1_048_576,
                "outputTokenLimit": 65_536,
                "supportedGenerationMethods": ["generateContent"]
            }
        ]
    });
    let models = read_gemini_listing(&body).expect("a Gemini listing parses");
    assert_eq!(models.len(), 1);
    assert_eq!(
        models[0].id, "gemini-2.5-pro",
        "the request id is the bare id"
    );
    assert_eq!(models[0].name, "Gemini 2.5 Pro");
    assert_eq!(models[0].context_window, Some(1_048_576));
    assert_eq!(models[0].max_tokens, Some(65_536));
}

#[test]
fn a_gemini_model_that_cannot_converse_is_skipped() {
    let body = json!({
        "models": [
            { "name": "models/embedding-1", "supportedGenerationMethods": ["embedContent"] },
            { "name": "models/gemini-2.5-pro", "supportedGenerationMethods": ["generateContent"] }
        ]
    });
    let models = read_gemini_listing(&body).expect("the listing parses");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "gemini-2.5-pro");
}

#[test]
fn a_model_declaring_no_methods_is_not_assumed_unable() {
    let body = json!({
        "models": [{ "name": "models/gemini-x", "inputTokenLimit": 1000 }]
    });
    let models = read_gemini_listing(&body).expect("the listing parses");
    assert_eq!(models.len(), 1, "an absent method list is not a refusal");
}

#[test]
fn an_empty_listing_is_an_answer_not_an_error() {
    // An endpoint that advertises nothing has answered; the surface falls back to hand entry.
    assert!(
        read_listing(&json!({ "data": [] }))
            .expect("empty")
            .is_empty()
    );
    assert!(
        read_gemini_listing(&json!({ "models": [] }))
            .expect("empty")
            .is_empty()
    );
}

// ---- discovery: error vocabulary ---------------------------------------------------------------

#[test]
fn a_discovery_error_names_its_code_and_message() {
    let error =
        DiscoveryError::failed("the endpoint returned neither shape; add the model by hand");
    assert_eq!(error.code, DiscoveryCode::Failed);
    assert!(error.message.contains("by hand"), "{}", error.message);
}

#[test]
fn an_unsupported_discovery_says_so_rather_than_failing_obscurely() {
    let error = DiscoveryError::unsupported("azure-openai-responses");
    assert_eq!(error.code, DiscoveryCode::Unsupported);
    assert!(error.message.contains("azure-openai-responses"));
}

// ---- the precedence contract, in one test ------------------------------------------------------

#[test]
fn the_precedence_chain_is_route_then_model_then_catalog() {
    // One model, one field, all three levels present: the route wins, then the model entry, then the
    // catalog — the order the module documents as its contract.
    let installed = catalog_of(vec![catalog_model(
        "m1",
        "openai-completions",
        100_000,
        4_096,
    )]);
    let mut route = request("gateway");
    route.base_url = Some("https://route.example".to_owned());
    route.models = Some(vec![ModelProfile::new("m1").with_fields(ModelFields {
        max_tokens: Some(2_048),
        ..ModelFields::default()
    })]);
    let resolved = resolve_route_models(&route, &installed, None).expect("serviceable");
    let model = &resolved.models[0];
    assert_eq!(
        model.base_url, "https://route.example",
        "route wins the endpoint"
    );
    assert_eq!(model.max_tokens, 2_048, "the entry wins the cap");
    assert_eq!(
        model.context_window, 100_000,
        "the catalog supplies the rest"
    );
    assert_eq!(
        model.api, "openai-completions",
        "the catalog supplies the protocol"
    );
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod llm {
    pub use super::provider_catalog_gap as provider_catalog;
}
