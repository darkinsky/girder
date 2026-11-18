//! Tests for the model-facing delegation surface: route authorization, preflight, presets,
//! wording, and discovery.
//!
//! The properties under test are the ones a wrong port gets silently wrong, in the order they
//! matter to a running delegation:
//!
//! - **omission is not permission** — a Session with no recorded policy authorizes exactly the
//!   parent's route, so a model that invents `provider`/`model` is refused. The refusal names the
//!   route and says why, because that is the only way the model can correct itself.
//! - **a partial selection is not a route** — `provider` without `model` (and effort without both)
//!   is refused rather than filled in from a default, which is how an unroutable request would be
//!   produced.
//! - **an omitted effort is not a retained effort** — selecting a route clears the configured
//!   effort, and inheritance from the parent happens only when the route did not change. The wrong
//!   version asks a model for an effort it does not advertise.
//! - **the projection is first-wins** — a later policy event must not widen an allowlist after the
//!   Session's first model request.
//! - **presets are validated where they are configured, not where they are used** — a duplicate id
//!   or a default outside its allowlist fails the mount, and a provider that cannot honor an
//!   independent preset is refused at mount even though the request would have looked valid.
//! - **wording follows the backend's facts** — a fork is not told it sees nothing, a one-shot
//!   backend is not offered `send_message`, and discovery never advertises a route that delegation
//!   would refuse.
//!
//! Isolation: every test builds its own policy, catalog, and options; nothing is global.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the surface as written before the crate root wires it in. It spells its paths through
//! `girder_core::`, which is the alias the library declares for itself, so the same source builds
//! here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/subagent_final_gap.rs"]
mod subagent_final_gap;

use girder_core::{AgentOptions, SubagentCapabilities};
use serde_json::json;
use subagent_final_gap::{
    AllowedModelRoute, CallRoute, DelegationModelRequest, DescriptionOptions, DiscoveryError,
    DiscoveryRequest, ModelCatalog, ModelInfo, ModelSelectionEvent, ModelSelectionPolicy,
    ModelSelectionSettings, PresetError, PresetSelection, PresetSelectionConfig,
    PresetSelectionRequest, ProviderInfo, ReasoningEffortInfo, RouteError, SelectionError,
    apply_policy_event, assert_allowed_routes, assert_allowed_selection, delegation_depth_context,
    delegation_description, delegation_prompt_section, has_configured_llm_selection,
    merge_child_defaults, parse_discovery_request, parse_routes, policy_for_child,
    policy_for_new_top_level, preflight_route, provider_wording, requested_child_options,
    requires_route_preflight,
};

/// A route from two static ids.
fn route(provider: &str, model: &str) -> AllowedModelRoute {
    AllowedModelRoute::new(provider, model).expect("valid test route")
}

/// A policy from route pairs.
fn policy(routes: &[(&str, &str)]) -> ModelSelectionPolicy {
    ModelSelectionPolicy::new(
        routes
            .iter()
            .map(|(provider, model)| route(provider, model))
            .collect(),
    )
    .expect("valid test policy")
}

/// Options naming a route, with no effort.
fn options(provider: &str, model: &str) -> AgentOptions {
    AgentOptions::new(provider, model)
}

/// A model with one default effort.
fn model(id: &str, name: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_owned(),
        name: name.to_owned(),
        description: None,
        efforts: Vec::new(),
    }
}

/// A catalog with the `DeepSeek` and `Anthropic` providers.
fn catalog() -> ModelCatalog {
    ModelCatalog::new(vec![
        ProviderInfo {
            id: "deepseek".to_owned(),
            name: "DeepSeek".to_owned(),
            models: vec![
                ModelInfo {
                    description: Some("the fast one".to_owned()),
                    efforts: vec![
                        ReasoningEffortInfo {
                            id: "low".to_owned(),
                            name: "Low".to_owned(),
                            description: None,
                            default: false,
                        },
                        ReasoningEffortInfo {
                            id: "high".to_owned(),
                            name: "High".to_owned(),
                            description: Some("think harder".to_owned()),
                            default: true,
                        },
                    ],
                    ..model("chat", "Chat")
                },
                model("coder", "Coder"),
            ],
        },
        ProviderInfo {
            id: "anthropic".to_owned(),
            name: "Anthropic".to_owned(),
            models: vec![model("sonnet", "Sonnet")],
        },
        ProviderInfo {
            id: "untouched".to_owned(),
            name: "Untouched".to_owned(),
            models: vec![model("mystery", "Mystery")],
        },
    ])
}

// ---------------------------------------------------------------------------------------------
// Route identity
// ---------------------------------------------------------------------------------------------

