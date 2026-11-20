//! End-to-end proof that an approval rule actually gates a tool call.
//!
//! # The gap this file closes
//!
//! `approval_rules` has unit tests that show `Rules::decide` returns the right `Decision`, and
//! `approval` has unit tests that show an `Approver` records what it was asked. Both pass no matter
//! what `core::agent_loop::dispatch` does with the answer. A rule layer that is never consulted is
//! indistinguishable, from the unit tests, from one that is consulted and ignored -- and the earlier
//! audit that motivated this file found `with_approval_rules`/`with_approver` had zero production
//! callers, which is exactly the condition under which those unit tests keep passing while the
//! feature does nothing.
//!
//! So every test here goes through the real `dispatch` with the real `file_tools` tools, and every
//! assertion about "the tool did not run" is made against a *file on disk* rather than against an
//! error string. An error message is produced by both "refused before running" and "ran and then
//! complained"; only the absent file distinguishes them. That is the fact a user asked the rule to
//! guarantee.
//!
//! # The one assertion that matters most
//!
//! [`an_ask_rule_with_no_approver_refuses_instead_of_running`] pins the fail-closed default. If a
//! deployment configures "ask me before writing" and mounts no approver, the only two coherent
//! readings are "refuse" and "silently write anyway". The second is a security hole that produces no
//! error, no log line, and no user-visible signal; it is the failure this whole file exists to make
//! impossible to introduce silently.
//!
//! # What precedence actually is
//!
//! `approval_rules` documents **last-match-wins**, not "most specific wins". `Rules::decide` walks
//! the list newest-first and stops at the first match, so a rule's position is its specificity. This
//! file pins the documented behaviour rather than the behaviour a user might assume from the word
//! "specific": [`the_later_rule_wins_and_reports_which_rule_it_was`] shows a broad rule added last
//! overriding a narrow one added first, and
//! [`a_narrow_rule_added_last_overrides_a_broad_one_added_first`] shows the same mechanism used to
//! get the intuitive result. Confusing the two is how someone widens their own policy by accident.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use girder_core::{
    AgentId, ApprovalAction, ApprovalError, ApprovalRequest, ApprovalRule, ApprovalRules, Approver,
    BoxFut, ContentBlock, Decision, EventBus, LlmError, LlmProvider, LlmRequest, LoopContext,
    MemorySession, Session, ToolCallId, ToolDefinition, ToolExecution, ToolRegistry,
    ToolRegistryView, ToolResult,
};
use tokio_util::sync::CancellationToken;

// ---- fixtures ----------------------------------------------------------------------------------

/// A workspace directory unique to one test.
///
/// The process id and a counter keep two tests in one run from sharing a path, because the entire
/// assertion in most of these tests is "this exact path is absent afterwards".
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-approval-e2e-{name}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// One tool call, spelled exactly as the loop hands it to `dispatch`.
fn execution(name: &str, arguments: &str) -> ToolExecution {
    ToolExecution {
        call_id: ToolCallId::new("call-1").expect("valid call id"),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
        agent_id: AgentId::new("agent-1").expect("valid agent id"),
    }
}

/// A provider that must never be reached: these tests drive `dispatch`, not `run`.
struct NoProvider;

