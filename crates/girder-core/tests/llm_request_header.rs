//! Tests for `request_header_gap`: the request envelope's canonical form, its equality relation, and
//! the folds that answer "which header is in force".
//!
//! The cases are upstream's (`packages/core/session/tests/request-header.spec.ts`, 147 lines), with
//! the assertions aimed at the side that can fail rather than the side that trivially passes:
//!
//! - Canonicalization is tested by its *effect on equality*, not only by comparing structs. A
//!   `canonical_header` that dropped the tool list entirely would satisfy "empty becomes absent" and
//!   still be a bug; the test that folds two non-empty lists through it pins the populated case.
//! - Equality is tested from both directions of every asymmetric-looking rule: absent-vs-empty tools
//!   equal, absent-vs-empty `stop` different, reordered tools different. Each of those is a place
//!   where a plausible implementation disagrees with the specification, and each assertion names the
//!   reason.
//! - The folds are tested against mixed event slices, because "non-header events are skipped" cannot
//!   fail on a slice that holds nothing but headers. The incremental fold is then asserted to agree
//!   with the batch fold over the same events, which is the property a live session depends on.
//! - JSON fidelity is tested because the header is a *log* type: a canonical header and a header
//!   decoded from its own JSON must compare equal, or a resumed session would believe its request
//!   changed on the first step after restart.
//!
//! The module is included by path rather than through the crate root, so it compiles against the
//! file as written even before `lib.rs` declares it, and its `girder_core::…` imports resolve through
//! the alias the library declares for itself.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

#[path = "../src/llm/request_header.rs"]
pub mod request_header_gap;

use girder_core::{SessionEvent, ToolSchema};
use request_header_gap::{
    AdapterDefaults, EpochHeader, HeaderConfig, HeaderEvent, HeaderFold, HeaderReason,
    HeaderSnapshot, canonical_header, fold_request_header, header_equals,
};
use serde_json::json;

/// The config every fixture starts from, matching upstream's `CONFIG`.
fn config() -> HeaderConfig {
    HeaderConfig::new("mock", "m")
}

/// A tool schema. `description` is a parameter so a "changed description" case is one call.
fn tool(name: &str, description: &str) -> ToolSchema {
    ToolSchema {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters: json!({ "type": "object" }),
    }
}

/// The default fixture tool: named, with the shared description `d`.
fn named(name: &str) -> ToolSchema {
    tool(name, "d")
}

// ---- canonical_header -------------------------------------------------------------------------

#[test]
fn canonical_drops_empty_tools_and_a_default_less_adapter_record() {
    // Upstream: `canonicalHeader({config, adapterDefaults: {}, tools: []})` equals `{config}`.
    let header = EpochHeader {
        config: config(),
        adapter_defaults: Some(AdapterDefaults::none()),
        tools: Some(vec![]),
    };
    let canonical = canonical_header(&header);
    assert_eq!(canonical, EpochHeader::new(config()));
    assert!(canonical.adapter_defaults.is_none());
    assert!(canonical.tools.is_none());
}

#[test]
fn canonical_preserves_populated_tools_and_a_populated_adapter_record() {
    let header = EpochHeader {
        config: config().with_max_tokens(256_000),
        adapter_defaults: Some(AdapterDefaults {
            reasoning_effort: false,
            max_tokens: true,
        }),
        tools: Some(vec![named("a")]),
    };
    let canonical = canonical_header(&header);
    assert_eq!(canonical, header);
    assert_eq!(canonical.tool_slice().len(), 1);
    assert_eq!(
        canonical.adapter_defaults,
        Some(AdapterDefaults {
            reasoning_effort: false,
            max_tokens: true,
        })
    );
}

#[test]
fn canonical_keeps_a_record_with_only_the_reasoning_flag() {
    // The two flags are independent: dropping a record that carries just one would lose the
    // provenance of that one field.
    let header = EpochHeader::new(config()).with_adapter_defaults(AdapterDefaults {
        reasoning_effort: true,
        max_tokens: false,
    });
    assert_eq!(
        canonical_header(&header).adapter_defaults,
        header.adapter_defaults
    );
}

