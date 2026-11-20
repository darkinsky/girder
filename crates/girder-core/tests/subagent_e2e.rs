//! End-to-end tests for the delegation path: admission, release, depth, interrupt, and failure.
//!
//! Everything here drives the *real* pipeline — `traced_subagent_tool` → `SubagentRegistry` →
//! `delegate` → `core::agent_loop::run` → a provider — instead of the primitives in isolation. That is
//! deliberate: `subagent_pool.rs`, `subagent_depth.rs`, `subagent_interrupt.rs` and
//! `subagent_tools.rs` each have unit tests, and a unit test of a part cannot show that the parts
//! are wired to each other or that the numbers they hold agree with the text a parent's model reads.
//! Every case below therefore asserts a *model-facing* outcome together with the exact bookkeeping,
//! because those two are the things that must not disagree.
//!
//! # Why the providers hold children open
//!
//! The interesting property is how the harness behaves while a child is *in flight*, which no
//! provider that answers immediately can show: a bound, a release, and an interrupt are all
//! statements about the window in which work exists. Three providers exist for that reason:
//!
//! - [`GatedAnswer`] parks each child inside its first model call until the test opens that child's
//!   gate, so "the pool is full" and "the child is still running" are *observed* rather than raced;
//! - [`CountingAnswer`] counts model calls, which is how a test shows that a refused delegation
//!   costs nothing;
//! - [`FlakyAnswer`] fails exactly once, so the failure path and the recovery after it are both
//!   real delegations rather than recorded states.
//!
//! # What is deliberately *not* asserted here
//!
//! Two guarantees are compile-time, not runtime: a [`girder_core::SubagentSlot`] cannot be released
//! twice (it is neither `Copy` nor `Clone`), and it cannot be shared across threads while held
//! (there is one owner). No test asserts a panic the type system makes unreachable.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "tests assert by panicking: a failed unwrap is a failed test; the indexes are over \
              fixed-size slices built in the test itself, and the long cases are kept as one \
              narrative so the sequence a reader follows is the sequence that ran"
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, PoisonError};
use std::time::Duration;

use girder_core::{
    AgentId, BoxFut, Cancellers, ChunkStream, ContentBlock, Depth, EventBus, FinishReason,
    Interrupter, LlmError, LlmProvider, LlmRequest, LoopContext, MemorySession, Session,
    StreamChunk, SubagentPool, SubagentRegistry, SubagentStatus, ToolCallId, ToolDefinition,
    ToolExecution, ToolRegistry, ToolRegistryView, ToolResult, ToolRunContext,
    subagent_interrupt_tool, subagent_tool, traced_subagent_tool,
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

// ---- providers ---------------------------------------------------------------------------------

/// A provider that parks each child until that child's own gate is opened.
///
/// Gates are handed out in call order, which is what lets one test finish *one* child while its
/// sibling is still parked — the case a shared gate cannot express, and the case that proves a
/// released place is reusable rather than merely counted.
struct GatedAnswer {
    /// One gate per model call, in call order. A call with no gate left is a test-construction bug
    /// and panics rather than passing silently as an answer that was never gated.
    gates: Mutex<VecDeque<Arc<Semaphore>>>,
    /// How many children have reached their model call. Polled by tests so they never sleep a fixed
    /// amount and hope.
    entered: AtomicUsize,
    /// Mints one answer per call, so two children that ran are distinguishable in the results.
    answer_seq: AtomicUsize,
}

impl GatedAnswer {
    fn new(gates: Vec<Arc<Semaphore>>) -> Self {
        Self {
            gates: Mutex::new(gates.into()),
            entered: AtomicUsize::new(0),
            answer_seq: AtomicUsize::new(0),
        }
    }

    fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }
}

impl LlmProvider for GatedAnswer {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>> {
        let gate = {
            let mut gates = self.gates.lock().unwrap_or_else(PoisonError::into_inner);
            gates.pop_front()
        };
        let answer = format!("answer-{}", self.answer_seq.fetch_add(1, Ordering::SeqCst));
        self.entered.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let gate = gate.expect("every gated model call must be given a gate");
            // The permit is forgotten rather than returned: this is a one-shot release, and the
            // pool under test is the subagent pool, not this test's gate.
            gate.acquire_owned()
                .await
                .expect("the gate is never closed")
                .forget();
            Ok(text_stream(&answer))
        })
    }
}

