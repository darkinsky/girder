//! End-to-end tests for the tool-call pipeline: limits, approval, deadline, sanitizing, cache.
//!
//! Every module these tests touch already has a unit test of its own -- `argument_limits` measures
//! bytes, `approval_rules` decides actions, `tool_watchdog` does the deadline arithmetic,
//! `text_safety` rewrites escapes, `tool_cache` counts hits. What none of those can prove is that
//! the modules are *reached in the right order by a real call*. A unit test for `check` passes
//! whether or not `dispatch` calls it; a test for `sanitize` passes whether or not the loop ever
//! routes a result through it. That gap is what this file covers, and it is why each assertion below
//! is about an observable fact (a counter, a file on disk, a byte in the returned text) rather than
//! about the absence of a panic.
//!
//! # The order this file was written against
//!
//! Confirmed by reading `agent_loop::{dispatch, run}` before writing any test:
//!
//! 1. **tool lookup** -- an unknown name never reaches any other layer;
//! 2. **cache lookup** (`cached_result`) -- before the rule waterfall, so a hit is served without
//!    asking anyone (see [`a_cached_read_is_served_without_consulting_the_rules`], which pins that
//!    ordering because it means a `deny` rule does not apply to a cached call);
//! 3. **`ToolPreExecute` waterfall**, whose terminal is `decide_by_rules`; `Ask` is resolved through
//!    `ask_approver`, and no approver is a refusal, not a permission;
//! 4. **deadline** (`run_with_deadline`) wrapping the execute waterfall, which wraps
//! 5. **`run_body`**: `argument_limits::check` on the raw text *before* the JSON parse, then the
//!    parse, then the tool body;
//! 6. **cache invalidation** (`invalidate_after`) -- after the body, whatever its outcome;
//! 7. **`ToolPostExecute` waterfall**, then the cache `put` of what the model will actually see;
//! 8. **`spill_if_needed`**, on the loop side rather than in `dispatch`, where `sanitize_blocks` runs
//!    first: an escape sequence is removed before the budget counts the text, the spill store writes
//!    it, or the model reads it.
//!
//! Step 8 is the one ordering fact worth stating loudly: sanitizing is **not** part of `dispatch`.
//! A caller that invokes `dispatch` directly gets the tool's bytes verbatim, escape sequences and
//! all. [`sanitizing_happens_in_the_loop_not_in_dispatch`] asserts both halves of that, because the
//! distinction is invisible from the outside and a future refactor could move it either way.
//!
//! # Why the fixtures are the real tools
//!
//! `read` and `write` are the tools whose side effects the cache and the approval rules reason about,
//! so the tests that make claims about those layers use the real ones from `file_tools`. A stub with
//! a counter would prove that `dispatch` calls *something*; only the real tool proves that a denied
//! `write` left no file behind, which is the fact a user cares about.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::float_cmp
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use girder_core::{
    AgentId, ApprovalAction, ApprovalRule, ApprovalRules, ChunkStream, ContentBlock, EventBus,
    FinishReason, LlmError, LlmProvider, LlmRequest, LoopContext, LoopStep, MemorySession, Session,
    StreamChunk, TokenUsage, ToolCacheEntry, ToolCacheKey, ToolCallId, ToolDefinition,
    ToolExecution, ToolLimit, ToolRegistry, ToolRegistryView, ToolResult, WatchdogPolicy,
};
use tokio_util::sync::CancellationToken;

// ---- fixtures ----------------------------------------------------------------------------------

/// A workspace directory no other test can collide with.
///
/// A per-process counter rather than a random name: a failure has to be reproducible from the
/// directory name in the log, and two tests in one run must not share a file whose mtime matters.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-tools-e2e-{name}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// One tool call, spelled the way `dispatch` receives it from the loop.
fn execution(name: &str, arguments: &str) -> ToolExecution {
    ToolExecution {
        call_id: ToolCallId::new("call-1").expect("valid call id"),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
        agent_id: AgentId::new("agent-1").expect("valid agent id"),
    }
}

/// Drives one call through `dispatch`, which is the path the loop uses.
async fn dispatch_call(context: &LoopContext, name: &str, arguments: &str) -> ToolResult {
    girder_core::dispatch(
        context,
        execution(name, arguments),
        CancellationToken::new(),
    )
    .await
    .expect("dispatch itself does not fail for a policy refusal")
}

