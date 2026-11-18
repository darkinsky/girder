//! The tool-invocation *decision* layer: outcome identities, policy application, and batch ledgers.
//!
//! Upstream `packages/core/tools` is ten files and 3,582 raw lines (2,361 effective). Three of them
//! are already ported here:
//!
//! | upstream | lines | local | status |
//! |---|---|---|---|
//! | `json-schema.ts` + `schema.ts` | 1,302 | [`girder_core::core_final_gap`] | ported |
//! | `presentation.ts` | 389 | [`girder_core::tools_contract_gap`] | ported |
//! | `contracts.ts` (argument half) | — | [`girder_core::tool_contracts`] | ported |
//! | `batch.ts` (strategy half) | — | [`girder_core::choose_batch_strategy`] | ported |
//!
//! What is left is this module's subject, and it is the part that decides *what the model is told*:
//!
//! | upstream | here |
//! |---|---|
//! | `error-codes.ts` + `contracts.ts` identities | [`TOOL_ABORTED`], [`UNKNOWN_TOOL`], [`ErrorInfo`], … |
//! | `pipeline.ts` result builders | [`aborted_outcome`], [`aborted_before_dispatch_outcome`], [`approval_denial`] |
//! | `pipeline.ts` `serviceAsk` | [`resolve_ask`] |
//! | `pipeline.ts` `postExecute` | [`apply_post_decision`] |
//! | `pipeline.ts` cancellation precedence | [`finalize_after_cancellation`] |
//! | `batch.ts` `parseArguments` | [`parse_arguments`] |
//! | `batch.ts` `BatchSettlement` | [`Settlement`], [`BatchReport`] |
//! | `batch.ts` group classification | [`group_end`], [`plan_groups`] |
//! | `index.ts` `restrict()` | [`compile_restriction`], [`admits_through`] |
//! | `schema.ts` `defineTool` callback split | [`Callback::rejects_invalid_arguments`] |
//! | `pipeline.ts` `executionMode` | [`execution_mode`] |
//!
//! # Why these pieces and not the pipelines themselves
//!
//! `pipeline.ts` (794 lines) and `index.ts` (412) are Cordis plugins: they reach `ctx.waterfall`,
//! `ctx.get('approval')`, `ctx.events.dispatch` and a `WeakMap` of live execution objects. None of
//! that survives translation, and a Rust port of the *plugin plumbing* would be a new design rather
//! than a port. But buried in that plumbing is a body of decisions that are pure functions of data,
//! and they are the decisions that determine whether a tool call reaches the model as a denial, an
//! abort, or a success. Those are portable exactly, so they are what this module carries.
//!
//! # The one invariant every piece here shares
//!
//! **A successful outcome must never be reported after a cancellation, and a tool-owned failure must
//! never be rewritten.** The two halves pull in opposite directions — cancellation is the caller
//! withdrawing permission, tool failure is the tool reporting a result — and upstream resolves it
//! once, in `finish`: cancellation replaces only `!isError`. Porting that as
//! [`finalize_after_cancellation`] rather than re-deriving it at each call site is what keeps the
//! rule from drifting: a caller that "helpfully" also converted tool errors would erase the error the
//! model needs to read.
//!
//! # Deviations, all caused by Rust's type system being stricter than JavaScript's
//!
//! - **Absent versus explicit `undefined`.** Upstream distinguishes `Object.hasOwn(decision,
//!   'content')` from `content: undefined`, because a `tools/post-execute` listener can return the
//!   latter. `Option<Vec<ContentBlock>>` collapses the two. The only observable difference is a
//!   listener that explicitly returns `content: undefined`, which would keep the original content
//!   either way — so the collapse changes no outcome.
//! - **No hostile thrown values.** `toolErrorMessage` exists because a JavaScript `throw` can carry a
//!   `Proxy`, so reading `.message` may itself throw. A Rust `Result` carries a type, so this module
//!   takes messages that are already strings and the total-function fallback `'<unprintable thrown
//!   value>'` has no counterpart.
//! - **No `symbol` tokens.** Upstream's `ToolExecutionToken` keys a `WeakMap` so a wrapper cannot
//!   forge a "canonical" result. Rust has no such aliasing hazard within one value, so the
//!   canonical-marking machinery has no counterpart and is dropped rather than emulated.
//! - **No listener waterwalls.** `apply_post_decision` is given the decision the waterfall produced;
//!   running the waterfall is the caller's business. That keeps this module testable without a bus.

use std::collections::BTreeSet;
use std::fmt;

use girder_core::{ChatMessage, ContentBlock};
use serde_json::Value;

// ============================================================================================
// Error identities
// ============================================================================================

/// Identity recorded when cancellation lands after the tool body was invoked.
///
/// Upstream's `TOOL_ABORTED`, from `contracts.ts`. A distinct identity from
/// [`TOOL_ABORTED_BEFORE_DISPATCH`] because the two demand opposite advice: a call that never
/// started provably had no side effect, while one that was interrupted may have had half of one.
pub const TOOL_ABORTED: &str = "ABORTED";

/// Identity recorded when cancellation prevented the tool body from being invoked.
pub const TOOL_ABORTED_BEFORE_DISPATCH: &str = "ABORTED_BEFORE_DISPATCH";

/// Identity of a call naming a tool that is not reachable.
///
/// Upstream's `ToolNotFoundError` code. Carrying it makes an unknown tool as routable as a
/// tool-thrown failure, which is what lets retry and replay code tell "the model hallucinated a
/// name" apart from "the tool ran and failed".
pub const UNKNOWN_TOOL: &str = "UNKNOWN_TOOL";

/// Identity of a body or policy value that violates its declared output schema.
pub const INVALID_TOOL_OUTPUT: &str = "INVALID_TOOL_OUTPUT";

/// Identity of a failed step-time activation condition.
pub const TOOL_ACTIVATION_FAILED: &str = "TOOL_ACTIVATION_FAILED";

/// Identity of a tool that needs composition context it was not given.
pub const TOOL_CONTEXT_UNAVAILABLE: &str = "TOOL_CONTEXT_UNAVAILABLE";

/// Identity of arguments that do not match the tool's parameter schema.
pub const INVALID_ARGS: &str = "INVALID_ARGS";

/// Error *name* an approval denial is recorded under.
///
/// A name rather than a code, because upstream routes on the pair: the name says "this was the
/// approval path" and the code says which of the three non-grants it was.
pub const APPROVAL_ERROR_NAME: &str = "ApprovalError";

/// Approval code for a call a person rejected.
pub const APPROVAL_REJECTED: &str = "APPROVAL_REJECTED";

/// Approval code for a request cancelled without a caller cancellation.
pub const APPROVAL_CANCELLED: &str = "APPROVAL_CANCELLED";

/// Approval code for a call needing approval that nothing could answer.
pub const APPROVAL_UNAVAILABLE: &str = "APPROVAL_UNAVAILABLE";

/// The reserved model-facing transport name.
///
/// Reserved unconditionally: any agent may select a code mode for itself, so a name free to take
/// under the deployment default becomes a collision the moment such a preset mounts.
pub const RUN_CODE_NAME: &str = "run_code";

/// The default cap on how many parallel-safe calls may be in flight.
///
/// Upstream's agent-loop default. One of the few numbers in this layer that is a policy rather than
/// a consequence: a batch of fifty reads should not open fifty sockets.
pub const DEFAULT_MAX_PARALLEL: usize = 10;

/// A structured error identity carried beside a model-facing message.
///
/// Upstream's `ToolErrorInfo`. `reason` is deliberately *not* rendered into model-facing content:
/// it is the human-facing detail a UI shows, and folding it into the text the model reads would tell
/// the model things the tool chose not to say to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorInfo {
    /// Error class name, such as [`APPROVAL_ERROR_NAME`].
    pub name: String,
    /// Stable code routed on.
    pub code: String,
    /// Optional raw user-facing detail.
    pub reason: Option<String>,
}

impl ErrorInfo {
    /// An identity with no user-facing detail.
    #[must_use]
    pub fn new(name: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            code: code.into(),
            reason: None,
        }
    }

    /// Attaches user-facing detail.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// A failure detail: what the model is told, plus the optional identity for routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolFailure {
    /// The message without the `Error: ` envelope content carries.
    pub message: String,
    /// Routing identity, when the failure had one.
    pub info: Option<ErrorInfo>,
}

impl ToolFailure {
    /// A failure with no structured identity.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            info: None,
        }
    }

    /// A failure carrying an identity.
    #[must_use]
    pub fn identified(message: impl Into<String>, info: ErrorInfo) -> Self {
        Self {
            message: message.into(),
            info: Some(info),
        }
    }
}

// ============================================================================================
// Outcomes
// ============================================================================================

/// The normalized result of one tool call, as the pipeline commits it.
///
/// A module-local type rather than [`girder_core::ToolResult`] because the pipeline's outcome carries
/// three things that type does not: the structured error identity, deferred additional contexts, and
/// the turn-conclusion marker. Adding them to the crate's type would change a public shape for the
/// benefit of one layer; carrying them here keeps the addition local.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    /// Blocks the model reads.
    pub content: Vec<ContentBlock>,
    /// Whether the model should treat this as a failure.
    pub is_error: bool,
    /// The canonical value, present only on success.
    pub value: Option<Value>,
    /// Structured failure detail, present only on failure.
    pub error: Option<ToolFailure>,
    /// Context to append to the transcript after this result, in call order.
    pub additional_contexts: Vec<ChatMessage>,
    /// Whether a *successful* outcome ends the current turn.
    pub concludes_turn: bool,
}

