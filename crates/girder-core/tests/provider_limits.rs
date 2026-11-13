//! Tests for applying a model's own limits to a request.
//!
//! Three levels answer three questions, and the order matters: what this call wants, what this model accepts, and what
//! the deployment configured when it knew neither. A wrong order is invisible until a model rejects a request or
//! silently truncates an answer.

use girder_core::{CallConfig, ConfigTable, DeepSeekConfig, DeepSeekProvider};

fn provider() -> DeepSeekProvider {
    DeepSeekProvider::new(DeepSeekConfig::new("https://example.invalid", "sk-test"))
        .expect("a client builds")
}

fn request(model: &str) -> girder_core::LlmRequest {
    girder_core::LlmRequest {
        model: model.to_owned(),
        messages: Vec::new(),
        tools: Vec::new(),
        system_prompt: None,
        max_tokens: None,
    }
}

#[test]
fn an_unconfigured_model_uses_the_provider_default() {
    let provider = provider();

    let body = provider.body(&request("anything"));

    assert_eq!(body["max_tokens"], provider.config().max_tokens);
}

#[test]
fn a_model_limit_beats_the_provider_default() {
    let mut calls = ConfigTable::new(CallConfig::default());
    calls
        .set(
            "deepseek-*",
            CallConfig {
                max_tokens: Some(4096),
                ..CallConfig::default()
            },
        )
        .expect("a valid pattern");
    let provider = provider().with_calls(calls);

    let body = provider.body(&request("deepseek-chat"));

    assert_eq!(body["max_tokens"], 4096);
}

#[test]
fn a_request_limit_beats_the_model_limit() {
    // The call is the most specific statement of intent, so it wins over both defaults.
    let mut calls = ConfigTable::new(CallConfig::default());
    calls
        .set(
            "deepseek-*",
            CallConfig {
                max_tokens: Some(4096),
                ..CallConfig::default()
            },
        )
        .expect("a valid pattern");
    let provider = provider().with_calls(calls);
    let mut asked = request("deepseek-chat");
    asked.max_tokens = Some(512);

    let body = provider.body(&asked);

    assert_eq!(body["max_tokens"], 512);
}

#[test]
fn an_exact_pattern_beats_a_wildcard() {
    // The longest match rule, exercised through the provider rather than the table alone: a mis-resolution here would
    // apply one model's limits to another.
    let mut calls = ConfigTable::new(CallConfig::default());
    calls
        .set(
            "deepseek-*",
            CallConfig {
                max_tokens: Some(1000),
                ..CallConfig::default()
            },
        )
        .expect("a valid pattern");
    calls
        .set(
            "deepseek-reasoner",
            CallConfig {
                max_tokens: Some(64000),
                ..CallConfig::default()
            },
        )
        .expect("a valid pattern");
    let provider = provider().with_calls(calls);

    assert_eq!(provider.limits("deepseek-reasoner").max_tokens, Some(64000));
    assert_eq!(provider.limits("deepseek-chat").max_tokens, Some(1000));
}

#[test]
fn a_model_with_no_limit_of_its_own_falls_through() {
    // `None` means "nobody said", which is not the same as zero: a limit of zero would ask the provider for an empty
    // answer.
    let mut calls = ConfigTable::new(CallConfig::default());
    calls
        .set(
            "other-*",
            CallConfig {
                max_tokens: Some(4096),
                ..CallConfig::default()
            },
        )
        .expect("a valid pattern");
    let provider = provider().with_calls(calls);

    let body = provider.body(&request("deepseek-chat"));

    assert_eq!(body["max_tokens"], provider.config().max_tokens);
}