#[test]
fn route_key_cannot_be_forged_by_field_boundaries() {
    // Without a separator that cannot appear in an id, ("ab", "c") and ("a", "bc") would collide.
    let left = route("ab", "c");
    let right = route("a", "bc");
    assert_ne!(left.key(), right.key());
    assert_eq!(left.key(), "ab\0c");
    assert_eq!(left.display(), "ab/c");
    assert_eq!(left.provider(), "ab");
    assert_eq!(left.model(), "c");
}

#[test]
fn route_rejects_blank_and_separator_bearing_ids() {
    assert_eq!(
        AllowedModelRoute::new("", "m"),
        Err(RouteError::EmptyProvider)
    );
    assert_eq!(AllowedModelRoute::new("p", ""), Err(RouteError::EmptyModel));
    assert_eq!(
        AllowedModelRoute::new("p\0q", "m"),
        Err(RouteError::Separator { field: "provider" })
    );
    assert_eq!(
        AllowedModelRoute::new("p", "m\0n"),
        Err(RouteError::Separator { field: "model" })
    );
}

#[test]
fn duplicate_routes_are_rejected_in_input_order() {
    let repeated = vec![route("a", "1"), route("b", "2"), route("a", "1")];
    assert_eq!(
        assert_allowed_routes(&repeated),
        Err(RouteError::Repeated {
            route: "a/1".to_owned()
        })
    );
    assert_eq!(
        assert_allowed_routes(&[route("a", "1"), route("b", "2")]),
        Ok(())
    );
}

// ---------------------------------------------------------------------------------------------
// Reading untrusted JSON
// ---------------------------------------------------------------------------------------------

#[test]
fn parse_routes_accepts_objects_and_ignores_extra_keys() {
    let parsed = parse_routes(&json!([{"provider": "p", "model": "m", "note": "ignored"}]))
        .expect("a valid entry");
    assert_eq!(parsed, vec![route("p", "m")]);
}

#[test]
fn parse_routes_refuses_every_malformed_shape() {
    assert_eq!(
        parse_routes(&json!({"provider": "p"})),
        Err(RouteError::NotAnArray)
    );
    assert_eq!(
        parse_routes(&json!(["p/m"])),
        Err(RouteError::Malformed { index: 0 })
    );
    assert_eq!(
        parse_routes(&json!([{"provider": "p"}])),
        Err(RouteError::Malformed { index: 0 })
    );
    assert_eq!(
        parse_routes(&json!([{"provider": "", "model": "m"}])),
        Err(RouteError::Malformed { index: 0 })
    );
    assert_eq!(
        parse_routes(&json!([{"provider": "p", "model": 7}])),
        Err(RouteError::Malformed { index: 0 })
    );
    // The duplicate check reports the position of the repeat, not the original.
    assert_eq!(
        parse_routes(&json!([
            {"provider": "p", "model": "m"},
            {"provider": "q", "model": "n"},
            {"provider": "p", "model": "m"}
        ])),
        Err(RouteError::Repeated {
            route: "p/m".to_owned()
        })
    );
}

// ---------------------------------------------------------------------------------------------
// Policy: recording, inheritance, and the first-wins projection
// ---------------------------------------------------------------------------------------------

#[test]
fn a_recorded_policy_needs_at_least_one_route_and_no_repeats() {
    assert_eq!(
        ModelSelectionPolicy::recorded(Vec::new()),
        Err(RouteError::EmptyPolicy)
    );
    assert_eq!(
        ModelSelectionPolicy::recorded(vec![route("p", "m"), route("p", "m")]),
        Err(RouteError::Repeated {
            route: "p/m".to_owned()
        })
    );
    let recorded = ModelSelectionPolicy::recorded(vec![route("p", "m")]).expect("one route");
    assert!(recorded.allows(&route("p", "m")));
    assert!(!recorded.allows(&route("p", "other")));
    // A non-recorded policy may be empty, because "selection configured away" is representable.
    assert!(ModelSelectionPolicy::new(Vec::new()).is_ok());
}

#[test]
fn the_policy_projection_keeps_the_first_event() {
    let first = apply_policy_event(
        None,
        &ModelSelectionEvent::RecordPolicy {
            allowed_models: vec![route("p", "m")],
        },
    )
    .expect("a first policy");
    let widened = apply_policy_event(
        first.as_ref(),
        &ModelSelectionEvent::RecordPolicy {
            allowed_models: vec![route("p", "m"), route("p", "other")],
        },
    )
    .expect("a later event is accepted but ignored");
    assert_eq!(widened, first);
    assert_eq!(widened.expect("recorded").routes(), &[route("p", "m")]);
}

#[test]
fn an_empty_policy_event_is_refused_rather_than_recorded() {
    assert_eq!(
        apply_policy_event(
            None,
            &ModelSelectionEvent::RecordPolicy {
                allowed_models: Vec::new()
            }
        ),
        Err(RouteError::EmptyPolicy)
    );
}