impl Outcome {
    /// A successful outcome with model-facing content and an optional canonical value.
    #[must_use]
    pub fn success(content: Vec<ContentBlock>, value: Option<Value>) -> Self {
        Self {
            content,
            is_error: false,
            value,
            error: None,
            additional_contexts: Vec::new(),
            concludes_turn: false,
        }
    }

    /// A one-block text outcome.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::success(vec![ContentBlock::Text { text: text.into() }], None)
    }

    /// A failure with the canonical `Error: ` envelope and its message.
    #[must_use]
    pub fn failure(failure: ToolFailure) -> Self {
        let text = format!("Error: {}", failure.message);
        Self {
            content: vec![ContentBlock::Text { text }],
            is_error: true,
            value: None,
            error: Some(failure),
            additional_contexts: Vec::new(),
            concludes_turn: false,
        }
    }

    /// A failure whose model-facing content is supplied rather than derived from its message.
    ///
    /// Policy denials use this: the content is the reason the model reads, and the message is the
    /// same reason without presentation.
    #[must_use]
    pub fn error_content(
        content: Vec<ContentBlock>,
        message: impl Into<String>,
        info: Option<ErrorInfo>,
    ) -> Self {
        let message = message.into();
        Self {
            content,
            is_error: true,
            value: None,
            error: Some(ToolFailure { message, info }),
            additional_contexts: Vec::new(),
            concludes_turn: false,
        }
    }

    /// Attaches deferred additional contexts, appending after any already present.
    #[must_use]
    pub fn with_contexts(mut self, contexts: Vec<ChatMessage>) -> Self {
        self.additional_contexts.extend(contexts);
        self
    }

    /// Marks the outcome as concluding the turn.
    ///
    /// Only meaningful on success, and ignored otherwise, because a failed call that ended a turn
    /// would hide the failure from the model's next step.
    #[must_use]
    pub fn concluding(mut self, concludes: bool) -> Self {
        self.concludes_turn = concludes && !self.is_error;
        self
    }
}

// ============================================================================================
// Canonical results the pipeline synthesizes
// ============================================================================================

/// The canonical result when cancellation lands after the tool body was invoked.
///
/// `prior` carries any additional contexts the body already deferred, because those contexts were
/// produced by work that did happen: dropping them on cancellation would discard a nested dispatch's
/// context for a call whose side effects are precisely the ones a reader most needs to know about.
#[must_use]
pub fn aborted_outcome(prior: Option<&Outcome>) -> Outcome {
    let contexts = prior.map_or_else(Vec::new, |outcome| outcome.additional_contexts.clone());
    Outcome {
        content: vec![ContentBlock::Text {
            text: String::from("Error: tool call aborted"),
        }],
        is_error: true,
        value: None,
        error: Some(ToolFailure::identified(
            "tool call aborted",
            ErrorInfo::new("AbortError", TOOL_ABORTED),
        )),
        additional_contexts: contexts,
        concludes_turn: false,
    }
}

/// The canonical result when cancellation prevented the tool body from being invoked.
#[must_use]
pub fn aborted_before_dispatch_outcome(prior: Option<&Outcome>) -> Outcome {
    let contexts = prior.map_or_else(Vec::new, |outcome| outcome.additional_contexts.clone());
    Outcome {
        content: vec![ContentBlock::Text {
            text: String::from("Error: tool call aborted before dispatch"),
        }],
        is_error: true,
        value: None,
        error: Some(ToolFailure::identified(
            "tool call aborted before dispatch",
            ErrorInfo::new("AbortError", TOOL_ABORTED_BEFORE_DISPATCH),
        )),
        additional_contexts: contexts,
        concludes_turn: false,
    }
}

/// The canonical cancellation outcome, selected by whether the body started.
///
/// This is the single place the two abort identities are chosen. Upstream reaches the same decision
/// through `cancellationResult`, keyed off a `bodyInvoked` flag kept outside the wrapper-mutable
/// execution object — the flag has to live outside because a `tools/execute` wrapper can replace the
/// signal, and a wrapper is exactly the code that must not be able to claim the body never ran.
#[must_use]
pub fn cancellation_outcome(body_started: bool, prior: Option<&Outcome>) -> Outcome {
    if body_started {
        aborted_outcome(prior)
    } else {
        aborted_before_dispatch_outcome(prior)
    }
}

// ---- the failures the pipeline itself raises ------------------------------------------------
//
// Each of these is an upstream error *class* reduced to the two things a Rust caller needs: the
// model-facing message it produces and the identity it records. They are constructors rather than
// bare constants because the phrasing is part of the contract too — a reworded message is a
// behaviour change for every model reading it, and these are the four phrasings that recur.

/// The failure when a call names a tool that is not reachable.
///
/// `reachable_from` is for the case where the name *is* visible but only through another transport:
/// the message then says how to reach it, because "unknown tool" alone sends a model looking for a
/// typo that is not there.
#[must_use]
pub fn unknown_tool(tool_name: &str, reachable_from: Option<&str>) -> Outcome {
    let message = reachable_from.map_or_else(
        || format!("unknown tool \"{tool_name}\""),
        |how| format!("unknown tool \"{tool_name}\": {how}"),
    );
    Outcome::failure(ToolFailure::identified(
        message,
        ErrorInfo::new("ToolNotFoundError", UNKNOWN_TOOL),
    ))
}

/// The failure when a body or policy value violates its declared output schema.
///
/// Every violation is listed: a caller that reported only the first would make a tool with two
/// projection bugs look like two separate fixes.
#[must_use]
pub fn invalid_tool_output(tool_name: &str, violations: &[String]) -> Outcome {
    Outcome::failure(ToolFailure::identified(
        format!(
            "tool \"{tool_name}\" returned invalid output: {}",
            violations.join("; ")
        ),
        ErrorInfo::new("ToolOutputError", INVALID_TOOL_OUTPUT),
    ))
}

/// The failure when arguments do not match the tool's parameter schema.
#[must_use]
pub fn invalid_arguments(violations: &[String]) -> Outcome {
    Outcome::failure(ToolFailure::identified(
        format!("invalid arguments: {}", violations.join("; ")),
        ErrorInfo::new("ToolArgsError", INVALID_ARGS),
    ))
}

/// The failure when a tool needs composition context it was not given.
///
/// Not an argument the model can correct, which is why it carries its own identity: retrying the
/// same call cannot help, and a caller that treated this as `INVALID_ARGS` would make the model
/// rewrite a call that was already correct.
#[must_use]
pub fn context_unavailable(message: impl Into<String>) -> Outcome {
    Outcome::failure(ToolFailure::identified(
        message,
        ErrorInfo::new("ToolContextUnavailableError", TOOL_CONTEXT_UNAVAILABLE),
    ))
}

/// The failure when a step-time activation condition failed.
///
/// Distinct from "the tool is unavailable": a condition that *failed* may succeed if retried, while
/// a condition that answered false will not. Collapsing the two would tell the caller to retry
/// something that will never change.
#[must_use]
pub fn activation_failed(tool_name: &str) -> Outcome {
    Outcome::failure(ToolFailure::identified(
        format!("cannot resolve availability of tool \"{tool_name}\""),
        ErrorInfo::new("ToolActivationError", TOOL_ACTIVATION_FAILED),
    ))
}

/// Applies cancellation precedence to a settled outcome.
///
/// Cancellation replaces **only a successful** outcome. A tool-owned error passes through untouched,
/// which is the rule that keeps an interrupted-but-failed call reporting its failure: the model needs
/// to read "the file does not exist" more than it needs to read "aborted", and the side effect
/// question is already answered by `is_error` being the tool's own verdict.
#[must_use]
pub fn finalize_after_cancellation(
    cancelled: bool,
    body_started: bool,
    outcome: Outcome,
) -> Outcome {
    if cancelled && !outcome.is_error {
        cancellation_outcome(body_started, Some(&outcome))
    } else {
        outcome
    }
}

// ============================================================================================
// Approval: the four-value question and its one-to-one mapping
// ============================================================================================

/// What an approval channel answered.
///
/// Upstream's `ApprovalOutcome`, closed to four values. The three non-grants are kept distinct
/// because they demand different model behaviour: a rejection is a decision to respect, a
/// cancellation is an interruption to stop on, and an unavailable channel is an environment fault to
/// report — a model told only "denied" retries, and a model told "no channel" asks the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// A person allowed this one call.
    AllowedOnce,
    /// A person said no.
    Rejected,
    /// The request was cancelled.
    Cancelled,
    /// No channel could answer.
    Unavailable,
}

/// Why an approval request could not produce a grant.
///
/// Mirrors [`ApprovalOutcome`] minus [`ApprovalOutcome::AllowedOnce`], because a grant has no
/// denial to explain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalCode {
    /// Upstream's [`APPROVAL_REJECTED`].
    Rejected,
    /// Upstream's [`APPROVAL_CANCELLED`].
    Cancelled,
    /// Upstream's [`APPROVAL_UNAVAILABLE`].
    Unavailable,
}

impl ApprovalCode {
    /// The persisted code string.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Rejected => APPROVAL_REJECTED,
            Self::Cancelled => APPROVAL_CANCELLED,
            Self::Unavailable => APPROVAL_UNAVAILABLE,
        }
    }
}