/// The text a result carries, concatenated.
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

/// A context over `tools`, with a fresh session and bus and nothing else configured.
fn context(tools: &Arc<ToolRegistry>) -> LoopContext {
    LoopContext::new(
        Arc::new(NoProvider),
        Arc::clone(tools) as Arc<dyn ToolRegistryView>,
        Arc::new(MemorySession::new()) as Arc<dyn Session>,
        Arc::new(EventBus::new()),
        "test-model",
        AgentId::new("agent-1").expect("valid agent id"),
    )
}

/// A provider that must never be called: these tests drive `dispatch` directly.
struct NoProvider;

impl LlmProvider for NoProvider {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<ChunkStream, LlmError>> {
        Box::pin(async { panic!("dispatch-only test must not reach the provider") })
    }
}

/// A provider that asks for one tool call, then answers with text.
///
/// Needed for the sanitizing test because `spill_if_needed` -- the only caller of
/// `sanitize_blocks` -- lives in `run`, not in `dispatch`. Anything observing that layer has to go
/// through the loop.
struct CallsOnceThenAnswers {
    tool: &'static str,
    arguments: &'static str,
    emitted: std::sync::atomic::AtomicBool,
}

impl LlmProvider for CallsOnceThenAnswers {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<ChunkStream, LlmError>> {
        let first = !self.emitted.swap(true, Ordering::SeqCst);
        let name = self.tool;
        let arguments = self.arguments;
        Box::pin(async move {
            let mut chunks = vec![
                StreamChunk::Usage(TokenUsage {
                    input: 1,
                    output: 1,
                    cached: 0,
                }),
                StreamChunk::BlockStart {
                    index: 0,
                    block_type: if first { "tool-call" } else { "text" }.to_owned(),
                },
            ];
            if first {
                chunks.push(StreamChunk::ToolCallDelta {
                    index: 0,
                    id: ToolCallId::new("call-1").expect("valid call id"),
                    name: Some(name.to_owned()),
                    arguments_delta: arguments.to_owned(),
                });
                chunks.push(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::ToolCall {
                        id: ToolCallId::new("call-1").expect("valid call id"),
                        name: name.to_owned(),
                        arguments: arguments.to_owned(),
                    },
                });
            } else {
                chunks.push(StreamChunk::TextDelta {
                    index: 0,
                    text: "done".to_owned(),
                });
                chunks.push(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text {
                        text: "done".to_owned(),
                    },
                });
            }
            chunks.push(StreamChunk::Finish {
                reason: if first {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                },
            });
            Ok(Box::pin(futures::stream::iter(chunks)) as ChunkStream)
        })
    }
}

/// Runs a turn and returns every `ToolFinished` content block the loop emitted.
async fn finished_content(
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
) -> Vec<Vec<ContentBlock>> {
    let session: Arc<dyn Session> = Arc::new(MemorySession::new());
    let context = LoopContext::new(
        provider,
        Arc::clone(&tools) as Arc<dyn ToolRegistryView>,
        session,
        Arc::new(EventBus::new()),
        "test-model",
        AgentId::new("agent-1").expect("valid agent id"),
    );
    let mut stream = Box::pin(girder_core::run(
        Arc::new(context),
        "go".to_owned(),
        CancellationToken::new(),
    ));
    let mut finished = Vec::new();
    while let Some(step) = stream.next().await {
        if let Ok(LoopStep::ToolFinished { content, .. }) = step {
            finished.push(content);
        }
    }
    finished
}

/// The file's modification time in the same unit `agent_loop::read_mtime` uses.
fn mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .expect("metadata")
        .modified()
        .expect("mtime")
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis() as u64
}

/// Rewrites a file and puts its modification time back exactly where it was.
///
/// This is the only way to test a cache hit against the *real* `read` tool: the cache is invalidated
/// when the mtime moves, so a hit can only be observed by changing the bytes while leaving the
/// timestamp alone. That is also the precise hazard the mtime gate cannot see on a coarse-grained
/// filesystem, which is why `cache_invalidate` exists.
fn rewrite_preserving_mtime(path: &Path, content: &str) {
    let before = std::fs::metadata(path)
        .expect("metadata")
        .modified()
        .expect("mtime");
    std::fs::write(path, content).expect("rewrite");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("reopen for timestamps");
    file.set_times(std::fs::FileTimes::new().set_modified(before))
        .expect("restore mtime");
}

