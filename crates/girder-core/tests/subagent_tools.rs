//! Tests for the subagent control tools.
//!
//! The property that matters is not that a registry stores rows — it is that a refusal stays
//! *distinguishable* from a result. `subagent_wait` must not report a still-running child as an
//! answer, `subagent_list` must not hide a failed spawn, and `subagent_send` must say what the
//! delegation API is missing rather than accepting a message it cannot deliver.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the tools as written even before the crate root wires them in. It resolves `girder_core::…`
// through the alias the library declares for itself, which is why the same source builds here.
#[path = "../src/subagent_tools.rs"]
mod subagent_tools;

use std::sync::Arc;
use std::time::Duration;

use girder_core::{
    AgentId, ContentBlock, DelegationResult, Depth, EventBus, LlmError, LlmProvider, LlmRequest,
    LoopContext, MemorySession, Session, ToolCallId, ToolDefinition, ToolExecution, ToolRegistry,
    ToolRegistryView, ToolResult, ToolRunContext,
};
use tokio_util::sync::CancellationToken;

use subagent_tools::{
    MAX_WAIT_MS, Status, SubagentRegistry, register_subagent_tools, subagent_list_tool,
    subagent_send_tool, subagent_wait_tool, traced_subagent_tool,
};

/// An isolated scratch directory: `temp_dir()` plus pid plus a counter, so two test binaries and two
/// tests in one binary cannot collide.
///
/// None of these tests need a file yet, but every path a tool could take is rooted here, so a future
/// test that does write has somewhere isolated to write.
fn scratch(name: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-subagent-tools-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
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
            name: "test".to_owned(),
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

/// A provider that answers with one line of text and never calls a tool.
///
/// The traced spawn needs a real `delegate` round trip to prove that a finished child is recorded as
/// finished; a provider that does nothing would leave the entry `running` and every wait would time
/// out for the wrong reason.
struct PlainAnswer {
    answer: String,
}

impl LlmProvider for PlainAnswer {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, LlmError>> {
        let answer = self.answer.clone();
        Box::pin(async move {
            let chunks = vec![
                girder_core::StreamChunk::BlockStart {
                    index: 0,
                    block_type: "text".to_owned(),
                },
                girder_core::StreamChunk::TextDelta {
                    index: 0,
                    text: answer,
                },
                girder_core::StreamChunk::Finish {
                    reason: girder_core::FinishReason::Stop,
                },
            ];
            Ok(Box::pin(futures::stream::iter(chunks)) as girder_core::ChunkStream)
        })
    }
}

/// A context at `depth`, with `max_depth` as its ceiling.
fn context(answer: &str, depth: Depth, max_depth: u32) -> Arc<LoopContext> {
    let tools = Arc::new(ToolRegistry::new()) as Arc<dyn ToolRegistryView>;
    Arc::new(
        LoopContext::new(
            Arc::new(PlainAnswer {
                answer: answer.to_owned(),
            }),
            tools,
            Arc::new(MemorySession::new()) as Arc<dyn Session>,
            Arc::new(EventBus::new()),
            "test-model",
            AgentId::new("agent").expect("valid test id"),
        )
        .with_depth(depth, max_depth),
    )
}

fn delegation(text: &str, turns: u32) -> DelegationResult {
    DelegationResult {
        text: text.to_owned(),
        turns,
        child_id: AgentId::new("agent/1").expect("valid test id"),
    }
}

// ---- the registry, without any model -----------------------------------------------------------

#[test]
fn an_empty_registry_lists_nothing() {
    // The distinction the whole module exists to preserve: "nothing was spawned" is an answer, not a
    // blank page a model would read as a broken tool. Asserted through the tool, because that is the
    // surface a model actually sees.
    let registry = SubagentRegistry::shared();
    assert!(registry.entries().is_empty());
    assert!(registry.get("sa-0").is_none());
}

#[test]
fn a_spawn_is_recorded_as_running() {
    let registry = SubagentRegistry::new();
    let id = registry.record_start("read the parser", Depth::root());

    assert_eq!(id, "sa-0", "the first handle is deterministic");
    let entry = registry.get(&id).expect("recorded");
    assert!(entry.is_running());
    assert_eq!(entry.status, Status::Running);
    assert_eq!(entry.depth, Depth::root());
    assert_eq!(entry.description, "read the parser");
}

#[test]
fn handles_are_sequential_and_do_not_collide() {
    // `delegate` derives the child id from parent plus depth, so two sequential delegations produce
    // the same child id. The registry id is what makes them addressable one at a time.
    let registry = SubagentRegistry::new();
    let first = registry.record_start("one", Depth::root());
    let second = registry.record_start("two", Depth::root());

    assert_eq!(first, "sa-0");
    assert_eq!(second, "sa-1");
    assert_ne!(first, second);
}

#[test]
fn entries_are_listed_in_spawn_order_past_ten() {
    // A string sort puts `sa-10` before `sa-2`, and a listing that reshuffles at the tenth child
    // looks like entries appearing out of order.
    let registry = SubagentRegistry::new();
    for index in 0..12 {
        registry.record_start(&format!("task {index}"), Depth::root());
    }

    let ids: Vec<String> = registry
        .entries()
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    assert_eq!(ids.first().map(String::as_str), Some("sa-0"));
    assert_eq!(ids.get(2).map(String::as_str), Some("sa-2"));
    assert_eq!(ids.last().map(String::as_str), Some("sa-11"));
}

#[test]
fn a_finished_child_keeps_its_answer_and_child_id() {
    let registry = SubagentRegistry::new();
    let id = registry.record_start("summarize", Depth::root().child());

    assert!(registry.record_finished(&id, &delegation("it parses JSON", 3)));

    let entry = registry.get(&id).expect("recorded");
    assert!(!entry.is_running());
    assert_eq!(entry.answer(), Some("it parses JSON"));
    assert_eq!(
        entry.status,
        Status::Finished {
            child_id: AgentId::new("agent/1").expect("valid"),
            turns: 3,
        }
    );
}

#[test]
fn a_finished_child_with_blank_text_has_no_answer() {
    // `delegate` folds "finished with no answer" into an error, but the registry records what it
    // actually got: an entry that claimed an answer of "" would render as an empty success.
    let registry = SubagentRegistry::new();
    let id = registry.record_start("say nothing", Depth::root());
    registry.record_finished(&id, &delegation("   \n", 1));

    let entry = registry.get(&id).expect("recorded");
    assert_eq!(entry.answer(), None);
    assert!(matches!(entry.status, Status::Finished { .. }));
}

#[test]
fn a_failure_stays_visible_rather_than_vanishing() {
    // An entry that disappeared would make "the child crashed" indistinguishable from "nothing was
    // spawned", and the model would delegate again instead of reporting the failure.
    let registry = SubagentRegistry::new();
    let id = registry.record_start("doomed", Depth::root());
    assert!(registry.record_failed(&id, "provider exploded"));

    let entries = registry.entries();
    assert_eq!(entries.len(), 1, "a failed spawn is still a spawn");
    assert_eq!(entries[0].status.label(), "failed");
    assert!(!entries[0].is_running());
}

#[test]
fn recording_against_an_unknown_handle_reports_false() {
    // Not an error to panic on: a caller that lost its handle should get a signal it can log.
    let registry = SubagentRegistry::new();
    assert!(!registry.record_finished("sa-99", &delegation("ignored", 1)));
    assert!(!registry.record_failed("sa-99", "ignored"));
}

#[test]
fn a_long_prompt_is_collapsed_onto_one_line_and_elided() {
    // A listing row is one line. A raw newline inside a prompt would make one entry look like two,
    // and an unbounded prompt would push the other rows off the screen.
    let registry = SubagentRegistry::new();
    let prompt = format!("first line\n\nsecond   line {}", "x".repeat(200));
    let id = registry.record_start(&prompt, Depth::root());

    let description = registry.get(&id).expect("recorded").description;
    assert!(!description.contains('\n'), "{description:?}");
    assert!(!description.contains("  "), "whitespace is collapsed");
    assert!(description.ends_with('…'), "{description:?}");
    assert!(
        description.chars().count() <= 81,
        "one elision character over the bound: {description:?}"
    );
}

// ---- subagent_list ------------------------------------------------------------------------------

#[tokio::test]
async fn listing_says_so_when_nothing_was_spawned() {
    let tool = subagent_list_tool(SubagentRegistry::shared());
    let result = call(&tool, serde_json::json!({})).await;

    assert!(!result.is_error, "an empty listing is not a failure");
    assert!(text_of(&result).contains("no subagents"), "{result:?}");
}

#[tokio::test]
async fn listing_renders_one_row_per_subagent_with_its_state() {
    let registry = SubagentRegistry::shared();
    let running = registry.record_start("still working", Depth::root());
    let done = registry.record_start("already done", Depth::root());
    registry.record_finished(&done, &delegation("the answer", 2));
    let broken = registry.record_start("went wrong", Depth::root());
    registry.record_failed(&broken, "boom");

    let tool = subagent_list_tool(Arc::clone(&registry));
    let result = call(&tool, serde_json::json!({})).await;
    let text = text_of(&result);

    assert!(!result.is_error);
    assert!(text.contains("3 subagent(s)"), "{text}");
    assert!(
        text.contains(&running) && text.contains("[running]"),
        "{text}"
    );
    assert!(
        text.contains(&done) && text.contains("[finished]"),
        "{text}"
    );
    assert!(
        text.contains(&broken) && text.contains("[failed]"),
        "{text}"
    );
    assert!(text.contains("boom"), "the failure reason is shown: {text}");
    // The description is what a model uses to pick an id, so it must be in the row.
    assert!(text.contains("still working"), "{text}");
}

// ---- subagent_wait ------------------------------------------------------------------------------

#[tokio::test]
async fn waiting_on_a_finished_child_returns_its_answer_immediately() {
    let registry = SubagentRegistry::shared();
    let id = registry.record_start("answer me", Depth::root());
    registry.record_finished(&id, &delegation("forty-two", 1));

    let tool = subagent_wait_tool(registry);
    // A one-millisecond budget proves there is no sleeping: the answer is already known.
    let result = call(&tool, serde_json::json!({ "id": id, "timeout_ms": 1 })).await;

    assert!(!result.is_error, "{result:?}");
    assert_eq!(text_of(&result), "forty-two");
}

#[tokio::test]
async fn waiting_on_a_child_that_finished_without_an_answer_is_an_error() {
    // The failure mode this guards: returning "" would read as the child having answered with
    // nothing, which is a different and much less alarming statement.
    let registry = SubagentRegistry::shared();
    let id = registry.record_start("silent", Depth::root());
    registry.record_finished(&id, &delegation("", 4));

    let tool = subagent_wait_tool(registry);
    let result = call(&tool, serde_json::json!({ "id": id })).await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("4 turn"), "{result:?}");
}