/// A pre-dispatch decision.
///
/// Upstream's `PreToolDecision`. Separate from [`girder_core::PreToolDecision`] only in that its
/// `Deny` carries a structured identity, which is what makes an approval refusal routable.
#[derive(Clone, Debug, PartialEq)]
pub enum Gate {
    /// Run the call.
    Allow,
    /// Stop and ask a person.
    Ask {
        /// Audited reason for the question.
        reason: Option<String>,
    },
    /// Refuse with a model-facing reason.
    Deny {
        /// What the model reads.
        reason: String,
        /// Routing identity, when the denial had one.
        info: Option<ErrorInfo>,
    },
    /// Refuse as a cancellation rather than a policy decision.
    Cancel,
}

impl Gate {
    /// Whether the gate lets the call run.
    #[must_use]
    pub fn allows(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// A denial carrying the routable approval identity.
///
/// Every non-granting approval outcome becomes one of these, so a client can tell a human "no" from
/// an approval channel that was simply absent.
#[must_use]
pub fn approval_denial(reason: impl Into<String>, code: ApprovalCode) -> Gate {
    Gate::Deny {
        reason: reason.into(),
        info: Some(ErrorInfo::new(APPROVAL_ERROR_NAME, code.code())),
    }
}

/// What the approval seam could answer for one call.
///
/// Three variants rather than an `Option<ApprovalOutcome>` because "no approval service is mounted"
/// and "the service is mounted but this call has no agent to route through" deny with *different*
/// text, and a caller that collapsed them would tell the model to ask a user who cannot be reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskChannel {
    /// No approval service is composed in this deployment.
    Absent,
    /// A service exists, but the call has no agent to audit to or route through.
    NoAgent,
    /// The service answered.
    Answered(ApprovalOutcome),
}

/// The resolved result of an `ask` gate.
#[derive(Clone, Debug, PartialEq)]
pub struct AskResolution {
    /// The gate the ask resolved to: [`Gate::Allow`] or a denial.
    pub gate: Gate,
    /// Whether the approval channel reported a cancellation.
    ///
    /// Carried separately because it changes *which* abort the caller records: an approval
    /// cancellation that also saw caller cancellation means the body never ran, so the canonical
    /// result is [`TOOL_ABORTED_BEFORE_DISPATCH`] rather than a denial.
    pub approval_cancelled: bool,
}

/// Resolves an `ask` gate through the approval seam.
///
/// This is upstream's `serviceAsk`, with the seam passed in as data. Two degradation paths come
/// first and both deny rather than allow, because an `ask` that quietly became an `allow` whenever
/// nobody was listening is a fail-open on exactly the calls that asked for a human.
///
/// `reason` is the ask's audited reason; when absent, the tool name is used in the text so a model
/// reading the denial knows which call was refused.
#[must_use]
pub fn resolve_ask(tool_name: &str, reason: Option<&str>, channel: AskChannel) -> AskResolution {
    match channel {
        AskChannel::Absent => AskResolution {
            gate: approval_denial(
                reason.map_or_else(
                    || format!("tool \"{tool_name}\" requires approval (not yet supported)"),
                    str::to_owned,
                ),
                ApprovalCode::Unavailable,
            ),
            approval_cancelled: false,
        },
        AskChannel::NoAgent => AskResolution {
            gate: approval_denial(
                format!(
                    "tool \"{tool_name}\" requires approval, but the call has no agent to route it through"
                ),
                ApprovalCode::Unavailable,
            ),
            approval_cancelled: false,
        },
        AskChannel::Answered(ApprovalOutcome::AllowedOnce) => AskResolution {
            gate: Gate::Allow,
            approval_cancelled: false,
        },
        AskChannel::Answered(ApprovalOutcome::Rejected) => AskResolution {
            gate: approval_denial(
                format!("the user rejected tool \"{tool_name}\""),
                ApprovalCode::Rejected,
            ),
            approval_cancelled: false,
        },
        AskChannel::Answered(ApprovalOutcome::Cancelled) => AskResolution {
            gate: approval_denial(
                format!("approval for tool \"{tool_name}\" was cancelled"),
                ApprovalCode::Cancelled,
            ),
            approval_cancelled: true,
        },
        AskChannel::Answered(ApprovalOutcome::Unavailable) => AskResolution {
            gate: approval_denial(
                format!(
                    "tool \"{tool_name}\" requires approval, but no approval channel is available"
                ),
                ApprovalCode::Unavailable,
            ),
            approval_cancelled: false,
        },
    }
}

/// Materializes a denial gate into the outcome the model sees.
#[must_use]
pub fn denial_outcome(gate: &Gate) -> Option<Outcome> {
    match gate {
        Gate::Deny { reason, info } => Some(Outcome::error_content(
            vec![ContentBlock::Text {
                text: format!("Error: {reason}"),
            }],
            reason.clone(),
            info.clone(),
        )),
        Gate::Allow | Gate::Ask { .. } | Gate::Cancel => None,
    }
}

// ============================================================================================
// The post-execute decision
// ============================================================================================

/// What `tools/post-execute` decided about a dispatched result.
///
/// Upstream's `PostToolDecision`, minus the two `accept` variants that differ only in which
/// field they carry: in Rust one struct with two [`Option`]s expresses the same union, and the
/// mutual exclusion is enforced by [`PostDecisionError::BothValueAndContent`] at application time.
#[derive(Clone, Debug, PartialEq)]
pub enum PostDecision {
    /// Keep the result, optionally replacing its content or its canonical value.
    Accept {
        /// Replacement content, or [`None`] to keep the rendered content.
        content: Option<Vec<ContentBlock>>,
        /// Replacement canonical value, or [`None`] to keep the existing one.
        value: Option<Value>,
        /// Context this decision attaches, appended after the result's own.
        additional_contexts: Vec<ChatMessage>,
    },
    /// Turn the result into a failure carrying corrective feedback.
    Block {
        /// What the model reads instead of the result.
        feedback: Vec<ContentBlock>,
        /// Routing identity for the new failure.
        info: Option<ErrorInfo>,
        /// Context the blocking decision supplies; it replaces, rather than extends, the result's.
        additional_contexts: Vec<ChatMessage>,
    },
}

impl PostDecision {
    /// Accepts unchanged.
    #[must_use]
    pub fn accept() -> Self {
        Self::Accept {
            content: None,
            value: None,
            additional_contexts: Vec::new(),
        }
    }

    /// Accepts with replacement content.
    #[must_use]
    pub fn accept_content(content: Vec<ContentBlock>) -> Self {
        Self::Accept {
            content: Some(content),
            value: None,
            additional_contexts: Vec::new(),
        }
    }

    /// Blocks with corrective feedback.
    #[must_use]
    pub fn block(feedback: Vec<ContentBlock>) -> Self {
        Self::Block {
            feedback,
            info: None,
            additional_contexts: Vec::new(),
        }
    }
}

/// Why a post-execute decision could not be applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostDecisionError {
    /// An `accept` replaced both the value and the content.
    ///
    /// Refused rather than resolved by precedence: the two describe the same thing — what the model
    /// reads — and a listener that set both has a bug that would otherwise pass silently on every
    /// call where the two happened to agree.
    BothValueAndContent,
    /// An `accept` tried to replace the value of a failed result.
    ///
    /// A failure carries no value, so there is nothing to replace; a listener that reached this has
    /// confused a success path for a failure path.
    ReplaceValueOfFailure,
}

impl fmt::Display for PostDecisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BothValueAndContent => {
                "tools/post-execute accept decision cannot replace both value and content"
            }
            Self::ReplaceValueOfFailure => {
                "tools/post-execute cannot replace the value of a failed result"
            }
        })
    }
}

impl std::error::Error for PostDecisionError {}

/// Applies a post-execute decision to one settled result.
///
/// `render` supplies the tool's projection from a replacement value to model-facing content, which is
/// the step upstream reaches back into `tool.output.render` for. Passing it in keeps the *decision*
/// (what may be replaced, in what order contexts are appended) separate from the *projection* (how a
/// value becomes blocks), and means this function can be tested without a registry.
///
/// Three rules survive here that are not obvious from the shapes:
///
/// 1. **A block discards deferred context.** The body's context describes work that happened; when
///    the result is blocked, the model must not receive context explaining output it was not shown.
///    Only what the blocking decision explicitly supplies survives.
/// 2. **Context order is body first, then decision.** Contexts are appended to the transcript in call
///    order, so a nested dispatch's context must precede the policy's comment on it.
/// 3. **A block clears the turn-conclusion marker.** A success that was going to end the turn no
///    longer does once it is blocked, and leaving the marker set would end the turn on a failure.
pub fn apply_post_decision<F>(
    result: Outcome,
    decision: PostDecision,
    render: F,
) -> Result<Outcome, PostDecisionError>
where
    F: FnOnce(&Value) -> Vec<ContentBlock>,
{
    match decision {
        PostDecision::Block {
            feedback,
            info,
            additional_contexts,
        } => {
            let message = failure_message_from_content(&feedback);
            Ok(Outcome {
                content: feedback,
                is_error: true,
                value: None,
                error: Some(ToolFailure { message, info }),
                additional_contexts,
                concludes_turn: false,
            })
        }
        PostDecision::Accept {
            content,
            value,
            additional_contexts,
        } => {
            if content.is_some() && value.is_some() {
                return Err(PostDecisionError::BothValueAndContent);
            }
            let mut contexts = result.additional_contexts.clone();
            contexts.extend(additional_contexts);
            if let Some(replacement) = value {
                if result.is_error {
                    return Err(PostDecisionError::ReplaceValueOfFailure);
                }
                let rendered = render(&replacement);
                return Ok(Outcome {
                    content: rendered,
                    is_error: false,
                    value: Some(replacement),
                    error: None,
                    additional_contexts: contexts,
                    concludes_turn: result.concludes_turn,
                });
            }
            Ok(Outcome {
                content: content.unwrap_or(result.content),
                is_error: result.is_error,
                value: result.value,
                error: result.error,
                additional_contexts: contexts,
                concludes_turn: result.concludes_turn,
            })
        }
    }
}