#[test]
fn canonical_leaves_omitted_optionals_absent() {
    // Rule 3: an omitted temperature is not filled with a provider default. If it were, the log
    // would claim the caller chose a number nobody chose.
    let canonical = canonical_header(&EpochHeader::new(config()));
    assert_eq!(canonical.config.temperature, None);
    assert_eq!(canonical.config.max_tokens, None);
    assert_eq!(canonical.config.stop, None);
    assert_eq!(canonical.config.reasoning_effort, None);
}

#[test]
fn canonical_is_idempotent() {
    for header in [
        EpochHeader::new(config()),
        EpochHeader::new(config()).with_tools(vec![named("a"), named("b")]),
        EpochHeader {
            config: config()
                .with_temperature(0.2)
                .with_stop(vec!["\n".to_owned()]),
            adapter_defaults: Some(AdapterDefaults {
                reasoning_effort: true,
                max_tokens: true,
            }),
            tools: Some(vec![]),
        },
    ] {
        let once = canonical_header(&header);
        let twice = canonical_header(&once);
        assert_eq!(once, twice, "canonicalization must be a fixed point");
    }
}

#[test]
fn canonical_does_not_reorder_tools() {
    // Order is the model's view, so canonicalization may not touch it: sorting would silently make
    // two different requests compare equal.
    let header = EpochHeader::new(config()).with_tools(vec![named("b"), named("a")]);
    let canonical = canonical_header(&header);
    let names: Vec<&str> = canonical
        .tool_slice()
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(names, ["b", "a"]);
}

// ---- header_equals ----------------------------------------------------------------------------

#[test]
fn equals_every_canonical_field_and_preserves_tool_order() {
    let base = canonical_header(&EpochHeader::new(config()).with_tools(vec![named("a")]));

    // Identical headers are equal.
    assert!(header_equals(&base, &base.clone()));

    // A different model is a different request.
    assert!(!header_equals(
        &base,
        &EpochHeader::new(HeaderConfig::new("mock", "other")).with_tools(vec![named("a")])
    ));

    // A different reasoning effort is a different request.
    assert!(!header_equals(
        &base,
        &EpochHeader::new(config().with_reasoning_effort("high")).with_tools(vec![named("a")])
    ));

    // An adapter-materialized token cap differs from a caller-proposed one, even at the same value:
    // who chose the number is part of the configuration's identity.
    let proposed = EpochHeader::new(config().with_max_tokens(256_000)).with_tools(vec![named("a")]);
    let materialized = EpochHeader {
        config: config().with_max_tokens(256_000),
        adapter_defaults: Some(AdapterDefaults {
            reasoning_effort: false,
            max_tokens: true,
        }),
        tools: Some(vec![named("a")]),
    };
    assert!(!header_equals(&proposed, &materialized));

    // A different description is a different tool.
    assert!(!header_equals(
        &base,
        &EpochHeader::new(config()).with_tools(vec![tool("a", "changed")])
    ));

    // Removing the tools is a change: `base` offers one tool, the other offers none.
    assert!(!header_equals(&base, &EpochHeader::new(config())));

    // Reordering the same two tools is a change.
    let forward = EpochHeader::new(config()).with_tools(vec![named("a"), named("b")]);
    let reversed = EpochHeader::new(config()).with_tools(vec![named("b"), named("a")]);
    assert!(!header_equals(&forward, &reversed));
}

#[test]
fn equals_treats_absent_and_empty_tools_as_the_same_request() {
    // The two representations of a tool-less request must be equal, from both sides: a loop that
    // compared them as different would log a `request/header` on every step of every tool-less turn.
    let absent = EpochHeader::new(config());
    let empty = EpochHeader::new(config()).with_tools(vec![]);
    assert!(header_equals(&absent, &empty));
    assert!(header_equals(&empty, &absent));
    assert!(header_equals(&absent, &absent));
}