#[test]
fn a_child_inherits_its_parents_policy_and_nothing_invents_one() {
    let parent = policy(&[("p", "m")]);
    assert_eq!(policy_for_child(Some(&parent)), Some(parent.clone()));
    assert_eq!(policy_for_child(None), None);
}

#[test]
fn the_top_level_default_applies_only_to_a_fresh_unseeded_session() {
    let settings = ModelSelectionSettings::new(true, vec![route("p", "m")]).expect("enabled");
    assert_eq!(
        policy_for_new_top_level(&settings, false),
        Some(policy(&[("p", "m")]))
    );
    assert_eq!(policy_for_new_top_level(&settings, true), None);
    let off = ModelSelectionSettings::new(false, Vec::new()).expect("disabled settings");
    assert_eq!(policy_for_new_top_level(&off, false), None);
}

#[test]
fn a_deployment_setting_is_validated_where_it_is_configured() {
    assert_eq!(
        ModelSelectionSettings::new(true, Vec::new()),
        Err(RouteError::EnabledWithoutRoutes)
    );
    assert_eq!(
        ModelSelectionSettings::new(true, vec![route("p", "m"), route("p", "m")]),
        Err(RouteError::Repeated {
            route: "p/m".to_owned()
        })
    );
    let settings = ModelSelectionSettings::new(true, vec![route("p", "m")]).expect("enabled");
    assert!(settings.enabled());
    assert_eq!(settings.allowed_models(), &[route("p", "m")]);
}

// ---------------------------------------------------------------------------------------------
// The model-facing request
// ---------------------------------------------------------------------------------------------

#[test]
fn an_absent_request_selects_nothing_and_an_empty_string_is_refused() {
    assert!(
        !DelegationModelRequest::from_json(&json!({}))
            .expect("empty")
            .has_selection()
    );
    // `null` means "not sent", which is what a serializer that clears a field produces.
    assert!(
        !DelegationModelRequest::from_json(&json!({"provider": null, "model": null}))
            .expect("nulls")
            .has_selection()
    );
    assert_eq!(
        DelegationModelRequest::from_json(&json!({"provider": ""})),
        Err(RouteError::EmptyProvider)
    );
    assert_eq!(
        DelegationModelRequest::from_json(&json!({"model": ""})),
        Err(RouteError::EmptyModel)
    );
    assert_eq!(
        DelegationModelRequest::from_json(&json!({"reasoning_effort": ""})),
        Err(RouteError::EmptyEffort)
    );
    assert_eq!(
        DelegationModelRequest::from_json(&json!({"model": 3})),
        Err(RouteError::WrongType { field: "model" })
    );
}

#[test]
fn a_request_reading_records_all_three_fields() {
    let request = DelegationModelRequest::from_json(&json!({
        "provider": "p",
        "model": "m",
        "reasoning_effort": "high"
    }))
    .expect("a full request");
    assert_eq!(request.provider.as_deref(), Some("p"));
    assert_eq!(request.model.as_deref(), Some("m"));
    assert_eq!(request.reasoning_effort.as_deref(), Some("high"));
    assert!(request.has_selection());
}

#[test]
fn preflight_is_required_by_either_the_request_or_the_configured_defaults() {
    let none = DelegationModelRequest::new();
    assert!(!requires_route_preflight(&none, None));
    assert!(!has_configured_llm_selection(None));
    assert!(requires_route_preflight(&none, Some(&options("p", "m"))));
    assert!(has_configured_llm_selection(Some(&AgentOptions {
        reasoning_effort: Some("high".to_owned()),
        ..AgentOptions::default()
    })));
    assert!(requires_route_preflight(
        &DelegationModelRequest::new().with_model("m"),
        None
    ));
}

// ---------------------------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------------------------

#[test]
fn without_a_policy_only_the_parent_route_is_allowed() {
    let parent = route("p", "m");
    assert_eq!(
        assert_allowed_selection(None, Some(&parent), &route("p", "m")),
        Ok(())
    );
    let refused = assert_allowed_selection(None, Some(&parent), &route("p", "other"))
        .expect_err("a different route is refused");
    assert_eq!(
        refused,
        RouteError::NotAllowed {
            route: "p/other".to_owned(),
            parent_only: true
        }
    );
    // The guidance suffix is what makes the refusal correctable.
    assert!(refused.message().contains("limited to the parent route"));
    // No parent route at all authorizes nothing.
    assert!(assert_allowed_selection(None, None, &route("p", "m")).is_err());
}

#[test]
fn a_recorded_policy_authorizes_exactly_its_routes() {
    let policy = policy(&[("p", "m"), ("q", "n")]);
    assert_eq!(
        assert_allowed_selection(Some(&policy), Some(&route("p", "parent")), &route("q", "n")),
        Ok(())
    );
    let refused = assert_allowed_selection(Some(&policy), None, &route("p", "parent"))
        .expect_err("the parent route is not in the allowlist");
    assert_eq!(
        refused,
        RouteError::NotAllowed {
            route: "p/parent".to_owned(),
            parent_only: false
        }
    );
    // No suffix: the policy exists, so the model is not told the Session lacks one.
    assert!(!refused.message().contains("limited to the parent route"));
}

