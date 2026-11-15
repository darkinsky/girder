//! Tests for the post-failure retry decision.
//!
//! The properties pinned here are the ones that cost the user something when they are wrong:
//!
//! - **A non-idempotent timeout asks a human.** This is the case the module exists for. Retrying a
//!   `write` that may already have truncated its file repeats an effect nobody can undo; reporting
//!   it as a plain error invites the model to resend the same call. Both branches are wrong and the
//!   only correct answer is to stop and look.
//! - **An idempotent timeout is retried.** Asking a human about a `read` would make the safe case
//!   expensive enough that callers would route around the check.
//! - **Bad arguments are never retried**, whatever the tool's idempotency and whatever budget
//!   remains, because the arguments a resend carries are the ones that were just rejected.
//! - **A reported error is retried for either kind of tool**, because a failure the tool reported
//!   is a failure that did not complete, so there is nothing for the repeat to duplicate.
//! - **The budget is a hard stop**, and it never turns an unknown state into a known one.
//!
//! The module is included by path so these tests compile against `src/tool_retry_plan.rs` as
//! written, before the crate root wires it in. It names the crate through `::girder_core`, which
//! the library alias lets resolve from either side.

#[path = "../src/tool_retry_plan.rs"]
mod tool_retry_plan;

use tool_retry_plan::{Failure, Kind, Plan, plan};

// ---- fixtures ----------------------------------------------------------------------------------

fn timeout(tool: &str, attempt: u32) -> Failure {
    Failure::new(tool, attempt, Kind::Timeout)
}

fn error(tool: &str, attempt: u32, message: &str) -> Failure {
    Failure::new(
        tool,
        attempt,
        Kind::Error {
            message: message.to_owned(),
        },
    )
}

fn bad_arguments(tool: &str, attempt: u32) -> Failure {
    Failure::new(tool, attempt, Kind::BadArguments)
}

/// Extracts the wait from a `Retry`, failing the test with the actual plan otherwise.
fn retry_after(decision: &Plan) -> (u64, u32) {
    match decision {
        Plan::Retry { after_ms, attempt } => (*after_ms, *attempt),
        other => panic!("expected Retry, got {other:?}"),
    }
}

fn give_up_why(decision: &Plan) -> String {
    match decision {
        Plan::GiveUp { why } => why.clone(),
        other => panic!("expected GiveUp, got {other:?}"),
    }
}

fn ask_why(decision: &Plan) -> String {
    match decision {
        Plan::Ask { why } => why.clone(),
        other => panic!("expected Ask, got {other:?}"),
    }
}

// ---- the core: timeout vs idempotency -----------------------------------------------------------

#[test]
fn a_non_idempotent_timeout_asks_a_human() {
    // The central rule. A `write` that timed out may have half-written its file; retrying repeats
    // the side effect and giving up hands the model an error it will answer by resending.
    let decision = plan(&timeout("write", 1), false, 3);
    let why = ask_why(&decision);
    assert!(
        why.contains("write"),
        "the question must name the tool: {why}"
    );
    assert!(
        why.contains("timed out"),
        "the question must say the call timed out: {why}"
    );
}

#[test]
fn a_non_idempotent_timeout_asks_even_with_budget_left_and_attempts_used() {
    // Every attempt number below the budget reaches the same answer: the uncertainty comes from the
    // failure kind, not from how many tries have already been spent.
    for attempt in 1..=4 {
        let decision = plan(&timeout("write", attempt), false, 5);
        assert!(
            matches!(decision, Plan::Ask { .. }),
            "attempt {attempt} should have asked, got {decision:?}"
        );
    }
}

#[test]
fn an_idempotent_timeout_is_retried() {
    // A `read` that ran twice returns the same bytes, so a retry is strictly cheaper than a
    // question. This is the other half of the central rule.
    let (after_ms, attempt) = retry_after(&plan(&timeout("read", 1), true, 3));
    assert!(after_ms > 0, "an immediate retry would be a busy loop");
    assert_eq!(attempt, 2, "the retry is the second attempt");
}

