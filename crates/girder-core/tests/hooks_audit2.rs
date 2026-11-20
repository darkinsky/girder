//! Tests for the durable subagent descriptor and the projections folded from it.
//!
//! Three families of property are pinned here, each corresponding to a decision the module docs
//! argue for:
//!
//! - **Schema strictness.** A current-version payload that carries a field its mode does not admit, or
//!   a `toolFilter` that constrains nothing, is refused. An *unknown* version is not a refusal — it is
//!   `Ok(None)`, "this build cannot classify the child" — and the difference is what lets a downgraded
//!   harness read a newer log without calling it corrupt.
//! - **Identity resets, timing resets.** A fork seed replays an ancestor's descriptor; both folds must
//!   end up describing the child and not the ancestor. The label a client shows is additionally gated
//!   on the descriptor being in the child's own suffix, because a projection can hold an inherited one.
//! - **Archive admission.** Running descendants are selected through `subagent`-origin ancestry only, an
//!   offline intermediate does not become a hole, and a cycle terminates.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. That is also why nothing here reaches
//! for anything else in `girder_core`.

#[path = "../src/hooks/audit2.rs"]
pub mod audit2_gap;

use audit2_gap::{
    ActiveInterval, ChildAgentView, ChildHeader, DescriptorError, DescriptorMode, IdentityFold,
    KNOWN_FORMAT_V0_DESCRIPTOR_VERSIONS, LoggedEvent, SUBAGENT_DESCRIPTOR_VERSION,
    SubagentDescriptor, SubagentIdentity, TimingFold, ToolRestriction, activity_family,
    describe_child, fold_subagent_descriptor, header_ancestors, parse_subagent_descriptor,
    running_descendants, snapshot_subagent_descriptor,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// A continuable descriptor payload at the current version, with every optional field set.
fn full_continuable() -> Value {
    json!({
        "version": SUBAGENT_DESCRIPTOR_VERSION,
        "mode": "continuable",
        "provider": "spawn",
        "label": "child",
        "agentProvider": "deepseek",
        "agentModel": "deepseek-v4.1-flash",
        "agentReasoningEffort": "high",
        "persona": "reviewer",
        "toolFilter": { "allow": ["read", "grep"], "deny": ["write"] },
    })
}

/// A one-shot descriptor payload at the current version.
fn one_shot(label: Option<&str>) -> Value {
    let mut payload = json!({
        "version": SUBAGENT_DESCRIPTOR_VERSION,
        "mode": "one-shot",
        "provider": "spawn",
    });
    if let Some(label) = label {
        payload["label"] = Value::from(label);
    }
    payload
}

/// A log row of the given type at `seq`/`time` with the given payload.
fn event<'a>(event_type: &'a str, seq: u64, time: u64, data: &'a Value) -> LoggedEvent<'a> {
    LoggedEvent {
        event_type,
        seq,
        time,
        data,
    }
}

/// The payload of a `turn/start`, which carries only its turn number.
fn turn_start(turn: u64) -> Value {
    json!({ "turn": turn })
}

/// The payload of a `turn/end` with the given stop reason.
fn turn_end(turn: u64, kind: &str) -> Value {
    json!({ "turn": turn, "reason": { "kind": kind } })
}

// ---------------------------------------------------------------------------------------------
// SUBAGENT_DESCRIPTOR_VERSION and the historical table
// ---------------------------------------------------------------------------------------------

#[test]
fn the_current_version_is_the_one_upstream_writes() {
    // Upstream `descriptor.ts`: `SUBAGENT_DESCRIPTOR_VERSION = 3`. Pinned because it is stamped into
    // every appended descriptor and required verbatim by the fold.
    assert_eq!(SUBAGENT_DESCRIPTOR_VERSION, 3);
}

#[test]
fn the_historical_table_admits_exactly_the_three_released_versions() {
    // Upstream `payload-validation.ts`: `[1, 2, 3]`. A format-v0 log can contain any of them, so a
    // migration that only knew version 3 would declare every earlier child unreadable.
    assert_eq!(KNOWN_FORMAT_V0_DESCRIPTOR_VERSIONS, [1, 2, 3]);
}

// ---------------------------------------------------------------------------------------------
// Parsing: the schema is strict on the current version
// ---------------------------------------------------------------------------------------------

#[test]
fn a_full_continuable_payload_round_trips_every_composition_field() {
    let parsed = parse_subagent_descriptor(&full_continuable())
        .expect("the payload is well formed")
        .expect("version 3 is current");

    let SubagentDescriptor::Continuable {
        provider,
        label,
        agent_provider,
        agent_model,
        agent_reasoning_effort,
        persona,
        tool_filter,
    } = &parsed
    else {
        panic!("expected a continuable descriptor, got {parsed:?}");
    };
    assert_eq!(provider, "spawn");
    assert_eq!(label, "child");
    assert_eq!(agent_provider.as_deref(), Some("deepseek"));
    assert_eq!(agent_model.as_deref(), Some("deepseek-v4.1-flash"));
    assert_eq!(agent_reasoning_effort.as_deref(), Some("high"));
    assert_eq!(persona.as_deref(), Some("reviewer"));
    let filter = tool_filter.as_ref().expect("a tool filter was declared");
    assert_eq!(
        filter.allow.as_deref(),
        Some(&["read".to_owned(), "grep".to_owned()][..])
    );
    assert_eq!(filter.deny.as_deref(), Some(&["write".to_owned()][..]));

    // The rendering omits absent optionals instead of writing `null`: a `null` and an omission are
    // different composition facts on resume.
    assert_eq!(parsed.mode(), DescriptorMode::Continuable);
    assert_eq!(parsed.label(), Some("child"));
    assert_eq!(parsed.to_payload(), full_continuable());
}

#[test]
fn a_one_shot_payload_may_omit_its_label_but_never_carry_composition() {
    // Label absent: allowed, because a terminal run has no durable name to resume under.
    let bare = parse_subagent_descriptor(&one_shot(None))
        .expect("well formed")
        .expect("current version");
    assert_eq!(bare.mode(), DescriptorMode::OneShot);
    assert_eq!(bare.label(), None);

    // A one-shot payload carrying `agentProvider` is a writer bug: it claims resumable composition
    // for a run that cannot be resumed. The refusal names the field, not just the payload.
    let mut with_composition = one_shot(None);
    with_composition["agentProvider"] = Value::from("deepseek");
    assert_eq!(
        parse_subagent_descriptor(&with_composition),
        Err(DescriptorError::UnknownField {
            path: "payload",
            field: "agentProvider".to_owned(),
        })
    );

    // A one-shot payload *is* allowed a label, and it survives.
    let labelled = parse_subagent_descriptor(&one_shot(Some("probe")))
        .expect("well formed")
        .expect("current version");
    assert_eq!(labelled.label(), Some("probe"));
}

#[test]
fn an_unknown_version_is_not_a_defect() {
    // Upstream returns `undefined` here, and the distinction is the module's central claim: a newer
    // build's descriptor says "you cannot classify this child", not "this child is corrupt".
    let mut newer = full_continuable();
    newer["version"] = Value::from(4);
    assert_eq!(parse_subagent_descriptor(&newer), Ok(None));

    let mut older = full_continuable();
    older["version"] = Value::from(2);
    assert_eq!(parse_subagent_descriptor(&older), Ok(None));

    // JSON has one number type, so `3.0` is the same version as `3` — upstream's `!==` comparison
    // accepts both, and so must the port.
    let mut spelled = full_continuable();
    spelled["version"] = json!(3.0);
    assert!(
        parse_subagent_descriptor(&spelled)
            .expect("well formed")
            .is_some()
    );
}

#[test]
fn a_current_version_payload_must_match_its_own_schema() {
    let mut not_an_object = json!(["not", "an", "object"]);
    assert_eq!(
        parse_subagent_descriptor(&not_an_object),
        Err(DescriptorError::PayloadNotAnObject)
    );

    not_an_object = Value::Null;
    assert_eq!(
        parse_subagent_descriptor(&not_an_object),
        Err(DescriptorError::PayloadNotAnObject)
    );

    // A missing version is a version that is not a number, not a schema violation elsewhere.
    assert_eq!(
        parse_subagent_descriptor(&json!({ "mode": "one-shot", "provider": "spawn" })),
        Err(DescriptorError::VersionNotANumber)
    );

    // A non-numeric version is refused rather than compared as text.
    assert_eq!(
        parse_subagent_descriptor(&json!({
            "version": "3", "mode": "one-shot", "provider": "spawn",
        })),
        Err(DescriptorError::VersionNotANumber)
    );

    // Both mode spellings are required; a third is refused.
    let mut bad_mode = full_continuable();
    bad_mode["mode"] = Value::from("later");
    assert_eq!(
        parse_subagent_descriptor(&bad_mode),
        Err(DescriptorError::Mode)
    );

    // A missing mode is the same refusal: the discriminant is not optional.
    let mut no_mode = full_continuable();
    no_mode.as_object_mut().expect("object").remove("mode");
    assert_eq!(
        parse_subagent_descriptor(&no_mode),
        Err(DescriptorError::Mode)
    );

    // `provider` is required on both arms.
    let mut no_provider = one_shot(None);
    no_provider
        .as_object_mut()
        .expect("object")
        .remove("provider");
    assert_eq!(
        parse_subagent_descriptor(&no_provider),
        Err(DescriptorError::ProviderNotAString)
    );

    // `label` is required on the continuable arm specifically.
    let mut no_label = full_continuable();
    no_label.as_object_mut().expect("object").remove("label");
    assert_eq!(
        parse_subagent_descriptor(&no_label),
        Err(DescriptorError::LabelNotAString)
    );

    // An optional field present with the wrong type is refused, not read as absent: "the writer meant
    // nothing" and "the writer meant something unreadable" resume differently.
    let mut wrong_type = full_continuable();
    wrong_type["persona"] = Value::from(7);
    assert_eq!(
        parse_subagent_descriptor(&wrong_type),
        Err(DescriptorError::FieldNotAString {
            field: "persona".to_owned(),
        })
    );

    // An unknown field on a continuable payload is refused for the same reason: dropping it would
    // silently discard composition the writer meant the resumer to apply.
    let mut unknown = full_continuable();
    unknown["maxTokens"] = Value::from(4096);
    assert_eq!(
        parse_subagent_descriptor(&unknown),
        Err(DescriptorError::UnknownField {
            path: "payload",
            field: "maxTokens".to_owned(),
        })
    );
}

#[test]
fn a_tool_filter_must_constrain_something_and_say_only_allow_or_deny() {
    // Not an object.
    let mut not_object = full_continuable();
    not_object["toolFilter"] = Value::from("read");
    assert_eq!(
        parse_subagent_descriptor(&not_object),
        Err(DescriptorError::ToolFilterNotAnObject)
    );

    // An unknown key inside the filter, reported against `toolFilter` rather than `payload`.
    let mut unknown_key = full_continuable();
    unknown_key["toolFilter"] = json!({ "allow": ["read"], "except": ["write"] });
    assert_eq!(
        parse_subagent_descriptor(&unknown_key),
        Err(DescriptorError::UnknownField {
            path: "toolFilter",
            field: "except".to_owned(),
        })
    );

    // An empty object reads as "constrain the tools" while constraining nothing: a resume treating it
    // as "allow all" would grant more than the writer intended.
    let mut empty = full_continuable();
    empty["toolFilter"] = json!({});
    assert_eq!(
        parse_subagent_descriptor(&empty),
        Err(DescriptorError::ToolFilterEmpty)
    );

    // A side that is not an array of strings names the side that failed.
    let mut not_array = full_continuable();
    not_array["toolFilter"] = json!({ "allow": "read" });
    assert_eq!(
        parse_subagent_descriptor(&not_array),
        Err(DescriptorError::ToolFilterNotAnArray {
            key: "allow".to_owned(),
        })
    );

    let mut not_strings = full_continuable();
    not_strings["toolFilter"] = json!({ "deny": ["write", 3] });
    assert_eq!(
        parse_subagent_descriptor(&not_strings),
        Err(DescriptorError::ToolFilterNotAnArray {
            key: "deny".to_owned(),
        })
    );

    // One side alone is a legitimate filter; the accessor confirms neither side is required together.
    let allow_only = parse_subagent_descriptor(&json!({
        "version": SUBAGENT_DESCRIPTOR_VERSION,
        "mode": "continuable",
        "provider": "spawn",
        "label": "child",
        "toolFilter": { "allow": [] },
    }))
    .expect("well formed")
    .expect("current version");
    let SubagentDescriptor::Continuable { tool_filter, .. } = allow_only else {
        panic!("expected continuable");
    };
    let filter = tool_filter.expect("filter declared");
    assert_eq!(filter.allow.as_deref(), Some(&[][..]));
    assert_eq!(filter.deny, None);
    assert!(!filter.is_empty());
    assert!(ToolRestriction::default().is_empty());
}

#[test]
fn a_snapshot_returns_the_payload_it_can_read_back() {
    // The snapshot exists so a caller can append exactly what a later fold will accept. The payload it
    // returns therefore has to re-parse to the same descriptor.
    let descriptor = SubagentDescriptor::Continuable {
        provider: "spawn".to_owned(),
        label: "child".to_owned(),
        agent_provider: None,
        agent_model: None,
        agent_reasoning_effort: None,
        persona: Some("reviewer".to_owned()),
        tool_filter: Some(ToolRestriction {
            allow: None,
            deny: Some(vec!["write".to_owned()]),
        }),
    };
    let payload =
        snapshot_subagent_descriptor(&descriptor).expect("the descriptor is serializable");
    assert_eq!(
        parse_subagent_descriptor(&payload).expect("well formed"),
        Some(descriptor)
    );

    // The one-shot arm omits its absent label rather than writing `null`.
    let bare = SubagentDescriptor::OneShot {
        provider: "fork".to_owned(),
        label: None,
    };
    let payload = snapshot_subagent_descriptor(&bare).expect("serializable");
    assert_eq!(
        payload,
        json!({
            "version": SUBAGENT_DESCRIPTOR_VERSION,
            "mode": "one-shot",
            "provider": "fork",
        })
    );
}

// ---------------------------------------------------------------------------------------------
// Folding the descriptor: the first record is authoritative
// ---------------------------------------------------------------------------------------------

#[test]
fn the_first_descriptor_wins_because_exactly_one_is_meant_to_exist() {
    let first = full_continuable();
    let second = json!({
        "version": SUBAGENT_DESCRIPTOR_VERSION,
        "mode": "one-shot",
        "provider": "fork",
    });
    let start = turn_start(1);
    let events = [
        event("turn/start", 1, 0, &start),
        event("subagent/descriptor", 2, 10, &first),
        event("subagent/descriptor", 3, 20, &second),
    ];
    let folded = fold_subagent_descriptor(&events)
        .expect("well formed")
        .expect("the first descriptor is current-version");
    assert_eq!(folded.mode(), DescriptorMode::Continuable);
    assert_eq!(folded.provider(), "spawn");
    assert_eq!(folded.label(), Some("child"));
}

#[test]
fn a_log_without_a_descriptor_folds_to_nothing_and_a_broken_first_one_is_an_error() {
    let start = turn_start(1);
    assert_eq!(
        fold_subagent_descriptor(&[event("turn/start", 1, 0, &start)]),
        Ok(None)
    );

    // A current-version payload that violates its schema is an error, not "no descriptor": the caller
    // has to be able to tell a child it cannot classify from a child whose log is damaged.
    let damaged = json!({ "version": SUBAGENT_DESCRIPTOR_VERSION, "mode": "continuable", "provider": "spawn" });
    assert_eq!(
        fold_subagent_descriptor(&[event("subagent/descriptor", 1, 0, &damaged)]),
        Err(DescriptorError::LabelNotAString)
    );

    // An unknown-version first descriptor stops the fold even when a later one is current: substituting
    // the later record would answer with composition the establishing writer never declared.
    let newer =
        json!({ "version": 4, "mode": "continuable", "provider": "spawn", "label": "child" });
    let current = one_shot(Some("later"));
    assert_eq!(
        fold_subagent_descriptor(&[
            event("subagent/descriptor", 1, 0, &newer),
            event("subagent/descriptor", 2, 5, &current),
        ]),
        Ok(None)
    );
}

// ---------------------------------------------------------------------------------------------
// Identity projection: last-wins, and an invalid payload resets
// ---------------------------------------------------------------------------------------------

#[test]
fn identity_folds_last_wins_so_a_child_overrides_an_inherited_ancestor() {
    let inherited = json!({
        "version": SUBAGENT_DESCRIPTOR_VERSION,
        "mode": "continuable",
        "provider": "fork",
        "label": "ancestor",
    });
    let own = one_shot(Some("child"));
    let mut fold = IdentityFold::new();
    assert_eq!(fold.value(), None);

    fold.apply(&event("subagent/descriptor", 7, 0, &inherited));
    assert_eq!(
        fold.value(),
        Some(&SubagentIdentity::Continuable {
            label: "ancestor".to_owned(),
            seq: 7,
        })
    );

    fold.apply(&event("turn/start", 8, 1, &turn_start(1)));
    assert_eq!(
        fold.value().map(SubagentIdentity::label),
        Some(Some("ancestor")),
        "a non-descriptor event must not disturb the identity"
    );

    fold.apply(&event("subagent/descriptor", 9, 2, &own));
    assert_eq!(
        fold.value(),
        Some(&SubagentIdentity::OneShot {
            label: Some("child".to_owned()),
            seq: 9,
        })
    );
}

#[test]
fn an_unusable_descriptor_resets_identity_instead_of_keeping_the_previous_one() {
    let good = full_continuable();
    let mut fold = IdentityFold::new();
    fold.apply(&event("subagent/descriptor", 4, 0, &good));
    assert!(fold.value().is_some());

    // A malformed current-version payload must not leave the earlier identity in place: that is how a
    // forked child inherits a name it never established. A projection fold never throws, so the reset is
    // `None`.
    let malformed = json!({ "version": SUBAGENT_DESCRIPTOR_VERSION, "mode": "one-shot" });
    fold.apply(&event("subagent/descriptor", 5, 1, &malformed));
    assert_eq!(fold.value(), None);

    // Re-establish, then an unknown version: also a reset, because this build cannot classify the child.
    fold.apply(&event("subagent/descriptor", 6, 2, &good));
    assert!(fold.value().is_some());
    let mut newer = full_continuable();
    newer["version"] = Value::from(4);
    fold.apply(&event("subagent/descriptor", 7, 3, &newer));
    assert_eq!(fold.value(), None);
}

#[test]
fn a_label_is_shown_only_when_the_descriptor_is_in_the_childs_own_suffix() {
    let identity = SubagentIdentity::Continuable {
        label: "child".to_owned(),
        seq: 12,
    };

    // Own suffix: the label may be shown.
    let own = describe_child("session-c", Some(&identity), |seq| seq == 12);
    assert_eq!(own.label.as_deref(), Some("child"));
    assert_eq!(own.id, "session-c");

    // Folding from the inherited prefix: the item falls back to the id alone. Ugly, but a fork listed
    // under its ancestor's name is how a `send` reaches the wrong child.
    let inherited = describe_child("session-c", Some(&identity), |_| false);
    assert_eq!(inherited.label, None);
    assert_eq!(inherited.id, "session-c");

    // No identity at all: same fallback.
    assert_eq!(describe_child("session-d", None, |_| true).label, None);
}

// ---------------------------------------------------------------------------------------------
// Timing projection: the descriptor is the clock's origin
// ---------------------------------------------------------------------------------------------

#[test]
fn a_settled_turn_after_the_descriptor_accumulates_its_duration() {
    let descriptor = full_continuable();
    let mut fold = TimingFold::new();
    fold.apply(&event("subagent/descriptor", 1, 100, &descriptor));
    fold.apply(&event("turn/start", 2, 150, &turn_start(1)));
    assert_eq!(
        fold.view().active,
        Some(ActiveInterval {
            since: 150,
            through: 150
        })
    );

    // An event inside the open turn extends the observed bound without settling anything.
    fold.apply(&event("tool/call", 3, 190, &json!({})));
    assert_eq!(
        fold.view()
            .active
            .map(audit2_gap::ActiveInterval::observed_ms),
        Some(40)
    );
    assert_eq!(fold.view().settled_ms, 0);

    fold.apply(&event("turn/end", 4, 400, &turn_end(1, "completed")));
    let view = fold.view();
    assert_eq!(view.settled_ms, 250);
    assert_eq!(view.active, None);
    assert_eq!(view.last_turn_completed, Some(true));
}

#[test]
fn a_pre_descriptor_turn_is_promoted_only_by_the_childs_own_descriptor() {
    let descriptor = full_continuable();

    // The fork seed opens a turn before the child's descriptor exists: it is pending, not active.
    let mut fold = TimingFold::new();
    fold.apply(&event("turn/start", 1, 100, &turn_start(1)));
    assert_eq!(fold.view().active, None);
    assert_eq!(fold.view().settled_ms, 0);

    // The descriptor promotes the pending start and stamps `through` with its own time, so the open
    // interval covers the inherited turn up to the child's origin.
    fold.apply(&event("subagent/descriptor", 2, 300, &descriptor));
    assert_eq!(
        fold.view().active,
        Some(ActiveInterval {
            since: 100,
            through: 300
        })
    );

    // A replayed ancestor turn/end *before* any descriptor settles nothing: it belongs to another clock.
    let mut replayed = TimingFold::new();
    replayed.apply(&event("turn/start", 1, 100, &turn_start(1)));
    replayed.apply(&event("turn/end", 2, 900, &turn_end(1, "completed")));
    let view = replayed.view();
    assert_eq!(view.settled_ms, 0);
    assert_eq!(view.active, None);
    assert_eq!(view.last_turn_completed, None);
}

#[test]
fn a_second_descriptor_resets_the_accumulated_clock() {
    let descriptor = full_continuable();
    let mut fold = TimingFold::new();
    fold.apply(&event("subagent/descriptor", 1, 100, &descriptor));
    fold.apply(&event("turn/start", 2, 100, &turn_start(1)));
    fold.apply(&event("turn/end", 3, 600, &turn_end(1, "completed")));
    assert_eq!(fold.view().settled_ms, 500);

    // The child's own descriptor arrives after the inherited prefix: everything accumulated so far was
    // the ancestor's, so it is discarded and the clock restarts.
    fold.apply(&event("subagent/descriptor", 4, 700, &descriptor));
    let view = fold.view();
    assert_eq!(view.settled_ms, 0);
    assert_eq!(view.active, None);
    assert_eq!(view.last_turn_completed, None);
}

#[test]
fn an_open_turn_clears_the_completion_flag_rather_than_reporting_failure() {
    let descriptor = full_continuable();
    let mut fold = TimingFold::new();
    fold.apply(&event("subagent/descriptor", 1, 0, &descriptor));
    fold.apply(&event("turn/start", 2, 10, &turn_start(1)));
    // A non-`completed` stop reason is a failure the client should render.
    fold.apply(&event("turn/end", 3, 110, &turn_end(1, "interrupted")));
    assert_eq!(fold.view().last_turn_completed, Some(false));

    // A new turn clears the flag: "still running" and "failed" are different answers, and reporting the
    // latter for the former would show every live child as broken.
    fold.apply(&event("turn/start", 4, 200, &turn_start(2)));
    assert_eq!(fold.view().last_turn_completed, None);
}

#[test]
fn a_turn_end_without_an_open_turn_settles_nothing() {
    let descriptor = full_continuable();
    let mut fold = TimingFold::new();
    fold.apply(&event("subagent/descriptor", 1, 0, &descriptor));
    fold.apply(&event("turn/end", 2, 500, &turn_end(1, "completed")));
    let view = fold.view();
    assert_eq!(view.settled_ms, 0);
    assert_eq!(view.last_turn_completed, None);

    // A clock that appears to go backwards saturates rather than wrapping.
    let mut backwards = TimingFold::new();
    backwards.apply(&event("subagent/descriptor", 1, 0, &descriptor));
    backwards.apply(&event("turn/start", 2, 900, &turn_start(1)));
    backwards.apply(&event("turn/end", 3, 100, &turn_end(1, "completed")));
    assert_eq!(backwards.view().settled_ms, 0);
    assert_eq!(backwards.view().last_turn_completed, Some(true));
}

// ---------------------------------------------------------------------------------------------
// Archive admission
// ---------------------------------------------------------------------------------------------

/// A `subagent`-origin header.
fn child<'a>(id: &'a str, parent: &'a str) -> ChildHeader<'a> {
    ChildHeader {
        id,
        origin_subagent: true,
        parent_session: Some(parent),
    }
}

#[test]
fn ancestry_follows_subagent_origin_and_stops_at_an_ordinary_session() {
    // c -> b -> a, and `a` is an ordinary client-created session: the walk yields b then a, and stops
    // because `a` is not subagent-origin. Walking *through* an ordinary session would make an archived
    // ancestor block a copy that never belonged to it.
    let c = child("c", "b");
    let b = child("b", "a");
    let a = ChildHeader {
        id: "a",
        origin_subagent: false,
        parent_session: None,
    };
    let lookup = |id: &str| -> Option<ChildHeader<'static>> {
        match id {
            "b" => Some(ChildHeader {
                id: "b",
                origin_subagent: true,
                parent_session: Some("a"),
            }),
            "a" => Some(ChildHeader {
                id: "a",
                origin_subagent: false,
                parent_session: None,
            }),
            _ => None,
        }
    };
    assert_eq!(
        header_ancestors(c, lookup),
        vec!["b".to_owned(), "a".to_owned()]
    );
    assert!(header_ancestors(a, lookup).is_empty());
    // `b` is reachable on its own and walks to `a` too.
    assert_eq!(header_ancestors(b, lookup), vec!["a".to_owned()]);
}

