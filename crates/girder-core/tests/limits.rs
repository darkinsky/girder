//! Tests for budgets.
//!
//! A budget reports rather than enforces, so the tests pin what a caller reads before it acts: whether the next spend
//! fits, and an honest remaining count after an overshoot.

use girder_core::{Budget, Ceiling, SessionBudget};

// ---- ceilings ---------------------------------------------------------------------------------

#[test]
fn an_unlimited_ceiling_allows_everything() {
    assert!(Ceiling::Unlimited.allows(u64::MAX, 1));
    assert_eq!(Ceiling::Unlimited.limit(), None);
}

#[test]
fn a_finite_ceiling_allows_up_to_its_limit() {
    let ceiling = Ceiling::AtMost(100);

    assert!(ceiling.allows(50, 50));
    assert!(!ceiling.allows(50, 51));
    assert_eq!(ceiling.limit(), Some(100));
}

// ---- one budget -------------------------------------------------------------------------------

#[test]
fn a_spend_is_recorded() {
    let mut budget = Budget::at_most(100);
    budget.spend(30);

    assert_eq!(budget.used(), 30);
    assert_eq!(budget.remaining(), Some(70));
}

#[test]
fn an_overshoot_reports_a_negative_remainder() {
    // Not clamped at zero: a budget that hid the overshoot would conceal that one request cost twice the ceiling.
    let mut budget = Budget::at_most(100);
    budget.spend(150);

    assert_eq!(budget.remaining(), Some(-50));
    assert!(budget.is_exhausted());
}

#[test]
fn a_budget_exactly_at_its_limit_is_exhausted() {
    let mut budget = Budget::at_most(100);
    budget.spend(100);

    assert!(!budget.allows(1));
    assert!(budget.is_exhausted());
    assert_eq!(budget.remaining(), Some(0));
}

#[test]
fn a_refund_returns_room() {
    let mut budget = Budget::at_most(100);
    budget.spend(60);
    budget.refund(20);

    assert_eq!(budget.used(), 40);
    assert!(budget.allows(60));
}

#[test]
fn a_refund_larger_than_the_spend_stops_at_zero() {
    // A budget that went negative would report room it does not have.
    let mut budget = Budget::at_most(100);
    budget.spend(10);
    budget.refund(100);

    assert_eq!(budget.used(), 0);
}

#[test]
fn a_spend_overflow_saturates() {
    let mut budget = Budget::at_most(100);
    budget.spend(u64::MAX);
    budget.spend(10);

    assert_eq!(budget.used(), u64::MAX);
}

#[test]
fn an_unlimited_budget_has_no_remainder() {
    let mut budget = Budget::unlimited();
    budget.spend(1_000_000);

    assert_eq!(budget.remaining(), None);
    assert!(!budget.is_exhausted());
    assert_eq!(budget.used(), 1_000_000);
}

// ---- a session's budgets ----------------------------------------------------------------------

#[test]
fn a_session_tracks_each_quantity_separately() {
    // Named rather than generic: a token count and a byte count are both `u64` and both budgets, and mixing them
    // would produce a spend that looks right and is not.
    let mut session = SessionBudget::with(
        Ceiling::AtMost(1000),
        Ceiling::AtMost(5000),
        Ceiling::AtMost(60_000),
        Ceiling::AtMost(100),
    );

    session.record_tokens(400);
    session.record_tool_output(2000);
    session.record_elapsed(30_000);
    session.record_cost(40);

    assert_eq!(session.tokens.used(), 400);
    assert_eq!(session.tool_bytes.used(), 2000);
    assert_eq!(session.milliseconds.used(), 30_000);
    assert_eq!(session.cost.used(), 40);
}

#[test]
fn a_session_answers_whether_the_next_call_fits() {
    // The one spend that cannot be undone, so the caller asks before it acts.
    let mut session = SessionBudget::with(
        Ceiling::AtMost(1000),
        Ceiling::Unlimited,
        Ceiling::Unlimited,
        Ceiling::Unlimited,
    );
    session.record_tokens(900);

    assert!(session.can_spend_tokens(100));
    assert!(!session.can_spend_tokens(101));
}

#[test]
fn exhausted_budgets_are_named() {
    let mut session = SessionBudget::with(
        Ceiling::AtMost(10),
        Ceiling::AtMost(10),
        Ceiling::Unlimited,
        Ceiling::Unlimited,
    );
    session.record_tokens(10);
    session.record_tool_output(10);

    let names = session.exhausted();

    assert!(names.contains(&"tokens"));
    assert!(names.contains(&"tool_bytes"));
    assert!(!names.contains(&"milliseconds"), "unlimited never exhausts");
}

#[test]
fn a_default_session_is_unlimited() {
    let session = SessionBudget::default();

    assert!(session.can_spend_tokens(u64::MAX));
    assert!(session.exhausted().is_empty());
}
