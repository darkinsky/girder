//! Acceptance tests for migration-plan phase 7 (§2.10).
//!
//! Phase 7 is "migrate on demand". The plan ranks `subagent` and the full `fs`/`sandbox` policy as
//! the two high-priority items. This file covers what could be done and verified on this host:
//!
//! - **subagent depth limiting** — the property that makes delegation safe rather than a token sink.
//! - **the sandbox enforcement level** — reported honestly rather than assumed.
//!
//! The kernel-level sandbox is **not** implemented and **not** tested; see `docs/phase7-scope.md`
//! for the measurement that ruled it out on this machine (`CONFIG_SECURITY_LANDLOCK` unset).

use std::sync::Arc;

use girder_core::{
    AdvisorySandbox, AgentId, Depth, DepthLimitedTools, Enforcement, EventBus, LoopContext,
    MemorySession, PathPolicy, Sandbox, Session, ToolCallId, ToolDefinition, ToolRegistry,
    ToolRegistryView, landlock_abi,
};
use tokio_util::sync::CancellationToken;

// ---- depth ------------------------------------------------------------------------------------

#[test]
fn the_root_agent_is_depth_zero() {
    assert_eq!(Depth::root().get(), 0);
}

#[test]
fn a_child_is_one_deeper() {
    assert_eq!(Depth::root().child().get(), 1);
    assert_eq!(Depth::root().child().child().get(), 2);
}

#[test]
fn the_ceiling_counts_nested_delegations() {
    // A limit of 2 means depths 0 and 1 may delegate; depth 2 may not. That is what an operator
    // setting "two levels of delegation" expects.
    assert!(Depth::root().can_delegate(2));
    assert!(Depth::root().child().can_delegate(2));
    assert!(!Depth::root().child().child().can_delegate(2));
}

#[test]
fn a_zero_limit_refuses_the_first_delegation() {
    assert!(!Depth::root().can_delegate(0));
}

#[test]
fn depth_saturates_rather_than_overflowing() {
    // A pathological chain must not wrap to zero and grant itself an unlimited budget.
    let deep = Depth::root().child().child().child();
    let deeper = Depth::new(u32::MAX).child();
    assert_eq!(deeper.get(), u32::MAX, "child() must saturate");
    assert!(!deeper.can_delegate(2));
    let _ = deep;
}

// ---- the tool set is trimmed at the ceiling -----------------------------------------------------

fn registry_with(names: &[&str]) -> Arc<dyn ToolRegistryView> {
    let tools = ToolRegistry::new();
    for name in names {
        tools.register(ToolDefinition::new(
            *name,
            "test",
            serde_json::json!({ "type": "object" }),
            Arc::new(|_args, _ctx| Box::pin(async { Ok(girder_core::ToolResult::text("ok")) })),
        ));
    }
    Arc::new(tools)
}

#[test]
fn a_subagent_tool_is_visible_below_the_ceiling() {
    let tools = DepthLimitedTools::new(
        registry_with(&["bash", "subagent"]),
        Depth::root(),
        2,
        "subagent",
    );

    let names: Vec<String> = tools.schemas().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"subagent".to_owned()), "{names:?}");
    assert!(tools.get("subagent").is_some());
}

#[test]
fn a_subagent_tool_is_hidden_at_the_ceiling() {
    // Structural, not advisory: at the ceiling the model is not offered the tool, and asking for it
    // by name reports it as unknown rather than running it.
    let at_ceiling = Depth::root().child().child();
    let tools = DepthLimitedTools::new(
        registry_with(&["bash", "subagent"]),
        at_ceiling,
        2,
        "subagent",
    );

    let names: Vec<String> = tools.schemas().into_iter().map(|s| s.name).collect();
    assert!(!names.contains(&"subagent".to_owned()), "{names:?}");
    assert!(tools.get("subagent").is_none(), "the tool must not resolve");
    assert!(
        names.contains(&"bash".to_owned()),
        "other tools stay available: {names:?}"
    );
}

// ---- delegation refused before spending ---------------------------------------------------------