#[test]
fn ancestry_keeps_an_offline_creator_and_terminates_on_a_cycle() {
    // `b` exists as an id but is not resident: it still appears in the chain, because a released
    // intermediate must not become a hole through which a step reaches an archived ancestor.
    let c = child("c", "b");
    let offline = header_ancestors(c, |_| None);
    assert_eq!(offline, vec!["b".to_owned()]);

    // A corrupt lineage that names its own descendant terminates instead of looping.
    let cyclic = child("x", "y");
    let lookup = |id: &str| -> Option<ChildHeader<'static>> {
        match id {
            "y" => Some(ChildHeader {
                id: "y",
                origin_subagent: true,
                parent_session: Some("x"),
            }),
            _ => None,
        }
    };
    assert_eq!(header_ancestors(cyclic, lookup), vec!["y".to_owned()]);
}

#[test]
fn only_running_children_are_reported_as_activity() {
    let agents = vec![
        ChildAgentView {
            id: "running-child".to_owned(),
            running: true,
            ancestors: vec!["parent".to_owned()],
        },
        ChildAgentView {
            id: "settled-child".to_owned(),
            running: false,
            ancestors: vec!["parent".to_owned()],
        },
        ChildAgentView {
            id: "somebody-elses".to_owned(),
            running: true,
            ancestors: vec!["other-parent".to_owned()],
        },
    ];
    // A settled child is not work archiving would have to stop, and reporting it would make every
    // archive prompt for a confirmation nothing needs.
    assert_eq!(
        running_descendants("parent", &agents),
        vec!["running-child".to_owned()]
    );
    assert!(running_descendants("nobody", &agents).is_empty());
}