/// A provider that answers immediately and counts how often it was called.
///
/// The count is the evidence for "a refusal spends nothing": the depth ceiling is checked before
/// the child's first turn, so a refused delegation must not appear in this counter.
struct CountingAnswer {
    answer: String,
    calls: AtomicUsize,
}

impl CountingAnswer {
    fn new(answer: &str) -> Self {
        Self {
            answer: answer.to_owned(),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl LlmProvider for CountingAnswer {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = self.answer.clone();
        Box::pin(async move { Ok(text_stream(&answer)) })
    }
}

/// A provider whose first model call fails and whose later calls succeed.
///
/// One provider for both halves of the failure case: the parent must see the error *and* the same
/// parent must be able to delegate again afterwards, which is only meaningful if the second
/// delegation is a genuine run on the same wiring.
struct FlakyAnswer {
    calls: AtomicUsize,
}

impl FlakyAnswer {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

impl LlmProvider for FlakyAnswer {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if call == 0 {
                return Err(LlmError::Provider("provider exploded".to_owned()));
            }
            Ok(text_stream("recovered"))
        })
    }
}

/// One visible-text answer, as the assembler expects to receive it.
fn text_stream(text: &str) -> ChunkStream {
    let chunks = vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "text".to_owned(),
        },
        StreamChunk::TextDelta {
            index: 0,
            text: text.to_owned(),
        },
        StreamChunk::Finish {
            reason: FinishReason::Stop,
        },
    ];
    Box::pin(futures::stream::iter(chunks))
}

// ---- harness helpers ---------------------------------------------------------------------------

/// A loop context at `depth` with `max_depth` as its delegation ceiling.
fn context(provider: Arc<dyn LlmProvider>, depth: Depth, max_depth: u32) -> Arc<LoopContext> {
    let tools = Arc::new(ToolRegistry::new()) as Arc<dyn ToolRegistryView>;
    Arc::new(
        LoopContext::new(
            provider,
            tools,
            Arc::new(MemorySession::new()) as Arc<dyn Session>,
            Arc::new(EventBus::new()),
            "test-model",
            AgentId::new("agent").expect("a literal id is valid"),
        )
        .with_depth(depth, max_depth),
    )
}

/// The context a turn hands a tool body.
fn run_context(cancel: CancellationToken) -> ToolRunContext {
    ToolRunContext {
        execution: ToolExecution {
            call_id: ToolCallId::new("c1").expect("a literal id is valid"),
            name: "subagent".to_owned(),
            arguments: "{}".to_owned(),
            agent_id: AgentId::new("parent").expect("a literal id is valid"),
        },
        cancel,
        ..Default::default()
    }
}

/// Runs a tool body the way the executor does.
async fn call(tool: &ToolDefinition, args: serde_json::Value) -> ToolResult {
    call_with(tool, args, run_context(CancellationToken::new())).await
}

/// Runs a tool body with a caller-supplied run context.
async fn call_with(
    tool: &ToolDefinition,
    args: serde_json::Value,
    ctx: ToolRunContext,
) -> ToolResult {
    (tool.execute)(args, ctx)
        .await
        .expect("a tool body returns a model-facing result, not an error")
}

/// Every text block of a result, concatenated.
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