// ---------------------------------------------------------------------------------------------
// Turning a request into child options
// ---------------------------------------------------------------------------------------------

#[test]
fn an_absent_request_leaves_the_configured_defaults_untouched() {
    let configured = options("p", "m").with_max_tokens(64);
    let resolved = requested_child_options(
        Some(&configured),
        &DelegationModelRequest::new(),
        true,
        None,
        Some(&route("p", "parent")),
    )
    .expect("no request needs no authorization");
    assert_eq!(resolved, Some(configured));
    assert_eq!(
        requested_child_options(None, &DelegationModelRequest::new(), false, None, None),
        Ok(None)
    );
}

#[test]
fn selection_is_refused_when_the_instance_forbids_it() {
    let request = DelegationModelRequest::new()
        .with_provider("p")
        .with_model("m");
    assert_eq!(
        requested_child_options(None, &request, false, None, Some(&route("p", "m"))),
        Err(SelectionError::Disabled)
    );
}

#[test]
fn a_partial_request_is_refused_instead_of_filled_in() {
    let only_provider = DelegationModelRequest::new().with_provider("p");
    assert_eq!(
        requested_child_options(None, &only_provider, true, None, None),
        Err(SelectionError::NeedsBothHalves)
    );
    let only_effort = DelegationModelRequest::new().with_reasoning_effort("high");
    assert_eq!(
        requested_child_options(None, &only_effort, true, None, None),
        Err(SelectionError::EffortNeedsRoute)
    );
    let empty_provider = DelegationModelRequest {
        provider: Some(String::new()),
        model: Some("m".to_owned()),
        reasoning_effort: None,
    };
    assert_eq!(
        requested_child_options(None, &empty_provider, true, None, None),
        Err(SelectionError::EmptyField { field: "provider" })
    );
}

#[test]
fn an_unauthorized_request_is_refused_through_the_same_path() {
    let request = DelegationModelRequest::new()
        .with_provider("p")
        .with_model("evil");
    let refused = requested_child_options(None, &request, true, None, Some(&route("p", "m")))
        .expect_err("not the parent route");
    assert_eq!(
        refused,
        SelectionError::Route(RouteError::NotAllowed {
            route: "p/evil".to_owned(),
            parent_only: true
        })
    );
}

#[test]
fn a_selected_route_replaces_configured_values_and_clears_the_effort() {
    let configured = options("old", "old")
        .with_reasoning("low")
        .with_max_tokens(32);
    let request = DelegationModelRequest::new()
        .with_provider("p")
        .with_model("m");
    let policy = policy(&[("p", "m")]);
    let resolved = requested_child_options(Some(&configured), &request, true, Some(&policy), None)
        .expect("authorized");
    let resolved = resolved.expect("options");
    assert_eq!(resolved.provider.as_deref(), Some("p"));
    assert_eq!(resolved.model.as_deref(), Some("m"));
    // An omitted effort is cleared, not retained: the new route's default wins.
    assert_eq!(resolved.reasoning_effort, None);
    // The output cap is not a route field, so it survives.
    assert_eq!(resolved.max_tokens, Some(32));
}

#[test]
fn an_explicit_effort_is_carried_onto_the_selected_route() {
    let request = DelegationModelRequest::new()
        .with_provider("p")
        .with_model("m")
        .with_reasoning_effort("high");
    let policy = policy(&[("p", "m")]);
    let resolved = requested_child_options(None, &request, true, Some(&policy), None)
        .expect("authorized")
        .expect("options");
    assert_eq!(resolved.reasoning_effort.as_deref(), Some("high"));
}

// ---------------------------------------------------------------------------------------------
// Defaults merging and preflight
// ---------------------------------------------------------------------------------------------

#[test]
fn provider_defaults_merge_under_the_instance_and_only_when_preflighting() {
    let provider = options("p", "m").with_reasoning("low");
    let configured = AgentOptions {
        model: Some("override".to_owned()),
        max_tokens: Some(100),
        ..AgentOptions::default()
    };
    let merged = merge_child_defaults(Some(&provider), Some(&configured), true).expect("merged");
    assert_eq!(merged.provider.as_deref(), Some("p"));
    assert_eq!(merged.model.as_deref(), Some("override"));
    assert_eq!(merged.reasoning_effort.as_deref(), Some("low"));
    assert_eq!(merged.max_tokens, Some(100));

    // Without a route to resolve, the provider defaults are the provider's business.
    assert_eq!(
        merge_child_defaults(Some(&provider), Some(&configured), false),
        Some(configured)
    );
    assert_eq!(
        merge_child_defaults(Some(&provider), None, true),
        Some(provider)
    );
    assert_eq!(merge_child_defaults(None, None, true), None);
}