#[test]
fn the_activity_family_is_absent_when_nothing_runs() {
    let settled = vec![ChildAgentView {
        id: "settled".to_owned(),
        running: false,
        ancestors: vec!["parent".to_owned()],
    }];
    // `None`, not an empty family: an empty row reads as "this session has subagent work" to anything
    // that counts rows.
    assert!(activity_family("parent", &settled, |_| None, |_, _| true).is_none());

    assert!(activity_family("parent", &[], |_| None, |_, _| true).is_none());
}

#[test]
fn the_activity_family_lists_running_children_with_trusted_labels_only() {
    let agents = vec![
        ChildAgentView {
            id: "named".to_owned(),
            running: true,
            ancestors: vec!["parent".to_owned()],
        },
        ChildAgentView {
            id: "inherited".to_owned(),
            running: true,
            ancestors: vec!["parent".to_owned(), "grandparent".to_owned()],
        },
    ];
    let family = activity_family(
        "parent",
        &agents,
        |id| match id {
            "named" => Some(SubagentIdentity::Continuable {
                label: "child".to_owned(),
                seq: 3,
            }),
            "inherited" => Some(SubagentIdentity::OneShot {
                label: Some("ancestor".to_owned()),
                seq: 1,
            }),
            _ => None,
        },
        // Only `named`'s descriptor is in its own suffix; `inherited`'s came from the fork seed.
        |id, seq| id == "named" && seq == 3,
    )
    .expect("two running children");

    assert_eq!(
        family.items,
        vec![
            audit2_gap::ActivityItem {
                id: "named".to_owned(),
                label: Some("child".to_owned()),
            },
            audit2_gap::ActivityItem {
                id: "inherited".to_owned(),
                label: None,
            },
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// Error rendering
// ---------------------------------------------------------------------------------------------

#[test]
fn every_refusal_names_the_fact_a_user_needs_to_fix_it() {
    // These strings are what a cold resume shows: a reworded message breaks the search that finds the
    // schema which produced it, so each is asserted verbatim.
    assert_eq!(
        DescriptorError::UnknownField {
            path: "payload",
            field: "maxTokens".to_owned(),
        }
        .to_string(),
        "persisted subagent descriptor payload has unknown field \"maxTokens\""
    );
    assert_eq!(
        DescriptorError::FieldNotAString {
            field: "persona".to_owned(),
        }
        .to_string(),
        "persisted subagent descriptor persona must be a string"
    );
    assert_eq!(
        DescriptorError::ToolFilterNotAnArray {
            key: "deny".to_owned(),
        }
        .to_string(),
        "persisted subagent descriptor toolFilter.deny must be an array of strings"
    );
    assert_eq!(
        DescriptorError::Mode.to_string(),
        "persisted subagent descriptor mode must be \"one-shot\" or \"continuable\""
    );
    assert_eq!(
        DescriptorError::ToolFilterEmpty.to_string(),
        "persisted subagent descriptor toolFilter must declare allow and/or deny"
    );
    assert_eq!(
        DescriptorError::NotSerializable.to_string(),
        "subagent descriptor is not losslessly JSON-serializable"
    );
}

#[test]
fn the_mode_spellings_round_trip_and_display_as_the_wire_form() {
    for mode in [DescriptorMode::OneShot, DescriptorMode::Continuable] {
        assert_eq!(DescriptorMode::from_wire(mode.as_str()), Some(mode));
        assert_eq!(mode.to_string(), mode.as_str());
    }
    assert_eq!(DescriptorMode::from_wire("one_shot"), None);
    assert_eq!(DescriptorMode::from_wire(""), None);
    assert_eq!(DescriptorMode::OneShot.as_str(), "one-shot");
    assert_eq!(DescriptorMode::Continuable.as_str(), "continuable");
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod hooks {
    pub use super::audit2_gap as audit2;
}