/// Waits until `predicate` holds, then returns; panics with `what` after a generous bound.
///
/// Polling rather than sleeping a fixed amount: a fixed sleep that is long enough on this machine is
/// a test that fails on a slower one, and one that is short enough is a race.
async fn wait_for(predicate: impl Fn() -> bool, what: &str) {
    for _ in 0..2_000 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---- 1. the pool bound, asserted at the primitive and at the tool -------------------------------

#[test]
fn a_pool_of_two_admits_exactly_two_of_three_requests() {
    // The primitive is checked first, because "exactly the capacity, never one more" is the
    // invariant every caller of `try_acquire` inherits; the tool-level case below then shows the
    // same bound reaching the model as a refusal it can act on.
    let pool = SubagentPool::new(2);
    let first = pool.try_acquire().expect("a pool of two admits two");
    let second = pool.try_acquire().expect("a pool of two admits two");

    assert_eq!(pool.active(), 2, "both places are held");
    assert!(!pool.has_room());
    assert!(
        pool.try_acquire().is_none(),
        "the third request must be refused, not queued: the caller decides whether to wait"
    );
    assert_eq!(pool.active(), 2, "a refusal must not consume a place");

    pool.release(first);
    assert_eq!(pool.active(), 1);
    pool.release(second);
    assert_eq!(pool.active(), 0, "every place was handed back");
}

#[tokio::test]
async fn three_concurrent_delegations_into_a_registry_of_two_admit_exactly_two() {
    // Two children are parked inside their model call, so the pool really is full when the third
    // request arrives. That is the only way to tell "refused because full" apart from "refused
    // because the first two had already finished".
    const CAPACITY: usize = 2;
    const CEILING: u32 = 2;

    let gate = Arc::new(Semaphore::new(0));
    // Two gates for the two places: a third child must never reach a model call.
    let provider = Arc::new(GatedAnswer::new(vec![Arc::clone(&gate), Arc::clone(&gate)]));
    let ctx = context(provider.clone(), Depth::root(), CEILING);
    let registry = Arc::new(SubagentRegistry::with_concurrency(CAPACITY));
    let tool = Arc::new(traced_subagent_tool(
        Arc::clone(&ctx),
        CEILING,
        Arc::clone(&registry),
    ));

    let spawn = |prompt: &'static str| {
        let tool = Arc::clone(&tool);
        tokio::spawn(async move { call(&tool, serde_json::json!({ "prompt": prompt })).await })
    };

    let first = spawn("first task");
    wait_for(|| registry.active() == 1, "the first child to be recorded").await;
    let second = spawn("second task");
    wait_for(
        || registry.active() == CAPACITY,
        "the second child to be recorded",
    )
    .await;
    assert_eq!(
        registry.active(),
        CAPACITY,
        "the bound is reached, not exceeded"
    );
    assert!(!registry.has_room());

    // The third request is made while both places are held, on this task.
    let refusal = call(&tool, serde_json::json!({ "prompt": "third task" })).await;

    assert!(
        refusal.is_error,
        "a full pool refuses rather than admitting"
    );
    let text = text_of(&refusal);
    assert!(
        text.contains("delegation refused"),
        "the refusal must be model-facing, not a silent failure: {text}"
    );
    assert!(
        text.contains("2 subagent(s) are running"),
        "the refusal must report the live count the caller is competing with: {text}"
    );
    assert!(
        text.contains("allows 2"),
        "the refusal must report the bound as configured: {text}"
    );
    assert!(
        text.contains("Wait for one to finish"),
        "the refusal must say what to do instead: {text}"
    );
    assert_eq!(
        registry.active(),
        CAPACITY,
        "a refused delegation must not hold a place"
    );
    assert_eq!(
        registry.entries().len(),
        CAPACITY,
        "a refused delegation must not be recorded as work in progress"
    );

    // Open the gates: both parked children answer.
    gate.add_permits(64);
    let first_result = first.await.expect("the first caller must not panic");
    let second_result = second.await.expect("the second caller must not panic");

    assert!(!first_result.is_error, "{first_result:?}");
    assert!(!second_result.is_error, "{second_result:?}");
    let answers = [text_of(&first_result), text_of(&second_result)];
    assert_ne!(
        answers[0], answers[1],
        "the two admitted places belonged to two distinct children: {answers:?}"
    );
    assert_eq!(registry.active(), 0, "both places are back after the run");
    assert!(registry.has_room());
}

// ---- 2. a released place is usable again --------------------------------------------------------

#[test]
fn releasing_a_place_makes_it_usable_again() {
    // Splitting release from reuse is what catches a "pool" that only decrements a counter: the
    // place must be *usable*, which means a later `try_acquire` succeeds and then the pool is full
    // again, not merely that the count went down.
    let pool = SubagentPool::new(2);
    let first = pool.try_acquire().expect("a pool of two admits two");
    let second = pool.try_acquire().expect("a pool of two admits two");
    assert!(pool.try_acquire().is_none());

    pool.release(first);
    assert_eq!(pool.active(), 1, "exactly one place was handed back");
    assert!(pool.has_room());

    let reused = pool.try_acquire().expect("the released place is usable");
    assert_eq!(pool.active(), 2, "reuse fills the pool again");
    assert!(
        pool.try_acquire().is_none(),
        "reuse must not have created a third place"
    );

    // Dropping a slot returns its place too, which is what keeps an aborted task from leaking
    // capacity forever.
    drop(reused);
    assert_eq!(pool.active(), 1, "a dropped slot hands its place back");
    drop(second);
    assert_eq!(pool.active(), 0);
}