// ---- 1. oversized arguments are refused before the tool runs -----------------------------------

#[tokio::test]
async fn oversized_arguments_are_refused_before_the_tool_body_runs() {
    // Two facts, and the second is the one that matters: an error message alone would also be
    // produced by a tool that ran and then complained, so the counter is what proves the refusal
    // happened *instead of* execution.
    let ran = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(ToolRegistry::new());
    tools.register(ToolDefinition::new(
        "probe",
        "records that it ran",
        serde_json::json!({"type": "object"}),
        {
            let ran = Arc::clone(&ran);
            Arc::new(
                move |_args: serde_json::Value, _ctx: girder_core::ToolRunContext| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(ToolResult::text("ran")) })
                },
            )
        },
    ));
    let context = context(&tools);

    // Deliberately *not* valid JSON: the default total limit is 4 MiB, and the size check runs
    // before the parser. If the order were reversed the result would be the JSON corrective
    // message, so the assertion below is about ordering and not only about rejection.
    let arguments = format!(
        "\"{}\"",
        "x".repeat(girder_core::default_argument_limits().max_total_bytes + 1)
    );
    let result = dispatch_call(&context, "probe", &arguments).await;

    assert!(result.is_error, "an oversized call is an error: {result:?}");
    let text = text_of(&result);
    assert!(
        text.contains("the arguments are") && text.contains("the limit is 4194304 bytes"),
        "the refusal must name the size that was measured and the limit it broke: {text}"
    );
    assert!(
        !text.contains("not valid JSON"),
        "the size check must run before the parse, so the parser never gets to report: {text}"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the tool body must not run for a call that was refused on size"
    );
}

// ---- 2. an `Ask` rule with no approver refuses instead of running ------------------------------

#[tokio::test]
async fn an_ask_rule_without_an_approver_refuses_and_leaves_no_side_effect() {
    // The side effect is a real file, written by the real `write` tool: after the refusal the path
    // must not exist. A counter would only say the body was not called; the absent file is the fact
    // the rule was written to guarantee.
    let workspace = scratch("approval");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::write_tool(workspace.clone()));

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Ask,
        reason: "writes to the workspace need a person".to_owned(),
    });

    // No `with_approver`: the deployment has no interactive surface, which is the case the refusal
    // exists for. Silently allowing an unanswered `Ask` is the one reading that cannot be undone.
    let context = context(&tools).with_approval_rules(rules);
    let target = workspace.join("must-not-exist.txt");
    let arguments = serde_json::json!({
        "path": target.display().to_string(),
        "content": "this must never be written",
    })
    .to_string();
    let result = dispatch_call(&context, "write", &arguments).await;

    assert!(
        result.is_error,
        "an unresolved `Ask` is a refusal: {result:?}"
    );
    let text = text_of(&result);
    assert!(
        text.contains("needs approval but no approver is configured"),
        "the refusal must say why nobody could answer: {text}"
    );
    assert!(
        text.contains("writes to the workspace need a person") && text.contains("rule 0"),
        "the refusal must name the rule that asked, otherwise a user cannot find it: {text}"
    );
    assert!(
        !target.exists(),
        "a refused write must leave no file behind: {}",
        target.display()
    );
}

// ---- 3. a tool that overruns its deadline is stopped -------------------------------------------

