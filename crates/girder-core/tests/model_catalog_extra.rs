//! Tests for the model capability catalog.
//!
//! Three properties carry the module, and each is a way the code could be plausible and wrong:
//!
//! - **`false` means "not required", not "must not"** — the requirement semantics. A filter that read
//!   [`Capabilities`] as an exact specification would pass every test that only asks for `true`, and
//!   would still hide most of a deployment's models from any caller that did not fill in all five
//!   fields. The tests here ask for one ability at a time and assert the models that have it are all
//!   present, including ones that also have abilities nobody asked for.
//! - **An unknown size is not a small one** — `context_window: None` must never be filtered out for
//!   having an unknown window, or a catalog that simply did not say would look like a catalog of
//!   unusable models.
//! - **The rendered order is stable** — two clients asking the same question must render the same
//!   list, and a sort that reordered equal keys would break that without failing any assertion about
//!   contents.
//!
//! The module is included by path rather than through the crate's re-exports, because the crate root
//! has not been wired to it yet. It resolves `girder_core::…` through the alias the library declares
//! for itself, which is why the same source builds here.

#[path = "../src/model_catalog_extra.rs"]
mod model_catalog_extra;

use model_catalog_extra::{Capabilities, ModelEntry, filter_by, to_catalog_json};

// ---- fixtures ----------------------------------------------------------------------------------

/// A model that can do nothing and declares nothing: the base every fixture builds on.
fn bare(provider: &str, name: &str) -> ModelEntry {
    ModelEntry {
        provider: provider.to_owned(),
        name: name.to_owned(),
        capabilities: Capabilities::default(),
        recommended: false,
    }
}

/// A model with the given abilities and no declared sizes.
fn able(provider: &str, name: &str, tools: bool, vision: bool, reasoning: bool) -> ModelEntry {
    ModelEntry {
        capabilities: Capabilities {
            tools,
            vision,
            reasoning,
            ..Capabilities::default()
        },
        ..bare(provider, name)
    }
}

/// The names of a filtered catalog, in the order the filter returned them.
fn names(entries: &[&ModelEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| format!("{}/{}", entry.provider, entry.name))
        .collect()
}

/// A three-model deployment, deliberately in a non-sorted order: one text-and-tools model, one
/// vision model without tools, and one model that reasons and knows its size.
fn deployment() -> Vec<ModelEntry> {
    vec![
        able("zeta", "chat", true, false, false),
        able("alpha", "vision-pro", false, true, false),
        ModelEntry {
            capabilities: Capabilities {
                tools: true,
                vision: true,
                reasoning: true,
                context_window: Some(128_000),
                max_output: Some(16_384),
            },
            ..bare("alpha", "omni")
        },
    ]
}

// ---- the empty catalog -------------------------------------------------------------------------

#[test]
fn an_empty_catalog_filters_to_nothing() {
    let matched = filter_by(&[], &Capabilities::default());

    assert!(matched.is_empty());
}

#[test]
fn an_empty_catalog_renders_an_empty_but_shaped_value() {
    // A client that received `null` would have to special-case it; the shape must be the same one a
    // populated catalog has, just empty.
    let value = to_catalog_json(&[]);

    assert_eq!(value["groups"], serde_json::json!([]));
    assert_eq!(value["routableProviders"], serde_json::json!([]));
    assert_eq!(value["failures"], serde_json::json!([]));
    assert!(value["default"].is_null());
}

// ---- requirements are what is asked for, not what is forbidden (the core property) --------------

#[test]
fn no_requirements_matches_every_model() {
    // `Capabilities::default()` is all-false and all-unknown. Read as an exact specification it would
    // match only the model with no abilities at all; read as a requirement it matches everything,
    // which is the only useful answer to "I have not decided yet".
    let catalog = deployment();

    let matched = filter_by(&catalog, &Capabilities::default());

    assert_eq!(matched.len(), catalog.len());
}

#[test]
fn a_false_flag_does_not_exclude_a_model_that_has_the_ability() {
    // The mistake this pins down: `tools: false` must not mean "models without tools". Every model
    // here has at least one ability, and all of them must survive an empty requirement.
    let catalog = deployment();
    let need = Capabilities {
        tools: false,
        vision: false,
        reasoning: false,
        context_window: None,
        max_output: None,
    };

    let matched = filter_by(&catalog, &need);

    assert_eq!(matched.len(), 3, "false is not a prohibition");
}

