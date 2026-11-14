//! Tests for choosing a fallback model.
//!
//! Four properties carry the module, and each is a plausible way to get it wrong:
//!
//! - **Preference orders, requirement filters** — the easy bug is a sort by preference with no filter,
//!   or a filter that then ignores preference. Both pass a test that only checks "a model was
//!   returned" and both break a real request, in opposite directions.
//! - **A model that cannot serve the request is skipped, not failed on** — the point of the module.
//!   The highest-preference candidate here deliberately lacks the one required ability, so a
//!   test that picked it would be picking the wrong model with no other test failing.
//! - **No fit is a described failure, not a panic and not a bare `None`** — the counts are what make
//!   the failure actionable, so they are asserted exactly rather than merely being non-empty.
//! - **An unknown window is unverified, not insufficient** — the same contract the catalog filter
//!   keeps, and the same reason: refusing a model for a fact nobody claimed hides usable models.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written before the crate root is wired to it. It resolves `crate::…` through the
//! alias the library declares for itself, which is why the same source builds here.

use girder_core::ModelCapabilities;
use girder_core::{
    FallbackCandidate as Candidate, ModelNeeds as Needs, ModelShortfall as Shortfall,
    candidates_from_routes, choose_model as choose,
};

// ---- fixtures ----------------------------------------------------------------------------------

/// A candidate with the stated abilities, window, and preference.
fn candidate(
    provider: &str,
    model: &str,
    abilities: (bool, bool, bool),
    context_window: Option<u64>,
    preference: u32,
) -> Candidate {
    Candidate {
        provider: provider.to_owned(),
        model: model.to_owned(),
        capabilities: ModelCapabilities {
            tools: abilities.0,
            vision: abilities.1,
            reasoning: abilities.2,
            context_window,
            max_output: None,
        },
        context_window: None,
        preference,
    }
}

/// A candidate that can call tools, see images, and reason, with a large window.
fn full(provider: &str, model: &str, preference: u32) -> Candidate {
    candidate(
        provider,
        model,
        (true, true, true),
        Some(200_000),
        preference,
    )
}

/// A request that needs tools and nothing else.
fn needs_tools() -> Needs {
    Needs {
        tools: true,
        ..Needs::default()
    }
}

// ---- the happy paths ---------------------------------------------------------------------------

#[test]
fn picks_the_only_candidate_that_satisfies() {
    let candidates = [full("anthropic", "sonnet", 10)];
    let chosen = choose(&needs_tools(), &candidates).expect("a candidate satisfies");
    assert_eq!(chosen.model, "sonnet");
}

#[test]
fn the_highest_preference_wins_among_satisfying_candidates() {
    let candidates = [
        full("anthropic", "haiku", 10),
        full("anthropic", "opus", 90),
        full("anthropic", "sonnet", 50),
    ];
    let chosen = choose(&needs_tools(), &candidates).expect("three candidates satisfy");
    assert_eq!(chosen.model, "opus");
}

#[test]
fn no_stated_requirement_accepts_every_candidate() {
    // `Needs::default()` asks for nothing, so preference alone decides. This is the case a caller
    // reaches on a plain text request, and refusing it would make the module unusable where it is
    // most often called.
    let candidates = [
        candidate("openai", "plain", (false, false, false), None, 1),
        full("openai", "rich", 7),
    ];
    let chosen = choose(&Needs::default(), &candidates).expect("any candidate satisfies");
    assert_eq!(chosen.model, "rich");
}

// ---- requirement filtering (the core property) -------------------------------------------------

#[test]
fn a_more_preferred_candidate_without_the_ability_is_skipped() {
    // The whole point: "gpt-fast" is the deployment's favourite, but the request carries tools and it
    // cannot take them. Choosing it would send a request the provider rejects, so the next candidate
    // must be used even though it is less preferred.
    let candidates = [
        candidate(
            "openai",
            "gpt-fast",
            (false, true, true),
            Some(200_000),
            100,
        ),
        candidate(
            "openai",
            "gpt-tools",
            (true, false, false),
            Some(200_000),
            1,
        ),
    ];
    let chosen = choose(&needs_tools(), &candidates).expect("the second candidate satisfies");
    assert_eq!(chosen.model, "gpt-tools");
}

