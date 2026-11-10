//! Tests for the spill decision.
//!
//! A pure function, so the tests pin the boundary cases that a tool would otherwise have to guess: exactly at the
//! limit, and already truncated.

use girder_core::{SpillAction, SpillContext, SpillReason, decide_spill, spill_omitted};

#[test]
fn a_small_result_is_kept() {
    let (action, reason) = decide_spill(
        SpillContext {
            bytes: 10,
            truncated: false,
        },
        100,
    );

    assert_eq!(action, SpillAction::Keep);
    assert_eq!(reason, SpillReason::Small);
}

#[test]
fn a_large_result_spills() {
    let (action, reason) = decide_spill(
        SpillContext {
            bytes: 101,
            truncated: false,
        },
        100,
    );

    assert_eq!(action, SpillAction::Spill);
    let SpillReason::TooLarge { bytes, limit } = reason else {
        panic!("expected TooLarge");
    };
    assert_eq!(bytes, 101);
    assert_eq!(limit, 100);
}

#[test]
fn a_result_exactly_at_the_limit_is_kept() {
    // The boundary is `>`, not `>=`, so a tool configured for 100 KiB passes a 100 KiB result.
    let (action, _) = decide_spill(
        SpillContext {
            bytes: 100,
            truncated: false,
        },
        100,
    );

    assert_eq!(action, SpillAction::Keep);
}

#[test]
fn an_already_truncated_result_is_kept_however_large() {
    // It is readable as it stands; spilling it would point at the same text the model already has.
    let (action, reason) = decide_spill(
        SpillContext {
            bytes: 1_000_000,
            truncated: true,
        },
        100,
    );

    assert_eq!(action, SpillAction::Keep);
    assert_eq!(reason, SpillReason::AlreadyBounded);
}

#[test]
fn the_reason_explains_a_spill() {
    // A result replaced without a recorded reason is a behaviour nobody can explain afterwards.
    let (_, reason) = decide_spill(
        SpillContext {
            bytes: 500,
            truncated: false,
        },
        100,
    );

    let text = reason.as_str();
    assert!(text.contains("500"), "{text}");
    assert!(text.contains("100"), "{text}");
}

#[test]
fn the_small_reason_reads_plainly() {
    assert_eq!(SpillReason::Small.as_str(), "under the spill threshold");
    assert_eq!(
        SpillReason::AlreadyBounded.as_str(),
        "the tool already bounded its output"
    );
}

#[test]
fn a_spill_omits_nothing() {
    // The model is told where the text is, not that some of it was dropped. A count here would claim the spill lost
    // something, and a model that believed that would not read the file.
    assert_eq!(spill_omitted(), girder_core::Omitted::None);
}

#[test]
fn the_action_reports_itself() {
    assert!(SpillAction::Spill.should_spill());
    assert!(!SpillAction::Keep.should_spill());
}