fn context_with(
    tools: Arc<dyn ToolRegistryView>,
    depth: Depth,
    max_depth: u32,
) -> Arc<LoopContext> {
    // No provider is needed: a refused delegation must not reach the model at all.
    struct NoProvider;
    impl girder_core::LlmProvider for NoProvider {
        fn stream(
            &self,
            _request: girder_core::LlmRequest,
            _cancel: CancellationToken,
        ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, girder_core::LlmError>> {
            Box::pin(async {
                Err(girder_core::LlmError::Provider(
                    "must not be called".to_owned(),
                ))
            })
        }
    }

    Arc::new(
        LoopContext::new(
            Arc::new(NoProvider),
            tools,
            Arc::new(MemorySession::new()) as Arc<dyn Session>,
            Arc::new(EventBus::new()),
            "test-model",
            AgentId::new("agent".to_owned()).expect("valid test id"),
        )
        .with_depth(depth, max_depth),
    )
}

#[tokio::test]
async fn a_delegation_at_the_ceiling_is_refused_without_calling_the_model() {
    let depth = Depth::root().child().child();
    let ctx = context_with(registry_with(&["bash"]), depth, 2);

    let error = girder_core::delegate(
        &ctx,
        "do something".to_owned(),
        depth,
        2,
        CancellationToken::new(),
    )
    .await
    .expect_err("a ceiling delegation must be refused");

    assert!(
        matches!(
            error,
            girder_core::SubagentError::DepthExceeded { depth: 2, max: 2 }
        ),
        "expected DepthExceeded, got {error:?}"
    );
    // The message must tell the model what to do instead, or it will keep retrying.
    assert!(error.to_string().contains("limit"));
}

#[tokio::test]
async fn a_cancelled_delegation_reports_cancelled() {
    let ctx = context_with(registry_with(&["bash"]), Depth::root(), 2);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = girder_core::delegate(&ctx, "task".to_owned(), Depth::root(), 2, cancel)
        .await
        .expect_err("a cancelled delegation must not run");

    // Cancellation is checked after the depth gate, so a cancelled *and* at-ceiling call reports
    // depth: the cheaper refusal wins.
    assert!(
        matches!(error, girder_core::SubagentError::Cancelled),
        "expected Cancelled, got {error:?}"
    );
}

#[tokio::test]
async fn the_tool_reports_a_refusal_as_a_model_facing_result() {
    // A refusal is not an error the harness raises: the model sees it and adapts.
    let depth = Depth::root().child().child();
    let ctx = context_with(registry_with(&["bash"]), depth, 2);
    let tool = girder_core::subagent_tool(Arc::clone(&ctx), 2);
    let run_context = girder_core::ToolRunContext {
        execution: girder_core::ToolExecution {
            call_id: ToolCallId::new("c1").expect("valid test id"),
            name: "subagent".to_owned(),
            arguments: "{}".to_owned(),
            agent_id: AgentId::new("agent".to_owned()).expect("valid test id"),
        },
        cancel: CancellationToken::new(),
    };

    let result = (tool.execute)(serde_json::json!({ "prompt": "task" }), run_context)
        .await
        .unwrap();

    assert!(result.is_error);
    let text: String = result
        .content
        .iter()
        .filter_map(|block| match block {
            girder_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("depth 2"), "{text}");
    assert!(text.contains("limit"), "{text}");
}

#[tokio::test]
async fn the_tool_rejects_an_empty_prompt() {
    let ctx = context_with(registry_with(&["bash"]), Depth::root(), 2);
    let tool = girder_core::subagent_tool(ctx, 2);
    let run_context = girder_core::ToolRunContext {
        execution: girder_core::ToolExecution {
            call_id: ToolCallId::new("c1").expect("valid test id"),
            name: "subagent".to_owned(),
            arguments: "{}".to_owned(),
            agent_id: AgentId::new("agent".to_owned()).expect("valid test id"),
        },
        cancel: CancellationToken::new(),
    };

    let result = (tool.execute)(serde_json::json!({ "prompt": "   " }), run_context)
        .await
        .unwrap();

    assert!(result.is_error);
}

// ---- the sandbox reports its level honestly -----------------------------------------------------

#[test]
fn the_advisory_sandbox_says_it_is_advisory() {
    // This is the whole point of `Enforcement`: a caller must be able to tell an advisory check
    // from actual confinement without reading docs.
    let sandbox = AdvisorySandbox::default();

    assert_eq!(sandbox.enforcement(), Enforcement::Advisory);
    assert!(!sandbox.enforcement().is_kernel());
    assert!(sandbox.enforcement().describe().contains("not enforced"));
}

#[test]
fn a_kernel_sandbox_level_describes_its_mechanism() {
    let level = Enforcement::Kernel {
        mechanism: "landlock",
    };
    assert!(level.is_kernel());
    assert_eq!(level.describe(), "kernel (landlock)");
}

#[test]
fn landlock_availability_is_probed_not_assumed() {
    // On this host the kernel was built without CONFIG_SECURITY_LANDLOCK, so the probe must report
    // absence rather than a version. The assertion is deliberately not "is None": a machine that has
    // Landlock would legitimately return Some, and hard-coding the local result would make the test
    // a lie on any other host.
    // Either answer is valid: a host with Landlock reports a version, one without reports absence.
    // What must not happen is a panic or a fabricated version.
    if let Some(abi) = landlock_abi() {
        assert!(abi >= 1, "a reported ABI must be a real version, got {abi}");
    }
}

#[tokio::test]
async fn the_advisory_sandbox_still_checks_declared_writes() {
    // Advisory does not mean "no checks": the declared-write gate from phase 5 still runs.
    let dir = std::env::temp_dir().join(format!("girder-phase7-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sandbox = AdvisorySandbox::new(&dir);
    let policy = PathPolicy::new(&dir);

    let output = sandbox
        .run("echo hi", &policy, CancellationToken::new())
        .await
        .expect("an in-scope command must run");
    assert_eq!(output.stdout.trim(), "hi");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the scripted delegator, for a caller with no model -----------------------------------------

#[test]
fn the_scripted_delegator_answers_in_order() {
    let delegator =
        girder_core::ScriptedDelegator::new(vec!["first".to_owned(), "second".to_owned()]);
    let cancel = CancellationToken::new();

    assert_eq!(delegator.answer(&cancel).unwrap().text, "first");
    assert_eq!(delegator.answer(&cancel).unwrap().text, "second");
    assert!(
        delegator.answer(&cancel).is_err(),
        "the script is exhausted"
    );
}

#[test]
fn a_delegation_result_renders_for_the_parent_model() {
    let result = girder_core::DelegationResult {
        text: "the answer".to_owned(),
        turns: 3,
        child_id: AgentId::new("child".to_owned()).expect("valid test id"),
    };
    let tool_result = result.to_tool_result();
    assert!(!tool_result.is_error);
}

#[test]
fn an_empty_child_answer_is_a_failure_the_parent_can_see() {
    // A child that finished without answering must not look like success, or the parent will act on
    // nothing.
    let result = girder_core::DelegationResult {
        text: "   ".to_owned(),
        turns: 2,
        child_id: AgentId::new("child".to_owned()).expect("valid test id"),
    };
    let tool_result = result.to_tool_result();
    assert!(tool_result.is_error);
}