#[test]
fn equals_compares_sampling_scalars_that_change_nothing_else() {
    // The four sampling fields are each independently load-bearing — a loop that only compared
    // provider and model would refuse to log a temperature change, and a replay would then show the
    // old temperature.
    let base = EpochHeader::new(config());
    assert!(!header_equals(
        &base,
        &EpochHeader::new(config().with_temperature(0.7))
    ));
    assert!(!header_equals(
        &base,
        &EpochHeader::new(config().with_max_tokens(1_024))
    ));
    assert!(!header_equals(
        &base,
        &EpochHeader::new(config().with_stop(vec!["\n".to_owned()]))
    ));
    assert!(header_equals(
        &EpochHeader::new(config().with_temperature(0.7)),
        &EpochHeader::new(config().with_temperature(0.7))
    ));
}

#[test]
fn equals_distinguishes_an_absent_stop_list_from_an_empty_one() {
    // Unlike `tools`, `stop: []` is a statement ("no stop sequences") and absence is silence.
    let absent = EpochHeader::new(config());
    let empty = EpochHeader::new(config().with_stop(vec![]));
    assert!(!header_equals(&absent, &empty));
    assert!(!header_equals(&empty, &absent));
    assert!(header_equals(
        &empty,
        &EpochHeader::new(config().with_stop(vec![]))
    ));
}

#[test]
fn equals_compares_stop_elements_in_order() {
    let one = EpochHeader::new(config().with_stop(vec!["a".to_owned(), "b".to_owned()]));
    let same = EpochHeader::new(config().with_stop(vec!["a".to_owned(), "b".to_owned()]));
    let reversed = EpochHeader::new(config().with_stop(vec!["b".to_owned(), "a".to_owned()]));
    let shorter = EpochHeader::new(config().with_stop(vec!["a".to_owned()]));
    assert!(header_equals(&one, &same));
    assert!(!header_equals(&one, &reversed));
    assert!(!header_equals(&one, &shorter));
}

#[test]
fn equals_follows_ieee_for_temperature() {
    // JavaScript's `!==`: NaN is always a change, and the sign of zero is not.
    let nan = EpochHeader::new(config().with_temperature(f32::NAN));
    assert!(
        !header_equals(&nan, &nan.clone()),
        "NaN must not equal itself"
    );
    let negative_zero = EpochHeader::new(config().with_temperature(-0.0));
    let positive_zero = EpochHeader::new(config().with_temperature(0.0));
    assert!(header_equals(&negative_zero, &positive_zero));
}

#[test]
fn equals_compares_schemas_by_canonical_json_not_key_order() {
    // Two spellings of one JSON Schema are one schema. `JSON.stringify` would call these different
    // when the map backend preserves insertion order; the canonical encoder sorts, so they agree.
    let left = ToolSchema {
        name: "a".to_owned(),
        description: "d".to_owned(),
        parameters: json!({ "type": "object", "required": ["x"], "properties": { "x": { "type": "string" } } }),
    };
    let right = ToolSchema {
        name: "a".to_owned(),
        description: "d".to_owned(),
        parameters: json!({ "properties": { "x": { "type": "string" } }, "required": ["x"], "type": "object" }),
    };
    assert!(header_equals(
        &EpochHeader::new(config()).with_tools(vec![left]),
        &EpochHeader::new(config()).with_tools(vec![right]),
    ));
}

#[test]
fn equals_distinguishes_a_nested_schema_change() {
    let base = ToolSchema {
        name: "a".to_owned(),
        description: "d".to_owned(),
        parameters: json!({ "type": "object", "properties": { "x": { "type": "string" } } }),
    };
    let changed = ToolSchema {
        name: "a".to_owned(),
        description: "d".to_owned(),
        parameters: json!({ "type": "object", "properties": { "x": { "type": "number" } } }),
    };
    assert!(!header_equals(
        &EpochHeader::new(config()).with_tools(vec![base]),
        &EpochHeader::new(config()).with_tools(vec![changed]),
    ));
}

