//! Tests for the `subagent_interrupt` tool.
//!
//! The property that matters is not that a cancel signal is sent — it is that the three answers stay
//! *distinguishable*: a requested stop, a subagent that had already stopped, and an id that names
//! nothing call for three different next moves, and a tool that folded them together would make a
//! model retry a call that cannot succeed or report success it did not cause.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the tool as written even before the crate root wires it in. It resolves `girder_core::…`
// through the alias the library declares for itself, which is why the same source builds here.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use girder_core::{
    AgentId, ContentBlock, DelegationResult, Depth, InterruptOutcome, Interrupter,
    SubagentRegistry, ToolCallId, ToolDefinition, ToolExecution, ToolResult, ToolRunContext,
};
use tokio_util::sync::CancellationToken;

use girder_core::subagent_interrupt_tool;

/// An interrupter that answers a fixed outcome and counts how often it was asked.
///
/// The count is the evidence for "the registry answer is authoritative": a finished child must never
/// reach the cancel mechanism.
struct FixedInterrupter {
    outcome: InterruptOutcome,
    calls: AtomicUsize,
}

impl FixedInterrupter {
    fn new(outcome: InterruptOutcome) -> Arc<Self> {
        Arc::new(Self {
            outcome,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Interrupter for FixedInterrupter {
    fn interrupt(&self, _id: &str) -> InterruptOutcome {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.outcome.clone()
    }
}

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn run_context() -> ToolRunContext {
    ToolRunContext {
        execution: ToolExecution {
            call_id: ToolCallId::new("c1").expect("valid test id"),
            name: "subagent_interrupt".to_owned(),
            arguments: "{}".to_owned(),
            agent_id: AgentId::new("agent").expect("valid test id"),
        },
        cancel: CancellationToken::new(),
        ..Default::default()
    }
}

/// Runs a tool body the way the executor does.
async fn call(tool: &ToolDefinition, args: serde_json::Value) -> ToolResult {
    (tool.execute)(args, run_context())
        .await
        .expect("a tool body returns a model-facing result, not an error")
}

/// A registry holding one running child.
fn registry_with_running_child() -> (Arc<SubagentRegistry>, String) {
    let registry = SubagentRegistry::shared();
    let id = registry.record_start("count the files in the workspace", Depth::root());
    (registry, id)
}

#[tokio::test]
async fn interrupting_a_running_subagent_reports_requested() {
    let (registry, id) = registry_with_running_child();
    let interrupter = FixedInterrupter::new(InterruptOutcome::Requested);
    let tool = subagent_interrupt_tool(Arc::clone(&registry), interrupter.clone());

    let result = call(&tool, serde_json::json!({ "id": id })).await;

    assert!(
        !result.is_error,
        "a request is not a failure: {}",
        text_of(&result)
    );
    assert_eq!(
        interrupter.calls(),
        1,
        "the interrupter must actually be asked"
    );
    let text = text_of(&result);
    assert!(
        text.contains("requested"),
        "the outcome must name the request: {text}"
    );
    // The distinction the tool exists to preserve: the answer is "asked", not "stopped".
    assert!(
        text.contains("may still be working"),
        "the result must not claim the child has stopped: {text}"
    );
}

#[tokio::test]
async fn interrupting_a_finished_subagent_reports_its_status_without_requesting() {
    let (registry, id) = registry_with_running_child();
    registry.record_finished(
        &id,
        &DelegationResult {
            text: "42".to_owned(),
            turns: 2,
            child_id: AgentId::new("agent/1").expect("valid test id"),
        },
    );
    let interrupter = FixedInterrupter::new(InterruptOutcome::Requested);
    let tool = subagent_interrupt_tool(Arc::clone(&registry), interrupter.clone());

    let result = call(&tool, serde_json::json!({ "id": id })).await;

    // A child that already stopped satisfies the caller's goal; reporting it as an error would push
    // a model to retry a call that can never do anything.
    assert!(
        !result.is_error,
        "an already-finished child is an answer, not a failure: {}",
        text_of(&result)
    );
    let text = text_of(&result);
    assert!(
        text.contains("already_finished"),
        "the outcome must be explicit: {text}"
    );
    assert!(
        text.contains("finished"),
        "the status must be reported: {text}"
    );
    assert_eq!(
        interrupter.calls(),
        0,
        "a finished child must not reach the cancel mechanism"
    );
}

#[tokio::test]
async fn interrupting_a_failed_subagent_reports_the_status_it_reached() {
    let (registry, id) = registry_with_running_child();
    registry.record_failed(&id, "provider exploded");
    let interrupter = FixedInterrupter::new(InterruptOutcome::Requested);
    let tool = subagent_interrupt_tool(registry, interrupter.clone());

    let result = call(&tool, serde_json::json!({ "id": id })).await;

    assert!(!result.is_error);
    let text = text_of(&result);
    assert!(text.contains("already_finished"), "{text}");
    // "failed" is a different fact from "finished", and a caller deciding whether to retry needs it.
    assert!(
        text.contains("failed"),
        "the failure state must survive: {text}"
    );
    assert_eq!(interrupter.calls(), 0);
}

#[tokio::test]
async fn interrupting_an_unknown_id_is_an_error() {
    let registry = SubagentRegistry::shared();
    let interrupter = FixedInterrupter::new(InterruptOutcome::Requested);
    let tool = subagent_interrupt_tool(registry, interrupter.clone());

    let result = call(&tool, serde_json::json!({ "id": "sa-99" })).await;

    assert!(result.is_error, "an unknown id is a caller mistake");
    assert!(text_of(&result).contains("sa-99"));
    assert_eq!(
        interrupter.calls(),
        0,
        "nothing to cancel for an unknown id"
    );
}

#[tokio::test]
async fn an_interrupter_that_finds_the_child_already_gone_still_reports_finished() {
    // The race the trait exists for: the registry said running, the cancel mechanism knew better.
    let (registry, id) = registry_with_running_child();
    let interrupter = FixedInterrupter::new(InterruptOutcome::AlreadyFinished {
        status: "finished".to_owned(),
    });
    let tool = subagent_interrupt_tool(registry, interrupter.clone());

    let result = call(&tool, serde_json::json!({ "id": id })).await;

    assert!(!result.is_error);
    let text = text_of(&result);
    assert!(text.contains("already_finished"), "{text}");
    assert_eq!(interrupter.calls(), 1, "the fresher answer must be sought");
}

#[tokio::test]
async fn a_missing_id_is_rejected() {
    let registry = SubagentRegistry::shared();
    let interrupter = FixedInterrupter::new(InterruptOutcome::Requested);
    let tool = subagent_interrupt_tool(registry, interrupter.clone());

    let result = call(&tool, serde_json::json!({})).await;

    assert!(result.is_error);
    assert!(
        text_of(&result).contains("`id` is required"),
        "the refusal must name the missing argument: {}",
        text_of(&result)
    );
    assert_eq!(interrupter.calls(), 0);
}

#[tokio::test]
async fn a_blank_id_is_treated_as_missing() {
    let registry = SubagentRegistry::shared();
    let interrupter = FixedInterrupter::new(InterruptOutcome::Requested);
    let tool = subagent_interrupt_tool(registry, interrupter.clone());

    let result = call(&tool, serde_json::json!({ "id": "   " })).await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("`id` is required"));
    assert_eq!(interrupter.calls(), 0);
}

#[test]
fn the_schema_names_the_tool_and_requires_the_id() {
    let registry = SubagentRegistry::shared();
    let tool = subagent_interrupt_tool(
        registry,
        FixedInterrupter::new(InterruptOutcome::Requested) as Arc<dyn Interrupter>,
    );
    let schema = tool.schema();

    assert_eq!(schema.name, "subagent_interrupt");
    assert_eq!(schema.parameters["type"], "object");
    assert_eq!(schema.parameters["properties"]["id"]["type"], "string");
    assert_eq!(
        schema.parameters["required"],
        serde_json::json!(["id"]),
        "the id must be required, or the model will call it with no target"
    );
    assert!(
        schema.parameters["properties"]["message"].is_null(),
        "this tool takes no message; steering is subagent_send's job"
    );
}

#[test]
fn the_description_warns_that_unfinished_work_is_lost() {
    let tool = subagent_interrupt_tool(
        SubagentRegistry::shared(),
        FixedInterrupter::new(InterruptOutcome::Requested) as Arc<dyn Interrupter>,
    );
    let description = tool.description.to_lowercase();

    assert!(
        description.contains("discarded") || description.contains("lost"),
        "a model must be told the work is lost: {description}"
    );
    assert!(
        description.contains("request"),
        "a model must not read this as an immediate stop: {description}"
    );
    assert!(
        description.contains("subagent_wait"),
        "a caller needs to be told how to learn it actually stopped: {description}"
    );
}

#[test]
fn the_outcome_variants_stay_distinct() {
    // The equality is the contract the tool body matches on; a variant that compared equal to
    // another would make two different outcomes collapse in the match arm above.
    assert_ne!(
        InterruptOutcome::Requested,
        InterruptOutcome::AlreadyFinished {
            status: "finished".to_owned(),
        }
    );
    assert_ne!(
        InterruptOutcome::Unknown {
            id: "sa-1".to_owned(),
        },
        InterruptOutcome::AlreadyFinished {
            status: "finished".to_owned(),
        }
    );
}

// ---- the token-backed interrupter ---------------------------------------------------------------

#[test]
fn arming_a_subagent_makes_it_interruptible() {
    let cancellers = girder_core::Cancellers::new();
    let armed = cancellers.arm("child-1");

    let outcome = cancellers.interrupt("child-1");

    assert_eq!(outcome, girder_core::InterruptOutcome::Requested);
    assert!(
        armed.token().is_cancelled(),
        "the token the work holds is cancelled"
    );
}

#[test]
fn interrupting_an_unarmed_id_names_it() {
    let cancellers = girder_core::Cancellers::new();

    let outcome = cancellers.interrupt("nobody");

    assert!(matches!(
        outcome,
        girder_core::InterruptOutcome::Unknown { .. }
    ));
}

#[test]
fn dropping_the_guard_forgets_the_token() {
    // A guard that only forgot would leave the subagent running when its scope ended; one that only cancelled would
    // leave a token in the map for every failure path that returned early.
    let cancellers = girder_core::Cancellers::new();
    {
        let armed = cancellers.arm("child-1");
        assert!(!armed.token().is_cancelled());
    }

    assert!(matches!(
        cancellers.interrupt("child-1"),
        girder_core::InterruptOutcome::Unknown { .. }
    ));
}

#[test]
fn two_subagents_have_independent_tokens() {
    // Interrupting one must not stop its sibling, or a caller cancelling the slow child would kill the fast one.
    let cancellers = girder_core::Cancellers::new();
    let first = cancellers.arm("a");
    let second = cancellers.arm("b");

    cancellers.interrupt("a");

    assert!(first.token().is_cancelled());
    assert!(!second.token().is_cancelled(), "the sibling keeps running");
}