#[tokio::test]
async fn finishing_one_delegation_admits_the_next_one() {
    // The same property through the real pipeline: a delegation that ends must make room for the
    // next request, including when the one that ended was the *older* of two.
    const CEILING: u32 = 2;

    let gate_first = Arc::new(Semaphore::new(0));
    let gate_second = Arc::new(Semaphore::new(0));
    let gate_third = Arc::new(Semaphore::new(0));
    let provider = Arc::new(GatedAnswer::new(vec![
        Arc::clone(&gate_first),
        Arc::clone(&gate_second),
        Arc::clone(&gate_third),
    ]));
    let ctx = context(provider.clone(), Depth::root(), CEILING);
    let registry = Arc::new(SubagentRegistry::with_concurrency(2));
    let tool = Arc::new(traced_subagent_tool(
        Arc::clone(&ctx),
        CEILING,
        Arc::clone(&registry),
    ));

    let spawn = |prompt: &'static str| {
        let tool = Arc::clone(&tool);
        tokio::spawn(async move { call(&tool, serde_json::json!({ "prompt": prompt })).await })
    };

    let first = spawn("first task");
    wait_for(
        || provider.entered() == 1,
        "the first child to reach its model",
    )
    .await;
    let second = spawn("second task");
    wait_for(
        || provider.entered() == 2,
        "the second child to reach its model",
    )
    .await;
    assert_eq!(registry.active(), 2, "two of two are running");

    let refusal = call(&tool, serde_json::json!({ "prompt": "too early" })).await;
    assert!(refusal.is_error);
    assert!(
        text_of(&refusal).contains("2 subagent(s) are running"),
        "{}",
        text_of(&refusal)
    );

    // Finish only the first child. Its place must come back while the second is still parked.
    gate_first.add_permits(1);
    let first_result = first.await.expect("the first caller must not panic");
    assert!(!first_result.is_error, "{first_result:?}");
    wait_for(|| registry.active() == 1, "the first place to be released").await;
    assert!(registry.has_room(), "one of two running leaves room");

    // The next request is admitted by the place the first child gave back.
    let third = spawn("third task");
    wait_for(
        || provider.entered() == 3,
        "the third child to reach its model, which proves the place was reusable",
    )
    .await;
    assert_eq!(
        registry.active(),
        2,
        "the reused place brings the pool back to its bound"
    );

    // Drain in an order that is not the spawn order, so the test cannot pass by luck of ordering.
    gate_third.add_permits(1);
    let third_result = third.await.expect("the third caller must not panic");
    gate_second.add_permits(1);
    let second_result = second.await.expect("the second caller must not panic");

    assert!(!third_result.is_error, "{third_result:?}");
    assert!(!second_result.is_error, "{second_result:?}");
    let mut answers = [
        text_of(&first_result),
        text_of(&second_result),
        text_of(&third_result),
    ];
    answers.sort();
    assert_eq!(
        answers,
        ["answer-0", "answer-1", "answer-2"],
        "three delegations ran and produced three distinct answers"
    );
    assert_eq!(registry.active(), 0, "every place is back");
    assert_eq!(registry.entries().len(), 3);
    assert!(
        registry.entries().iter().all(|entry| !entry.is_running()),
        "a completed delegation is never left `running`"
    );
}

// ---- 3. the depth ceiling ----------------------------------------------------------------------