/// Derives a failure message from policy feedback without changing its rendered blocks.
///
/// Text blocks are joined with newlines; every other block becomes its bracketed type name, because a
/// message that silently dropped an image would report less than the model was shown. An empty
/// feedback list still needs a message, and the fallback names the policy that produced it rather
/// than an empty string that reads as a bug.
#[must_use]
pub fn failure_message_from_content(content: &[ContentBlock]) -> String {
    let text: String = content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.clone(),
            other => format!("[{} content]", other.type_name()),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        String::from("tool result blocked by post-execute policy")
    } else {
        text
    }
}

// ============================================================================================
// Batch: argument parsing, the concurrency cap, and group classification
// ============================================================================================

/// Parses model-supplied arguments, preserving malformed input as text.
///
/// Two deliberate choices, both upstream's:
///
/// - **Empty input is `{}`.** A tool with no parameters is called with an empty string by some
///   providers, and treating that as a parse error would fail a call that is well formed.
/// - **Malformed JSON is preserved as a string.** The parse error is a fact about the *model's*
///   input, and the tool that owns the arguments is the only one that can produce a useful diagnostic
///   for it. Throwing here would replace the tool's argument validation with a generic parse message
///   that names a byte offset and nothing the model can act on.
#[must_use]
pub fn parse_arguments(raw: &str) -> Value {
    if raw.is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

/// Why a concurrency cap was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaxParallelError;

impl fmt::Display for MaxParallelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("maxParallelToolCalls must be a positive integer")
    }
}

impl std::error::Error for MaxParallelError {}

/// Resolves the effective concurrency cap for one batch.
///
/// An explicit override wins over the executor's configured value, and zero is refused in both
/// positions: a cap of zero would mean "start nothing", so a batch would drain empty and the model
/// would receive no results at all for calls it made — the failure mode a *positive* lower bound
/// exists to prevent. Rust's `usize` already excludes a fractional or negative override, so the
/// "integer" half of upstream's check has no counterpart here.
pub fn resolve_max_parallel(
    override_value: Option<usize>,
    configured: usize,
) -> Result<usize, MaxParallelError> {
    let effective = override_value.unwrap_or(configured);
    if effective == 0 {
        Err(MaxParallelError)
    } else {
        Ok(effective)
    }
}

/// How one pending call is scheduled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// May overlap with sibling calls.
    Parallel,
    /// Runs alone and forms an ordering barrier.
    Exclusive,
}

impl Mode {
    /// Whether siblings may run alongside this call.
    #[must_use]
    pub const fn is_parallel(self) -> bool {
        matches!(self, Self::Parallel)
    }
}

/// Classifies one pending call, failing closed.
///
/// Only an exact "yes" opts a call into overlap. Every other answer — no classifier declared, a
/// classifier that returned something that is not `true`, a classifier that threw, or a call whose
/// name does not resolve at all — is [`Mode::Exclusive`]. The asymmetry is deliberate: an
/// over-conservative answer costs wall-clock time, and an over-permissive one loses a write.
#[must_use]
pub fn execution_mode(visible: bool, rejected: bool, declared_safe: Option<bool>) -> Mode {
    if visible && !rejected && declared_safe == Some(true) {
        Mode::Parallel
    } else {
        Mode::Exclusive
    }
}

/// Returns the exclusive end index of the group starting at `start`.
///
/// An exclusive call is a barrier of one. A parallel call begins a run that extends over every
/// following parallel call, because re-classifying between starts is what lets a registry change
/// turn a later call into a barrier without aborting the batch.
///
/// A `start` at or past the end yields `start`, so a caller iterating with
/// `next = group_end(...)` terminates rather than looping.
#[must_use]
pub fn group_end(modes: &[Mode], start: usize) -> usize {
    if start >= modes.len() {
        return start;
    }
    if modes[start] == Mode::Exclusive {
        return start + 1;
    }
    let mut end = start + 1;
    while end < modes.len() && modes[end] == Mode::Parallel {
        end += 1;
    }
    end
}

/// Splits a batch into the groups a scheduler would run in order.
///
/// Each entry is `(start, len)`. Concatenating them covers `0..modes.len()` exactly once, which is
/// the invariant a scheduler needs: a call that fell between groups would never start, and one
/// covered twice would be dispatched twice.
#[must_use]
pub fn plan_groups(modes: &[Mode]) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < modes.len() {
        let end = group_end(modes, start);
        groups.push((start, end - start));
        start = end;
    }
    groups
}

/// One committed call outcome, correlated with the call it answers.
#[derive(Clone, Debug, PartialEq)]
pub struct CallOutcome {
    /// The provider-issued call id.
    pub call_id: String,
    /// The settled outcome.
    pub outcome: Outcome,
}

/// Everything one batch settled, in model order.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchReport {
    /// One entry per input call, in the order the model requested them.
    pub results: Vec<CallOutcome>,
    /// Context to append to the transcript, in call order.
    pub additional_contexts: Vec<ChatMessage>,
    /// Whether any committed success concludes the turn.
    pub concluded: bool,
}

impl BatchReport {
    /// Whether every input call has a result.
    ///
    /// The provider requires one `tool_result` per `tool_use`, so a report where this is false is not
    /// a shorter answer — it is a transcript the next request will be rejected for.
    #[must_use]
    pub fn is_complete_for(&self, expected: usize) -> bool {
        self.results.len() == expected
    }
}

/// Accumulates settled outcomes in model order, including synthesized skips.
///
/// The ordering rule is the whole reason this type exists. Bodies may settle out of order, but the
/// transcript is read in model order, so the ledger commits by index and refuses to skip ahead: a
/// gap would either reorder the results or drop one.
#[derive(Debug, Default)]
pub struct Settlement {
    results: Vec<CallOutcome>,
    contexts: Vec<ChatMessage>,
    pending: BTreeSet<usize>,
    next: usize,
}

impl Settlement {
    /// An empty ledger for a batch of `expected` calls.
    ///
    /// Every index starts pending, so a caller that forgot to commit one can observe the gap with
    /// [`Self::is_complete`] rather than discovering it in the provider's rejection.
    #[must_use]
    pub fn new(expected: usize) -> Self {
        Self {
            results: Vec::new(),
            contexts: Vec::new(),
            pending: (0..expected).collect(),
            next: 0,
        }
    }

    /// How many calls the batch declared.
    #[must_use]
    pub fn expected(&self) -> usize {
        self.pending.len() + self.results.len()
    }

    /// Whether every declared call has committed a result.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.pending.is_empty() && self.results.len() == self.next
    }

    /// Indices still awaiting a result, in ascending order.
    #[must_use]
    pub fn outstanding(&self) -> Vec<usize> {
        self.pending.iter().copied().collect()
    }

    /// Commits the outcome for the call at `index`.
    ///
    /// Returns `false` when the index is out of range, already committed, or not the next one in
    /// model order. A refused commit changes nothing: a partially applied commit would leave the
    /// ledger attributing a result to the wrong call, which is worse than no result at all.
    pub fn commit(&mut self, index: usize, call_id: &str, outcome: Outcome) -> bool {
        if index != self.next || !self.pending.remove(&index) {
            return false;
        }
        self.contexts.extend(outcome.additional_contexts.clone());
        self.results.push(CallOutcome {
            call_id: call_id.to_owned(),
            outcome,
        });
        self.next += 1;
        true
    }

    /// Records the canonical aborted-before-dispatch result for a call that never started.
    ///
    /// Returns `false` under the same conditions as [`Self::commit`].
    pub fn skip(&mut self, index: usize, call_id: &str) -> bool {
        self.commit(index, call_id, aborted_before_dispatch_outcome(None))
    }

    /// Records skips for every outstanding index at or after `from`.
    ///
    /// Returns how many were synthesized. Used when a batch aborts: every call the model made still
    /// needs an answer, and a call that never started is provably side-effect free, which is exactly
    /// what [`TOOL_ABORTED_BEFORE_DISPATCH`] tells the reader.
    pub fn skip_from(&mut self, from: usize, calls: &[String]) -> usize {
        let mut skipped = 0;
        for (index, call_id) in calls.iter().enumerate().skip(from) {
            if self.pending.contains(&index) {
                // Every index between `next` and `from` must already have committed, which the
                // scheduler guarantees; a gap here would be refused, so report nothing skipped.
                if !self.skip(index, call_id) {
                    break;
                }
                skipped += 1;
            }
        }
        skipped
    }

    /// Finalizes the ledger into the report the caller returns.
    #[must_use]
    pub fn report(self) -> BatchReport {
        let concluded = self
            .results
            .iter()
            .any(|call| call.outcome.concludes_turn && !call.outcome.is_error);
        BatchReport {
            results: self.results,
            additional_contexts: self.contexts,
            concluded,
        }
    }
}

// ============================================================================================
// Tool restrictions
// ============================================================================================

/// One scope's compiled mask over the global tools.
///
/// Compiled once at registration rather than re-walked on every lookup: a restriction is consulted
/// for every name in every step's declaration, and a `Vec<String>` scan there would be quadratic in a
/// hot path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompiledRestriction {
    allow: Option<BTreeSet<String>>,
    deny: Option<BTreeSet<String>>,
}