#[test]
fn asking_for_tools_keeps_the_vision_only_model_out() {
    let catalog = deployment();
    let need = Capabilities {
        tools: true,
        ..Capabilities::default()
    };

    let matched = filter_by(&catalog, &need);

    // `alpha/vision-pro` lacks tools and is the only one excluded; the other two stay even though
    // neither is asked for.
    assert_eq!(names(&matched), vec!["alpha/omni", "zeta/chat"]);
}

#[test]
fn asking_for_vision_selects_only_the_models_that_take_images() {
    let catalog = deployment();
    let need = Capabilities {
        vision: true,
        ..Capabilities::default()
    };

    assert_eq!(
        names(&filter_by(&catalog, &need)),
        vec!["alpha/omni", "alpha/vision-pro"]
    );
}

#[test]
fn asking_for_reasoning_selects_only_the_model_that_reasons() {
    let catalog = deployment();
    let need = Capabilities {
        reasoning: true,
        ..Capabilities::default()
    };

    assert_eq!(names(&filter_by(&catalog, &need)), vec!["alpha/omni"]);
}

#[test]
fn several_requirements_are_all_required() {
    let catalog = deployment();
    let need = Capabilities {
        tools: true,
        vision: true,
        ..Capabilities::default()
    };

    assert_eq!(names(&filter_by(&catalog, &need)), vec!["alpha/omni"]);
}

// ---- unknown sizes -----------------------------------------------------------------------------

#[test]
fn an_unknown_context_window_is_not_filtered_out() {
    // `None` is "nobody said", not "zero": a caller that needs a large window must still see the
    // models whose window is unknown, or an under-specified catalog would look empty.
    let catalog = vec![
        bare("alpha", "unstated"),
        ModelEntry {
            capabilities: Capabilities {
                context_window: Some(200_000),
                ..Capabilities::default()
            },
            ..bare("alpha", "huge")
        },
    ];
    let need = Capabilities {
        context_window: Some(64_000),
        ..Capabilities::default()
    };

    assert_eq!(
        names(&filter_by(&catalog, &need)),
        vec!["alpha/huge", "alpha/unstated"]
    );
}

#[test]
fn an_unknown_max_output_is_not_filtered_out() {
    let catalog = vec![
        bare("alpha", "unstated"),
        ModelEntry {
            capabilities: Capabilities {
                max_output: Some(32_768),
                ..Capabilities::default()
            },
            ..bare("alpha", "long")
        },
    ];
    let need = Capabilities {
        max_output: Some(4_096),
        ..Capabilities::default()
    };

    assert_eq!(
        names(&filter_by(&catalog, &need)),
        vec!["alpha/long", "alpha/unstated"]
    );
}

#[test]
fn a_declared_window_smaller_than_required_is_filtered_out() {
    // The complement of the unknown-size rule: a *stated* small window is a stated fact, and a fact
    // that contradicts the requirement is the one case that excludes a model.
    let catalog = vec![
        ModelEntry {
            capabilities: Capabilities {
                context_window: Some(8_000),
                ..Capabilities::default()
            },
            ..bare("alpha", "small")
        },
        ModelEntry {
            capabilities: Capabilities {
                context_window: Some(64_000),
                ..Capabilities::default()
            },
            ..bare("alpha", "exact")
        },
    ];
    let need = Capabilities {
        context_window: Some(64_000),
        ..Capabilities::default()
    };

    assert_eq!(names(&filter_by(&catalog, &need)), vec!["alpha/exact"]);
}

#[test]
fn a_required_size_of_zero_is_still_a_requirement() {
    // `Some(0)` is a stated requirement even though it excludes nothing in practice; only `None`
    // means "unasked". Treating zero as "no requirement" would blur the two.
    let catalog = deployment();
    let need = Capabilities {
        context_window: Some(0),
        ..Capabilities::default()
    };

    assert_eq!(filter_by(&catalog, &need).len(), catalog.len());
}

// ---- the rendered catalog ----------------------------------------------------------------------