#[tokio::test]
async fn a_tool_overrunning_its_deadline_is_interrupted_not_awaited() {
    // `started` proves the body was entered, `completed` that it was not allowed to finish: the
    // dispatch path must drop the future at the deadline, not wait for it and report afterwards.
    let started = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(ToolRegistry::new());
    tools.register(ToolDefinition::new(
        "sleepy",
        "sleeps far longer than its budget",
        serde_json::json!({"type": "object"}),
        {
            let started = Arc::clone(&started);
            let completed = Arc::clone(&completed);
            Arc::new(
                move |_args: serde_json::Value, ctx: girder_core::ToolRunContext| {
                    started.fetch_add(1, Ordering::SeqCst);
                    let completed = Arc::clone(&completed);
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        // Only reached if the deadline failed to interrupt the call.
                        if !ctx.cancel.is_cancelled() {
                            completed.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(ToolResult::text("finished anyway"))
                    })
                },
            )
        },
    ));
    // A tenth of a second against a ten-second body: the gap is wide enough that the assertion is
    // about the wiring and not about scheduler luck.
    let context = Arc::new(context(&tools).with_tool_timeout(WatchdogPolicy::new(100, true)));

    let began = std::time::Instant::now();
    let result = girder_core::dispatch(
        &context,
        execution("sleepy", "{}"),
        CancellationToken::new(),
    )
    .await
    .expect("a timeout is a tool result, not a loop error");
    let elapsed = began.elapsed();

    assert!(
        result.is_error,
        "a stopped call is reported as a failure: {result:?}"
    );
    let text = text_of(&result);
    assert!(
        text.contains("did not finish within its budget and was stopped"),
        "the model must be told the call was stopped rather than that it failed: {text}"
    );
    assert_eq!(started.load(Ordering::SeqCst), 1, "the body was entered");
    assert_eq!(
        completed.load(Ordering::SeqCst),
        0,
        "the body must be interrupted, not awaited: it was allowed to run to completion"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "dispatch must return at the deadline, not when the tool finishes; took {elapsed:?}"
    );
}

// ---- 4. ANSI escapes never reach the model -----------------------------------------------------

#[tokio::test]
async fn sanitizing_happens_in_the_loop_not_in_dispatch() {
    // An escape sequence is a terminal instruction, not text: a build log can paint "0 failures" over
    // a real one. The assertion is that the loop's result has no `\x1b` at all -- not just that it
    // lacks colour, because a partially removed sequence (`[31m` left behind) is the failure mode a
    // per-character filter produces.
    let tools = Arc::new(ToolRegistry::new());
    tools.register(ToolDefinition::new(
        "colored",
        "returns text containing an ANSI colour",
        serde_json::json!({"type": "object"}),
        Arc::new(
            |_args: serde_json::Value, _ctx: girder_core::ToolRunContext| {
                Box::pin(async { Ok(ToolResult::text("build ok \u{1b}[31mRED\u{1b}[0m after")) })
            },
        ),
    ));

    // First: `dispatch` alone, which is the layer documented as not sanitizing. Pinning this is what
    // makes the second assertion meaningful -- otherwise it would pass even if nothing sanitized.
    let direct = dispatch_call(&context(&tools), "colored", "{}").await;
    assert!(
        text_of(&direct).contains('\u{1b}'),
        "dispatch returns the tool's bytes verbatim; sanitizing is the loop's step, not dispatch's"
    );

    // Through the loop, which is where `spill_if_needed` -> `sanitize_blocks` runs.
    let finished = finished_content(
        Arc::new(CallsOnceThenAnswers {
            tool: "colored",
            arguments: "{}",
            emitted: std::sync::atomic::AtomicBool::new(false),
        }),
        tools,
    )
    .await;

    assert_eq!(finished.len(), 1, "one tool result reached the caller");
    let text: String = finished[0]
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !text.contains('\u{1b}'),
        "no escape byte may reach the model: {text:?}"
    );
    assert!(
        !text.contains("[31m"),
        "the sequence must be removed whole; a leftover parameter list reads as printed text: {text:?}"
    );
    assert!(
        text.contains("build ok") && text.contains("RED"),
        "sanitizing removes the instruction, not the content: {text:?}"
    );
}

// ---- 5. a cache hit is served without touching the disk ----------------------------------------