#[tokio::test]
async fn a_third_level_delegation_is_refused_and_the_error_names_the_depth() {
    // A three-level chain: root (depth 0) → child (depth 1) → grandchild (depth 2). The grandchild
    // is the "third level" and may not delegate, because the ceiling is 2. The provider's call
    // count is what shows the refusal happened *before* any model call: three agents exist, two
    // model rounds ran.
    const CEILING: u32 = 2;

    let provider = Arc::new(CountingAnswer::new("level finished"));
    let root = context(provider.clone(), Depth::root(), CEILING);
    let level_one = context(provider.clone(), Depth::new(1), CEILING);
    let level_two = context(provider.clone(), Depth::new(2), CEILING);

    let child = girder_core::delegate(
        &root,
        "first level".to_owned(),
        Depth::root(),
        CEILING,
        CancellationToken::new(),
    )
    .await
    .expect("the root may delegate");
    assert_eq!(
        child.child_id.as_str(),
        "agent/1",
        "the child is one deeper"
    );
    assert_eq!(provider.calls(), 1);

    let grandchild = girder_core::delegate(
        &level_one,
        "second level".to_owned(),
        Depth::new(1),
        CEILING,
        CancellationToken::new(),
    )
    .await
    .expect("depth 1 may delegate under a ceiling of 2");
    assert_eq!(grandchild.child_id.as_str(), "agent/2");
    assert_eq!(provider.calls(), 2);

    // The third level: depth 2 is the ceiling itself, and it is terminal.
    let error = girder_core::delegate(
        &level_two,
        "third level".to_owned(),
        Depth::new(2),
        CEILING,
        CancellationToken::new(),
    )
    .await
    .expect_err("a depth-2 agent must not delegate under a ceiling of 2");

    assert!(
        matches!(
            error,
            girder_core::SubagentError::DepthExceeded { depth: 2, max: 2 }
        ),
        "the refusal must carry both numbers, got {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("depth 2"),
        "the message must name the depth that was refused: {message}"
    );
    assert!(
        message.contains("limit of 2"),
        "the message must name the configured ceiling: {message}"
    );
    assert_eq!(
        provider.calls(),
        2,
        "a refused delegation must not reach the model"
    );

    // The same refusal as the parent's model sees it: a result it can act on, naming the depth.
    let tool = subagent_tool(Arc::clone(&level_two), CEILING);
    let result = call(&tool, serde_json::json!({ "prompt": "delegate again" })).await;
    assert!(result.is_error);
    let text = text_of(&result);
    assert!(text.contains("depth 2"), "{text}");
    assert!(text.contains("limit is 2"), "{text}");
    assert!(
        text.contains("Do this work yourself"),
        "the refusal must tell the model what to do instead: {text}"
    );
    assert_eq!(
        provider.calls(),
        2,
        "the tool's refusal must not spend a model call either"
    );
}

#[tokio::test]
async fn the_delegation_tool_reads_its_build_time_depth_not_the_calling_childs() {
    // A tool carries the depth it was built with, and nothing about calling it changes that. That is
    // correct for a tool and was the bug one level up: `delegate` handed the child the parent's whole
    // registry, so a child's `subagent` tool reported the *parent's* depth and a chain never reached
    // the ceiling. `delegate` now wraps the child's registry in `DepthLimitedTools`, which is the
    // fix; this test stays because the property it names is still true and is what makes the wrapper
    // necessary -- passing the same registry twice would silently undo it.
    const CEILING: u32 = 1;

    let provider = Arc::new(CountingAnswer::new("delegated anyway"));
    let root = context(provider.clone(), Depth::root(), CEILING);
    let registry = Arc::new(SubagentRegistry::with_concurrency(4));
    let tool = traced_subagent_tool(Arc::clone(&root), CEILING, Arc::clone(&registry));

    // Two delegations, as a root and then its child would issue them. The ceiling of 1 admits only
    // depth 0, so the second call is the one a depth-aware tool would refuse.
    for _ in 0..2 {
        let result = call(&tool, serde_json::json!({ "prompt": "one more level" })).await;
        assert!(
            !result.is_error,
            "the tool delegates at its build-time depth, so the ceiling does not refuse: {}",
            text_of(&result)
        );
    }

    let depths: Vec<u32> = registry
        .entries()
        .iter()
        .map(|entry| entry.depth.get())
        .collect();
    assert_eq!(
        depths,
        [0, 0],
        "both delegations were recorded at the tool's build-time depth, not the child's"
    );
    assert_eq!(provider.calls(), 2, "both delegations really ran");
}

// ---- 4. interrupting a running child -----------------------------------------------------------