#[test]
fn each_stated_requirement_filters_independently() {
    let candidates = [
        candidate("a", "no-vision", (true, false, true), Some(200_000), 90),
        candidate("a", "no-reasoning", (true, true, false), Some(200_000), 80),
        candidate("a", "all", (true, true, true), Some(200_000), 10),
    ];
    let needs = Needs {
        tools: true,
        vision: true,
        reasoning: true,
        min_context: None,
    };
    let chosen = choose(&needs, &candidates).expect("`all` satisfies");
    assert_eq!(chosen.model, "all");

    // Asking for vision alone admits the reasoning-capable model, so the filter is per requirement
    // and not "all three or nothing".
    let needs = Needs {
        vision: true,
        ..Needs::default()
    };
    let chosen = choose(&needs, &candidates).expect("two candidates satisfy");
    assert_eq!(chosen.model, "no-reasoning");
}

#[test]
fn the_route_window_overrides_the_model_window() {
    // A route can be held below the model's published window (a plan limit, a proxy). The route value
    // is the one that applies, so a candidate whose model is large enough but whose route is not must
    // be skipped.
    let mut throttled = full("a", "large-model-small-plan", 100);
    throttled.context_window = Some(8_000);
    let candidates = [throttled, full("a", "small-model", 1)];
    let needs = Needs {
        min_context: Some(100_000),
        ..Needs::default()
    };
    let chosen = choose(&needs, &candidates).expect("the second candidate satisfies");
    assert_eq!(chosen.model, "small-model");
}

// ---- the shortfall -----------------------------------------------------------------------------

#[test]
fn no_fit_returns_a_shortfall_instead_of_panicking() {
    let candidates = [candidate("a", "text-only", (false, false, false), None, 10)];
    let shortfall = choose(&needs_tools(), &candidates).expect_err("nothing satisfies");
    assert_eq!(shortfall.eliminated.get("tools"), Some(&1));
    assert!(
        shortfall.message.contains("tools"),
        "the message should name the requirement worth relaxing: {}",
        shortfall.message
    );
}

#[test]
fn empty_candidates_are_a_shortfall_not_a_panic() {
    let shortfall: Shortfall = choose(&needs_tools(), &[]).expect_err("there are no candidates");
    // Every stated requirement is still reported, with zero eliminations: the caller learns that no
    // allowance would help because nothing was offered, which is a different problem from "too
    // strict".
    assert_eq!(shortfall.eliminated.get("tools"), Some(&0));
    assert!(
        shortfall.message.contains("no candidates were offered"),
        "unexpected message: {}",
        shortfall.message
    );
}

#[test]
fn shortfall_counts_only_requirements_whose_relaxation_would_help() {
    // Five candidates miss only tools, one misses only vision, and one misses both. Relaxing tools
    // admits the five; relaxing vision admits the one; relaxing neither admits the last, so it is
    // counted under neither. That is what makes "tools: 5, vision: 1" advice rather than a tally.
    let mut candidates: Vec<Candidate> = (0..5)
        .map(|index| {
            candidate(
                "a",
                &format!("no-tools-{index}"),
                (false, true, true),
                None,
                1,
            )
        })
        .collect();
    candidates.push(candidate("a", "no-vision", (true, false, true), None, 1));
    candidates.push(candidate(
        "a",
        "no-tools-no-vision",
        (false, false, true),
        None,
        1,
    ));

    let needs = Needs {
        tools: true,
        vision: true,
        ..Needs::default()
    };
    let shortfall = choose(&needs, &candidates).expect_err("nothing satisfies both");

    assert_eq!(shortfall.eliminated.get("tools"), Some(&5));
    assert_eq!(shortfall.eliminated.get("vision"), Some(&1));
    // Requirements nobody stated are absent, not zero: their absence is what says "not asked".
    assert!(!shortfall.eliminated.contains_key("reasoning"));
    assert!(!shortfall.eliminated.contains_key("min_context"));
    assert!(
        shortfall.message.contains("`tools`"),
        "the dominant requirement should be named: {}",
        shortfall.message
    );
}

#[test]
fn a_candidate_missing_every_requirement_is_counted_under_none() {
    let candidates = [candidate("a", "nothing", (false, false, false), None, 1)];
    let needs = Needs {
        tools: true,
        vision: true,
        ..Needs::default()
    };
    let shortfall = choose(&needs, &candidates).expect_err("nothing satisfies");
    assert_eq!(shortfall.eliminated.get("tools"), Some(&0));
    assert_eq!(shortfall.eliminated.get("vision"), Some(&0));
    assert!(
        shortfall
            .message
            .contains("relaxing one alone would admit none"),
        "unexpected message: {}",
        shortfall.message
    );
}

#[test]
fn the_shortfall_renders_its_message() {
    let candidates = [candidate("a", "tiny", (true, true, true), Some(1_000), 1)];
    let needs = Needs {
        min_context: Some(100_000),
        ..Needs::default()
    };
    let shortfall = choose(&needs, &candidates).expect_err("the window is too small");
    assert_eq!(shortfall.to_string(), shortfall.message);
    assert_eq!(shortfall.eliminated.get("min_context"), Some(&1));
}