#[test]
fn the_same_timeout_gets_opposite_answers_from_the_idempotency_flag_alone() {
    // Pinned explicitly because the failure is identical and only the second argument differs: a
    // regression that ignored the flag would still pass every single-argument test above.
    let failure = timeout("bash", 1);
    assert!(matches!(plan(&failure, true, 3), Plan::Retry { .. }));
    assert!(matches!(plan(&failure, false, 3), Plan::Ask { .. }));
}

// ---- reported failures --------------------------------------------------------------------------

#[test]
fn a_reported_error_is_retried_for_an_idempotent_tool() {
    let (after_ms, attempt) = retry_after(&plan(&error("read", 1, "no such file"), true, 3));
    assert!(after_ms > 0);
    assert_eq!(attempt, 2);
}

#[test]
fn a_reported_error_is_retried_for_a_non_idempotent_tool() {
    // The failure was reported, so the body did not complete and there is no partial effect for a
    // repeat to duplicate. Idempotency does not enter into it.
    let (after_ms, attempt) = retry_after(&plan(&error("write", 1, "disk full"), false, 3));
    assert!(after_ms > 0);
    assert_eq!(attempt, 2);
}

#[test]
fn a_reported_error_does_not_ask_even_when_the_tool_is_not_idempotent() {
    // `Ask` is reserved for the case where the state is unknown; a reported error is a known state.
    let decision = plan(&error("bash", 1, "exit status 1"), false, 3);
    assert!(
        matches!(decision, Plan::Retry { .. }),
        "a reported error must not ask: {decision:?}"
    );
}

// ---- bad arguments ------------------------------------------------------------------------------

#[test]
fn bad_arguments_are_never_retried() {
    let decision = plan(&bad_arguments("read", 1), true, 5);
    let why = give_up_why(&decision);
    assert!(why.contains("read"));
    assert!(
        why.contains("arguments"),
        "the reason must say the arguments are the problem: {why}"
    );
}

#[test]
fn bad_arguments_are_refused_for_a_non_idempotent_tool_too() {
    // Neither idempotency nor a large budget can rescue an argument error: the resend is the same
    // text. This is asserted separately because a `BadArguments` branch placed after the timeout
    // branch would still pass the idempotent case above.
    assert!(matches!(
        plan(&bad_arguments("write", 1), false, 10),
        Plan::GiveUp { .. }
    ));
}

#[test]
fn bad_arguments_outrank_the_ask_for_a_non_idempotent_tool() {
    // An argument error proves nothing ran, so the unknown-state question does not arise; asking a
    // human here would be noise. A rule ordering bug would produce `Ask`.
    assert!(matches!(
        plan(&bad_arguments("write", 1), false, 3),
        Plan::GiveUp { .. }
    ));
}

#[test]
fn bad_arguments_are_refused_even_on_the_last_attempt() {
    assert!(matches!(
        plan(&bad_arguments("read", 5), true, 5),
        Plan::GiveUp { .. }
    ));
}

// ---- the attempt budget -------------------------------------------------------------------------

#[test]
fn a_failure_on_the_last_permitted_attempt_gives_up() {
    let decision = plan(&error("read", 3, "timed out reading"), true, 3);
    let why = give_up_why(&decision);
    assert!(
        why.contains('3'),
        "the reason must state the attempt and the budget: {why}"
    );
    assert!(
        why.contains("timed out reading"),
        "the tool's message is kept: {why}"
    );
}

#[test]
fn the_budget_of_one_forbids_every_retry() {
    // `max_attempts` counts the attempt that just failed, so one means the call was single-shot.
    assert!(matches!(
        plan(&error("read", 1, "boom"), true, 1),
        Plan::GiveUp { .. }
    ));
    assert!(matches!(
        plan(&timeout("read", 1), true, 1),
        Plan::GiveUp { .. }
    ));
}

