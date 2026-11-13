//! Tests for per-model call configuration.
//!
//! The interesting behaviour is not that a table returns what was put into it, but that two rules
//! can both match one model: which one wins decides the deadline a request actually runs under, so
//! most of these tests are about precedence.

use std::time::Duration;

use girder_core::{CallConfig, CallConfigError as ConfigError, ConfigTable};

/// A configuration whose two deadlines are easy to tell apart in an assertion.
fn config(timeout_secs: u64) -> CallConfig {
    CallConfig {
        max_tokens: Some(1024),
        temperature: Some(0.0),
        timeout: Duration::from_secs(timeout_secs),
        first_token_timeout: Duration::from_secs(1),
        retries: 0,
    }
}

/// A table whose default is distinguishable from every configuration the tests install.
fn table() -> ConfigTable {
    ConfigTable::new(config(100))
}

// ---- defaults ---------------------------------------------------------------------------------

#[test]
fn the_default_leaves_sampling_and_limits_to_the_provider() {
    // `None` rather than a number: a copy of the provider's default here would drift from it, and
    // a request that omits the field is what the provider's own default is for.
    let default = CallConfig::default();

    assert_eq!(default.max_tokens, None);
    assert_eq!(default.temperature, None);
}

#[test]
fn the_default_reports_a_stall_long_before_the_total_deadline() {
    // The whole point of the second field: a hung provider is reported at 20s, not at two minutes.
    let default = CallConfig::default();

    assert_eq!(default.timeout, Duration::from_mins(2));
    assert_eq!(default.first_token_timeout, Duration::from_secs(20));
    assert!(default.first_token_timeout < default.timeout);
}

#[test]
fn the_default_retry_budget_matches_the_retry_policy() {
    // `RetryPolicy::default().max_retries` is 5 as well. Two layers that both count retries must
    // agree on what the number means, or the worst case is the product of two surprises.
    assert_eq!(CallConfig::default().retries, 5);
}

#[test]
fn an_empty_table_answers_with_its_default() {
    let table = ConfigTable::new(config(30));

    assert!(table.is_empty());
    assert_eq!(table.len(), 0);
    assert_eq!(table.resolve("deepseek-chat"), &config(30));
}

// ---- matching ---------------------------------------------------------------------------------

#[test]
fn an_exact_pattern_matches_only_that_model() {
    let mut table = table();
    table.set("deepseek-chat", config(10)).unwrap();

    assert_eq!(table.resolve("deepseek-chat"), &config(10));
    assert_eq!(table.resolve("deepseek-reasoner"), &config(100));
}

#[test]
fn a_trailing_star_matches_every_model_with_that_prefix() {
    let mut table = table();
    table.set("deepseek-*", config(10)).unwrap();

    assert_eq!(table.resolve("deepseek-chat"), &config(10));
    assert_eq!(table.resolve("deepseek-reasoner"), &config(10));
    assert_eq!(table.resolve("deepseek-"), &config(10));
    assert_eq!(table.resolve("gpt-4o"), &config(100));
}

#[test]
fn a_star_matches_across_dashes_and_a_bare_star_matches_everything() {
    let mut table = table();
    table.set("deepseek-*-pro", config(10)).unwrap();
    assert_eq!(table.resolve("deepseek-chat-pro"), &config(10));
    assert_eq!(table.resolve("deepseek-a-b-pro"), &config(10));
    assert_eq!(table.resolve("deepseek-pro"), &config(100));

    table.set("*", config(20)).unwrap();
    assert_eq!(table.resolve("anything"), &config(20));
    assert_eq!(table.resolve("deepseek-pro"), &config(20));
    assert_eq!(table.resolve("deepseek-chat-pro"), &config(10));
}

#[test]
fn a_pattern_without_a_star_is_not_a_prefix() {
    // `deepseek` names one model, not the family. A deployment that wants the family writes
    // `deepseek-*`; treating a bare word as a prefix would make the two indistinguishable.
    let mut table = table();
    table.set("deepseek", config(10)).unwrap();

    assert_eq!(table.resolve("deepseek"), &config(10));
    assert_eq!(table.resolve("deepseek-chat"), &config(100));
}

#[test]
fn a_non_ascii_model_name_matches_its_glob() {
    // Matching is byte-wise; a multi-byte character must not be split by the `*` backtracking.
    let mut table = table();
    table.set("模型-*", config(10)).unwrap();

    assert_eq!(table.resolve("模型-聊天"), &config(10));
    assert_eq!(table.resolve("other"), &config(100));
}

// ---- precedence -------------------------------------------------------------------------------

#[test]
fn an_exact_pattern_beats_a_wildcard_however_short_the_wildcard_is() {
    // The core rule. `deepseek-*` has more literal characters than `deepseek-chat`? No — but even
    // if it did, an exact rule is about that model specifically and must win.
    let mut table = table();
    table.set("deepseek-*", config(10)).unwrap();
    table.set("deepseek-chat", config(20)).unwrap();

    assert_eq!(table.resolve("deepseek-chat"), &config(20));
    assert_eq!(table.resolve("deepseek-reasoner"), &config(10));
}

#[test]
fn the_order_of_setting_does_not_decide_between_exact_and_wildcard() {
    let mut table = table();
    table.set("deepseek-chat", config(20)).unwrap();
    table.set("deepseek-*", config(10)).unwrap();

    assert_eq!(table.resolve("deepseek-chat"), &config(20));
}

