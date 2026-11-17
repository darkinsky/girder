//! Tests for the LLM runtime.
//!
//! The module is included by path because `girder-core`'s crate root does not declare it yet —
//! wiring `pub mod llm_final_gap;` and its `pub use` belongs to the integrator. Under that
//! inclusion the module is a child of *this* test crate, which is why it reaches the crate's types
//! through `girder_core::` re-exports rather than `crate::` paths: those resolve the same way in
//! both builds, so one source file is the single thing under test.
//!
//! # What these tests are for
//!
//! Four claims carry the weight, and each is checked from the side that can fail.
//!
//! The first is that **registration is atomic**. A test that registered `[a, b]` and checked that
//! `b` failed would pass against a table that had already committed `a`. So the atomicity tests
//! register into a runtime that already holds the colliding provider, then assert that the *first*
//! provider is still absent — the state that a half-commit would have changed.
//!
//! The second is that **every refusal raises a code from the shared table**. A `RuntimeError` whose
//! `code()` is not in `llm_extra_gap::FAILURE_CODES` would be invisible to the retry policy and to
//! the settings surface that lists retry choices, and nothing else in the build would notice. The
//! table test is therefore exhaustive over variants rather than a sample.
//!
//! The third is that **a materialized default is recorded as materialized**. The interesting case is
//! not "the adapter supplied a cap"; it is the case where the *callback* supplied one, which must
//! **not** be attributed to the adapter. A test that only checked the adapter case would pass
//! against an implementation that set the flag whenever the value changed.
//!
//! The fourth is that **a prepared call is dispatched once, under its own config**. The changed-config
//! case uses a difference the route cannot see — a different temperature on the same provider and
//! model — because a check that only compared the route would pass it.

#[path = "../src/llm_final_gap.rs"]
mod llm_final_gap;

use std::sync::Arc;

use girder_core::llm_extra_gap::FAILURE_CODES;
use girder_core::request_header_gap::AdapterDefaults;
use girder_core::{RetryMode, RetryPolicy};
use llm_final_gap::{
    Adapter, CallControls, ConfigurableProvider, DiscoveredModel, DiscoveryRequest, InputModality,
    LlmCallConfig, ModelInfo, ModelReasoningInfo, NativeAuth, ProviderInfo, ReasoningEffortInfo,
    ResolvedModelInfo, Runtime, RuntimeError, SystemPromptUpdate, ToolUpdate, call_config_equals,
};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// An adapter that answers a fixed model and records nothing.
///
/// `info` is an *override*: absent, the adapter echoes the provider it was asked about, which is what
/// lets one fixture register several routes at once. The override exists for the tests that need an
/// adapter to answer with metadata that does not describe the route it was registered under.
struct Fixed {
    info: Option<ProviderInfo>,
    model: ResolvedModelInfo,
    policy: Option<RetryPolicy>,
}

impl Fixed {
    fn new(provider: &str, model: &str) -> Self {
        Self {
            info: None,
            model: ResolvedModelInfo::bare(provider, model),
            policy: None,
        }
    }

    fn with_info(mut self, info: ProviderInfo) -> Self {
        self.info = Some(info);
        self
    }

    fn with_model(mut self, model: ResolvedModelInfo) -> Self {
        self.model = model;
        self
    }

    fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = Some(policy);
        self
    }
}

impl Adapter for Fixed {
    fn provider_info(&self, provider: &str) -> ProviderInfo {
        self.info
            .clone()
            .unwrap_or_else(|| ProviderInfo::named(provider))
    }

    fn provider_retry_policy(&self, _provider: &str) -> Option<RetryPolicy> {
        self.policy.clone()
    }

    fn resolve_model(
        &self,
        _provider: &str,
        _model: &str,
    ) -> Result<ResolvedModelInfo, RuntimeError> {
        Ok(self.model.clone())
    }
}

/// An adapter that answers whatever model it is asked about, with no facts.
struct Echo;

impl Adapter for Echo {}

fn fixed(provider: &str, model: &str) -> Arc<dyn Adapter> {
    Arc::new(Fixed::new(provider, model))
}

fn efforts(ids: &[&str], default: Option<&str>) -> ModelReasoningInfo {
    ModelReasoningInfo {
        efforts: ids
            .iter()
            .map(|id| ReasoningEffortInfo {
                id: (*id).to_owned(),
                name: (*id).to_uppercase(),
                description: None,
            })
            .collect(),
        default_effort: default.map(ToOwned::to_owned),
    }
}

fn entry(provider: &str) -> ConfigurableProvider {
    ConfigurableProvider::new(provider, "Display", "llm", vec!["models".to_owned()])
}

// ---------------------------------------------------------------------------------------------
// The code table
// ---------------------------------------------------------------------------------------------

/// Every variant, each in its cheapest construction.
///
/// Exhaustive by hand rather than by macro: a new variant has to be added here, and the compiler
/// says so, which is the property that keeps this list from rotting.
fn every_refusal() -> Vec<RuntimeError> {
    vec![
        RuntimeError::EmptyAdapterRegistration,
        RuntimeError::EmptyProviderName,
        RuntimeError::InvalidAdapterMetadata {
            provider: "p".to_owned(),
        },
        RuntimeError::DuplicateAdapter {
            provider: "p".to_owned(),
        },
        RuntimeError::DisposedAdapterRegistration,
        RuntimeError::DisposedDirectoryRegistration,
        RuntimeError::InvalidDirectoryEntry,
        RuntimeError::EmptySettingsPathSegment {
            provider: "p".to_owned(),
        },
        RuntimeError::EmptyDirectoryRegistration,
        RuntimeError::DuplicateDirectoryEntry {
            provider: "p".to_owned(),
        },
        RuntimeError::EmptyDiscoveryNamespace,
        RuntimeError::DuplicateDiscovery {
            settings_ns: "n".to_owned(),
        },
        RuntimeError::NoDiscovery {
            settings_ns: "n".to_owned(),
        },
        RuntimeError::DiscoveryWithoutRoute,
        RuntimeError::NoAdapter {
            provider: "p".to_owned(),
        },
        RuntimeError::InvalidModelInfo {
            provider: "p".to_owned(),
            model: "m".to_owned(),
        },
        RuntimeError::InvalidModelContext {
            provider: "p".to_owned(),
            model: "m".to_owned(),
        },
        RuntimeError::InvalidModelMaxTokens {
            provider: "p".to_owned(),
            model: "m".to_owned(),
        },
        RuntimeError::InvalidModelReasoning {
            provider: "p".to_owned(),
            model: "m".to_owned(),
        },
        RuntimeError::UnsupportedReasoningEffort {
            provider: "p".to_owned(),
            model: "m".to_owned(),
            effort: "max".to_owned(),
        },
        RuntimeError::InvalidCatalog {
            provider: "p".to_owned(),
        },
        RuntimeError::PreparedCallAlreadyDispatched,
        RuntimeError::PreparedCallConfigChanged,
    ]
}