impl LlmProvider for NoProvider {
    fn stream(
        &self,
        _request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, LlmError>> {
        Box::pin(async { panic!("a dispatch-only test must not reach the provider") })
    }
}

/// A context over `tools` with a fresh session and bus and no policy of any kind.
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

/// Drives one call through `dispatch`, which is the path the loop uses.
///
/// The `expect` is the point: a policy refusal is a *result* the model reads, not a `LoopError`. A
/// rule layer that escalated a denial to a loop error would break every caller that expects to
/// continue the turn, so the tests would rather fail here than paper over it.
async fn dispatch_call(context: &LoopContext, name: &str, arguments: &str) -> ToolResult {
    girder_core::dispatch(
        context,
        execution(name, arguments),
        CancellationToken::new(),
    )
    .await
    .expect("a policy refusal is a tool result, not a loop error")
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

/// A `write` call targeting `path` with fixed content.
fn write_arguments(path: &Path, content: &str) -> String {
    serde_json::json!({ "path": path.display().to_string(), "content": content }).to_string()
}

/// An approver that records every request it is asked and answers with a fixed decision.
///
/// Distinct from [`girder_core::ScriptedApprover`] because the fact these tests need is *whether a
/// person was consulted at all*: an `Allow` rule that still asked somebody is a different (and
/// wrong) implementation from one that did not, and an error string cannot tell them apart.
struct RecordingApprover {
    decision: Decision,
    asked: std::sync::Mutex<Vec<ApprovalRequest>>,
}

impl RecordingApprover {
    fn approving() -> Self {
        Self {
            decision: Decision::approved(),
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn denying(reason: &str) -> Self {
        Self {
            decision: Decision::denied_because(reason),
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn count(&self) -> usize {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn last_question(&self) -> Option<String> {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .map(|request| request.description.clone())
    }
}

impl Approver for RecordingApprover {
    fn ask(&self, request: ApprovalRequest) -> BoxFut<Result<Decision, ApprovalError>> {
        let decision = self.decision.clone();
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request);
        Box::pin(async move { Ok(decision) })
    }
}

/// A counter tool that records that its body ran.
///
/// Used where the claim is "the body was not reached" for a tool with no other observable effect, so
/// that the assertion is a concrete count rather than an inference from an error string.
fn counting_tool() -> (ToolDefinition, Arc<AtomicUsize>) {
    let ran = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ran);
    let tool = ToolDefinition::new(
        "probe",
        "records that it ran",
        serde_json::json!({ "type": "object" }),
        Arc::new(
            move |_args: serde_json::Value, _ctx: girder_core::ToolRunContext| {
                counter.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(ToolResult::text("ran")) })
            },
        ),
    );
    (tool, ran)
}

// ---- 1. a `Deny` rule stops the call before the body runs --------------------------------------

#[tokio::test]
async fn a_deny_rule_refuses_before_the_body_runs_and_leaves_no_file() {
    // Two assertions, and the file is the load-bearing one: `is_error` alone would also be produced by
    // a `write` that ran, failed halfway, and returned an error. The absent file is what proves the
    // refusal happened *instead of* execution.
    let workspace = scratch("deny");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::write_tool(workspace.clone()));

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Deny,
        reason: "this deployment forbids writing files".to_owned(),
    });

    // No approver on purpose: a `Deny` must not be resolvable by a person, so the approver's absence
    // cannot be what produced the refusal. It also proves `Deny` does not route through `ask_approver`.
    let context = context(&tools).with_approval_rules(rules);
    let target = workspace.join("denied.txt");
    let result = dispatch_call(
        &context,
        "write",
        &write_arguments(&target, "must not land"),
    )
    .await;

    assert!(
        result.is_error,
        "a denied call is an error the model sees: {result:?}"
    );
    let text = text_of(&result);
    assert!(
        text.contains("denied") && text.contains("forbids writing files"),
        "the refusal must name the decision and carry the rule's own reason: {text}"
    );
    assert!(
        !target.exists(),
        "the write body ran despite a `Deny` rule; the file exists at {}",
        target.display()
    );
}

// ---- 2. an `Allow` rule lets the body run ------------------------------------------------------