#[test]
fn equals_does_not_normalize_numbers() {
    // `1` and `1.0` are different JSON encodings; merging them would be inventing an equality the
    // log never recorded.
    let integer = ToolSchema {
        name: "a".to_owned(),
        description: "d".to_owned(),
        parameters: json!({ "minimum": 1 }),
    };
    let float = ToolSchema {
        name: "a".to_owned(),
        description: "d".to_owned(),
        parameters: json!({ "minimum": 1.0 }),
    };
    assert!(!header_equals(
        &EpochHeader::new(config()).with_tools(vec![integer]),
        &EpochHeader::new(config()).with_tools(vec![float]),
    ));
}

// ---- fold_request_header ----------------------------------------------------------------------

/// A non-header event, so a fold has something it must skip.
fn unrelated() -> SessionEvent {
    SessionEvent::TurnStart { turn: 1 }
}

#[test]
fn fold_returns_none_without_a_snapshot() {
    let event = unrelated();
    assert_eq!(
        fold_request_header(&[HeaderEvent::Other(&event)], None),
        None
    );
}

#[test]
fn fold_returns_the_supplied_baseline_when_no_snapshot_follows() {
    // Upstream asserts identity here: the caller's own value comes back, not a copy that has been
    // quietly canonicalized.
    let baseline = EpochHeader::new(config()).with_tools(vec![named("baseline")]);
    let event = unrelated();
    let folded = fold_request_header(&[HeaderEvent::Other(&event)], Some(&baseline));
    assert_eq!(folded.as_ref(), Some(&baseline));
}

#[test]
fn fold_takes_the_latest_snapshot_and_skips_unrelated_events() {
    let first = HeaderSnapshot::new(
        EpochHeader::new(config()).with_tools(vec![named("first")]),
        HeaderReason::Initial,
    );
    let event = unrelated();
    let second = HeaderSnapshot::new(
        EpochHeader::new(HeaderConfig::new("mock", "other")),
        HeaderReason::Change,
    );
    let events = [
        HeaderEvent::Other(&event),
        HeaderEvent::Snapshot(&first),
        HeaderEvent::Other(&event),
        HeaderEvent::Snapshot(&second),
        HeaderEvent::Other(&event),
    ];
    assert_eq!(
        fold_request_header(&events, None),
        Some(EpochHeader::new(HeaderConfig::new("mock", "other")))
    );
}

#[test]
fn fold_canonicalizes_a_non_canonical_snapshot() {
    // A record written before the canonicalization rule existed must still fold to a usable header,
    // or a resumed session would see a "change" on its first step.
    let snapshot = HeaderSnapshot::new(
        EpochHeader {
            config: config(),
            adapter_defaults: Some(AdapterDefaults::none()),
            tools: Some(vec![]),
        },
        HeaderReason::Resume,
    );
    let folded =
        fold_request_header(&[HeaderEvent::Snapshot(&snapshot)], None).expect("a snapshot");
    assert_eq!(folded, EpochHeader::new(config()));
    assert!(folded.tools.is_none());
}

#[test]
fn fold_ignores_the_snapshot_reason_and_starts_series() {
    // The reason explains the log; it must not change the answer, or a `series` snapshot and a
    // `change` snapshot over the same header would reconstruct differently.
    let header = EpochHeader::new(config()).with_tools(vec![named("a")]);
    let reasons = [
        HeaderReason::Initial,
        HeaderReason::Resume,
        HeaderReason::Change,
        HeaderReason::Series,
    ];
    let mut folded = Vec::new();
    for reason in reasons {
        let snapshot = HeaderSnapshot::new(header.clone(), reason).starting_series();
        let one = fold_request_header(&[HeaderEvent::Snapshot(&snapshot)], None);
        folded.push(one);
    }
    assert!(
        folded.iter().all(|one| *one == folded[0]),
        "the reason must not affect the folded header"
    );
}