impl CompiledRestriction {
    /// Whether this restriction keeps `name` visible.
    ///
    /// Both halves must admit it, and an allow-list that does not name the tool removes it: an
    /// allow-list is an exhaustive selection, not a hint.
    #[must_use]
    pub fn admits(&self, name: &str) -> bool {
        if let Some(allow) = &self.allow
            && !allow.contains(name)
        {
            return false;
        }
        if let Some(deny) = &self.deny
            && deny.contains(name)
        {
            return false;
        }
        true
    }

    /// The allow-list, when one was declared.
    #[must_use]
    pub fn allow(&self) -> Option<&BTreeSet<String>> {
        self.allow.as_ref()
    }

    /// The deny-list, when one was declared.
    #[must_use]
    pub fn deny(&self) -> Option<&BTreeSet<String>> {
        self.deny.as_ref()
    }
}

/// Why a restriction was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestrictionError {
    /// Neither list was supplied.
    ///
    /// Refused rather than treated as a no-op: an empty filter is almost always a configuration that
    /// materialized empty, and accepting it would make a misconfigured restriction look like a
    /// deliberate one.
    Empty,
    /// A list named the reserved transport.
    ReservedTransport {
        /// The name that was refused, always [`RUN_CODE_NAME`].
        name: String,
    },
    /// A list named a tool that is not a visible global tool.
    Unknown {
        /// The names that did not resolve, sorted.
        names: Vec<String>,
        /// The known global names, sorted, for the message.
        known: Vec<String>,
    },
}

impl fmt::Display for RestrictionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str(
                "tools.restrict({}) is a no-op: pass `allow` and/or `deny` (an empty filter is almost always a materialized-empty-config bug)",
            ),
            Self::ReservedTransport { name } => write!(
                formatter,
                "tools.restrict() cannot name reserved PTC mode presentation transport \"{name}\"; restrict end-capability tools instead"
            ),
            Self::Unknown { names, known } => {
                let plural = if names.len() > 1 { "s" } else { "" };
                let listed = names
                    .iter()
                    .map(|name| format!("\"{name}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let known = if known.is_empty() {
                    String::from("(none)")
                } else {
                    known.join(", ")
                };
                write!(
                    formatter,
                    "tools.restrict() names unknown global tool{plural} {listed}; known global tools: {known}"
                )
            }
        }
    }
}

impl std::error::Error for RestrictionError {}

/// Compiles a restriction, validating its names against the visible global tools.
///
/// `allow` and `deny` are [`Option`]s rather than possibly-empty slices because "not supplied" and
/// "supplied empty" mean opposite things: an empty allow-list admits nothing, while an absent one
/// admits everything the deny-list does not remove.
///
/// The reserved transport is refused from both lists. It is not an end-capability a scope may mask —
/// masking it would hide the transport a code-mode preset depends on while leaving native dispatch
/// reachable, which is a configuration that looks restricted and is not.
pub fn compile_restriction(
    allow: Option<&[&str]>,
    deny: Option<&[&str]>,
    known: &[String],
) -> Result<CompiledRestriction, RestrictionError> {
    if allow.is_none() && deny.is_none() {
        return Err(RestrictionError::Empty);
    }
    let mut named: Vec<String> = Vec::new();
    for list in [allow, deny].into_iter().flatten() {
        named.extend(list.iter().map(|name| (*name).to_owned()));
    }
    if named.iter().any(|name| name == RUN_CODE_NAME) {
        return Err(RestrictionError::ReservedTransport {
            name: String::from(RUN_CODE_NAME),
        });
    }
    let known_set: BTreeSet<&str> = known.iter().map(String::as_str).collect();
    let unknown: BTreeSet<String> = named
        .into_iter()
        .filter(|name| !known_set.contains(name.as_str()))
        .collect();
    if !unknown.is_empty() {
        let mut sorted_known: Vec<String> =
            known_set.iter().map(|name| (*name).to_owned()).collect();
        sorted_known.sort();
        return Err(RestrictionError::Unknown {
            names: unknown.into_iter().collect(),
            known: sorted_known,
        });
    }
    let compile = |list: Option<&[&str]>| -> Option<BTreeSet<String>> {
        list.map(|names| names.iter().map(|name| (*name).to_owned()).collect())
    };
    Ok(CompiledRestriction {
        allow: compile(allow),
        deny: compile(deny),
    })
}

/// Whether every restriction in the chain admits `name`.
///
/// Restrictions **intersect**: a tool masked by any layer in the scope chain is gone, because each
/// layer was installed by a party that had the authority to remove it and a union would let the
/// outermost layer undo an inner one.
#[must_use]
pub fn admits_through(restrictions: &[CompiledRestriction], name: &str) -> bool {
    restrictions
        .iter()
        .all(|restriction| restriction.admits(name))
}

// ============================================================================================
// Definition-time validation
// ============================================================================================

/// One callback a tool definition may declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Callback {
    /// The tool body.
    Execute,
    /// The pending-state presenter.
    PresentCall,
    /// The completed-state presenter.
    PresentResult,
    /// The sibling-overlap classifier.
    IsConcurrencySafe,
}

impl Callback {
    /// Whether invalid arguments make this callback *raise* rather than fall back.
    ///
    /// Only the body raises. Everything else runs during replay of a log that may predate the current
    /// schema, so a presenter that raised on obsolete arguments would make an old session unreadable
    /// and a classifier that raised would turn a display concern into a dispatch failure. The split
    /// is one line to check and one line to get wrong, which is why it is a method rather than a
    /// comment on three separate functions.
    #[must_use]
    pub const fn rejects_invalid_arguments(self) -> bool {
        matches!(self, Self::Execute)
    }

    /// What this callback returns when its arguments do not validate.
    #[must_use]
    pub const fn fallback_on_invalid_arguments(self) -> Fallback {
        match self {
            Self::Execute => Fallback::Raise,
            Self::PresentCall | Self::PresentResult => Fallback::Omit,
            Self::IsConcurrencySafe => Fallback::Exclusive,
        }
    }
}

/// What a callback does when its arguments do not match the schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fallback {
    /// Fail the call with [`INVALID_ARGS`].
    Raise,
    /// Return no view, so the client draws the generic card.
    Omit,
    /// Classify the call as exclusive.
    Exclusive,
}

/// Why a declared timeout was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeoutError;

impl fmt::Display for TimeoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("timeoutMs must be a positive finite number")
    }
}

impl std::error::Error for TimeoutError {}

/// Validates a declared cooperative timeout budget.
///
/// A timeout of zero or a non-finite value is refused at definition time, because both produce a
/// deadline that either expired before the body started or never expires — and a tool that declares a
/// budget it cannot honour is claiming cancellation safety it does not have.
pub fn validate_timeout_ms(timeout_ms: Option<f64>) -> Result<Option<f64>, TimeoutError> {
    match timeout_ms {
        None => Ok(None),
        Some(value) if value.is_finite() && value > 0.0 => Ok(Some(value)),
        Some(_) => Err(TimeoutError),
    }
}