#[tokio::test]
async fn waiting_on_a_failed_child_reports_the_failure() {
    let registry = SubagentRegistry::shared();
    let id = registry.record_start("doomed", Depth::root());
    registry.record_failed(&id, "provider exploded");

    let tool = subagent_wait_tool(registry);
    let result = call(&tool, serde_json::json!({ "id": id })).await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("provider exploded"), "{result:?}");
}

#[tokio::test]
async fn waiting_on_a_running_child_times_out_without_pretending_it_finished() {
    // The child never completes: nothing publishes a terminal state, so the wait must be refused
    // rather than silently returning `running` as if it were an answer.
    let registry = SubagentRegistry::shared();
    let id = registry.record_start("never ends", Depth::root());

    let tool = subagent_wait_tool(Arc::clone(&registry));
    let result = call(&tool, serde_json::json!({ "id": id, "timeout_ms": 1 })).await;

    assert!(result.is_error, "a timeout is not a success");
    let text = text_of(&result);
    assert!(text.contains("still running"), "{text}");
    assert!(
        text.contains("not failed"),
        "the model must not conclude the child broke: {text}"
    );
    // And the registry still holds it as running, so a later listing can retry it.
    assert!(registry.get(&id).expect("recorded").is_running());
}

#[tokio::test]
async fn a_running_child_is_woken_by_the_task_that_finishes_it() {
    // This is the only situation in which `wait` actually blocks, and it is the reason a `watch`
    // channel is used instead of polling: the waiter must be woken by the transition itself.
    let registry = SubagentRegistry::shared();
    let id = registry.record_start("worker", Depth::root());

    let finisher = {
        let registry = Arc::clone(&registry);
        let id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            registry.record_finished(&id, &delegation("done at last", 7));
        })
    };

    let entry = registry
        .wait(&id, Duration::from_secs(5), &CancellationToken::new())
        .await
        .expect("the child finished inside the budget");

    finisher.await.expect("the finisher task ran");
    assert_eq!(entry.answer(), Some("done at last"));
    assert!(!entry.is_running());
}