#[test]
fn the_budget_of_zero_forbids_every_retry() {
    // A caller that disables retries must not get one because the arithmetic is off by one.
    assert!(matches!(
        plan(&error("read", 1, "boom"), true, 0),
        Plan::GiveUp { .. }
    ));
}

#[test]
fn an_exhausted_budget_still_asks_about_a_non_idempotent_timeout() {
    // The budget limits spending, it does not resolve uncertainty. Handing this to the model would
    // invite exactly the resend whose effect is in doubt.
    let decision = plan(&timeout("write", 3), false, 3);
    let why = ask_why(&decision);
    assert!(why.contains("write"));
}

#[test]
fn an_attempt_of_zero_is_refused_rather_than_retried() {
    // A failure numbered zero is not a failure position this module can reason about; reading it as
    // "nothing has been tried yet" would grant a retry on an undefined attempt.
    let decision = plan(&error("read", 0, "boom"), true, 3);
    let why = give_up_why(&decision);
    assert!(
        why.contains("attempt 0"),
        "the reason must say why it was refused: {why}"
    );
}

// ---- first failure vs repeated failure ----------------------------------------------------------

#[test]
fn the_first_failure_and_a_later_one_differ_only_in_the_attempt_number() {
    let first = retry_after(&plan(&error("read", 1, "boom"), true, 4));
    let third = retry_after(&plan(&error("read", 3, "boom"), true, 4));
    assert_eq!(first.1, 2);
    assert_eq!(third.1, 4);
    // Backoff grows with the attempt, so the wait after a third failure is longer than after a
    // first: the caller already knows the call is struggling.
    assert!(
        third.0 > first.0,
        "wait after attempt 3 ({}) should exceed attempt 1 ({})",
        third.0,
        first.0
    );
}

#[test]
fn the_wait_is_capped_so_a_long_retry_sequence_does_not_park_the_turn() {
    // The doubling is clamped; without a ceiling, attempt 20 would be hours.
    let (after_ms, _) = retry_after(&plan(&error("read", 7, "boom"), true, 50));
    assert!(
        after_ms <= 10_000,
        "the wait must stay inside the strategy's ceiling, got {after_ms}"
    );
}

#[test]
fn a_repeated_bad_argument_failure_is_still_a_give_up() {
    // The model resent the same rejected arguments; nothing about repetition makes them valid.
    for attempt in 1..=3 {
        assert!(
            matches!(
                plan(&bad_arguments("read", attempt), true, 5),
                Plan::GiveUp { .. }
            ),
            "attempt {attempt} should have given up"
        );
    }
}

#[test]
fn the_next_attempt_number_saturates_instead_of_wrapping() {
    // A caller looping on the plan must not be handed attempt zero and start over.
    let (_, attempt) = retry_after(&plan(&user_attempt(u32::MAX - 1), true, u32::MAX));
    assert_eq!(attempt, u32::MAX);
}

/// A failure on a huge attempt number, for the saturation check.
fn user_attempt(attempt: u32) -> Failure {
    error("read", attempt, "boom")
}

// ---- the kinds' stable codes -------------------------------------------------------------------

#[test]
fn every_kind_has_a_distinct_stable_code() {
    let codes = [
        Kind::Timeout.code(),
        Kind::Error {
            message: String::new(),
        }
        .code(),
        Kind::BadArguments.code(),
    ];
    assert_eq!(codes, ["TIMEOUT", "ERROR", "BAD_ARGUMENTS"]);
}

// ---- reason wording carries what a reader needs ------------------------------------------------

#[test]
fn a_reason_without_a_tool_message_does_not_invent_one() {
    let decision = plan(&error("read", 2, "   "), true, 2);
    let why = give_up_why(&decision);
    assert!(
        why.contains("no message"),
        "an empty message is reported as absent: {why}"
    );
}