#[test]
fn fold_prefixes_composes() {
    // Folding `a` then `b` equals folding `a ++ b`. A live session that folds incrementally depends
    // on exactly this.
    let first = HeaderSnapshot::new(
        EpochHeader::new(config()).with_tools(vec![named("a")]),
        HeaderReason::Initial,
    );
    let second = HeaderSnapshot::new(
        EpochHeader::new(config().with_temperature(0.3)),
        HeaderReason::Change,
    );
    let prefix = [HeaderEvent::Snapshot(&first)];
    let suffix = [HeaderEvent::Snapshot(&second)];
    let both = [
        HeaderEvent::Snapshot(&first),
        HeaderEvent::Snapshot(&second),
    ];

    let stepwise = fold_request_header(&suffix, fold_request_header(&prefix, None).as_ref());
    assert_eq!(stepwise, fold_request_header(&both, None));
}

// ---- HeaderFold -------------------------------------------------------------------------------

#[test]
fn incremental_fold_agrees_with_the_batch_fold() {
    let first = HeaderSnapshot::new(
        EpochHeader::new(config()).with_tools(vec![named("a")]),
        HeaderReason::Initial,
    );
    let event = unrelated();
    let second = HeaderSnapshot::new(
        EpochHeader::new(HeaderConfig::new("mock", "other")).with_tools(vec![named("b")]),
        HeaderReason::Change,
    );
    let events = [
        HeaderEvent::Other(&event),
        HeaderEvent::Snapshot(&first),
        HeaderEvent::Other(&event),
        HeaderEvent::Snapshot(&second),
    ];

    let mut fold = HeaderFold::default();
    assert!(fold.is_empty());
    fold.observe_all(&events);
    assert!(!fold.is_empty());
    assert_eq!(fold.state().cloned(), fold_request_header(&events, None));
}

#[test]
fn incremental_fold_can_continue_from_a_baseline() {
    let baseline = EpochHeader::new(config()).with_tools(vec![named("baseline")]);
    let mut fold = HeaderFold::new(Some(baseline.clone()));
    let event = unrelated();
    fold.observe(HeaderEvent::Other(&event));
    assert_eq!(
        fold.state(),
        Some(&baseline),
        "an unrelated event leaves the baseline alone"
    );

    let snapshot = HeaderSnapshot::new(
        EpochHeader::new(config().with_max_tokens(64)),
        HeaderReason::Change,
    );
    fold.observe(HeaderEvent::Snapshot(&snapshot));
    assert_eq!(fold.state(), Some(&canonical_header(&snapshot.header)));
    assert_eq!(fold.into_state(), Some(canonical_header(&snapshot.header)));
}

#[test]
fn incremental_fold_observes_one_event_at_a_time() {
    let snapshot = HeaderSnapshot::new(EpochHeader::new(config()), HeaderReason::Initial);
    let mut fold = HeaderFold::default();
    fold.observe(HeaderEvent::Snapshot(&snapshot));
    assert_eq!(fold.state(), Some(&EpochHeader::new(config())));
}

// ---- JSON fidelity --------------------------------------------------------------------------

#[test]
fn a_canonical_header_round_trips_through_json_as_an_equal_header() {
    // The header is a log type. If decoding its own encoding produced an unequal header, every
    // resumed session would append a spurious `request/header` on its first step.
    let header = canonical_header(
        &EpochHeader::new(
            config()
                .with_reasoning_effort("high")
                .with_temperature(0.2)
                .with_max_tokens(4_096)
                .with_stop(vec!["\n\n".to_owned()]),
        )
        .with_adapter_defaults(AdapterDefaults {
            reasoning_effort: true,
            max_tokens: true,
        })
        .with_tools(vec![named("a"), named("b")]),
    );
    let text = serde_json::to_string(&header).expect("a header serializes");
    let decoded: EpochHeader = serde_json::from_str(&text).expect("a header deserializes");
    assert_eq!(canonical_header(&decoded), header);
    assert!(header_equals(&header, &decoded));
}