#[tokio::test]
async fn waiting_on_an_unknown_id_says_where_to_look() {
    let tool = subagent_wait_tool(SubagentRegistry::shared());
    let result = call(&tool, serde_json::json!({ "id": "sa-404" })).await;

    assert!(result.is_error);
    let text = text_of(&result);
    assert!(text.contains("sa-404"), "{text}");
    assert!(
        text.contains("subagent_list"),
        "say what to do next: {text}"
    );
}

#[tokio::test]
async fn waiting_without_an_id_is_refused_before_waiting() {
    let tool = subagent_wait_tool(SubagentRegistry::shared());
    for args in [
        serde_json::json!({}),
        serde_json::json!({ "id": "   " }),
        serde_json::json!({ "id": 7 }),
    ] {
        let result = call(&tool, args).await;
        assert!(result.is_error, "expected a refusal");
        assert!(text_of(&result).contains("`id` is required"), "{result:?}");
    }
}

#[tokio::test]
async fn a_cancelled_wait_reports_cancellation_rather_than_a_result() {
    // A user interrupt must not look like a child that never finished, or the model would retry.
    let registry = SubagentRegistry::shared();
    let id = registry.record_start("slow", Depth::root());
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = registry
        .wait(&id, Duration::from_secs(30), &cancel)
        .await
        .expect_err("an already-cancelled wait must not block");

    assert_eq!(
        error,
        subagent_tools::WaitError::Cancelled { id: id.clone() }
    );
}