#[tokio::test]
async fn an_allow_rule_lets_the_body_run() {
    // The control for test 1: identical setup, opposite action, and the file *must* appear. Without
    // this, test 1 would pass against an implementation that refuses every call.
    let workspace = scratch("allow");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::write_tool(workspace.clone()));

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Allow,
        reason: "writing is trusted in this deployment".to_owned(),
    });
    // An approver is mounted deliberately: `Allow` must not consult it. If it did, a person would be
    // asked about a call the policy already approved, and the count below catches that.
    let approver = Arc::new(RecordingApprover::approving());
    let context = context(&tools)
        .with_approval_rules(rules)
        .with_approver(Arc::clone(&approver) as Arc<dyn Approver>);

    let target = workspace.join("allowed.txt");
    let result = dispatch_call(&context, "write", &write_arguments(&target, "allowed")).await;

    assert!(!result.is_error, "an allowed call succeeds: {result:?}");
    assert!(target.exists(), "an `Allow` rule must let the body run");
    assert_eq!(
        std::fs::read_to_string(&target).expect("read back"),
        "allowed",
        "the content written must be the content asked for"
    );
    assert_eq!(
        approver.count(),
        0,
        "an `Allow` rule must not ask a person; it consulted the approver {} time(s)",
        approver.count()
    );
}

// ---- 3. an `Ask` rule with no approver refuses (the core assertion) ----------------------------

#[tokio::test]
async fn an_ask_rule_with_no_approver_refuses_instead_of_running() {
    // The failure this guards against is silent permission: a deployment configures "ask me before
    // writing", mounts no approver, and every write proceeds with no error and no prompt. That bug
    // produces no log line and no failing test anywhere else, so it is pinned here.
    let workspace = scratch("ask-no-approver");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::write_tool(workspace.clone()));

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Ask,
        reason: "writes need a person".to_owned(),
    });
    // `with_approver` is never called: this is the non-interactive deployment.
    let context = context(&tools).with_approval_rules(rules);

    let target = workspace.join("silent-write.txt");
    let result = dispatch_call(
        &context,
        "write",
        &write_arguments(&target, "must not land"),
    )
    .await;

    assert!(
        result.is_error,
        "an unanswered `Ask` must be a refusal, not a permission: {result:?}"
    );
    let text = text_of(&result);
    assert!(
        text.contains("needs approval but no approver is configured"),
        "the refusal must say that nobody could answer: {text}"
    );
    assert!(
        !target.exists(),
        "SILENT PERMISSION: an `Ask` with no approver let the write through; {} exists",
        target.display()
    );
}

// ---- 3b. an `Ask` rule whose approver is broken also refuses -----------------------------------

#[tokio::test]
async fn an_ask_rule_with_a_failing_approver_refuses_rather_than_assuming_consent() {
    // A mounted approver is not the same as an answer. An approver whose own machinery fails (no UI
    // attached, IPC dropped) returns `Err`, and treating that as consent is the same silent-permission
    // bug as test 3 one layer further in. `UnavailableApprover` is the shipped instance of it.
    let workspace = scratch("ask-unavailable");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::write_tool(workspace.clone()));

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Ask,
        reason: "writes need a person".to_owned(),
    });
    let context = context(&tools)
        .with_approval_rules(rules)
        .with_approver(Arc::new(girder_core::UnavailableApprover));

    let target = workspace.join("failing-approver.txt");
    let result = dispatch_call(
        &context,
        "write",
        &write_arguments(&target, "must not land"),
    )
    .await;

    assert!(
        result.is_error,
        "a failed approval is not consent: {result:?}"
    );
    let text = text_of(&result);
    assert!(
        text.contains("could not be approved"),
        "the refusal must distinguish a broken approver from a denial: {text}"
    );
    assert!(
        !target.exists(),
        "an approval error was read as consent; {} exists",
        target.display()
    );
}

// ---- 4. an `Ask` rule with an approving approver runs the call ---------------------------------

