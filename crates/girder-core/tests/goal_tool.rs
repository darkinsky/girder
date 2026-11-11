//! Tests for the `goal` tool.
//!
//! The tool's job is to let a model ask what it is working toward after its context was compacted, so the properties
//! are about what a read returns and what a refusal looks like: a refusal is a result the model can act on, not an
//! error that ends the turn.

use std::sync::{Arc, Mutex};

use girder_core::{GoalState, ToolResult, ToolRunContext, goal_tool};

fn state() -> Arc<Mutex<GoalState>> {
    Arc::new(Mutex::new(GoalState::new()))
}

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            girder_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn run(state: &Arc<Mutex<GoalState>>, args: serde_json::Value) -> ToolResult {
    let tool = goal_tool(Arc::clone(state));
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("a goal action is a result")
}

#[tokio::test]
async fn status_without_a_goal_says_so() {
    let state = state();

    let text = text_of(&run(&state, serde_json::json!({"action": "status"})).await);

    assert!(text.contains("no goal"), "{text}");
}

#[tokio::test]
async fn creating_a_goal_returns_it_and_makes_it_current() {
    let state = state();

    let text = text_of(
        &run(
            &state,
            serde_json::json!({"action": "create", "objective": "ship the feature", "max_rounds": 4}),
        )
        .await,
    );

    assert!(text.contains("ship the feature"), "{text}");
    assert!(text.contains("created goal"), "{text}");
    // The status afterwards proves the state actually changed rather than only the message.
    let after = text_of(&run(&state, serde_json::json!({"action": "status"})).await);
    assert!(after.contains("ship the feature"), "{after}");
}

#[tokio::test]
async fn creating_without_an_objective_is_refused() {
    let state = state();

    let result = run(&state, serde_json::json!({"action": "create"})).await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("objective"));
}

#[tokio::test]
async fn completing_a_goal_changes_its_phase() {
    let state = state();
    run(
        &state,
        serde_json::json!({"action": "create", "objective": "do the thing"}),
    )
    .await;

    let text = text_of(&run(&state, serde_json::json!({"action": "complete"})).await);

    assert!(text.contains("completed"), "{text}");
}

#[tokio::test]
async fn a_mutation_without_a_goal_is_refused_rather_than_failing() {
    // The model can correct this; an error would end the turn and lose the rest of what it was doing.
    let state = state();

    let result = run(&state, serde_json::json!({"action": "complete"})).await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("no goal"));
}

#[tokio::test]
async fn blocking_requires_a_reason() {
    // A blocked goal with no reason is one nobody can unblock: the reason is the whole record of what is wrong.
    let state = state();
    run(
        &state,
        serde_json::json!({"action": "create", "objective": "x"}),
    )
    .await;

    let result = run(&state, serde_json::json!({"action": "block"})).await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("reason"));
}

#[tokio::test]
async fn blocking_with_a_reason_shows_it_in_the_status() {
    let state = state();
    run(
        &state,
        serde_json::json!({"action": "create", "objective": "x"}),
    )
    .await;

    run(
        &state,
        serde_json::json!({"action": "block", "reason": "waiting on a review"}),
    )
    .await;

    let text = text_of(&run(&state, serde_json::json!({"action": "status"})).await);
    assert!(text.contains("waiting on a review"), "{text}");
}

#[tokio::test]
async fn an_unknown_action_names_the_valid_ones() {
    let state = state();

    let result = run(&state, serde_json::json!({"action": "explode"})).await;

    assert!(result.is_error);
    let text = text_of(&result);
    assert!(
        text.contains("status"),
        "the valid actions are listed: {text}"
    );
}

#[tokio::test]
async fn a_missing_action_is_refused() {
    let state = state();

    let result = run(&state, serde_json::json!({})).await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("action"));
}

#[tokio::test]
async fn the_status_reports_the_rounds_and_activation() {
    // A model told "disarmed" knows its next turn is the last, and one that did not know would plan work it will not
    // get to do.
    let state = state();
    run(
        &state,
        serde_json::json!({"action": "create", "objective": "y", "max_rounds": 3}),
    )
    .await;

    let text = text_of(&run(&state, serde_json::json!({"action": "status"})).await);
    assert!(text.contains("0/3"), "the round budget is shown: {text}");
    // Creating a goal arms it: the model asked to work toward something, and a goal that did not continue would need
    // a second action to start the work it just described.
    assert!(text.contains("armed"), "the activation is shown: {text}");
}

#[tokio::test]
async fn pausing_and_resuming_round_trip() {
    let state = state();
    run(
        &state,
        serde_json::json!({"action": "create", "objective": "z"}),
    )
    .await;

    let paused = text_of(&run(&state, serde_json::json!({"action": "pause"})).await);
    assert!(paused.contains("paused"), "{paused}");

    let resumed = text_of(&run(&state, serde_json::json!({"action": "resume"})).await);
    assert!(resumed.contains("resumed"), "{resumed}");
}

#[tokio::test]
async fn two_creates_with_one_objective_keep_one_goal() {
    // The id is derived from the objective's digest, so the same intent is the same goal.
    let state = state();
    run(
        &state,
        serde_json::json!({"action": "create", "objective": "same"}),
    )
    .await;
    let second = run(
        &state,
        serde_json::json!({"action": "create", "objective": "same"}),
    )
    .await;

    assert!(
        second.is_error,
        "an open goal with the same objective is refused"
    );
    assert!(
        state.lock().expect("unpoisoned").current().is_some(),
        "the original goal survives"
    );
}