#[test]
fn a_huge_timeout_is_clamped_so_one_call_cannot_pin_the_turn() {
    // Asserted on the clamp rather than by waiting: sleeping out `u64::MAX` is not a test.
    let clamped = subagent_tools::wait_timeout_ms(&serde_json::json!({ "timeout_ms": u64::MAX }));
    assert_eq!(clamped, MAX_WAIT_MS);

    // A missing or zero budget must still yield once rather than spin.
    assert_eq!(
        subagent_tools::wait_timeout_ms(&serde_json::json!({})),
        subagent_tools::DEFAULT_WAIT_MS
    );
    assert_eq!(
        subagent_tools::wait_timeout_ms(&serde_json::json!({ "timeout_ms": 0 })),
        subagent_tools::MIN_WAIT_MS
    );
}

#[tokio::test]
async fn a_still_running_child_times_out_without_blocking_past_the_budget() {
    // The clamp is what keeps the wait cheap; this proves the tool honours a small budget rather
    // than waiting for a transition that never arrives.
    let registry = SubagentRegistry::shared();
    let id = registry.record_start("never ends", Depth::root());

    let started = std::time::Instant::now();
    let error = registry
        .wait(&id, Duration::from_millis(50), &CancellationToken::new())
        .await
        .expect_err("a still-running child times out");

    assert!(started.elapsed() < Duration::from_secs(5), "{error:?}");
    assert!(
        matches!(error, subagent_tools::WaitError::Timeout { .. }),
        "{error:?}"
    );
}

// ---- subagent_send ------------------------------------------------------------------------------

#[tokio::test]
async fn sending_to_a_running_child_refuses_and_names_what_is_missing() {
    // The deliverable is the *error*, so its content is the assertion: a model that reads it must be
    // able to tell that the harness lacks a channel, not that it passed a bad argument.
    let registry = SubagentRegistry::shared();
    let id = registry.record_start("busy", Depth::root());

    let tool = subagent_send_tool(registry);
    let result = call(
        &tool,
        serde_json::json!({ "id": id, "message": "also check the tests" }),
    )
    .await;

    assert!(result.is_error, "a send that delivered nothing is an error");
    let text = text_of(&result);
    assert!(text.contains("no channel into a child agent"), "{text}");
    assert!(
        text.contains("running"),
        "it names the child's state: {text}"
    );
    // Each missing piece, so the report is actionable rather than merely negative.
    assert!(text.contains("one-shot future"), "{text}");
    assert!(text.contains("inbox"), "{text}");
    assert!(text.contains("steering"), "{text}");
}

#[tokio::test]
async fn sending_to_a_finished_child_refuses_too() {
    // A one-shot child is not resumable either: `delegate` dropped its session when it returned.
    let registry = SubagentRegistry::shared();
    let id = registry.record_start("done", Depth::root());
    registry.record_finished(&id, &delegation("finished", 1));

    let tool = subagent_send_tool(registry);
    let result = call(
        &tool,
        serde_json::json!({ "id": id, "message": "one more thing" }),
    )
    .await;

    assert!(result.is_error);
    let text = text_of(&result);
    assert!(text.contains("finished"), "{text}");
    assert!(text.contains("MemorySession"), "{text}");
}

#[tokio::test]
async fn sending_requires_both_an_id_and_a_message() {
    let registry = SubagentRegistry::shared();
    let id = registry.record_start("busy", Depth::root());
    let tool = subagent_send_tool(registry);

    for args in [
        serde_json::json!({}),
        serde_json::json!({ "id": id }),
        serde_json::json!({ "id": id, "message": "   " }),
    ] {
        let result = call(&tool, args).await;
        assert!(result.is_error);
        assert!(text_of(&result).contains("required"), "{result:?}");
    }
}