#[test]
fn an_unchanged_route_inherits_the_parent_effort() {
    let parent = options("p", "m").with_reasoning("high");
    let route = preflight_route(&parent, None, true).expect("a resolvable route");
    assert_eq!(
        route,
        CallRoute {
            provider: "p".to_owned(),
            model: "m".to_owned(),
            reasoning_effort: Some("high".to_owned()),
        }
    );
}

#[test]
fn a_changed_route_does_not_inherit_the_parent_effort() {
    let parent = options("p", "m").with_reasoning("high");
    let requested = options("q", "other");
    let route = preflight_route(&parent, Some(&requested), true).expect("a resolvable route");
    assert_eq!(route.provider, "q");
    assert_eq!(route.model, "other");
    assert_eq!(route.reasoning_effort, None);
    // Explicit selection also suppresses inheritance, which is what `false` encodes.
    let same = preflight_route(&parent, Some(&options("p", "m")), false).expect("a route");
    assert_eq!(same.reasoning_effort, None);
}

#[test]
fn a_requested_effort_wins_over_inheritance() {
    let parent = options("p", "m").with_reasoning("low");
    let requested = options("p", "m").with_reasoning("high");
    let route = preflight_route(&parent, Some(&requested), true).expect("a route");
    assert_eq!(route.reasoning_effort.as_deref(), Some("high"));
}

#[test]
fn preflight_fills_a_half_requested_route_from_the_parent() {
    let parent = options("p", "m");
    let half = AgentOptions {
        model: Some("other".to_owned()),
        ..AgentOptions::default()
    };
    let route = preflight_route(&parent, Some(&half), true).expect("the parent supplies provider");
    assert_eq!(route.provider, "p");
    assert_eq!(route.model, "other");
}

#[test]
fn preflight_refuses_when_no_complete_route_exists() {
    let parent = AgentOptions {
        provider: Some("p".to_owned()),
        ..AgentOptions::default()
    };
    assert_eq!(
        preflight_route(&parent, None, true),
        Err(SelectionError::Route(RouteError::EmptyProvider))
    );
}

// ---------------------------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------------------------

#[test]
fn preset_configuration_is_validated_at_mount_time() {
    assert_eq!(
        PresetSelection::new(
            PresetSelectionConfig::new().with_allowed(vec!["a".to_owned(), "a".to_owned()])
        ),
        Err(PresetError::DuplicateId { id: "a".to_owned() })
    );
    assert_eq!(
        PresetSelection::new(
            PresetSelectionConfig::new()
                .with_default("b")
                .with_allowed(vec!["a".to_owned()])
        ),
        Err(PresetError::DefaultNotAllowed { id: "b".to_owned() })
    );
    assert!(
        PresetSelection::new(
            PresetSelectionConfig::new()
                .with_default("a")
                .with_allowed(vec!["a".to_owned()])
        )
        .is_ok()
    );
}

#[test]
fn an_unconfigured_preset_is_neither_advertised_nor_explained() {
    let selection = PresetSelection::new(PresetSelectionConfig::new()).expect("valid");
    assert!(!selection.configured());
    assert_eq!(selection.choices(), None);
    assert_eq!(
        selection.description(),
        "Agent preset for this child. Omit to inherit the parent agent's current preset."
    );
    // A model that asks anyway is told the feature is off, not given the parent's preset.
    assert_eq!(
        selection.resolve(&PresetSelectionRequest {
            agent_preset: Some(json!("a"))
        }),
        Err(PresetError::Disabled)
    );
}

#[test]
fn a_configured_default_is_applied_only_when_the_model_omits_one() {
    let selection = PresetSelection::new(
        PresetSelectionConfig::new()
            .with_default("reviewer")
            .with_allowed(vec!["reviewer".to_owned(), "writer".to_owned()]),
    )
    .expect("valid");
    assert!(selection.configured());
    assert_eq!(
        selection.choices(),
        Some(["reviewer".to_owned(), "writer".to_owned()].as_slice())
    );
    assert_eq!(
        selection.description(),
        "Agent preset for this child. Omit to use reviewer."
    );
    assert_eq!(
        selection.resolve(&PresetSelectionRequest { agent_preset: None }),
        Ok(Some("reviewer".to_owned()))
    );
    assert_eq!(
        selection.resolve(&PresetSelectionRequest {
            agent_preset: Some(json!("writer"))
        }),
        Ok(Some("writer".to_owned()))
    );
    assert_eq!(
        selection.resolve(&PresetSelectionRequest {
            agent_preset: Some(json!("admin"))
        }),
        Err(PresetError::NotInChoices {
            choices: "reviewer, writer".to_owned()
        })
    );
    assert_eq!(
        selection.resolve(&PresetSelectionRequest {
            agent_preset: Some(json!(7))
        }),
        Err(PresetError::NotInChoices {
            choices: "reviewer, writer".to_owned()
        })
    );
}