#[test]
fn the_more_literal_wildcard_pattern_wins() {
    let mut table = table();
    table.set("deepseek-*", config(10)).unwrap();
    table.set("deepseek-chat-*", config(20)).unwrap();
    table.set("*", config(30)).unwrap();

    assert_eq!(table.resolve("deepseek-chat-max"), &config(20));
    assert_eq!(table.resolve("deepseek-reasoner"), &config(10));
    assert_eq!(table.resolve("gpt-4o"), &config(30));
}

#[test]
fn the_more_literal_wildcard_wins_regardless_of_setting_order() {
    let mut table = table();
    table.set("*", config(30)).unwrap();
    table.set("deepseek-chat-*", config(20)).unwrap();
    table.set("deepseek-*", config(10)).unwrap();

    assert_eq!(table.resolve("deepseek-chat-max"), &config(20));
}

#[test]
fn equally_specific_patterns_are_broken_by_which_was_set_last() {
    // `a*` and `*b` both match `ab` and both have exactly one literal character, so neither the
    // exact-match rule nor the specificity rule separates them. The later rule wins, so a reloaded
    // config takes effect and the answer never depends on iteration order.
    let mut table = table();
    table.set("a*", config(10)).unwrap();
    table.set("*b", config(20)).unwrap();
    assert_eq!(table.resolve("ab"), &config(20));

    table.set("a*", config(30)).unwrap();
    assert_eq!(table.resolve("ab"), &config(30));
}

#[test]
fn setting_a_pattern_again_replaces_it_instead_of_duplicating_it() {
    let mut table = table();
    table.set("deepseek-*", config(10)).unwrap();
    table.set("deepseek-*", config(40)).unwrap();

    assert_eq!(table.len(), 1);
    assert_eq!(table.resolve("deepseek-chat"), &config(40));
}

#[test]
fn a_model_no_rule_matches_falls_back_to_the_default() {
    let mut table = table();
    table.set("deepseek-*", config(10)).unwrap();

    assert_eq!(table.resolve(""), &config(100));
    assert_eq!(table.default_config(), &config(100));
}

// ---- validation -------------------------------------------------------------------------------

#[test]
fn a_first_token_timeout_equal_to_the_total_timeout_is_rejected() {
    // It could never fire: the total deadline always wins, so storing it would leave the stall it
    // exists to catch unreported until two minutes had passed.
    let mut table = table();
    let mut too_slow = config(30);
    too_slow.first_token_timeout = Duration::from_secs(30);

    assert_eq!(
        table.set("deepseek-*", too_slow),
        Err(ConfigError::FirstTokenNotShorter {
            timeout: Duration::from_secs(30),
            first_token_timeout: Duration::from_secs(30),
        })
    );
    assert!(table.is_empty());
}

#[test]
fn a_first_token_timeout_longer_than_the_total_timeout_is_rejected() {
    let mut table = table();
    let mut too_slow = config(30);
    too_slow.first_token_timeout = Duration::from_secs(31);

    assert!(matches!(
        table.set("deepseek-*", too_slow),
        Err(ConfigError::FirstTokenNotShorter { .. })
    ));
}

#[test]
fn a_zero_timeout_is_rejected() {
    let mut table = table();

    let mut no_total = config(30);
    no_total.timeout = Duration::ZERO;
    assert_eq!(
        table.set("a", no_total),
        Err(ConfigError::ZeroTimeout { field: "timeout" })
    );

    let mut no_first = config(30);
    no_first.first_token_timeout = Duration::ZERO;
    assert_eq!(
        table.set("a", no_first),
        Err(ConfigError::ZeroTimeout {
            field: "first_token_timeout"
        })
    );
}

#[test]
fn a_non_finite_temperature_is_rejected() {
    // NaN and infinity cannot be serialized as JSON; rejecting them here points at the config
    // rather than at a request body that failed to build.
    let mut table = table();

    let mut nan = config(30);
    nan.temperature = Some(f32::NAN);
    assert_eq!(table.set("a", nan), Err(ConfigError::NonFiniteTemperature));

    let mut infinite = config(30);
    infinite.temperature = Some(f32::INFINITY);
    assert_eq!(
        table.set("a", infinite),
        Err(ConfigError::NonFiniteTemperature)
    );
}

#[test]
fn an_empty_pattern_is_rejected() {
    // An empty pattern matches only the empty model name, so it would be a rule nobody can hit.
    let mut table = table();

    assert_eq!(table.set("", config(10)), Err(ConfigError::EmptyPattern));
    assert!(table.is_empty());
}

// ---- glob semantics --------------------------------------------------------------------------

#[test]
fn adjacent_stars_and_trailing_stars_behave_like_one() {
    // Collapsing them is not required by the matcher, but a config reader should not have to know
    // that `a**b` means `a*b`; these cases pin the behaviour either way.
    let mut doubled = table();
    doubled.set("deepseek-**", config(10)).unwrap();
    assert_eq!(doubled.resolve("deepseek-chat"), &config(10));

    let mut trailing = table();
    trailing.set("*-*", config(20)).unwrap();
    assert_eq!(trailing.resolve("a-b"), &config(20));
    assert_eq!(trailing.resolve("a"), &config(100));
}