#[tokio::test]
async fn sending_to_an_unknown_child_points_at_the_listing() {
    let tool = subagent_send_tool(SubagentRegistry::shared());
    let result = call(
        &tool,
        serde_json::json!({ "id": "sa-404", "message": "hello" }),
    )
    .await;

    assert!(result.is_error);
    let text = text_of(&result);
    assert!(text.contains("sa-404"), "{text}");
    assert!(text.contains("subagent_list"), "{text}");
}

// ---- the traced spawn ---------------------------------------------------------------------------

#[tokio::test]
async fn a_traced_spawn_runs_the_child_and_records_its_answer() {
    let registry = SubagentRegistry::shared();
    let ctx = context("the child's finding", Depth::root(), 2);
    let tool = traced_subagent_tool(Arc::clone(&ctx), 2, Arc::clone(&registry));

    // The depth budget is the only thing `delegate` refuses without a model, and a real provider is
    // cheap to fake, so this runs the genuine delegation path rather than a stub.
    let result = call(&tool, serde_json::json!({ "prompt": "look at the parser" })).await;

    assert!(!result.is_error, "{result:?}");
    let entries = registry.entries();
    assert_eq!(entries.len(), 1, "the spawn is tracked");
    let entry = &entries[0];
    assert!(!entry.is_running(), "a spawn call returns only when done");
    assert_eq!(entry.answer(), Some("the child's finding"));
    assert_eq!(entry.description, "look at the parser");
    assert_eq!(
        entry.depth,
        Depth::root(),
        "the record is the caller's depth"
    );
}

#[tokio::test]
async fn a_traced_spawn_at_the_ceiling_leaves_no_ghost_entry() {
    // A refusal must not appear in a listing as work in progress: the model would wait on a child
    // that was never started.
    let depth = Depth::root().child().child();
    let ctx = context("never reached", depth, 2);
    let registry = SubagentRegistry::shared();
    let tool = traced_subagent_tool(ctx, 2, Arc::clone(&registry));

    let result = call(&tool, serde_json::json!({ "prompt": "delegate again" })).await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("depth 2"), "{result:?}");
    assert!(
        registry.entries().is_empty(),
        "a refused spawn is not tracked"
    );
}

#[tokio::test]
async fn a_traced_spawn_rejects_an_empty_prompt_without_tracking_it() {
    let ctx = context("unused", Depth::root(), 2);
    let registry = SubagentRegistry::shared();
    let tool = traced_subagent_tool(ctx, 2, Arc::clone(&registry));

    for args in [serde_json::json!({}), serde_json::json!({ "prompt": "  " })] {
        let result = call(&tool, args).await;
        assert!(result.is_error);
        assert!(
            text_of(&result).contains("`prompt` is required"),
            "{result:?}"
        );
    }
    assert!(registry.entries().is_empty());
}

#[test]
fn the_traced_tool_keeps_the_name_and_arguments_of_the_untraced_one() {
    // A drop-in replacement: a model that learned `subagent` from the untraced tool must not have to
    // relearn it, and a deployment swaps one registration for the other without changing a prompt.
    let ctx = context("unused", Depth::root(), 2);
    let traced = traced_subagent_tool(ctx, 2, SubagentRegistry::shared());
    let untraced = girder_core::subagent_tool(context("unused", Depth::root(), 2), 2);

    assert_eq!(traced.name, untraced.name);
    assert_eq!(traced.parameters, untraced.parameters);
}

// ---- registration -------------------------------------------------------------------------------

#[test]
fn registration_mounts_all_four_tools() {
    let tools = ToolRegistry::new();
    let ctx = context("unused", Depth::root(), 2);

    register_subagent_tools(&tools, SubagentRegistry::shared(), ctx, 2);

    let names = tools.names();
    for expected in [
        "subagent",
        "subagent_list",
        "subagent_send",
        "subagent_wait",
    ] {
        assert!(
            names.contains(&expected.to_owned()),
            "missing `{expected}`: {names:?}"
        );
    }
    assert_eq!(tools.len(), 4);
}

#[test]
fn the_scratch_directory_is_isolated() {
    // Guards the isolation helper itself: two calls must not hand back the same path, or a future
    // test that writes would race another test in the same binary.
    let first = scratch("a");
    let second = scratch("a");

    assert_ne!(first, second);
    assert!(first.starts_with(std::env::temp_dir()), "{first:?}");
    assert!(first.exists(), "the directory is created: {first:?}");
}