/// The reason this module exists: fifteen codes that had no producer now have one.
#[test]
fn every_refusal_raises_a_code_from_the_shared_table() {
    let known: Vec<&str> = FAILURE_CODES.to_vec();
    for refusal in every_refusal() {
        let code = refusal.code();
        assert!(
            known.contains(&code),
            "{refusal:?} raises {code:?}, which is not in FAILURE_CODES"
        );
    }
}

/// A code outside the table would be invisible to a retry policy, so the set is also checked for
/// the codes this module *claims* to produce.
#[test]
fn the_produced_codes_are_the_ones_that_had_no_producer() {
    let produced: std::collections::BTreeSet<&str> =
        every_refusal().into_iter().map(|r| r.code()).collect();
    for expected in [
        "INVALID_ADAPTER",
        "DUPLICATE_ADAPTER",
        "REGISTRATION_DISPOSED",
        "INVALID_DIRECTORY",
        "DUPLICATE_DIRECTORY",
        "INVALID_DISCOVERY",
        "DUPLICATE_DISCOVERY",
        "NO_DISCOVERY",
        "NO_ADAPTER",
        "INVALID_CATALOG",
        "INVALID_MODEL_INFO",
        "INVALID_MODEL_CONTEXT",
        "INVALID_MODEL_MAX_TOKENS",
        "INVALID_MODEL_REASONING",
        "UNSUPPORTED_REASONING_EFFORT",
        "INVALID_PREPARED_CALL",
    ] {
        assert!(
            produced.contains(expected),
            "{expected} has no producer here"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Adapter registration
// ---------------------------------------------------------------------------------------------

#[test]
fn registering_an_adapter_lists_it() {
    let mut runtime = Runtime::new();
    let handle = runtime
        .register_adapter(&["alpha", "beta"], fixed("alpha", "m"))
        .expect("registers");
    assert_eq!(handle.providers(), ["alpha", "beta"]);
    assert_eq!(
        runtime.list_providers(),
        vec![ProviderInfo::named("alpha"), ProviderInfo::named("beta")]
    );
}

#[test]
fn registering_no_providers_is_refused() {
    let mut runtime = Runtime::new();
    let error = runtime
        .register_adapter(&[], fixed("p", "m"))
        .expect_err("empty registration");
    assert_eq!(error, RuntimeError::EmptyAdapterRegistration);
    assert_eq!(error.code(), "INVALID_ADAPTER");
}

#[test]
fn a_blank_provider_name_is_refused() {
    let mut runtime = Runtime::new();
    let error = runtime
        .register_adapter(&[""], fixed("p", "m"))
        .expect_err("blank name");
    assert_eq!(error, RuntimeError::EmptyProviderName);
}

/// The adapter answered with an id that is not the one it was registered under. A selector keyed on
/// the id would offer a route the runtime cannot dispatch, so the registration is refused.
#[test]
fn metadata_that_moves_the_id_is_refused() {
    let mut runtime = Runtime::new();
    let adapter: Arc<dyn Adapter> =
        Arc::new(Fixed::new("p", "m").with_info(ProviderInfo::named("other")));
    let error = runtime
        .register_adapter(&["p"], adapter)
        .expect_err("moved id");
    assert_eq!(
        error,
        RuntimeError::InvalidAdapterMetadata {
            provider: "p".to_owned()
        }
    );
}

#[test]
fn metadata_with_a_blank_name_is_refused() {
    let mut runtime = Runtime::new();
    let adapter: Arc<dyn Adapter> = Arc::new(Fixed::new("p", "m").with_info(ProviderInfo {
        id: "p".to_owned(),
        name: "  ".to_owned(),
    }));
    let error = runtime
        .register_adapter(&["p"], adapter)
        .expect_err("blank name");
    assert_eq!(
        error,
        RuntimeError::InvalidAdapterMetadata {
            provider: "p".to_owned()
        }
    );
}

#[test]
fn a_second_adapter_for_a_provider_is_refused() {
    let mut runtime = Runtime::new();
    let _first = runtime
        .register_adapter(&["p"], fixed("p", "m"))
        .expect("first");
    let error = runtime
        .register_adapter(&["p"], fixed("p", "m"))
        .expect_err("duplicate");
    assert_eq!(
        error,
        RuntimeError::DuplicateAdapter {
            provider: "p".to_owned()
        }
    );
}

/// The atomicity claim. `b` is taken, so the registration must install *nothing* — including `a`,
/// which would have succeeded on its own.
#[test]
fn a_failed_registration_installs_none_of_its_routes() {
    let mut runtime = Runtime::new();
    let _held = runtime
        .register_adapter(&["b"], fixed("b", "m"))
        .expect("held");
    let error = runtime
        .register_adapter(&["a", "b"], fixed("a", "m"))
        .expect_err("collides on b");
    assert_eq!(
        error,
        RuntimeError::DuplicateAdapter {
            provider: "b".to_owned()
        }
    );
    assert!(
        runtime.list_providers().iter().all(|info| info.id != "a"),
        "a half-committed registration left `a` behind"
    );
    assert_eq!(runtime.list_providers().len(), 1);
}

#[test]
fn a_repeated_name_within_one_registration_is_refused() {
    let mut runtime = Runtime::new();
    let error = runtime
        .register_adapter(&["a", "a"], fixed("a", "m"))
        .expect_err("repeated");
    assert_eq!(
        error,
        RuntimeError::DuplicateAdapter {
            provider: "a".to_owned()
        }
    );
}

#[test]
fn disposing_a_registration_releases_its_routes() {
    let mut runtime = Runtime::new();
    let mut handle = runtime
        .register_adapter(&["a", "b"], fixed("a", "m"))
        .expect("registers");
    runtime.dispose_adapter(&mut handle);
    assert!(handle.providers().is_empty());
    assert!(handle.is_disposed());
    assert!(runtime.list_providers().is_empty());
}

#[test]
fn disposing_twice_is_the_same_as_once() {
    let mut runtime = Runtime::new();
    let mut handle = runtime
        .register_adapter(&["a"], fixed("a", "m"))
        .expect("registers");
    runtime.dispose_adapter(&mut handle);
    let revision = runtime.revision();
    runtime.dispose_adapter(&mut handle);
    assert_eq!(runtime.revision(), revision, "the second dispose committed");
    assert!(runtime.list_providers().is_empty());
}

/// The other atomicity claim: a replacement that fails keeps the routes the handle already had, so a
/// caller that retried would not lose `b` in the process.
#[test]
fn a_failed_replacement_keeps_the_previous_routes() {
    let mut runtime = Runtime::new();
    let mut handle = runtime
        .register_adapter(&["a", "b"], fixed("a", "m"))
        .expect("registers");
    let _held = runtime
        .register_adapter(&["c"], fixed("c", "m"))
        .expect("held");
    let error = runtime
        .replace_routes(&mut handle, &["a", "c"])
        .expect_err("c is taken");
    assert_eq!(
        error,
        RuntimeError::DuplicateAdapter {
            provider: "c".to_owned()
        }
    );
    assert_eq!(
        handle.providers(),
        ["a", "b"],
        "the replacement was partial"
    );
    assert!(runtime.list_providers().iter().any(|info| info.id == "b"));
}

#[test]
fn replacing_routes_drops_the_ones_that_moved_out() {
    let mut runtime = Runtime::new();
    let mut handle = runtime
        .register_adapter(&["a", "b"], fixed("a", "m"))
        .expect("registers");
    runtime
        .replace_routes(&mut handle, &["a"])
        .expect("replaces");
    assert_eq!(handle.providers(), ["a"]);
    assert_eq!(runtime.list_providers().len(), 1);
}

#[test]
fn a_disposed_registration_cannot_replace_its_routes() {
    let mut runtime = Runtime::new();
    let mut handle = runtime
        .register_adapter(&["a"], fixed("a", "m"))
        .expect("registers");
    runtime.dispose_adapter(&mut handle);
    let error = runtime
        .replace_routes(&mut handle, &["a"])
        .expect_err("disposed");
    assert_eq!(error, RuntimeError::DisposedAdapterRegistration);
    assert_eq!(error.code(), "REGISTRATION_DISPOSED");
}

#[test]
fn every_commit_advances_the_revision() {
    let mut runtime = Runtime::new();
    assert_eq!(runtime.revision(), 0);
    let mut handle = runtime
        .register_adapter(&["a"], fixed("a", "m"))
        .expect("registers");
    assert_eq!(runtime.revision(), 1);
    runtime
        .replace_routes(&mut handle, &["a", "b"])
        .expect("grows");
    assert_eq!(runtime.revision(), 2);
    runtime.dispose_adapter(&mut handle);
    assert_eq!(runtime.revision(), 3);
}

// ---------------------------------------------------------------------------------------------
// Retry policy
// ---------------------------------------------------------------------------------------------

#[test]
fn an_adapter_policy_is_reported() {
    let mut runtime = Runtime::new();
    let policy = RetryPolicy {
        mode: RetryMode::Always,
        max_retries: 9,
        ..RetryPolicy::default()
    };
    let _handle = runtime
        .register_adapter(
            &["p"],
            Arc::new(Fixed::new("p", "m").with_policy(policy.clone())),
        )
        .expect("registers");
    assert_eq!(runtime.provider_retry_policy("p").expect("known"), policy);
}

/// A provider with no opinion gets the runtime default rather than a hand-written copy of it, so the
/// two cannot drift.
#[test]
fn an_adapter_without_a_policy_gets_the_default() {
    let mut runtime = Runtime::new();
    let _handle = runtime
        .register_adapter(&["p"], fixed("p", "m"))
        .expect("registers");
    assert_eq!(
        runtime.provider_retry_policy("p").expect("known"),
        RetryPolicy::default()
    );
}

#[test]
fn an_unregistered_provider_has_no_policy() {
    let runtime = Runtime::new();
    assert_eq!(
        runtime.provider_retry_policy("p").expect_err("absent"),
        RuntimeError::NoAdapter {
            provider: "p".to_owned()
        }
    );
}

// ---------------------------------------------------------------------------------------------
// Model normalization
// ---------------------------------------------------------------------------------------------

#[test]
fn a_well_formed_model_resolves() {
    let mut runtime = Runtime::new();
    let model = ResolvedModelInfo::bare("p", "m")
        .with_context(1_048_576)
        .with_default_max_tokens(8_192);
    let _handle = runtime
        .register_adapter(
            &["p"],
            Arc::new(Fixed::new("p", "m").with_model(model.clone())),
        )
        .expect("registers");
    assert_eq!(
        runtime.resolve_model_info("p", "m").expect("resolves"),
        model
    );
}

#[test]
fn resolving_an_unregistered_provider_names_the_code() {
    let runtime = Runtime::new();
    let error = runtime.resolve_model_info("p", "m").expect_err("absent");
    assert_eq!(
        error,
        RuntimeError::NoAdapter {
            provider: "p".to_owned()
        }
    );
    assert_eq!(error.code(), "NO_ADAPTER");
}

/// The model id moved, not the provider: the answer describes a real model, just not the one asked
/// for. A route dispatch keys on the id, so this has to be refused rather than accepted as a rename.
#[test]
fn a_model_that_answers_with_another_id_is_refused() {
    let mut runtime = Runtime::new();
    let _handle = runtime
        .register_adapter(
            &["p"],
            Arc::new(Fixed::new("p", "m").with_model(ResolvedModelInfo::bare("p", "other"))),
        )
        .expect("registers");
    let error = runtime.resolve_model_info("p", "m").expect_err("moved id");
    assert_eq!(
        error,
        RuntimeError::InvalidModelInfo {
            provider: "p".to_owned(),
            model: "m".to_owned()
        }
    );
}

#[test]
fn a_model_that_answers_with_another_provider_is_refused() {
    let mut runtime = Runtime::new();
    let _handle = runtime
        .register_adapter(
            &["p"],
            Arc::new(Fixed::new("p", "m").with_model(ResolvedModelInfo::bare("q", "m"))),
        )
        .expect("registers");
    let error = runtime
        .resolve_model_info("p", "m")
        .expect_err("moved provider");
    assert_eq!(error.code(), "INVALID_MODEL_INFO");
}

#[test]
fn a_blank_model_name_is_refused() {
    let refused = Runtime::normalize_model_info(
        "p",
        "m",
        ResolvedModelInfo {
            info: ModelInfo::new("p", "m").with_name("   "),
            ..ResolvedModelInfo::bare("p", "m")
        },
    )
    .expect_err("blank name");
    assert_eq!(refused.code(), "INVALID_MODEL_INFO");
}

/// Zero makes compaction divide by nothing, which is why the window is checked separately from the
/// rest of the identity.
#[test]
fn a_zero_context_window_is_refused() {
    let refused =
        Runtime::normalize_model_info("p", "m", ResolvedModelInfo::bare("p", "m").with_context(0))
            .expect_err("zero window");
    assert_eq!(
        refused,
        RuntimeError::InvalidModelContext {
            provider: "p".to_owned(),
            model: "m".to_owned()
        }
    );
}

#[test]
fn a_zero_default_output_cap_is_refused() {
    let refused = Runtime::normalize_model_info(
        "p",
        "m",
        ResolvedModelInfo::bare("p", "m").with_default_max_tokens(0),
    )
    .expect_err("zero cap");
    assert_eq!(refused.code(), "INVALID_MODEL_MAX_TOKENS");
}

/// A cap beyond 2^53 would not survive a JSON round trip, so it is refused rather than truncated.
#[test]
fn an_output_cap_beyond_the_safe_integer_range_is_refused() {
    let refused = Runtime::normalize_model_info(
        "p",
        "m",
        ResolvedModelInfo::bare("p", "m")
            .with_default_max_tokens(girder_core::MAX_SAFE_INTEGER + 1),
    )
    .expect_err("unsafe cap");
    assert_eq!(refused.code(), "INVALID_MODEL_MAX_TOKENS");
    assert_eq!(
        Runtime::normalize_model_info(
            "p",
            "m",
            ResolvedModelInfo::bare("p", "m")
                .with_default_max_tokens(girder_core::MAX_SAFE_INTEGER)
        )
        .expect("the boundary itself is representable")
        .default_max_tokens,
        Some(girder_core::MAX_SAFE_INTEGER)
    );
}

/// "This model reasons" plus "no levels" is a claim the caller cannot dispatch, which is why an
/// empty effort list is refused rather than treated as inheritance.
#[test]
fn a_reasoning_block_with_no_efforts_is_refused() {
    let refused = Runtime::normalize_model_info(
        "p",
        "m",
        ResolvedModelInfo::bare("p", "m").with_reasoning(efforts(&[], None)),
    )
    .expect_err("no efforts");
    assert_eq!(refused.code(), "INVALID_MODEL_REASONING");
}

#[test]
fn a_duplicate_effort_id_is_refused() {
    let refused = Runtime::normalize_model_info(
        "p",
        "m",
        ResolvedModelInfo::bare("p", "m").with_reasoning(efforts(&["low", "low"], None)),
    )
    .expect_err("duplicate");
    assert_eq!(refused.code(), "INVALID_MODEL_REASONING");
}

#[test]
fn an_effort_with_a_blank_name_is_refused() {
    let mut reasoning = efforts(&["low"], None);
    reasoning.efforts[0].name = " ".to_owned();
    let refused = Runtime::normalize_model_info(
        "p",
        "m",
        ResolvedModelInfo::bare("p", "m").with_reasoning(reasoning),
    )
    .expect_err("blank name");
    assert_eq!(refused.code(), "INVALID_MODEL_REASONING");
}

/// A default outside the offered set names an effort that can never be dispatched.
#[test]
fn a_default_effort_outside_the_offered_set_is_refused() {
    let refused = Runtime::normalize_model_info(
        "p",
        "m",
        ResolvedModelInfo::bare("p", "m").with_reasoning(efforts(&["low", "high"], Some("max"))),
    )
    .expect_err("unknown default");
    assert_eq!(refused.code(), "INVALID_MODEL_REASONING");
}

#[test]
fn a_default_effort_inside_the_offered_set_resolves() {
    let resolved = Runtime::normalize_model_info(
        "p",
        "m",
        ResolvedModelInfo::bare("p", "m").with_reasoning(efforts(&["low", "high"], Some("high"))),
    )
    .expect("resolves");
    assert_eq!(
        resolved.reasoning.expect("present").default_effort,
        Some("high".to_owned())
    );
}

// ---------------------------------------------------------------------------------------------
// Catalogs
// ---------------------------------------------------------------------------------------------

#[test]
fn a_well_formed_catalog_is_returned() {
    let models = vec![
        ModelInfo::new("p", "a"),
        ModelInfo::new("p", "b").with_name("B"),
    ];
    assert_eq!(
        Runtime::normalize_catalog("p", &models).expect("valid"),
        models
    );
}

/// A listing row from another provider is an adapter disagreeing with itself, so the whole listing is
/// refused rather than shortened: dropping the row would hide the disagreement.
#[test]
fn a_catalog_row_from_another_provider_refuses_the_listing() {
    let models = vec![ModelInfo::new("p", "a"), ModelInfo::new("q", "b")];
    let error = Runtime::normalize_catalog("p", &models).expect_err("foreign row");
    assert_eq!(
        error,
        RuntimeError::InvalidCatalog {
            provider: "p".to_owned()
        }
    );
}

#[test]
fn a_duplicate_catalog_id_refuses_the_listing() {
    let models = vec![ModelInfo::new("p", "a"), ModelInfo::new("p", "a")];
    assert_eq!(
        Runtime::normalize_catalog("p", &models).expect_err("duplicate"),
        RuntimeError::InvalidCatalog {
            provider: "p".to_owned()
        }
    );
}

#[test]
fn an_empty_catalog_id_or_name_refuses_the_listing() {
    for bad in [
        ModelInfo::new("p", ""),
        ModelInfo {
            name: String::new(),
            ..ModelInfo::new("p", "a")
        },
    ] {
        assert_eq!(
            Runtime::normalize_catalog("p", &[bad]).expect_err("unusable row"),
            RuntimeError::InvalidCatalog {
                provider: "p".to_owned()
            }
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Call resolution
// ---------------------------------------------------------------------------------------------

#[test]
fn an_omitted_output_cap_is_filled_from_the_model() {
    let info = ResolvedModelInfo::bare("p", "m").with_default_max_tokens(8_192);
    let resolved = Runtime::resolve_call_with_info(&LlmCallConfig::new("p", "m"), &info);
    assert!(resolved.unsupported.is_none());
    assert_eq!(resolved.config.max_tokens, Some(8_192));
}

#[test]
fn a_proposed_output_cap_wins_over_the_model_default() {
    let info = ResolvedModelInfo::bare("p", "m").with_default_max_tokens(8_192);
    let resolved =
        Runtime::resolve_call_with_info(&LlmCallConfig::new("p", "m").with_max_tokens(512), &info);
    assert_eq!(resolved.config.max_tokens, Some(512));
}

/// A model that does not reason rejects *any* effort, including one a caller believed universal.
#[test]
fn an_effort_on_a_non_reasoning_model_is_unsupported() {
    let info = ResolvedModelInfo::bare("p", "m");
    let resolved = Runtime::resolve_call_with_info(
        &LlmCallConfig::new("p", "m").with_reasoning_effort("high"),
        &info,
    );
    let error = resolved.into_result().expect_err("no reasoning");
    assert_eq!(
        error,
        RuntimeError::UnsupportedReasoningEffort {
            provider: "p".to_owned(),
            model: "m".to_owned(),
            effort: "high".to_owned()
        }
    );
}

/// The resolved config is still usable after a refusal, which is why `resolve_call_with_info` returns
/// the facts rather than a bare `Result`: a caller reporting the refusal can name the efforts that
/// *are* offered.
#[test]
fn a_refused_effort_still_reports_the_model() {
    let info = ResolvedModelInfo::bare("p", "m")
        .with_default_max_tokens(1_024)
        .with_reasoning(efforts(&["low", "high"], Some("high")));
    let resolved = Runtime::resolve_call_with_info(
        &LlmCallConfig::new("p", "m").with_reasoning_effort("max"),
        &info,
    );
    assert_eq!(resolved.config.max_tokens, Some(1_024));
    assert_eq!(resolved.model_info.id(), "m");
    assert!(resolved.unsupported.is_some());
}

#[test]
fn a_defaulted_effort_is_written_into_the_config() {
    let info =
        ResolvedModelInfo::bare("p", "m").with_reasoning(efforts(&["low", "high"], Some("high")));
    let resolved = Runtime::resolve_call_with_info(&LlmCallConfig::new("p", "m"), &info);
    assert!(resolved.unsupported.is_none());
    assert_eq!(resolved.config.reasoning_effort, Some("high".to_owned()));
}

#[test]
fn a_proposed_effort_that_is_offered_is_kept() {
    let info =
        ResolvedModelInfo::bare("p", "m").with_reasoning(efforts(&["low", "high"], Some("high")));
    let resolved = Runtime::resolve_call_with_info(
        &LlmCallConfig::new("p", "m").with_reasoning_effort("low"),
        &info,
    );
    assert_eq!(resolved.config.reasoning_effort, Some("low".to_owned()));
}

#[test]
fn resolving_a_config_against_the_registry_fills_its_cap() {
    let mut runtime = Runtime::new();
    let model = ResolvedModelInfo::bare("p", "m").with_default_max_tokens(2_048);
    let _handle = runtime
        .register_adapter(&["p"], Arc::new(Fixed::new("p", "m").with_model(model)))
        .expect("registers");
    let config = runtime
        .resolve_call_config(&LlmCallConfig::new("p", "m"))
        .expect("resolves");
    assert_eq!(config.max_tokens, Some(2_048));
}

#[test]
fn resolving_a_config_for_an_unregistered_provider_fails() {
    let runtime = Runtime::new();
    let error = runtime
        .resolve_call_config(&LlmCallConfig::new("p", "m"))
        .expect_err("absent");
    assert_eq!(error.code(), "NO_ADAPTER");
}

// ---------------------------------------------------------------------------------------------
// Call config equality
// ---------------------------------------------------------------------------------------------

#[test]
fn the_same_route_and_controls_are_equal() {
    let left = LlmCallConfig::new("p", "m").with_max_tokens(512);
    assert!(call_config_equals(&left, &left.clone()));
}

#[test]
fn a_different_route_is_not_equal() {
    let left = LlmCallConfig::new("p", "m");
    assert!(!call_config_equals(&left, &LlmCallConfig::new("q", "m")));
    assert!(!call_config_equals(&left, &LlmCallConfig::new("p", "n")));
}

/// A sampling change is as much a different request as a model change, which is the case a
/// route-only comparison would wrongly accept.
#[test]
fn a_different_temperature_is_not_equal() {
    let left = LlmCallConfig::new("p", "m").with_temperature(0.2);
    assert!(!call_config_equals(
        &left,
        &LlmCallConfig::new("p", "m").with_temperature(0.7)
    ));
    assert!(call_config_equals(
        &left,
        &LlmCallConfig::new("p", "m").with_temperature(0.2)
    ));
}

/// `NaN !== NaN`, so a re-proposed `NaN` is always a change — reporting it unchanged would keep a
/// request alive with a value that cannot be dispatched.
#[test]
fn a_nan_temperature_is_always_a_change() {
    let nan = LlmCallConfig::new("p", "m").with_temperature(f32::NAN);
    assert!(!call_config_equals(&nan, &nan.clone()));
}

/// `-0.0 === 0.0`, so the sign of zero is not a sampling decision.
#[test]
fn the_sign_of_zero_is_not_a_change() {
    let negative = LlmCallConfig::new("p", "m").with_temperature(-0.0);
    let positive = LlmCallConfig::new("p", "m").with_temperature(0.0);
    assert!(call_config_equals(&negative, &positive));
}

/// Absence and an empty list differ: `[]` is an explicit "no stop sequences".
#[test]
fn an_absent_stop_list_differs_from_an_empty_one() {
    let absent = LlmCallConfig::new("p", "m");
    let empty = LlmCallConfig {
        stop: Some(Vec::new()),
        ..LlmCallConfig::new("p", "m")
    };
    assert!(!call_config_equals(&absent, &empty));
    assert!(call_config_equals(&empty, &empty.clone()));
}

#[test]
fn stop_lists_compare_element_wise() {
    let one = LlmCallConfig {
        stop: Some(vec!["a".to_owned(), "b".to_owned()]),
        ..LlmCallConfig::new("p", "m")
    };
    let reordered = LlmCallConfig {
        stop: Some(vec!["b".to_owned(), "a".to_owned()]),
        ..LlmCallConfig::new("p", "m")
    };
    assert!(!call_config_equals(&one, &reordered));
    assert!(call_config_equals(&one, &one.clone()));
}

// ---------------------------------------------------------------------------------------------
// Preparing a call
// ---------------------------------------------------------------------------------------------

#[test]
fn preparing_a_call_records_what_the_adapter_materialized() {
    let mut runtime = Runtime::new();
    let model = ResolvedModelInfo::bare("p", "m")
        .with_context(100_000)
        .with_default_max_tokens(4_096)
        .with_reasoning(efforts(&["low", "high"], Some("high")));
    let _handle = runtime
        .register_adapter(&["p"], Arc::new(Fixed::new("p", "m").with_model(model)))
        .expect("registers");
    let prepared = runtime
        .prepare_call(&LlmCallConfig::new("p", "m"))
        .expect("prepares");
    assert_eq!(prepared.config().max_tokens, Some(4_096));
    assert_eq!(prepared.config().reasoning_effort, Some("high".to_owned()));
    assert_eq!(
        prepared.adapter_defaults(),
        AdapterDefaults {
            reasoning_effort: true,
            max_tokens: true
        }
    );
    assert_eq!(prepared.context().expect("context").context_window, 100_000);
}

/// The caller proposed both values, so nothing came from the adapter — and a header written from
/// this call must say so, or a settings page would offer to "restore" what the user chose.
#[test]
fn a_caller_proposed_control_is_not_an_adapter_default() {
    let mut runtime = Runtime::new();
    let model = ResolvedModelInfo::bare("p", "m")
        .with_default_max_tokens(4_096)
        .with_reasoning(efforts(&["low", "high"], Some("high")));
    let _handle = runtime
        .register_adapter(&["p"], Arc::new(Fixed::new("p", "m").with_model(model)))
        .expect("registers");
    let prepared = runtime
        .prepare_call(
            &LlmCallConfig::new("p", "m")
                .with_max_tokens(1_024)
                .with_reasoning_effort("low"),
        )
        .expect("prepares");
    assert_eq!(prepared.adapter_defaults(), AdapterDefaults::none());
    assert_eq!(prepared.config().max_tokens, Some(1_024));
    assert_eq!(prepared.config().reasoning_effort, Some("low".to_owned()));
}

/// The subtle one. The callback chose the effort, not the adapter, so the flag must stay false even
/// though the config changed from "unset" to "set".
#[test]
fn a_callback_chosen_effort_is_not_an_adapter_default() {
    let mut runtime = Runtime::new();
    let model =
        ResolvedModelInfo::bare("p", "m").with_reasoning(efforts(&["low", "high"], Some("high")));
    let _handle = runtime
        .register_adapter(&["p"], Arc::new(Fixed::new("p", "m").with_model(model)))
        .expect("registers");
    let configure = |controls: &CallControls, _model: &ResolvedModelInfo| CallControls {
        reasoning_effort: Some("low".to_owned()),
        ..controls.clone()
    };
    let prepared = runtime
        .prepare_call_with(&LlmCallConfig::new("p", "m"), Some(&configure))
        .expect("prepares");
    assert_eq!(prepared.config().reasoning_effort, Some("low".to_owned()));
    assert!(
        !prepared.adapter_defaults().reasoning_effort,
        "a callback choice was attributed to the adapter"
    );
}

/// The callback sees the normalized model, which is what lets it pick an offered effort.
#[test]
fn the_callback_observes_the_normalized_model() {
    let mut runtime = Runtime::new();
    let model = ResolvedModelInfo::bare("p", "m")
        .with_context(64_000)
        .with_reasoning(efforts(&["low", "high"], None));
    let _handle = runtime
        .register_adapter(&["p"], Arc::new(Fixed::new("p", "m").with_model(model)))
        .expect("registers");
    let configure = |controls: &CallControls, model: &ResolvedModelInfo| {
        let effort = model
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.efforts.last())
            .map(|effort| effort.id.clone());
        CallControls {
            reasoning_effort: effort,
            ..controls.clone()
        }
    };
    let prepared = runtime
        .prepare_call_with(&LlmCallConfig::new("p", "m"), Some(&configure))
        .expect("prepares");
    assert_eq!(prepared.config().reasoning_effort, Some("high".to_owned()));
}

/// The callback is handed the controls only: a closure that could rewrite `provider` would be able to
/// dispatch to a route the caller never selected, so the route is not reachable from it.
#[test]
fn the_callback_cannot_move_the_route() {
    let mut runtime = Runtime::new();
    let _handle = runtime
        .register_adapter(&["p", "q"], fixed("p", "m"))
        .expect("registers");
    let configure = |controls: &CallControls, _model: &ResolvedModelInfo| CallControls {
        temperature: Some(0.5),
        ..controls.clone()
    };
    let prepared = runtime
        .prepare_call_with(&LlmCallConfig::new("p", "m"), Some(&configure))
        .expect("prepares");
    assert_eq!(prepared.config().provider, "p");
    assert_eq!(prepared.config().model, "m");
    assert_eq!(prepared.config().temperature, Some(0.5));
}

#[test]
fn preparing_a_call_for_an_unknown_model_is_refused() {
    let mut runtime = Runtime::new();
    let _handle = runtime
        .register_adapter(
            &["p"],
            Arc::new(Fixed::new("p", "m").with_model(ResolvedModelInfo::bare("p", "other"))),
        )
        .expect("registers");
    let error = runtime
        .prepare_call(&LlmCallConfig::new("p", "m"))
        .expect_err("moved id");
    assert_eq!(error.code(), "INVALID_MODEL_INFO");
}

#[test]
fn preparing_a_call_carries_the_adapter_policy() {
    let mut runtime = Runtime::new();
    let policy = RetryPolicy {
        mode: RetryMode::Always,
        max_retries: 2,
        ..RetryPolicy::default()
    };
    let _handle = runtime
        .register_adapter(
            &["p"],
            Arc::new(Fixed::new("p", "m").with_policy(policy.clone())),
        )
        .expect("registers");
    let prepared = runtime
        .prepare_call(&LlmCallConfig::new("p", "m"))
        .expect("prepares");
    assert_eq!(prepared.retry_policy(), &policy);
}

#[test]
fn a_prepared_call_carries_the_model_modes_and_modalities() {
    let mut runtime = Runtime::new();
    let model = ResolvedModelInfo {
        info: ModelInfo::new("p", "m").with_inputs(vec![InputModality::Text, InputModality::Image]),
        system_prompt_update: Some(SystemPromptUpdate::InHistory),
        tool_update: Some(ToolUpdate::AdditionOnly),
        ..ResolvedModelInfo::bare("p", "m")
    };
    let _handle = runtime
        .register_adapter(&["p"], Arc::new(Fixed::new("p", "m").with_model(model)))
        .expect("registers");
    let prepared = runtime
        .prepare_call(&LlmCallConfig::new("p", "m"))
        .expect("prepares");
    assert_eq!(
        prepared.input_modalities(),
        Some([InputModality::Text, InputModality::Image].as_slice())
    );
    assert_eq!(
        prepared.system_prompt_update(),
        Some(SystemPromptUpdate::InHistory)
    );
    assert_eq!(prepared.tool_update(), Some(ToolUpdate::AdditionOnly));
}

// ---------------------------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------------------------

#[test]
fn a_prepared_call_dispatches_once() {
    let mut runtime = Runtime::new();
    let _handle = runtime
        .register_adapter(&["p"], fixed("p", "m"))
        .expect("registers");
    let mut prepared = runtime
        .prepare_call(&LlmCallConfig::new("p", "m"))
        .expect("prepares");
    assert!(!prepared.is_dispatched());
    prepared
        .begin_dispatch(&prepared.config().clone())
        .expect("first dispatch");
    assert!(prepared.is_dispatched());
    assert_eq!(
        prepared
            .begin_dispatch(&prepared.config().clone())
            .expect_err("second dispatch"),
        RuntimeError::PreparedCallAlreadyDispatched
    );
}

/// The difference the route cannot see. A check that only compared provider and model would accept
/// this and send the stream under controls the header never recorded.
#[test]
fn a_changed_temperature_refuses_dispatch() {
    let mut runtime = Runtime::new();
    let _handle = runtime
        .register_adapter(&["p"], fixed("p", "m"))
        .expect("registers");
    let mut prepared = runtime
        .prepare_call(&LlmCallConfig::new("p", "m").with_temperature(0.2))
        .expect("prepares");
    let edited = LlmCallConfig::new("p", "m").with_temperature(0.9);
    let error = prepared.begin_dispatch(&edited).expect_err("changed");
    assert_eq!(error, RuntimeError::PreparedCallConfigChanged);
    assert_eq!(error.code(), "INVALID_PREPARED_CALL");
    assert!(
        !prepared.is_dispatched(),
        "a refused dispatch burned the call"
    );
    prepared
        .begin_dispatch(&prepared.config().clone())
        .expect("the original config still dispatches");
}

#[test]
fn a_changed_route_refuses_dispatch() {
    let mut runtime = Runtime::new();
    let _handle = runtime
        .register_adapter(&["p", "q"], fixed("p", "m"))
        .expect("registers");
    let mut prepared = runtime
        .prepare_call(&LlmCallConfig::new("p", "m"))
        .expect("prepares");
    let error = prepared
        .begin_dispatch(&LlmCallConfig::new("q", "m"))
        .expect_err("changed route");
    assert_eq!(error, RuntimeError::PreparedCallConfigChanged);
}

// ---------------------------------------------------------------------------------------------
// Configurable providers
// ---------------------------------------------------------------------------------------------

#[test]
fn a_directory_entry_is_listed() {
    let mut runtime = Runtime::new();
    let handle = runtime
        .register_configurable_providers(vec![
            ConfigurableProvider {
                native_auth: Some(NativeAuth {
                    env: vec!["KEY".to_owned()],
                    oauth: true,
                }),
                ..entry("b")
            },
            entry("a"),
        ])
        .expect("registers");
    assert_eq!(handle.providers(), ["b", "a"]);
    let listed = runtime.list_configurable_providers();
    assert_eq!(
        listed
            .iter()
            .map(|entry| entry.provider.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"],
        "the listing is sorted by id rather than by declaration order"
    );
}

#[test]
fn a_directory_registration_with_no_entries_is_refused() {
    let mut runtime = Runtime::new();
    assert_eq!(
        runtime
            .register_configurable_providers(Vec::new())
            .expect_err("empty"),
        RuntimeError::EmptyDirectoryRegistration
    );
}

#[test]
fn a_directory_entry_missing_a_required_name_is_refused() {
    for broken in [
        ConfigurableProvider::new("", "Display", "llm", vec!["a".to_owned()]),
        ConfigurableProvider::new("p", "", "llm", vec!["a".to_owned()]),
        ConfigurableProvider::new("p", "Display", "", vec!["a".to_owned()]),
    ] {
        let mut runtime = Runtime::new();
        assert_eq!(
            runtime
                .register_configurable_providers(vec![broken])
                .expect_err("incomplete"),
            RuntimeError::InvalidDirectoryEntry
        );
    }
}

#[test]
fn an_empty_settings_path_segment_is_refused() {
    let mut runtime = Runtime::new();
    let error = runtime
        .register_configurable_providers(vec![ConfigurableProvider::new(
            "p",
            "Display",
            "llm",
            vec!["models".to_owned(), String::new()],
        )])
        .expect_err("empty segment");
    assert_eq!(
        error,
        RuntimeError::EmptySettingsPathSegment {
            provider: "p".to_owned()
        }
    );
}

#[test]
fn a_duplicate_directory_entry_is_refused() {
    let mut runtime = Runtime::new();
    let _held = runtime
        .register_configurable_providers(vec![entry("p")])
        .expect("held");
    assert_eq!(
        runtime
            .register_configurable_providers(vec![entry("p")])
            .expect_err("duplicate"),
        RuntimeError::DuplicateDirectoryEntry {
            provider: "p".to_owned()
        }
    );
}

/// The atomicity rule again, for the directory: a failed replacement keeps what the handle had.
#[test]
fn a_failed_directory_replacement_keeps_the_previous_entries() {
    let mut runtime = Runtime::new();
    let mut handle = runtime
        .register_configurable_providers(vec![entry("a")])
        .expect("registers");
    let _held = runtime
        .register_configurable_providers(vec![entry("c")])
        .expect("held");
    let error = runtime
        .replace_configurable_providers(&mut handle, vec![entry("a"), entry("c")])
        .expect_err("c is taken");
    assert_eq!(
        error,
        RuntimeError::DuplicateDirectoryEntry {
            provider: "c".to_owned()
        }
    );
    assert_eq!(handle.providers(), ["a"]);
}

#[test]
fn disposing_a_directory_registration_releases_its_entries() {
    let mut runtime = Runtime::new();
    let mut handle = runtime
        .register_configurable_providers(vec![entry("a")])
        .expect("registers");
    runtime.dispose_directory(&mut handle);
    assert!(handle.is_disposed());
    assert!(runtime.list_configurable_providers().is_empty());
    assert!(
        runtime
            .replace_configurable_providers(&mut handle, vec![entry("a")])
            .is_err_and(|error| error == RuntimeError::DisposedDirectoryRegistration)
    );
}

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

#[test]
fn discovery_answers_after_registering() {
    let mut runtime = Runtime::new();
    let handle = runtime
        .register_model_discovery("openai")
        .expect("registers");
    assert_eq!(handle.settings_ns(), "openai");
    let found = runtime
        .discover_models(
            "openai",
            &DiscoveryRequest {
                base_url: Some("https://x/v1".to_owned()),
                ..DiscoveryRequest::default()
            },
            &[DiscoveredModel::new("a"), DiscoveredModel::new("b")],
        )
        .expect("answers");
    assert_eq!(
        found
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[test]
fn an_empty_discovery_namespace_is_refused() {
    let mut runtime = Runtime::new();
    assert_eq!(
        runtime.register_model_discovery("").expect_err("empty"),
        RuntimeError::EmptyDiscoveryNamespace
    );
}

#[test]
fn a_duplicate_discovery_namespace_is_refused() {
    let mut runtime = Runtime::new();
    let _first = runtime.register_model_discovery("ns").expect("first");
    assert_eq!(
        runtime
            .register_model_discovery("ns")
            .expect_err("duplicate"),
        RuntimeError::DuplicateDiscovery {
            settings_ns: "ns".to_owned()
        }
    );
}

#[test]
fn discovery_for_an_unregistered_namespace_is_refused() {
    let runtime = Runtime::new();
    let error = runtime
        .discover_models("ns", &DiscoveryRequest::default(), &[])
        .expect_err("absent");
    assert_eq!(
        error,
        RuntimeError::NoDiscovery {
            settings_ns: "ns".to_owned()
        }
    );
}

/// A request that names neither a route nor a URL has nowhere to ask, which is a refusal rather than
/// an empty answer: an empty answer looks like "this endpoint has no models".
#[test]
fn discovery_without_a_route_is_refused() {
    let mut runtime = Runtime::new();
    let _handle = runtime.register_model_discovery("ns").expect("registers");
    let error = runtime
        .discover_models("ns", &DiscoveryRequest::default(), &[])
        .expect_err("no route");
    assert_eq!(error, RuntimeError::DiscoveryWithoutRoute);
    assert_eq!(error.code(), "INVALID_DISCOVERY");
}

#[test]
fn a_blank_provider_counts_as_naming_nowhere() {
    let mut runtime = Runtime::new();
    let _handle = runtime.register_model_discovery("ns").expect("registers");
    assert_eq!(
        runtime
            .discover_models(
                "ns",
                &DiscoveryRequest {
                    provider: Some(String::new()),
                    ..DiscoveryRequest::default()
                },
                &[]
            )
            .expect_err("blank provider"),
        RuntimeError::DiscoveryWithoutRoute
    );
}

/// A discovery answer is a suggestion list, so an unusable row is dropped rather than refusing the
/// whole answer — the opposite of a catalog, where a bad row means the adapter disagrees with itself.
#[test]
fn an_unusable_discovery_row_is_dropped_rather_than_refused() {
    let mut runtime = Runtime::new();
    let _handle = runtime.register_model_discovery("ns").expect("registers");
    let found = runtime
        .discover_models(
            "ns",
            &DiscoveryRequest {
                provider: Some("p".to_owned()),
                ..DiscoveryRequest::default()
            },
            &[
                DiscoveredModel::new("a"),
                DiscoveredModel::new(""),
                DiscoveredModel::new("a"),
                DiscoveredModel::new("b"),
            ],
        )
        .expect("answers");
    assert_eq!(
        found
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
}

#[test]
fn disposing_a_discovery_registration_releases_the_namespace() {
    let mut runtime = Runtime::new();
    let mut handle = runtime.register_model_discovery("ns").expect("registers");
    runtime.dispose_model_discovery(&mut handle);
    assert!(handle.is_disposed());
    assert_eq!(
        runtime
            .discover_models("ns", &DiscoveryRequest::default(), &[])
            .expect_err("released"),
        RuntimeError::NoDiscovery {
            settings_ns: "ns".to_owned()
        }
    );
    runtime
        .register_model_discovery("ns")
        .expect("the namespace is free again");
}

// ---------------------------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------------------------

#[test]
fn modality_spellings_round_trip() {
    for modality in [InputModality::Text, InputModality::Image] {
        assert_eq!(InputModality::parse(modality.as_str()), Some(modality));
    }
    assert_eq!(InputModality::parse("audio"), None);
}

#[test]
fn the_mode_spellings_match_the_wire() {
    assert_eq!(SystemPromptUpdate::InHistory.as_str(), "in-history");
    assert_eq!(ToolUpdate::InHistory.as_str(), "in-history");
    assert_eq!(ToolUpdate::AdditionOnly.as_str(), "addition-only");
}

/// An adapter that only streams — the common case — is three lines, because every method has a
/// default that is already consistent with the validation ladder.
#[test]
fn an_adapter_can_implement_nothing() {
    let mut runtime = Runtime::new();
    let _handle = runtime
        .register_adapter(&["p"], Arc::new(Echo))
        .expect("registers");
    let resolved = runtime.resolve_model_info("p", "m").expect("resolves");
    assert_eq!(resolved.info, ModelInfo::new("p", "m"));
    assert_eq!(
        runtime.provider_retry_policy("p").expect("known"),
        RetryPolicy::default()
    );
}

#[test]
fn a_partial_route_is_not_a_route() {
    assert!(!DiscoveryRequest::default().has_route());
    assert!(
        DiscoveryRequest {
            base_url: Some("https://x".to_owned()),
            ..DiscoveryRequest::default()
        }
        .has_route()
    );
}
