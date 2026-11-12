//! Tests for reading a provider configuration.
//!
//! The protocol is Anthropic's, so a deployment pointing this at a compatible endpoint should not have to set a variable
//! named after a provider it is not using. The historical names stay first, because existing deployments already set
//! them and a rename would be a breaking change disguised as a convenience.
//!
//! These drive [`DeepSeekConfig::from_values`] rather than the process environment: `std::env::set_var` is unsafe in
//! this toolchain, and a test that mutated it would race every other test in the binary.

use std::collections::BTreeMap;

use girder_core::DeepSeekConfig;

/// A lookup over a fixed map, standing in for the environment.
fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    move |name: &str| map.get(name).cloned()
}

#[test]
fn no_key_means_no_configuration() {
    assert!(DeepSeekConfig::from_values(lookup(&[])).is_none());
}

#[test]
fn a_key_alone_is_enough() {
    let config =
        DeepSeekConfig::from_values(lookup(&[("ANTHROPIC_API_KEY", "sk-x")])).expect("configured");

    // The defaults come from the compiled-in values, so a minimal environment is usable.
    assert_eq!(config.api_key, "sk-x");
    assert!(!config.base_url.is_empty());
    assert!(!config.default_model.is_empty());
}

#[test]
fn the_historical_name_still_works() {
    // A rename would be a breaking change for every deployment that already set this.
    let config = DeepSeekConfig::from_values(lookup(&[("DEEPSEEK_API_KEY", "sk-historical")]))
        .expect("configured");

    assert_eq!(config.api_key, "sk-historical");
}

#[test]
fn the_anthropic_name_is_accepted() {
    // A deployment pointing at a compatible endpoint should not set a variable named after another provider.
    let config = DeepSeekConfig::from_values(lookup(&[("ANTHROPIC_API_KEY", "sk-anthropic")]))
        .expect("configured");

    assert_eq!(config.api_key, "sk-anthropic");
}

#[test]
fn the_historical_names_take_precedence() {
    // Both set is a deployment mid-migration; the one it already had is the one it meant.
    let config = DeepSeekConfig::from_values(lookup(&[
        ("DEEPSEEK_API_KEY", "sk-old"),
        ("ANTHROPIC_API_KEY", "sk-new"),
        ("DEEPSEEK_BASE_URL", "https://old.example"),
        ("ANTHROPIC_BASE_URL", "https://new.example"),
    ]))
    .expect("configured");

    assert_eq!(config.api_key, "sk-old");
    assert_eq!(config.base_url, "https://old.example");
}

#[test]
fn a_base_url_can_be_pointed_elsewhere() {
    let config = DeepSeekConfig::from_values(lookup(&[
        ("ANTHROPIC_API_KEY", "sk-x"),
        ("ANTHROPIC_BASE_URL", "https://compatible.example"),
    ]))
    .expect("configured");

    assert_eq!(config.base_url, "https://compatible.example");
}

#[test]
fn the_model_name_comes_from_whichever_variable_is_set() {
    let config = DeepSeekConfig::from_values(lookup(&[
        ("ANTHROPIC_API_KEY", "sk-x"),
        ("ANTHROPIC_MODEL", "claude-compatible"),
    ]))
    .expect("configured");

    assert_eq!(config.default_model, "claude-compatible");
}

#[test]
fn the_harness_model_variable_wins_over_the_provider_one() {
    // `GIRDER_MODEL` is this build's own name and is what a caller of the harness sets.
    let config = DeepSeekConfig::from_values(lookup(&[
        ("ANTHROPIC_API_KEY", "sk-x"),
        ("GIRDER_MODEL", "chosen"),
        ("ANTHROPIC_MODEL", "fallback"),
    ]))
    .expect("configured");

    assert_eq!(config.default_model, "chosen");
}

#[test]
fn an_empty_key_is_still_a_key() {
    // Whether an empty string counts as configured is the transport's business; this layer only reports what it read.
    let config =
        DeepSeekConfig::from_values(lookup(&[("ANTHROPIC_API_KEY", "")])).expect("configured");

    assert_eq!(config.api_key, "");
}