#[tokio::test]
async fn an_ask_rule_with_an_approving_approver_runs_the_call() {
    // The positive path: `Ask` is a pause, not a refusal. If this failed, the feature would be
    // indistinguishable from `Deny` and nobody could ever approve anything.
    let workspace = scratch("ask-approve");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::write_tool(workspace.clone()));

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Ask,
        reason: "writes need a person".to_owned(),
    });
    let approver = Arc::new(RecordingApprover::approving());
    let context = context(&tools)
        .with_approval_rules(rules)
        .with_approver(Arc::clone(&approver) as Arc<dyn Approver>);

    let target = workspace.join("approved.txt");
    let result = dispatch_call(&context, "write", &write_arguments(&target, "approved")).await;

    assert!(!result.is_error, "an approved call succeeds: {result:?}");
    assert!(
        target.exists(),
        "an approved `Ask` must run the body; {} is absent",
        target.display()
    );
    assert_eq!(approver.count(), 1, "the person must be asked exactly once");
    let question = approver.last_question().expect("a recorded question");
    assert!(
        question.contains("writes need a person") && question.contains("write"),
        "the question must carry the rule's reason and the tool name: {question}"
    );
}

// ---- 5. an `Ask` rule with a denying approver does not run the call ----------------------------

#[tokio::test]
async fn an_ask_rule_with_a_denying_approver_does_not_run_the_call() {
    // The mirror of test 4, and the one that makes the approver's answer meaningful: if a denial still
    // ran the tool, the approver would be a notification rather than a gate.
    let workspace = scratch("ask-deny");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::write_tool(workspace.clone()));

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Ask,
        reason: "writes need a person".to_owned(),
    });
    let approver = Arc::new(RecordingApprover::denying("not this time"));
    let context = context(&tools)
        .with_approval_rules(rules)
        .with_approver(Arc::clone(&approver) as Arc<dyn Approver>);

    let target = workspace.join("denied-by-person.txt");
    let result = dispatch_call(
        &context,
        "write",
        &write_arguments(&target, "must not land"),
    )
    .await;

    assert!(
        result.is_error,
        "a person's denial is a failure: {result:?}"
    );
    let text = text_of(&result);
    assert!(
        text.contains("was not approved"),
        "the refusal must say the call was not approved: {text}"
    );
    assert!(
        text.contains("writes need a person"),
        "the refusal must repeat what the person was asked: {text}"
    );
    assert!(
        !target.exists(),
        "a denied `Ask` still ran the body; {} exists",
        target.display()
    );
    assert_eq!(approver.count(), 1, "the person must be asked exactly once");
}

// ---- 6. precedence: the last matching rule wins -----------------------------------------------

#[tokio::test]
async fn a_narrow_rule_added_last_overrides_a_broad_one_added_first() {
    // The documented way to write "trust writes, except this one": a broad baseline first, the
    // exception appended. Under first-match-wins the exception would never fire, which is why
    // `approval_rules` chose last-match-wins. This test pins that the choice reaches `dispatch`.
    let workspace = scratch("precedence-narrow-last");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::write_tool(workspace.clone()));

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Allow,
        reason: "writes are trusted".to_owned(),
    });
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: vec!["secret".to_owned()],
        action: ApprovalAction::Deny,
        reason: "secret paths are forbidden".to_owned(),
    });
    let context = context(&tools).with_approval_rules(rules);

    // The narrow rule matches, and it is last, so it governs.
    let secret = workspace.join("secret.txt");
    let denied = dispatch_call(&context, "write", &write_arguments(&secret, "nope")).await;
    assert!(denied.is_error, "the narrow rule must deny: {denied:?}");
    assert!(
        text_of(&denied).contains("secret paths are forbidden"),
        "the denial must come from the narrow rule: {}",
        text_of(&denied)
    );
    assert!(
        !secret.exists(),
        "the narrow rule's denial must stop the body"
    );

    // The narrow rule does not match, so the call falls through to the broad one.
    let ordinary = workspace.join("ordinary.txt");
    let allowed = dispatch_call(&context, "write", &write_arguments(&ordinary, "ok")).await;
    assert!(!allowed.is_error, "the broad rule must allow: {allowed:?}");
    assert!(ordinary.exists(), "the broad `Allow` must let the body run");
}