#[test]
fn an_absent_optional_field_is_absent_from_the_json_not_null() {
    // A recorded `null` is indistinguishable from a field that was set to null, and every consumer
    // would then have to treat two spellings of "not set" as one.
    let text = serde_json::to_string(&EpochHeader::new(config())).expect("a header serializes");
    assert!(
        !text.contains("null"),
        "no optional field serializes as null: {text}"
    );
    assert!(!text.contains("tools"));
    assert!(!text.contains("adapterDefaults"));
    assert!(!text.contains("temperature"));
    assert!(text.contains("\"provider\":\"mock\""));
}

#[test]
fn a_snapshot_round_trips_with_its_reason_and_series_flag() {
    let snapshot =
        HeaderSnapshot::new(EpochHeader::new(config()), HeaderReason::Series).starting_series();
    let text = serde_json::to_string(&snapshot).expect("a snapshot serializes");
    assert!(text.contains("\"reason\":\"series\""), "{text}");
    assert!(text.contains("\"startsSeries\":true"), "{text}");
    let decoded: HeaderSnapshot = serde_json::from_str(&text).expect("a snapshot deserializes");
    assert!(decoded.starts_series);
    assert_eq!(decoded.reason, HeaderReason::Series);
}

// ---- what "cross-turn inheritance" actually means ---------------------------------------------

#[test]
fn a_turn_that_changes_nothing_inherits_the_previous_header_silently() {
    // The loop's decision, restated as an assertion over this module's primitive: two turns whose
    // assembled header is identical log exactly one snapshot, and the fold reports that one header
    // for both. This is the property that keeps the log readable — an unchanged header repeated per
    // step would make every real change impossible to find.
    let initial = HeaderSnapshot::new(
        EpochHeader::new(config()).with_tools(vec![named("a")]),
        HeaderReason::Initial,
    );
    let second_turn_header =
        canonical_header(&EpochHeader::new(config()).with_tools(vec![named("a")]));
    let baseline = fold_request_header(&[HeaderEvent::Snapshot(&initial)], None);
    let changed = baseline
        .as_ref()
        .is_some_and(|baseline| !header_equals(baseline, &second_turn_header));
    assert!(!changed, "an identical header must not be logged again");
}

#[test]
fn a_turn_that_adds_a_tool_is_observable_as_a_change() {
    // The other side of the same rule: adding a tool *is* a change, and a replay that reconstructed
    // the second turn's header from the first would show the model a tool it did not have.
    let first_turn = HeaderSnapshot::new(
        EpochHeader::new(config()).with_tools(vec![named("a")]),
        HeaderReason::Initial,
    );
    let second_turn = HeaderSnapshot::new(
        EpochHeader::new(config()).with_tools(vec![named("a"), named("b")]),
        HeaderReason::Change,
    );
    let events = [
        HeaderEvent::Snapshot(&first_turn),
        HeaderEvent::Snapshot(&second_turn),
    ];

    let after_first = fold_request_header(&events[..1], None).expect("one snapshot");
    assert_eq!(after_first.tool_slice().len(), 1);

    let after_second = fold_request_header(&events, None).expect("two snapshots");
    assert_eq!(after_second.tool_slice().len(), 2);
    assert!(!header_equals(&after_first, &after_second));
}

#[test]
fn a_removed_tool_is_observable_as_a_change() {
    // Removal is as much a change as addition, and it is the direction a "does the new set contain
    // the old one" check would miss.
    let with_two = EpochHeader::new(config()).with_tools(vec![named("a"), named("b")]);
    let with_one = EpochHeader::new(config()).with_tools(vec![named("a")]);
    assert!(!header_equals(&with_two, &with_one));
    assert!(!header_equals(&with_one, &with_two));
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod llm {
    pub use super::request_header_gap as request_header;
}