#[test]
fn an_allowlist_without_a_default_still_selects_nothing_on_omission() {
    let selection =
        PresetSelection::new(PresetSelectionConfig::new().with_allowed(vec!["a".to_owned()]))
            .expect("valid");
    assert_eq!(
        selection.resolve(&PresetSelectionRequest { agent_preset: None }),
        Ok(None)
    );
}

#[test]
fn a_provider_that_cannot_honor_a_preset_is_refused_at_mount() {
    let selection =
        PresetSelection::new(PresetSelectionConfig::new().with_allowed(vec!["a".to_owned()]))
            .expect("valid");

    let capable = SubagentCapabilities::full();
    assert_eq!(selection.assert_provider("fork", capable, false), Ok(()));

    // No declared capability: the selection would be silently ignored.
    let mut no_preset = SubagentCapabilities::full();
    no_preset.agent_preset = false;
    assert_eq!(
        selection.assert_provider("spawn", no_preset, false),
        Err(PresetError::ProviderUnsupported {
            name: "spawn".to_owned()
        })
    );
    // Inheriting the parent's conversation makes an independent preset impossible.
    assert_eq!(
        selection.assert_provider("fork", capable, true),
        Err(PresetError::ProviderUnsupported {
            name: "fork".to_owned()
        })
    );
    // An unconfigured selection imposes nothing, even on a provider that cannot do presets.
    let unconfigured = PresetSelection::new(PresetSelectionConfig::new()).expect("valid");
    assert_eq!(
        unconfigured.assert_provider("spawn", no_preset, true),
        Ok(())
    );
}

// ---------------------------------------------------------------------------------------------
// Wording
// ---------------------------------------------------------------------------------------------

#[test]
fn fork_wording_says_the_child_has_the_history_and_fresh_wording_says_it_does_not() {
    let fork = provider_wording(true);
    assert!(fork.description.contains("inherits this conversation"));
    assert!(
        fork.description
            .contains("does not see the current in-flight turn")
    );
    assert!(!fork.description.contains("does not share this"));
    assert!(fork.prompt_description.contains("build on them freely"));

    let fresh = provider_wording(false);
    assert!(fresh.description.contains("self-contained"));
    assert!(fresh.description.contains("own context"));
    assert!(!fresh.description.contains("inherits this conversation"));
    assert!(
        fresh
            .prompt_description
            .contains("does not share this conversation's context")
    );
}

#[test]
fn the_description_offers_follow_up_messages_only_for_a_continuable_backend() {
    let continuable = DelegationDescriptionBuilder::new()
        .continuable(true)
        .render();
    assert!(continuable.contains("Use `send_message` to steer it"));
    let one_shot = DelegationDescriptionBuilder::new()
        .continuable(false)
        .render();
    assert!(one_shot.contains("This backend does not accept follow-up messages."));
    assert!(!one_shot.contains("send_message"));
}

#[test]
fn the_description_explains_omission_only_when_selection_is_exposed() {
    let hidden = DelegationDescriptionBuilder::new().render();
    assert!(!hidden.contains("Omit `provider`"));
    let exposed = DelegationDescriptionBuilder::new()
        .model_selection(true)
        .render();
    assert!(exposed.contains("Omit `provider`, `model`, and `reasoning_effort`"));
    assert!(exposed.contains("compatible parent values"));
    assert!(!exposed.contains("this backend's route defaults"));
}

#[test]
fn a_backend_with_route_defaults_is_described_with_those_defaults() {
    let described = DelegationDescriptionBuilder::new()
        .model_selection(true)
        .route_defaults(true)
        .render();
    assert!(described.contains("falling back to this backend's route defaults"));
    assert!(!described.contains("compatible parent values"));
}

#[test]
fn a_route_change_warning_appears_only_for_an_inheriting_backend_with_selection() {
    let warned = DelegationDescriptionBuilder::new()
        .model_selection(true)
        .inherits_conversation(true)
        .render();
    assert!(warned.contains("prevent provider-side reuse of the inherited conversation prefix"));
    let unwarned = DelegationDescriptionBuilder::new()
        .model_selection(true)
        .inherits_conversation(false)
        .render();
    assert!(!unwarned.contains("inherited conversation prefix"));
    // The warning belongs to the selection sentence, so it is absent when selection is hidden.
    let hidden = DelegationDescriptionBuilder::new()
        .inherits_conversation(true)
        .render();
    assert!(!hidden.contains("inherited conversation prefix"));
}

/// A small builder so each description test names only the flags it is about.
struct DelegationDescriptionBuilder {
    options: DescriptionOptions,
}