#[tokio::test]
async fn the_later_rule_wins_and_reports_which_rule_it_was() {
    // The dangerous direction of the same mechanism, pinned on purpose: appending a *broad* rule after
    // a narrow one widens the policy. It is defensible (the newest sentence governs) but it is not
    // what "more specific wins" suggests, so a future change to specificity-based precedence must
    // break this test and become a decision rather than an accident. The rule index in the message is
    // what lets a UI point at the rule that actually fired.
    let workspace = scratch("precedence-broad-last");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::write_tool(workspace.clone()));

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: vec!["secret".to_owned()],
        action: ApprovalAction::Deny,
        reason: "secret paths are forbidden".to_owned(),
    });
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Allow,
        reason: "writes are trusted".to_owned(),
    });
    let context = context(&tools).with_approval_rules(rules);

    let secret = workspace.join("secret.txt");
    let result = dispatch_call(&context, "write", &write_arguments(&secret, "lands")).await;

    assert!(
        !result.is_error,
        "the last matching rule wins, so the broad `Allow` governs: {result:?}"
    );
    assert!(
        secret.exists(),
        "the later, broader rule must decide the call; {} is absent",
        secret.display()
    );

    // The message reports the winning rule's index, which is rule 1 (the broad one added second).
    // Proving it here keeps the "which rule fired" contract from silently becoming "some rule".
    let decision = {
        let mut rules = ApprovalRules::new();
        rules.add(ApprovalRule {
            tool: "write".to_owned(),
            contains: vec!["secret".to_owned()],
            action: ApprovalAction::Deny,
            reason: "secret paths are forbidden".to_owned(),
        });
        rules.add(ApprovalRule {
            tool: "write".to_owned(),
            contains: Vec::new(),
            action: ApprovalAction::Allow,
            reason: "writes are trusted".to_owned(),
        });
        rules.decide("write", &write_arguments(&secret, "lands"))
    };
    assert_eq!(
        decision.matched,
        Some(1),
        "the decision must name the rule that won, not merely report an action"
    );
}

// ---- 7. rules do not apply to tools they do not name ------------------------------------------

#[tokio::test]
async fn a_deny_rule_for_write_does_not_block_read() {
    // A rule is scoped to a tool, and a policy about writing must not silently become a policy about
    // reading. The risk is real in the other direction too: `read` is the one cacheable tool, and
    // `dispatch` consults the cache before the rule waterfall, so a *denied read* could be served
    // from memory -- this test keeps `read` allowed so it measures tool scoping and nothing else.
    let workspace = scratch("read-unaffected");
    let seed = workspace.join("present.txt");
    std::fs::write(&seed, "READABLE\n").expect("seed file");

    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::read_tool(workspace.clone()));
    tools.register(girder_core::write_tool(workspace.clone()));

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "write".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Deny,
        reason: "this deployment forbids writing files".to_owned(),
    });
    // No approver: with the default-asks policy, an *unmatched* call would ask and therefore refuse.
    // Adding an explicit `Allow` for `read` is what makes this test about tool scoping rather than
    // about the default -- see `an_unmatched_call_asks_rather_than_proceeding` for the default.
    rules.add(ApprovalRule {
        tool: "read".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Allow,
        reason: "reads are trusted".to_owned(),
    });
    let context = context(&tools).with_approval_rules(rules);

    let read_arguments = serde_json::json!({ "path": seed.display().to_string() }).to_string();
    let result = dispatch_call(&context, "read", &read_arguments).await;

    assert!(
        !result.is_error,
        "a `Deny write` rule must not affect `read`: {result:?}"
    );
    assert!(
        text_of(&result).contains("READABLE"),
        "the read must return the file's content: {}",
        text_of(&result)
    );

    // The same rule set still denies the write, which is the control proving the rule was live.
    let target = workspace.join("blocked.txt");
    let denied = dispatch_call(&context, "write", &write_arguments(&target, "no")).await;
    assert!(denied.is_error, "the write is still denied: {denied:?}");
    assert!(!target.exists(), "the denied write must not create a file");
}