#[test]
fn the_catalog_groups_models_by_provider() {
    let catalog = deployment();

    let value = to_catalog_json(&catalog);
    let groups = value["groups"].as_array().expect("groups is an array");

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["provider"], "alpha");
    assert_eq!(groups[0]["models"].as_array().map(Vec::len), Some(2));
    assert_eq!(groups[1]["provider"], "zeta");
    assert_eq!(groups[1]["models"].as_array().map(Vec::len), Some(1));
}

#[test]
fn a_rendered_model_carries_its_capabilities() {
    let catalog = vec![ModelEntry {
        capabilities: Capabilities {
            tools: true,
            vision: true,
            reasoning: true,
            context_window: Some(128_000),
            max_output: Some(16_384),
        },
        recommended: true,
        ..bare("alpha", "omni")
    }];

    let model = &to_catalog_json(&catalog)["groups"][0]["models"][0];

    assert_eq!(model["provider"], "alpha");
    assert_eq!(model["model"], "omni");
    assert_eq!(model["displayName"], "omni");
    assert_eq!(model["contextWindow"], 128_000);
    assert_eq!(model["maxOutput"], 16_384);
    assert_eq!(model["capabilities"]["tools"], true);
    assert_eq!(model["capabilities"]["vision"], true);
    assert_eq!(model["capabilities"]["reasoning"], true);
    assert_eq!(model["recommended"], true);
}

#[test]
fn an_unknown_window_renders_as_null_rather_than_zero() {
    // A client that showed "0 tokens" would be claiming a fact nobody stated.
    let catalog = vec![bare("alpha", "unstated")];

    let model = &to_catalog_json(&catalog)["groups"][0]["models"][0];

    assert!(model["contextWindow"].is_null());
    assert!(model["maxOutput"].is_null());
}

#[test]
fn the_default_is_the_first_recommended_model() {
    let catalog = vec![
        bare("alpha", "plain"),
        ModelEntry {
            recommended: true,
            ..bare("beta", "preferred")
        },
    ];

    let value = to_catalog_json(&catalog);

    assert_eq!(value["default"]["provider"], "beta");
    assert_eq!(value["default"]["model"], "preferred");
}

#[test]
fn the_default_falls_back_to_the_first_model_when_nothing_is_recommended() {
    let catalog = deployment();

    let value = to_catalog_json(&catalog);

    // Sorted order, so the fallback is deterministic rather than whatever was inserted first.
    assert_eq!(value["default"]["provider"], "alpha");
    assert_eq!(value["default"]["model"], "omni");
}

#[test]
fn the_routable_providers_are_the_grouped_ones() {
    let catalog = deployment();

    let value = to_catalog_json(&catalog);
    let routable: Vec<&str> = value["routableProviders"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|item| item.as_str().expect("a string"))
        .collect();

    assert_eq!(routable, vec!["alpha", "zeta"]);
}

// ---- ordering ----------------------------------------------------------------------------------

#[test]
fn filtering_orders_by_provider_then_model() {
    let catalog = vec![
        bare("zeta", "b"),
        bare("alpha", "z"),
        bare("zeta", "a"),
        bare("alpha", "a"),
    ];

    assert_eq!(
        names(&filter_by(&catalog, &Capabilities::default())),
        vec!["alpha/a", "alpha/z", "zeta/a", "zeta/b"]
    );
}

#[test]
fn the_order_does_not_depend_on_insertion_order() {
    let mut forwards = deployment();
    forwards.sort_by(|left, right| left.name.cmp(&right.name));

    let mut backwards = deployment();
    backwards.reverse();

    assert_eq!(
        to_catalog_json(&forwards)["groups"],
        to_catalog_json(&backwards)["groups"],
        "the same catalog must render the same way whatever order it was built in"
    );
}

#[test]
fn equal_keys_keep_the_callers_order() {
    // Two mounts of one model are told apart by nothing the catalog records, so the only order they
    // can keep is the one they arrived in; a non-stable sort would be free to swap them.
    let first = ModelEntry {
        recommended: true,
        ..bare("alpha", "same")
    };
    let second = ModelEntry {
        capabilities: Capabilities {
            vision: true,
            ..Capabilities::default()
        },
        ..bare("alpha", "same")
    };
    let catalog = vec![second.clone(), first.clone()];

    let matched = filter_by(&catalog, &Capabilities::default());

    assert_eq!(matched.len(), 2);
    assert_eq!(matched[0].capabilities, second.capabilities);
    assert_eq!(matched[1].capabilities, first.capabilities);
}