#[tokio::test]
async fn interrupting_a_running_child_is_recorded_and_the_result_says_it_was_interrupted() {
    // The interrupt is a *request*: the tool must say so, the token the child holds must fire, and
    // the parent must end up with a result that says the work was interrupted rather than an answer
    // that silently vanished.
    const CEILING: u32 = 2;

    let gate = Arc::new(Semaphore::new(0));
    let provider = Arc::new(GatedAnswer::new(vec![Arc::clone(&gate)]));
    let ctx = context(provider.clone(), Depth::root(), CEILING);
    let registry = Arc::new(SubagentRegistry::with_concurrency(2));
    let tool = Arc::new(traced_subagent_tool(
        Arc::clone(&ctx),
        CEILING,
        Arc::clone(&registry),
    ));

    // A fresh registry mints `sa-0` for its first spawn, so the token can be armed before the
    // spawn exists. The token handed out here is the one the child must run under.
    let cancellers = Arc::new(Cancellers::new());
    let armed = cancellers.arm("sa-0");
    let child_token = armed.token();
    let interrupter: Arc<dyn Interrupter> = Arc::clone(&cancellers) as Arc<dyn Interrupter>;
    let interrupt_tool = subagent_interrupt_tool(Arc::clone(&registry), interrupter);

    let spawn = {
        let tool = Arc::clone(&tool);
        let cancel = child_token.clone();
        tokio::spawn(async move {
            call_with(
                &tool,
                serde_json::json!({ "prompt": "a long task" }),
                run_context(cancel),
            )
            .await
        })
    };

    wait_for(
        || registry.active() == 1 && provider.entered() == 1,
        "the child to be running and parked in its model call",
    )
    .await;
    assert!(
        !child_token.is_cancelled(),
        "nothing has interrupted it yet"
    );

    let interrupt_result = call(&interrupt_tool, serde_json::json!({ "id": "sa-0" })).await;
    assert!(
        !interrupt_result.is_error,
        "asking a running child to stop is not a failure: {}",
        text_of(&interrupt_result)
    );
    let interrupt_text = text_of(&interrupt_result);
    assert!(
        interrupt_text.contains("\"outcome\":\"requested\""),
        "the outcome must be the recorded request: {interrupt_text}"
    );
    assert!(
        interrupt_text.contains("may still be working"),
        "the tool must not claim the child already stopped: {interrupt_text}"
    );
    assert!(
        child_token.is_cancelled(),
        "the interrupt must reach the token the child runs under"
    );

    // Let the parked child reach its next safe point. It observes the cancelled token there, which
    // is what makes the interrupt effective rather than merely recorded.
    gate.add_permits(1);
    let child_result = spawn.await.expect("the child's caller must not panic");

    assert!(
        child_result.is_error,
        "an interrupted child has no answer to hand the parent: {child_result:?}"
    );
    let child_text = text_of(&child_result);
    assert!(
        child_text.contains("delegation cancelled"),
        "the parent's result must say the delegation was cancelled: {child_text}"
    );

    let entry = registry.get("sa-0").expect("the child is still tracked");
    assert!(!entry.is_running(), "the child reached a terminal state");
    if let SubagentStatus::Failed { reason } = &entry.status {
        assert!(
            reason.contains("cancelled"),
            "the failure must record that it was a cancellation: {reason}"
        );
    } else {
        panic!(
            "an interrupted child must be recorded as failed, got {:?}",
            entry.status
        );
    }
    assert_eq!(entry.answer(), None, "an interrupted child has no answer");
    assert_eq!(
        registry.active(),
        0,
        "an interrupted child must release its place"
    );
}

// ---- 5. concurrent acquire/release -------------------------------------------------------------