// ---- 8. the default is to ask, and an unmatched tool therefore refuses -------------------

#[tokio::test]
async fn an_unmatched_call_asks_rather_than_proceeding() {
    // `approval_rules` documents that no matching rule means `Ask`, not `Allow`: a typo in a tool name
    // or a rules file that failed to load would otherwise quietly permit the call. With no approver
    // mounted, that default has to surface as a refusal. This is the same silent-permission hazard as
    // test 3, reached through the default path instead of an explicit `Ask` rule.
    let workspace = scratch("default-asks");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::write_tool(workspace.clone()));

    // A rule for a tool that does not exist: `write` is therefore unmatched.
    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "nonexistent-tool".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Allow,
        reason: "irrelevant to this call".to_owned(),
    });
    let context = context(&tools).with_approval_rules(rules);

    let target = workspace.join("default.txt");
    let result = dispatch_call(
        &context,
        "write",
        &write_arguments(&target, "must not land"),
    )
    .await;

    assert!(
        result.is_error,
        "an unmatched call must default to asking, and with nobody to ask it must refuse: {result:?}"
    );
    assert!(
        text_of(&result).contains("no approval rule matches this call"),
        "the refusal must say the default policy did it, not a rule: {}",
        text_of(&result)
    );
    assert!(
        !target.exists(),
        "the default policy allowed the write; {} exists",
        target.display()
    );
}

// ---- 9. a rule set is not consulted when none is configured ------------------------------------

#[tokio::test]
async fn without_a_rule_set_every_call_is_allowed() {
    // The other side of the fail-closed default: a deployment that never configures a policy must
    // still work. `decide_by_rules` treats `None` as `Allow`, and this pins that so a future change
    // cannot make the feature mandatory by refusing every call in an unconfigured deployment.
    let workspace = scratch("no-rules");
    let tools = Arc::new(ToolRegistry::new());
    tools.register(girder_core::write_tool(workspace.clone()));
    let context = context(&tools);

    let target = workspace.join("unconfigured.txt");
    let result = dispatch_call(&context, "write", &write_arguments(&target, "lands")).await;

    assert!(!result.is_error, "no policy means no denial: {result:?}");
    assert!(
        target.exists(),
        "an unconfigured deployment must still write files"
    );
}

// ---- 10. a `Deny` stops the call even when an approver exists ---------------------------------

#[tokio::test]
async fn a_deny_rule_does_not_consult_the_approver() {
    // `Deny` is final for the turn; `Ask` is not. A rule layer that routed every non-`Allow` decision
    // through the approver would let a person override a denial, which turns a hard policy into a
    // prompt. The zero count is the assertion.
    // A counting tool rather than a real `write`: the claim here is about whether the *body* was
    // reached, and a counter states that directly without depending on a filesystem.
    let (tool, ran) = counting_tool();
    let tools = Arc::new(ToolRegistry::new());
    tools.register(tool);

    let mut rules = ApprovalRules::new();
    rules.add(ApprovalRule {
        tool: "probe".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Deny,
        reason: "probe is forbidden".to_owned(),
    });
    let approver = Arc::new(RecordingApprover::approving());
    let context = context(&tools)
        .with_approval_rules(rules)
        .with_approver(Arc::clone(&approver) as Arc<dyn Approver>);

    let result = dispatch_call(&context, "probe", "{}").await;

    assert!(result.is_error, "the call is denied: {result:?}");
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the body must not run under a `Deny` rule"
    );
    assert_eq!(
        approver.count(),
        0,
        "a `Deny` must not be overridable by an approver; it was asked {} time(s)",
        approver.count()
    );
}