impl DelegationDescriptionBuilder {
    /// One-shot, fresh, no selection.
    fn new() -> Self {
        Self {
            options: DescriptionOptions {
                continuable: false,
                model_selection: false,
                route_defaults: false,
                inherits_conversation: false,
            },
        }
    }

    fn continuable(mut self, value: bool) -> Self {
        self.options.continuable = value;
        self
    }

    fn model_selection(mut self, value: bool) -> Self {
        self.options.model_selection = value;
        self
    }

    fn route_defaults(mut self, value: bool) -> Self {
        self.options.route_defaults = value;
        self
    }

    fn inherits_conversation(mut self, value: bool) -> Self {
        self.options.inherits_conversation = value;
        self
    }

    fn render(&self) -> String {
        delegation_description(self.options)
    }
}

#[test]
fn the_prompt_section_renders_once_and_names_every_visible_tool() {
    let names = vec!["subagent".to_owned(), "delegate".to_owned()];
    let section = delegation_prompt_section("delegate", &names);
    assert!(section.starts_with("Start independent delegations with `delegate` or `subagent`"));
    assert!(section.contains("end your turn instead of sleeping or polling"));
    assert!(section.contains("review the final diff and run relevant tests"));
    // The second tool by sort order is not the section's owner.
    assert_eq!(delegation_prompt_section("subagent", &names), "");
    // A tool that is not visible at all renders nothing.
    assert_eq!(delegation_prompt_section("hidden", &names), "");
    assert_eq!(delegation_prompt_section("subagent", &[]), "");
}

#[test]
fn the_depth_sentence_appears_at_the_ceiling_and_nowhere_below_it() {
    assert_eq!(delegation_depth_context("subagent", 0, 1), None);
    assert_eq!(
        delegation_depth_context("subagent", 1, 1).as_deref(),
        Some(
            "Delegation depth for `subagent`: current 1, maximum 1, remaining levels 0. Do not call \
         `subagent` at this depth; carry out the work directly."
        )
    );
    // Above the ceiling is also a refusal, not a silence.
    assert!(delegation_depth_context("subagent", 2, 1).is_some());
}

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

#[test]
fn discovery_reads_its_two_optional_parameters() {
    assert_eq!(
        parse_discovery_request(None, None),
        Ok(DiscoveryRequest::Providers)
    );
    assert_eq!(
        parse_discovery_request(Some(&json!("deepseek")), None),
        Ok(DiscoveryRequest::Models {
            provider: "deepseek".to_owned()
        })
    );
    assert_eq!(
        parse_discovery_request(Some(&json!("deepseek")), Some(&json!("chat"))),
        Ok(DiscoveryRequest::Efforts {
            provider: "deepseek".to_owned(),
            model: "chat".to_owned()
        })
    );
    assert_eq!(
        parse_discovery_request(None, Some(&json!("chat"))),
        Err(DiscoveryError::ModelRequiresProvider)
    );
    assert_eq!(
        parse_discovery_request(Some(&json!("")), None),
        Err(DiscoveryError::EmptyProvider)
    );
    assert_eq!(
        parse_discovery_request(Some(&json!("p")), Some(&json!(""))),
        Err(DiscoveryError::EmptyModel)
    );
    assert_eq!(
        parse_discovery_request(Some(&json!(3)), None),
        Err(DiscoveryError::Malformed)
    );
}

#[test]
fn discovery_answers_providers_from_the_policy_not_the_catalog() {
    let catalog = catalog();
    let policy = policy(&[("deepseek", "chat")]);
    let answer = catalog
        .discover(&policy, false, &DiscoveryRequest::Providers)
        .expect("providers");
    assert_eq!(answer, "deepseek — DeepSeek");
    // A policy matching nothing advertises nothing rather than everything.
    let empty = ModelSelectionPolicy::new(Vec::new()).expect("empty policy");
    assert_eq!(
        catalog
            .discover(&empty, false, &DiscoveryRequest::Providers)
            .expect("providers"),
        "(no LLM providers)"
    );
}

#[test]
fn without_an_opt_in_only_the_single_allowed_model_is_resolved() {
    let catalog = catalog();
    let policy = policy(&[("deepseek", "coder")]);
    let answer = catalog
        .discover(
            &policy,
            false,
            &DiscoveryRequest::Models {
                provider: "deepseek".to_owned(),
            },
        )
        .expect("models");
    // `selection_enabled: false` resolves exactly the authorized route, not the whole list.
    assert_eq!(answer, "deepseek/coder — Coder");
}

#[test]
fn with_an_opt_in_the_authorized_models_are_listed() {
    let catalog = catalog();
    let policy = policy(&[("deepseek", "chat"), ("deepseek", "coder")]);
    let answer = catalog
        .discover(
            &policy,
            true,
            &DiscoveryRequest::Models {
                provider: "deepseek".to_owned(),
            },
        )
        .expect("models");
    assert_eq!(
        answer,
        "deepseek/chat — Chat: the fast one\ndeepseek/coder — Coder"
    );
}