// ============================================================================================
// Tests live beside the module so `cargo test -p girder-core` runs them with the crate.
// ============================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(outcome: &Outcome) -> String {
        outcome
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// The error code an outcome carries, or the empty string when it carries none.
    fn code_of(outcome: &Outcome) -> &str {
        outcome
            .error
            .as_ref()
            .and_then(|failure| failure.info.as_ref())
            .map_or("", |info| info.code.as_str())
    }

    // ---- canonical results ---------------------------------------------------------------------

    #[test]
    fn the_two_abort_identities_are_distinct_and_self_describing() {
        let started = aborted_outcome(None);
        let skipped = aborted_before_dispatch_outcome(None);

        assert_eq!(text_of(&started), "Error: tool call aborted");
        assert_eq!(
            text_of(&skipped),
            "Error: tool call aborted before dispatch"
        );
        assert_eq!(
            started.error.as_ref().unwrap().info.as_ref().unwrap().code,
            TOOL_ABORTED
        );
        assert_eq!(
            skipped.error.as_ref().unwrap().info.as_ref().unwrap().code,
            TOOL_ABORTED_BEFORE_DISPATCH
        );
        // A reader must be able to tell a provably-effect-free call from a possibly partial one.
        assert_ne!(TOOL_ABORTED, TOOL_ABORTED_BEFORE_DISPATCH);
    }

    #[test]
    fn an_abort_preserves_context_the_body_already_deferred() {
        let prior = Outcome::text("partial").with_contexts(vec![context("nested")]);
        let aborted = aborted_outcome(Some(&prior));

        assert_eq!(aborted.additional_contexts.len(), 1);
        assert_eq!(aborted.additional_contexts[0].id.to_string(), "nested");
    }

    #[test]
    fn cancellation_selects_the_identity_from_whether_the_body_started() {
        assert_eq!(
            cancellation_outcome(true, None)
                .error
                .unwrap()
                .info
                .unwrap()
                .code,
            TOOL_ABORTED
        );
        assert_eq!(
            cancellation_outcome(false, None)
                .error
                .unwrap()
                .info
                .unwrap()
                .code,
            TOOL_ABORTED_BEFORE_DISPATCH
        );
    }

    #[test]
    fn cancellation_replaces_only_a_success() {
        let success = Outcome::text("the answer");
        let replaced = finalize_after_cancellation(true, true, success);
        assert!(replaced.is_error);
        assert_eq!(text_of(&replaced), "Error: tool call aborted");

        // A tool-owned failure keeps its own words: the model needs the failure, not the abort.
        let failure = Outcome::failure(ToolFailure::new("no such file"));
        let kept = finalize_after_cancellation(true, true, failure);
        assert_eq!(text_of(&kept), "Error: no such file");
        assert_eq!(kept.error.unwrap().message, "no such file");
    }

    #[test]
    fn no_cancellation_leaves_the_outcome_alone() {
        let success = Outcome::text("done");
        let kept = finalize_after_cancellation(false, true, success);
        assert!(!kept.is_error);
        assert_eq!(text_of(&kept), "done");
    }

    #[test]
    fn a_failure_carries_the_envelope_and_its_identity() {
        let outcome = Outcome::failure(ToolFailure::identified(
            "unknown tool \"x\"",
            ErrorInfo::new("ToolNotFoundError", UNKNOWN_TOOL),
        ));
        assert_eq!(text_of(&outcome), "Error: unknown tool \"x\"");
        let info = outcome.error.unwrap().info.unwrap();
        assert_eq!(info.name, "ToolNotFoundError");
        assert_eq!(info.code, UNKNOWN_TOOL);
    }

    #[test]
    fn a_user_facing_reason_rides_the_identity_but_never_the_models_text() {
        // The reason is for a UI to show a human; putting it in model-facing content would tell the
        // model things the tool chose not to say to it.
        let outcome = Outcome::failure(ToolFailure::identified(
            "tool call failed",
            ErrorInfo::new("SandboxError", "SANDBOX_DENIED")
                .with_reason("blocked by seatbelt profile"),
        ));

        assert_eq!(text_of(&outcome), "Error: tool call failed");
        assert!(!text_of(&outcome).contains("seatbelt"));
        let info = outcome.error.unwrap().info.unwrap();
        assert_eq!(info.reason.as_deref(), Some("blocked by seatbelt profile"));
    }

    #[test]
    fn a_reachable_unknown_tool_says_how_to_reach_it() {
        // The name exists, only not directly: without the hint a model hunts a typo that is not there.
        let outcome = unknown_tool(
            "read_file",
            Some("call `run_code` and use `tools.read_file`"),
        );
        assert_eq!(
            text_of(&outcome),
            "Error: unknown tool \"read_file\": call `run_code` and use `tools.read_file`"
        );
        assert_eq!(code_of(&outcome), UNKNOWN_TOOL);

        let missing = unknown_tool("nope", None);
        assert_eq!(text_of(&missing), "Error: unknown tool \"nope\"");
    }

    #[test]
    fn invalid_output_lists_every_violation() {
        let outcome = invalid_tool_output("write", &[String::from("value.rows: not an array")]);
        assert_eq!(code_of(&outcome), INVALID_TOOL_OUTPUT);
        assert!(text_of(&outcome).contains("value.rows: not an array"));
        assert!(
            outcome
                .error
                .unwrap()
                .message
                .contains("tool \"write\" returned invalid output")
        );
    }

    #[test]
    fn invalid_arguments_and_unavailable_context_are_distinguishable() {
        // One is the model's to fix; the other is not. Same rendering, different identity.
        let args = invalid_arguments(&[String::from("missing required property \"path\"")]);
        let context = context_unavailable("`shell` requires an open turn");

        assert_eq!(code_of(&args), INVALID_ARGS);
        assert_eq!(code_of(&context), TOOL_CONTEXT_UNAVAILABLE);
        assert_ne!(code_of(&args), code_of(&context));
    }

    #[test]
    fn a_failed_activation_is_not_the_same_as_an_absent_tool() {
        let failed = activation_failed("subagent");
        assert_eq!(code_of(&failed), TOOL_ACTIVATION_FAILED);
        assert_ne!(code_of(&failed), UNKNOWN_TOOL);
        assert!(text_of(&failed).contains("cannot resolve availability"));
    }

    // ---- approval ------------------------------------------------------------------------------

    #[test]
    fn a_grant_is_the_only_path_to_allow() {
        let granted = resolve_ask(
            "shell",
            None,
            AskChannel::Answered(ApprovalOutcome::AllowedOnce),
        );
        assert!(granted.gate.allows());
        assert!(!granted.approval_cancelled);

        for outcome in [
            ApprovalOutcome::Rejected,
            ApprovalOutcome::Cancelled,
            ApprovalOutcome::Unavailable,
        ] {
            let resolved = resolve_ask("shell", None, AskChannel::Answered(outcome));
            assert!(!resolved.gate.allows(), "{outcome:?} must not allow");
        }
    }

    #[test]
    fn the_three_non_grants_carry_three_distinct_codes() {
        let rejected = resolve_ask(
            "shell",
            None,
            AskChannel::Answered(ApprovalOutcome::Rejected),
        );
        let Gate::Deny { info, .. } = rejected.gate else {
            panic!("expected a denial");
        };
        let info = info.unwrap();
        assert_eq!(info.name, APPROVAL_ERROR_NAME);
        assert_eq!(info.code, APPROVAL_REJECTED);

        let cancelled = resolve_ask(
            "shell",
            None,
            AskChannel::Answered(ApprovalOutcome::Cancelled),
        );
        let Gate::Deny { info, .. } = cancelled.gate else {
            panic!("expected a denial");
        };
        assert_eq!(info.unwrap().code, APPROVAL_CANCELLED);
        // Only a cancelled approval means the body provably never ran.
        assert!(cancelled.approval_cancelled);
        assert!(!rejected.approval_cancelled);
    }

    #[test]
    fn a_missing_channel_denies_with_its_own_words() {
        let absent = resolve_ask("shell", None, AskChannel::Absent);
        let Gate::Deny { reason, info } = absent.gate else {
            panic!("expected a denial");
        };
        assert!(reason.contains("not yet supported"));
        assert_eq!(info.unwrap().code, APPROVAL_UNAVAILABLE);

        let no_agent = resolve_ask("shell", None, AskChannel::NoAgent);
        let Gate::Deny { reason, .. } = no_agent.gate else {
            panic!("expected a denial");
        };
        // The two degradation paths must not read the same: one says ask your user, one cannot.
        assert!(reason.contains("no agent"));
    }

    #[test]
    fn an_audited_reason_is_used_verbatim_when_absent_channel_denies() {
        let resolved = resolve_ask(
            "shell",
            Some("writes outside the workspace"),
            AskChannel::Absent,
        );
        let Gate::Deny { reason, .. } = resolved.gate else {
            panic!("expected a denial");
        };
        assert_eq!(reason, "writes outside the workspace");
    }

    #[test]
    fn a_denial_materializes_into_a_routable_error_outcome() {
        let denial = approval_denial("the user rejected tool \"shell\"", ApprovalCode::Rejected);
        let outcome = denial_outcome(&denial).expect("a denial has an outcome");

        assert!(outcome.is_error);
        assert_eq!(text_of(&outcome), "Error: the user rejected tool \"shell\"");
        assert_eq!(outcome.error.unwrap().info.unwrap().code, APPROVAL_REJECTED);
        // Allow, Ask and Cancel have no materialized denial of their own.
        assert!(denial_outcome(&Gate::Allow).is_none());
        assert!(denial_outcome(&Gate::Cancel).is_none());
        assert!(denial_outcome(&Gate::Ask { reason: None }).is_none());
    }

    // ---- post-execute --------------------------------------------------------------------------

    fn context(id: &str) -> ChatMessage {
        ChatMessage::user(girder_core::MessageId::new(id).expect("valid id"), "note")
    }

    #[test]
    fn accept_without_replacement_is_the_identity() {
        let result = Outcome::success(vec![ContentBlock::Text { text: "hi".into() }], None);
        let applied = apply_post_decision(result.clone(), PostDecision::accept(), |_| Vec::new())
            .expect("accept applies");
        assert_eq!(applied, result);
    }

    #[test]
    fn accept_replaces_content_and_keeps_the_value() {
        let result = Outcome::success(
            vec![ContentBlock::Text { text: "raw".into() }],
            Some(serde_json::json!({"n": 1})),
        );
        let applied = apply_post_decision(
            result,
            PostDecision::accept_content(vec![ContentBlock::Text {
                text: "polished".into(),
            }]),
            |_| Vec::new(),
        )
        .expect("accept applies");

        assert_eq!(text_of(&applied), "polished");
        assert_eq!(applied.value, Some(serde_json::json!({"n": 1})));
        assert!(!applied.is_error);
    }

    #[test]
    fn accept_replacing_a_value_re_renders_through_the_supplied_projection() {
        let result = Outcome::success(
            vec![ContentBlock::Text { text: "old".into() }],
            Some(serde_json::json!({"n": 1})),
        );
        let applied = apply_post_decision(
            result,
            PostDecision::Accept {
                content: None,
                value: Some(serde_json::json!({"n": 2})),
                additional_contexts: Vec::new(),
            },
            |value| {
                vec![ContentBlock::Text {
                    text: format!("rendered {}", value["n"]),
                }]
            },
        )
        .expect("accept applies");

        assert_eq!(text_of(&applied), "rendered 2");
        assert_eq!(applied.value, Some(serde_json::json!({"n": 2})));
        assert!(applied.error.is_none());
    }

    #[test]
    fn replacing_both_value_and_content_is_refused() {
        let result = Outcome::success(Vec::new(), Some(serde_json::json!(1)));
        let error = apply_post_decision(
            result,
            PostDecision::Accept {
                content: Some(vec![ContentBlock::Text { text: "x".into() }]),
                value: Some(serde_json::json!(2)),
                additional_contexts: Vec::new(),
            },
            |_| Vec::new(),
        )
        .expect_err("both is refused");

        assert_eq!(error, PostDecisionError::BothValueAndContent);
        assert_eq!(
            error.to_string(),
            "tools/post-execute accept decision cannot replace both value and content"
        );
    }

    #[test]
    fn replacing_the_value_of_a_failure_is_refused() {
        let failure = Outcome::failure(ToolFailure::new("boom"));
        let error = apply_post_decision(
            failure,
            PostDecision::Accept {
                content: None,
                value: Some(serde_json::json!(1)),
                additional_contexts: Vec::new(),
            },
            |_| Vec::new(),
        )
        .expect_err("a failure has no value to replace");

        assert_eq!(error, PostDecisionError::ReplaceValueOfFailure);
    }

    #[test]
    fn a_block_becomes_an_error_whose_message_comes_from_its_feedback() {
        let result = Outcome::success(
            vec![ContentBlock::Text { text: "raw".into() }],
            Some(serde_json::json!(1)),
        );
        let applied = apply_post_decision(
            result,
            PostDecision::Block {
                feedback: vec![
                    ContentBlock::Text {
                        text: "line one".into(),
                    },
                    ContentBlock::Text {
                        text: "line two".into(),
                    },
                ],
                info: Some(ErrorInfo::new("PolicyError", "BLOCKED")),
                additional_contexts: Vec::new(),
            },
            |_| Vec::new(),
        )
        .expect("block applies");

        assert!(applied.is_error);
        assert_eq!(applied.value, None, "a blocked result keeps no value");
        let failure = applied.error.unwrap();
        assert_eq!(failure.message, "line one\nline two");
        assert_eq!(failure.info.unwrap().code, "BLOCKED");
    }

    #[test]
    fn a_block_discards_context_the_body_deferred() {
        let result = Outcome::text("raw").with_contexts(vec![context("body-note")]);
        let applied = apply_post_decision(
            result,
            PostDecision::Block {
                feedback: vec![ContentBlock::Text {
                    text: "blocked".into(),
                }],
                info: None,
                additional_contexts: vec![context("policy-note")],
            },
            |_| Vec::new(),
        )
        .expect("block applies");

        // The body's context explains output the model was not shown; only the policy's survives.
        assert_eq!(applied.additional_contexts.len(), 1);
        assert_eq!(applied.additional_contexts[0].id.to_string(), "policy-note");
    }

    #[test]
    fn accept_appends_decision_context_after_the_bodys() {
        let result = Outcome::text("raw").with_contexts(vec![context("body-note")]);
        let applied = apply_post_decision(
            result,
            PostDecision::Accept {
                content: None,
                value: None,
                additional_contexts: vec![context("policy-note")],
            },
            |_| Vec::new(),
        )
        .expect("accept applies");

        let ids: Vec<String> = applied
            .additional_contexts
            .iter()
            .map(|message| message.id.to_string())
            .collect();
        assert_eq!(ids, vec!["body-note", "policy-note"]);
    }

    #[test]
    fn a_block_clears_the_turn_conclusion_marker() {
        let result = Outcome::text("done").concluding(true);
        assert!(result.concludes_turn);

        let applied = apply_post_decision(result, PostDecision::block(Vec::new()), |_| Vec::new())
            .expect("block applies");
        // Leaving the marker set would end the turn on a failure.
        assert!(!applied.concludes_turn);
    }

    #[test]
    fn accept_preserves_the_turn_conclusion_marker() {
        let result = Outcome::text("done").concluding(true);
        let applied = apply_post_decision(result, PostDecision::accept(), |_| Vec::new())
            .expect("accept applies");
        assert!(applied.concludes_turn);
    }

    #[test]
    fn a_failure_cannot_conclude_a_turn() {
        let failure = Outcome::failure(ToolFailure::new("boom")).concluding(true);
        assert!(!failure.concludes_turn);
    }

    #[test]
    fn empty_feedback_still_yields_a_named_message() {
        let message = failure_message_from_content(&[]);
        assert_eq!(message, "tool result blocked by post-execute policy");
    }

    #[test]
    fn non_text_blocks_are_named_rather_than_dropped() {
        let content = vec![
            ContentBlock::Text { text: "see".into() },
            ContentBlock::Image {
                attachment: "a1".into(),
            },
        ];
        assert_eq!(
            failure_message_from_content(&content),
            "see\n[image content]"
        );
    }

    // ---- batch parsing and caps ----------------------------------------------------------------

    #[test]
    fn empty_arguments_parse_to_an_empty_object() {
        assert_eq!(parse_arguments(""), serde_json::json!({}));
    }

    #[test]
    fn valid_arguments_parse_to_their_value() {
        assert_eq!(
            parse_arguments(r#"{"path":"a.txt"}"#),
            serde_json::json!({"path": "a.txt"})
        );
    }

    #[test]
    fn malformed_arguments_are_preserved_for_the_tool_to_diagnose() {
        // A generic parse error names a byte offset; the owning tool can say what to fix.
        assert_eq!(
            parse_arguments(r#"{"path":}"#),
            Value::String(String::from(r#"{"path":}"#))
        );
    }

    #[test]
    fn an_override_wins_over_the_configured_cap() {
        assert_eq!(resolve_max_parallel(Some(3), 10), Ok(3));
        assert_eq!(resolve_max_parallel(None, 10), Ok(10));
        assert_eq!(resolve_max_parallel(None, DEFAULT_MAX_PARALLEL), Ok(10));
    }

    #[test]
    fn a_zero_cap_is_refused_in_either_position() {
        assert_eq!(resolve_max_parallel(Some(0), 10), Err(MaxParallelError));
        assert_eq!(resolve_max_parallel(None, 0), Err(MaxParallelError));
        assert_eq!(
            MaxParallelError.to_string(),
            "maxParallelToolCalls must be a positive integer"
        );
    }

    // ---- group classification ------------------------------------------------------------------

    #[test]
    fn an_exclusive_call_is_a_barrier_of_one() {
        let modes = [Mode::Exclusive, Mode::Parallel, Mode::Parallel];
        assert_eq!(group_end(&modes, 0), 1);
        assert_eq!(plan_groups(&modes), vec![(0, 1), (1, 2)]);
    }

    #[test]
    fn a_parallel_run_extends_over_following_parallel_calls() {
        let modes = [Mode::Parallel, Mode::Parallel, Mode::Parallel];
        assert_eq!(group_end(&modes, 0), 3);
        assert_eq!(plan_groups(&modes), vec![(0, 3)]);
    }

    #[test]
    fn a_later_exclusive_call_stops_the_run_before_it_starts() {
        let modes = [
            Mode::Parallel,
            Mode::Parallel,
            Mode::Exclusive,
            Mode::Parallel,
        ];
        assert_eq!(group_end(&modes, 0), 2);
        assert_eq!(plan_groups(&modes), vec![(0, 2), (2, 1), (3, 1)]);
    }

    #[test]
    fn groups_cover_the_batch_exactly_once() {
        let cases: [&[Mode]; 4] = [
            &[],
            &[Mode::Exclusive],
            &[Mode::Parallel],
            &[
                Mode::Parallel,
                Mode::Exclusive,
                Mode::Parallel,
                Mode::Parallel,
            ],
        ];
        for modes in cases {
            let groups = plan_groups(modes);
            let mut covered = vec![0_usize; modes.len()];
            for (start, len) in groups {
                for count in covered.iter_mut().skip(start).take(len) {
                    *count += 1;
                }
            }
            assert!(
                covered.iter().all(|count| *count == 1),
                "every call starts exactly once for {modes:?}"
            );
        }
    }

    #[test]
    fn an_empty_batch_plans_no_groups() {
        assert!(plan_groups(&[]).is_empty());
        assert_eq!(group_end(&[], 0), 0);
    }

    #[test]
    fn only_an_exact_yes_opts_into_overlap() {
        assert_eq!(execution_mode(true, false, Some(true)), Mode::Parallel);
        // Every other answer fails closed: nothing declared, a non-true answer, an unresolvable
        // name, or an executor refusal.
        assert_eq!(execution_mode(true, false, None), Mode::Exclusive);
        assert_eq!(execution_mode(true, false, Some(false)), Mode::Exclusive);
        assert_eq!(execution_mode(false, false, Some(true)), Mode::Exclusive);
        assert_eq!(execution_mode(true, true, Some(true)), Mode::Exclusive);
        assert!(Mode::Parallel.is_parallel());
        assert!(!Mode::Exclusive.is_parallel());
    }

    // ---- settlement ----------------------------------------------------------------------------

    fn outcome_concluding(text: &str) -> Outcome {
        Outcome::text(text).concluding(true)
    }

    #[test]
    fn commits_are_refused_out_of_model_order() {
        let mut settlement = Settlement::new(2);
        // Committing index 1 first would put its result where the model expects index 0's.
        assert!(!settlement.commit(1, "b", Outcome::text("second")));
        assert_eq!(settlement.outstanding(), vec![0, 1]);
        assert!(settlement.commit(0, "a", Outcome::text("first")));
        assert!(settlement.commit(1, "b", Outcome::text("second")));
        assert!(settlement.is_complete());
    }

    #[test]
    fn results_keep_model_order_whatever_the_commit_order_of_bodies() {
        let mut settlement = Settlement::new(3);
        settlement.commit(0, "a", Outcome::text("a"));
        settlement.commit(1, "b", Outcome::text("b"));
        settlement.commit(2, "c", Outcome::text("c"));
        let report = settlement.report();

        let ids: Vec<&str> = report
            .results
            .iter()
            .map(|call| call.call_id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert!(report.is_complete_for(3));
    }

    #[test]
    fn a_duplicate_commit_is_refused_without_changing_the_ledger() {
        let mut settlement = Settlement::new(2);
        assert!(settlement.commit(0, "a", Outcome::text("once")));
        assert!(!settlement.commit(0, "a", Outcome::text("twice")));
        assert_eq!(
            settlement.report().results[0].outcome,
            Outcome::text("once")
        );
    }

    #[test]
    fn a_skipped_call_gets_the_aborted_before_dispatch_identity() {
        let mut settlement = Settlement::new(2);
        settlement.commit(0, "a", Outcome::text("ran"));
        assert!(settlement.skip(1, "b"));

        let report = settlement.report();
        let skipped = &report.results[1];
        assert_eq!(skipped.call_id, "b");
        assert!(skipped.outcome.is_error);
        assert_eq!(
            skipped
                .outcome
                .error
                .as_ref()
                .unwrap()
                .info
                .as_ref()
                .unwrap()
                .code,
            TOOL_ABORTED_BEFORE_DISPATCH
        );
        // Every `tool_use` must be answered, so a skipped call still gets a result.
        assert!(report.is_complete_for(2));
    }

    #[test]
    fn skipping_from_a_gap_synthesizes_nothing() {
        let mut settlement = Settlement::new(3);
        // Index 0 has not committed, so index 1 is not next and cannot be skipped.
        let calls = vec![String::from("a"), String::from("b"), String::from("c")];
        assert_eq!(settlement.skip_from(1, &calls), 0);
        assert_eq!(settlement.outstanding(), vec![0, 1, 2]);
    }

    #[test]
    fn skipping_from_the_next_index_synthesizes_the_rest() {
        let mut settlement = Settlement::new(3);
        settlement.commit(0, "a", Outcome::text("ran"));
        let calls = vec![String::from("a"), String::from("b"), String::from("c")];
        assert_eq!(settlement.skip_from(1, &calls), 2);
        assert!(settlement.is_complete());
    }

    #[test]
    fn a_batch_concludes_when_any_success_concludes() {
        let mut settlement = Settlement::new(2);
        settlement.commit(0, "a", outcome_concluding("done"));
        settlement.commit(1, "b", Outcome::text("also ran"));
        assert!(settlement.report().concluded);
    }

    #[test]
    fn a_batch_does_not_conclude_on_ordinary_success() {
        let mut settlement = Settlement::new(1);
        settlement.commit(0, "a", Outcome::text("done"));
        assert!(!settlement.report().concluded);
    }

    #[test]
    fn contexts_are_collected_in_call_order() {
        let mut settlement = Settlement::new(2);
        settlement.commit(
            0,
            "a",
            Outcome::text("a").with_contexts(vec![context("first")]),
        );
        settlement.commit(
            1,
            "b",
            Outcome::text("b").with_contexts(vec![context("second")]),
        );
        let report = settlement.report();

        let ids: Vec<String> = report
            .additional_contexts
            .iter()
            .map(|message| message.id.to_string())
            .collect();
        assert_eq!(ids, vec!["first", "second"]);
    }

    // ---- restrictions --------------------------------------------------------------------------

    fn known() -> Vec<String> {
        vec![
            String::from("read_file"),
            String::from("write_file"),
            String::from("shell"),
        ]
    }

    #[test]
    fn an_allow_list_hides_everything_it_does_not_name() {
        let restriction = compile_restriction(Some(&["read_file"]), None, &known()).expect("valid");
        assert!(restriction.admits("read_file"));
        assert!(!restriction.admits("write_file"));
        // A name that is not a global tool at all is also not admitted.
        assert!(!restriction.admits("nothing"));
    }

    #[test]
    fn a_deny_list_hides_exactly_what_it_names() {
        let restriction = compile_restriction(None, Some(&["shell"]), &known()).expect("valid");
        assert!(!restriction.admits("shell"));
        assert!(restriction.admits("read_file"));
    }

    #[test]
    fn an_empty_allow_list_admits_nothing_and_is_not_the_same_as_absent() {
        let empty = compile_restriction(Some(&[]), None, &known()).expect("valid");
        assert!(!empty.admits("read_file"));
        assert!(empty.allow().is_some());

        let absent = compile_restriction(None, Some(&["shell"]), &known()).expect("valid");
        assert!(absent.admits("read_file"));
        assert!(absent.allow().is_none());
    }

    #[test]
    fn a_filter_with_neither_list_is_refused_as_a_no_op() {
        let error = compile_restriction(None, None, &known()).expect_err("no-op");
        assert_eq!(error, RestrictionError::Empty);
        assert!(error.to_string().contains("is a no-op"));
    }

    #[test]
    fn the_compiled_lists_are_readable_back() {
        let both =
            compile_restriction(Some(&["read_file"]), Some(&["shell"]), &known()).expect("valid");
        // Both halves are consulted, and both are inspectable: a config editor showing a scope's
        // effective mask needs the lists, not just the decision.
        assert!(both.admits("read_file"));
        assert!(!both.admits("shell"));
        assert_eq!(
            both.allow().map(BTreeSet::len),
            Some(1),
            "the allow list survives compilation"
        );
        assert_eq!(both.deny().map(BTreeSet::len), Some(1));
    }

    #[test]
    fn the_expected_count_of_a_ledger_counts_settled_and_outstanding() {
        let mut settlement = Settlement::new(3);
        assert_eq!(settlement.expected(), 3);
        settlement.commit(0, "a", Outcome::text("a"));
        // A settled call is no longer pending, so the count must add both halves back.
        assert_eq!(settlement.expected(), 3);
        assert_eq!(settlement.outstanding(), vec![1, 2]);
    }

    #[test]
    fn the_reserved_transport_cannot_be_named_by_either_list() {
        for result in [
            compile_restriction(Some(&[RUN_CODE_NAME]), None, &known()),
            compile_restriction(None, Some(&[RUN_CODE_NAME]), &known()),
        ] {
            assert_eq!(
                result.expect_err("reserved"),
                RestrictionError::ReservedTransport {
                    name: String::from(RUN_CODE_NAME)
                }
            );
        }
    }

    #[test]
    fn an_unknown_name_is_refused_with_the_known_names_listed() {
        let error = compile_restriction(Some(&["read_fiel"]), None, &known()).expect_err("typo");
        let RestrictionError::Unknown { names, known } = &error else {
            panic!("expected Unknown, got {error:?}");
        };
        assert_eq!(names, &vec![String::from("read_fiel")]);
        assert_eq!(known.len(), 3);
        let message = error.to_string();
        assert!(message.contains("\"read_fiel\""));
        assert!(message.contains("read_file, shell, write_file"));
    }

    #[test]
    fn several_unknown_names_are_pluralized_and_sorted() {
        let error =
            compile_restriction(Some(&["zeta", "alpha"]), None, &known()).expect_err("typos");
        let message = error.to_string();
        assert!(message.contains("unknown global tools"));
        assert!(message.contains("\"alpha\", \"zeta\""));
    }

    #[test]
    fn an_unknown_name_with_no_known_tools_says_none() {
        let error = compile_restriction(Some(&["x"]), None, &[]).expect_err("unknown");
        assert!(error.to_string().contains("known global tools: (none)"));
    }

    #[test]
    fn restrictions_intersect_across_layers() {
        let outer = compile_restriction(Some(&["read_file", "shell"]), None, &known()).expect("ok");
        let inner = compile_restriction(None, Some(&["shell"]), &known()).expect("ok");

        assert!(admits_through(&[outer.clone(), inner.clone()], "read_file"));
        // The inner layer removed what the outer layer still allowed.
        assert!(!admits_through(&[outer, inner], "shell"));
    }

    #[test]
    fn no_restrictions_admits_everything() {
        assert!(admits_through(&[], "anything"));
    }

    // ---- definition-time callbacks -------------------------------------------------------------

    #[test]
    fn only_the_body_raises_on_invalid_arguments() {
        assert!(Callback::Execute.rejects_invalid_arguments());
        // Presenters run on replay of logs that may predate the schema, and the classifier runs
        // before dispatch: a raise there would make an old session unreadable or a display concern
        // fatal.
        for callback in [
            Callback::PresentCall,
            Callback::PresentResult,
            Callback::IsConcurrencySafe,
        ] {
            assert!(!callback.rejects_invalid_arguments(), "{callback:?}");
        }
    }

    #[test]
    fn each_callback_falls_back_to_the_answer_that_fails_safe() {
        assert_eq!(
            Callback::Execute.fallback_on_invalid_arguments(),
            Fallback::Raise
        );
        assert_eq!(
            Callback::PresentCall.fallback_on_invalid_arguments(),
            Fallback::Omit
        );
        assert_eq!(
            Callback::PresentResult.fallback_on_invalid_arguments(),
            Fallback::Omit
        );
        // Fall back to exclusive, never to parallel: the safe direction for overlap is "no".
        assert_eq!(
            Callback::IsConcurrencySafe.fallback_on_invalid_arguments(),
            Fallback::Exclusive
        );
    }

    #[test]
    fn a_timeout_must_be_positive_and_finite() {
        assert_eq!(validate_timeout_ms(None), Ok(None));
        assert_eq!(validate_timeout_ms(Some(1500.0)), Ok(Some(1500.0)));

        assert_eq!(validate_timeout_ms(Some(0.0)), Err(TimeoutError));
        assert_eq!(validate_timeout_ms(Some(-1.0)), Err(TimeoutError));
        assert_eq!(validate_timeout_ms(Some(f64::INFINITY)), Err(TimeoutError));
        assert_eq!(validate_timeout_ms(Some(f64::NAN)), Err(TimeoutError));
        assert_eq!(
            TimeoutError.to_string(),
            "timeoutMs must be a positive finite number"
        );
    }
}