// ---- unknown sizes -----------------------------------------------------------------------------

#[test]
fn an_unknown_window_is_treated_as_passing() {
    // Nothing claimed a window, so nothing refuted the requirement. Excluding the candidate would
    // hide it on the strength of a fact nobody stated; the provider is the only party that can say.
    let candidates = [candidate("a", "unknown", (true, true, true), None, 5)];
    let needs = Needs {
        min_context: Some(1_000_000),
        ..Needs::default()
    };
    let chosen = choose(&needs, &candidates).expect("an unknown window is not a failure");
    assert_eq!(chosen.model, "unknown");
}

#[test]
fn a_stated_window_is_compared_against_the_requirement() {
    let candidates = [
        candidate("a", "small", (true, true, true), Some(8_000), 100),
        candidate("a", "large", (true, true, true), Some(200_000), 1),
    ];
    let needs = Needs {
        min_context: Some(100_000),
        ..Needs::default()
    };
    let chosen = choose(&needs, &candidates).expect("`large` satisfies");
    assert_eq!(chosen.model, "large");
}

// ---- determinism -------------------------------------------------------------------------------

#[test]
fn a_preference_tie_is_broken_by_route_regardless_of_input_order() {
    let forwards = [
        full("zeta", "model", 50),
        full("alpha", "model", 50),
        full("alpha", "another", 50),
    ];
    let backwards: Vec<Candidate> = forwards.iter().rev().cloned().collect();

    let from_forwards = choose(&needs_tools(), &forwards).expect("all satisfy");
    let from_backwards = choose(&needs_tools(), &backwards).expect("all satisfy");
    // The pair is what identifies a route, and the smaller pair is the tie-break: two callers with the
    // same set must route the same way, or the choice depends on catalog assembly order.
    assert_eq!(from_forwards.provider, "alpha");
    assert_eq!(from_forwards.model, "another");
    assert_eq!(from_backwards.model, from_forwards.model);
}

// ---- the route bridge ---------------------------------------------------------------------------

#[test]
fn a_route_table_becomes_candidates() {
    // The selector is only useful if it can be asked about the models a deployment actually has. One route with one
    // model is the shape this build ships, and a bridge that could not handle it would be decorative.
    use girder_core::{Modality, ModelInfo, Route as ProviderRoute, RouteRegistry};

    let mut routes = RouteRegistry::new();
    let _ = routes.register(ProviderRoute {
        name: "deepseek".to_owned(),
        provider: "deepseek".to_owned(),
        display_name: "DeepSeek".to_owned(),
        default_model: Some("deepseek-chat".to_owned()),
        models: vec![ModelInfo {
            id: "deepseek-chat".to_owned(),
            display_name: "DeepSeek Chat".to_owned(),
            context_window: Some(64_000),
            max_tokens: Some(8_192),
            input_modalities: vec![Modality::Text],
            supports_tools: true,
            supports_reasoning: false,
        }],
    });

    let candidates = candidates_from_routes(&routes);

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].model, "deepseek-chat");
    assert_eq!(candidates[0].provider, "deepseek");
    assert!(candidates[0].capabilities.tools, "the model declares tools");
    assert!(!candidates[0].capabilities.vision, "text-only input");
}

#[test]
fn a_model_without_tools_is_not_chosen_for_a_request_that_needs_them() {
    // The gap this bridge closes: a request carrying tool definitions used to be sent to whatever model was configured,
    // and the provider answered with a validation error that read like a harness bug.
    use girder_core::{Modality, ModelInfo, ModelNeeds, Route as ProviderRoute, RouteRegistry};

    fn model(id: &str, tools: bool, preference_route: &str) -> ProviderRoute {
        ProviderRoute {
            name: preference_route.to_owned(),
            provider: "p".to_owned(),
            display_name: "P".to_owned(),
            default_model: Some(id.to_owned()),
            models: vec![ModelInfo {
                id: id.to_owned(),
                display_name: id.to_owned(),
                context_window: Some(128_000),
                max_tokens: Some(4_096),
                input_modalities: vec![Modality::Text],
                supports_tools: tools,
                supports_reasoning: false,
            }],
        }
    }

    let mut routes = RouteRegistry::new();
    let _ = routes.register(model("no-tools", false, "a"));
    let _ = routes.register(model("with-tools", true, "b"));

    let candidates = candidates_from_routes(&routes);
    let needs = ModelNeeds {
        tools: true,
        vision: false,
        reasoning: false,
        min_context: None,
    };
    let chosen = choose(&needs, &candidates).expect("one model can call tools");

    assert_eq!(chosen.model, "with-tools");
}