#[test]
fn discovery_refuses_an_unregistered_provider_and_names_the_authorized_ones() {
    let catalog = catalog();
    // The policy names `ghost`, so the refusal is registration rather than authorization, and the
    // available list is the registered providers the Session may actually use.
    let policy = policy(&[
        ("deepseek", "chat"),
        ("anthropic", "sonnet"),
        ("ghost", "nothing"),
    ]);
    let error = catalog
        .discover(
            &policy,
            true,
            &DiscoveryRequest::Models {
                provider: "ghost".to_owned(),
            },
        )
        .expect_err("not registered");
    assert_eq!(
        error,
        DiscoveryError::ProviderNotRegistered {
            id: "ghost".to_owned(),
            available: "deepseek, anthropic".to_owned()
        }
    );
    assert!(
        error
            .to_string()
            .contains("available providers: deepseek, anthropic")
    );
}

#[test]
fn discovery_checks_authorization_before_registration() {
    // Upstream reads the allowlist first, so a provider the Session may not use is refused with the
    // authorization message even when it is also unregistered — that is the message the model can
    // act on.
    let catalog = catalog();
    let policy = policy(&[("deepseek", "chat")]);
    assert_eq!(
        catalog.discover(
            &policy,
            true,
            &DiscoveryRequest::Models {
                provider: "openai".to_owned()
            }
        ),
        Err(DiscoveryError::ProviderNotAllowed {
            id: "openai".to_owned()
        })
    );
}

#[test]
fn discovery_refuses_a_registered_provider_the_session_may_not_use() {
    let catalog = catalog();
    let policy = policy(&[("deepseek", "chat")]);
    assert_eq!(
        catalog.discover(
            &policy,
            true,
            &DiscoveryRequest::Models {
                provider: "untouched".to_owned()
            }
        ),
        Err(DiscoveryError::ProviderNotAllowed {
            id: "untouched".to_owned()
        })
    );
}

#[test]
fn discovery_refuses_an_unauthorized_exact_route_even_when_the_provider_is_allowed() {
    let catalog = catalog();
    let policy = policy(&[("deepseek", "chat")]);
    assert_eq!(
        catalog.discover(
            &policy,
            true,
            &DiscoveryRequest::Efforts {
                provider: "deepseek".to_owned(),
                model: "coder".to_owned()
            }
        ),
        Err(DiscoveryError::RouteNotAllowed {
            route: "deepseek/coder".to_owned()
        })
    );
}

#[test]
fn discovery_lists_efforts_with_the_default_marked() {
    let catalog = catalog();
    let policy = policy(&[("deepseek", "chat")]);
    let answer = catalog
        .discover(
            &policy,
            true,
            &DiscoveryRequest::Efforts {
                provider: "deepseek".to_owned(),
                model: "chat".to_owned(),
            },
        )
        .expect("efforts");
    assert_eq!(
        answer,
        "deepseek/chat — Chat: the fast one\nReasoning efforts:\nlow — Low\nhigh (default) — High: think harder"
    );
}

#[test]
fn a_model_with_no_efforts_says_so_instead_of_rendering_an_empty_list() {
    let catalog = catalog();
    let policy = policy(&[("deepseek", "coder")]);
    let answer = catalog
        .discover(
            &policy,
            true,
            &DiscoveryRequest::Efforts {
                provider: "deepseek".to_owned(),
                model: "coder".to_owned(),
            },
        )
        .expect("efforts");
    assert!(answer.ends_with("Reasoning efforts:\n(no advertised reasoning efforts)"));
}

#[test]
fn a_provider_with_no_authorized_models_says_so() {
    let catalog = ModelCatalog::new(vec![ProviderInfo {
        id: "p".to_owned(),
        name: "P".to_owned(),
        models: Vec::new(),
    }]);
    let policy = policy(&[("p", "ghost")]);
    let answer = catalog
        .discover(
            &policy,
            true,
            &DiscoveryRequest::Models {
                provider: "p".to_owned(),
            },
        )
        .expect("models");
    assert_eq!(answer, "(no advertised models for p)");
}

#[test]
fn a_session_without_a_policy_discovers_from_the_parent_route_alone() {
    let parent_policy = ModelCatalog::policy_for_parent(Some("deepseek"), Some("chat"))
        .expect("the parent names a route");
    assert_eq!(parent_policy.routes(), &[route("deepseek", "chat")]);
    assert_eq!(
        ModelCatalog::policy_for_parent(None, Some("chat")),
        Err(DiscoveryError::NoEffectiveRoute)
    );
    assert_eq!(
        ModelCatalog::policy_for_parent(Some("deepseek"), None),
        Err(DiscoveryError::NoEffectiveRoute)
    );
}