#[test]
fn concurrent_acquire_and_release_never_over_issues_or_leaks_a_place() {
    // Real OS threads, released together through a barrier, so the acquire/release window is
    // entered by several threads at the same instant. The in-flight count is sampled *after* each
    // successful acquire and before each release: if the pool ever admitted one too many, the
    // sample would read `capacity + 1`, which no amount of tidying up afterwards can hide.
    const CAPACITY: usize = 3;
    const THREADS: usize = 8;
    const ROUNDS: usize = 250;

    let pool = Arc::new(SubagentPool::new(CAPACITY));
    let inflight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let acquired = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(THREADS));

    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let pool = Arc::clone(&pool);
        let inflight = Arc::clone(&inflight);
        let peak = Arc::clone(&peak);
        let acquired = Arc::clone(&acquired);
        let refused = Arc::clone(&refused);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..ROUNDS {
                if let Some(slot) = pool.try_acquire() {
                    acquired.fetch_add(1, Ordering::SeqCst);
                    let holders = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(holders, Ordering::SeqCst);
                    assert!(
                        holders <= CAPACITY,
                        "the pool admitted {holders} holders with a capacity of {CAPACITY}"
                    );
                    // Widen the window so other threads really do race the release path.
                    std::hint::spin_loop();
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    pool.release(slot);
                } else {
                    refused.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }
    for handle in handles {
        handle.join().expect("a caller thread must not panic");
    }

    assert_eq!(
        inflight.load(Ordering::SeqCst),
        0,
        "every holder released before its thread finished"
    );
    assert_eq!(
        pool.active(),
        0,
        "no place leaked: the live count is back to zero"
    );
    assert!(pool.has_room());
    assert!(
        peak.load(Ordering::SeqCst) <= CAPACITY,
        "the observed peak never exceeded the bound"
    );
    assert_eq!(
        acquired.load(Ordering::SeqCst) + refused.load(Ordering::SeqCst),
        THREADS * ROUNDS,
        "every attempt was either admitted or refused, never lost"
    );

    // The pool is still exactly as large as it started: fill it again and check the bound.
    let mut held = Vec::new();
    for _ in 0..CAPACITY {
        held.push(pool.try_acquire().expect("the pool is still intact"));
    }
    assert_eq!(pool.active(), CAPACITY);
    assert!(
        pool.try_acquire().is_none(),
        "a leaked place would have left room for one more"
    );
    held.clear();
    assert_eq!(pool.active(), 0, "dropping the slots returned every place");
}

// ---- 6. a child failure stays a child failure --------------------------------------------------

#[tokio::test]
async fn a_failed_child_is_reported_to_the_parent_and_its_place_is_returned() {
    // A child's failure must be visible to the parent as a result it can read, must be recorded as
    // failed rather than vanishing, and must not consume a place: the parent has to be able to
    // delegate again in the same turn.
    const CAPACITY: usize = 2;
    const CEILING: u32 = 2;

    let provider = Arc::new(FlakyAnswer::new());
    let ctx = context(provider.clone(), Depth::root(), CEILING);
    let registry = Arc::new(SubagentRegistry::with_concurrency(CAPACITY));
    let tool = traced_subagent_tool(Arc::clone(&ctx), CEILING, Arc::clone(&registry));

    let failure = call(&tool, serde_json::json!({ "prompt": "will fail" })).await;

    assert!(
        failure.is_error,
        "the parent must be told, not handed an empty answer: {failure:?}"
    );
    let failure_text = text_of(&failure);
    assert!(
        failure_text.contains("the subagent failed"),
        "the result must say the child failed, not that the parent did: {failure_text}"
    );
    assert!(
        failure_text.contains("provider exploded"),
        "the child's own error must survive to the parent: {failure_text}"
    );

    let entries = registry.entries();
    assert_eq!(entries.len(), 1, "the failed spawn stays visible");
    if let SubagentStatus::Failed { reason } = &entries[0].status {
        assert!(
            reason.contains("provider exploded"),
            "the recorded reason must name the cause: {reason}"
        );
    } else {
        panic!(
            "a failed delegation is recorded as failed, got {:?}",
            entries[0].status
        );
    }
    assert_eq!(entries[0].answer(), None, "there is no answer to collect");
    assert_eq!(registry.active(), 0, "a failed child must return its place");
    assert!(registry.has_room());

    // The same parent delegates again: a child failure must not have poisoned the parent or its
    // pool accounting.
    let recovery = call(&tool, serde_json::json!({ "prompt": "try again" })).await;
    assert!(
        !recovery.is_error,
        "a failure must not break the next delegation: {recovery:?}"
    );
    assert_eq!(text_of(&recovery), "recovered");

    let entries = registry.entries();
    assert_eq!(entries.len(), 2, "both spawns are tracked");
    assert_eq!(entries[0].status.label(), "failed");
    assert_eq!(entries[1].status.label(), "finished");
    assert_eq!(entries[1].answer(), Some("recovered"));
    assert_eq!(registry.active(), 0, "no place is held after either run");
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "two delegations, two model calls: the failure and the recovery"
    );
}