#[tokio::test]
async fn a_second_read_is_served_from_cache_without_reading_the_file_again() {
    // The only honest way to observe "no disk read" with the real `read` tool is to change what a
    // disk read would return while leaving the invalidation token alone. The file is rewritten with
    // different bytes and its mtime restored, so the cache's freshness gate passes; the second read
    // reporting the *old* text is then proof that the bytes came from memory.
    let workspace = scratch("cache-hit");
    let path = workspace.join("data.txt");
    std::fs::write(&path, "ORIGINAL-CONTENT\n").expect("seed file");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::read_tool(workspace.clone()));
    let context = context(&tools).with_tool_cache(8);
    let arguments = serde_json::json!({ "path": path.display().to_string() }).to_string();

    let first = dispatch_call(&context, "read", &arguments).await;
    assert!(!first.is_error, "the seed read succeeds: {first:?}");
    assert!(text_of(&first).contains("ORIGINAL-CONTENT"));

    rewrite_preserving_mtime(&path, "REPLACED-ON-DISK\n");
    // The premise of the test: a disk read now returns different text.
    assert_eq!(
        std::fs::read_to_string(&path).expect("read back"),
        "REPLACED-ON-DISK\n"
    );

    let second = dispatch_call(&context, "read", &arguments).await;
    let text = text_of(&second);
    assert!(
        text.contains("ORIGINAL-CONTENT"),
        "the second read must be answered from the cache: {text}"
    );
    assert!(
        !text.contains("REPLACED-ON-DISK"),
        "the disk was read again, so the cache did not answer: {text}"
    );

    // The counters are the corroborating evidence: one miss to fill the entry, one hit to serve it.
    let stats = context
        .tool_cache
        .as_ref()
        .expect("cache enabled")
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .stats();
    assert_eq!(stats.hits, 1, "one lookup served from memory: {stats:?}");
    assert_eq!(stats.misses, 1, "one lookup had to run the tool: {stats:?}");
}

#[tokio::test]
async fn a_deleted_file_makes_the_cache_refuse_rather_than_serve_stale_text() {
    // Documenting a behaviour worth knowing before anyone tries the obvious test: deleting the file
    // does *not* produce a hit. `read_mtime` returns `None` for a missing path and the cache only
    // serves an entry whose mtime matches, so deletion is a miss. The safe direction is the one
    // implemented -- text that is no longer on disk must not be handed to the model -- but it means
    // "the cache is read even though the file is gone" is not an observable fact about this system.
    let workspace = scratch("cache-deleted");
    let path = workspace.join("gone.txt");
    std::fs::write(&path, "WAS-HERE\n").expect("seed file");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::read_tool(workspace.clone()));
    let context = context(&tools).with_tool_cache(8);
    let arguments = serde_json::json!({ "path": path.display().to_string() }).to_string();

    let first = dispatch_call(&context, "read", &arguments).await;
    assert!(text_of(&first).contains("WAS-HERE"));

    std::fs::remove_file(&path).expect("remove the test's own fixture file");
    let second = dispatch_call(&context, "read", &arguments).await;

    assert!(
        second.is_error,
        "a read of a deleted file must fail, not be answered from memory: {second:?}"
    );
    let text = text_of(&second);
    assert!(
        text.contains("cannot read"),
        "the refusal must name the real cause: {text}"
    );
    assert!(
        !text.contains("WAS-HERE"),
        "cached text must not outlive the file it describes: {text}"
    );

    let stats = context
        .tool_cache
        .as_ref()
        .expect("cache enabled")
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .stats();
    assert_eq!(stats.hits, 0, "deletion is not a hit: {stats:?}");
    assert!(
        stats.stale >= 1,
        "the entry must be discarded as stale rather than kept: {stats:?}"
    );
}

// ---- 6. writing through the pipeline invalidates what the cache holds --------------------------

#[tokio::test]
async fn a_write_through_dispatch_makes_the_next_read_see_the_new_content() {
    // The failure this prevents is silent: a model that writes a file and then reads its own result
    // would be shown the pre-write text, build on it, and have no signal that it was lied to. The
    // whole chain is exercised -- read (fills the cache), write (invalidates), read (re-reads).
    let workspace = scratch("cache-invalidate");
    let path = workspace.join("doc.md");
    std::fs::write(&path, "BEFORE-THE-WRITE\n").expect("seed file");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::read_tool(workspace.clone()));
    tools.register(girder_core::write_tool(workspace.clone()));
    let context = context(&tools).with_tool_cache(8);

    let read_arguments = serde_json::json!({ "path": path.display().to_string() }).to_string();
    let first = dispatch_call(&context, "read", &read_arguments).await;
    assert!(text_of(&first).contains("BEFORE-THE-WRITE"));

    let write_result = dispatch_call(
        &context,
        "write",
        &serde_json::json!({
            "path": path.display().to_string(),
            "content": "AFTER-THE-WRITE\n",
        })
        .to_string(),
    )
    .await;
    assert!(
        !write_result.is_error,
        "the write succeeds: {write_result:?}"
    );

    let second = dispatch_call(&context, "read", &read_arguments).await;
    let text = text_of(&second);
    assert!(
        text.contains("AFTER-THE-WRITE"),
        "the read after a write must show the new content: {text}"
    );
    assert!(
        !text.contains("BEFORE-THE-WRITE"),
        "the pre-write text survived the write and was served again: {text}"
    );

    // A writer is never admitted to the cache in the first place; the count proves the second gate
    // held as well as the invalidation, because a `put` of `write` would show up here.
    let stats = context
        .tool_cache
        .as_ref()
        .expect("cache enabled")
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .stats();
    assert_eq!(stats.refused, 0, "no writer reached the store: {stats:?}");
}

// ---- 7. the cache is consulted before the rules ------------------------------------------------

#[tokio::test]
async fn a_cached_read_is_served_without_consulting_the_rules() {
    // An ordering fact with a security consequence, pinned deliberately: `dispatch` looks in the
    // cache before the `ToolPreExecute` waterfall, so a `deny` rule does not apply to a call whose
    // answer is already in memory. That is defensible -- nothing runs -- but it is not obvious, and
    // a reader who assumed "rules gate every call" would be wrong. If the order is ever changed,
    // this test fails and the change becomes a decision instead of an accident.
    let workspace = scratch("cache-before-rules");
    let path = workspace.join("secret.txt");
    std::fs::write(&path, "ON-DISK\n").expect("seed file");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::read_tool(workspace.clone()));

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "read".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Deny,
        reason: "reads are denied in this deployment".to_owned(),
    });
    let context = context(&tools)
        .with_approval_rules(rules)
        .with_tool_cache(8);

    let arguments = serde_json::json!({ "path": path.display().to_string() }).to_string();

    // The rule does govern a cold call, which is the control for the experiment below.
    let denied = dispatch_call(&context, "read", &arguments).await;
    assert!(
        text_of(&denied).contains("denied"),
        "the deny rule applies when the cache is cold: {denied:?}"
    );

    // Prime the cache directly, as a successful earlier call would have.
    {
        let mut cache = context
            .tool_cache
            .as_ref()
            .expect("cache enabled")
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.put(
            ToolCacheKey::of("read", &arguments),
            ToolCacheEntry {
                text: "FROM-CACHE\n".to_owned(),
                is_error: false,
                source_mtime_ms: Some(mtime_ms(&path)),
            },
        );
    }

    let served = dispatch_call(&context, "read", &arguments).await;
    let text = text_of(&served);
    assert!(
        text.contains("FROM-CACHE"),
        "a cache hit is answered before the rule waterfall: {text}"
    );
    assert!(
        !text.contains("denied"),
        "the deny rule did not run for the cached call: {text}"
    );
}

// ---- 8. a limit that is configured but never consulted ----------------------------------------

#[tokio::test]
async fn tool_limits_are_not_enforced_by_dispatch() {
    // `tool_limits` is enforced by `run` (the counter is recorded before `dispatch` is called), not
    // by `dispatch` itself. This test records that boundary: a caller that drives `dispatch`
    // directly bypasses the per-tool call ceiling entirely. It is written as an executable fact
    // rather than a comment because "where is the ceiling applied" is exactly the question this
    // end-to-end file exists to answer.
    let ran = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(ToolRegistry::new());
    tools.register(ToolDefinition::new(
        "probe",
        "records that it ran",
        serde_json::json!({"type": "object"}),
        {
            let ran = Arc::clone(&ran);
            Arc::new(
                move |_args: serde_json::Value, _ctx: girder_core::ToolRunContext| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(ToolResult::text("ran")) })
                },
            )
        },
    ));
    // One call per turn, and this test makes three.
    let context = context(&tools).with_tool_limits(ToolLimit::new(1, 1));

    for _ in 0..3 {
        let result = dispatch_call(&context, "probe", "{}").await;
        assert!(
            !result.is_error,
            "no ceiling applies at this layer: {result:?}"
        );
    }
    assert_eq!(
        ran.load(Ordering::SeqCst),
        3,
        "the per-tool ceiling lives in the loop, so dispatch ran every call"
    );
}
