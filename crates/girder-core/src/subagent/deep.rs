//! The Codex app-server wire protocol: one child turn as a closed message state machine.
//!
//! # Status: ported, not registered
//!
//! This module is not reachable from any RPC endpoint, and that is deliberate rather than an
//! oversight: it describes the **Codex** app-server protocol, which upstream speaks when a subagent
//! runs against a Codex backend. This build has no Codex backend to speak it to.
//!
//! The types below therefore carry `#[allow(dead_code)]` at module scope rather than one per item.
//! The alternative -- deleting 1,474 lines of an accurately ported protocol -- would lose the
//! mapping between upstream's message shapes and these, which is the part that takes reading to
//! reconstruct. Keeping it costs a lint suppression and tells a reader the truth: the work is done,
//! the consumer is absent.
#![allow(dead_code)]
//!
//! # What the measurement said
//!
//! Upstream `packages/subagent` is 9,900 raw lines (6,791 after stripping comments and blank
//! lines) across 43 source files in eleven packages. Measured by capability, it is seven families:
//!
//! | family | upstream files and symbols | lines |
//! | --- | --- | --- |
//! | external CLI drivers | `subagent-acp`, `-claude-code`, `-codex`, `-dsh-sdk` | 3,481 |
//! | orchestration | `manager.ts`, `activation.ts`, `lifecycle.ts`, `child-agent.ts`, `types.ts` | 2,303 |
//! | model-facing tools | `tool-subagent`, `tool-subagent-control` | 1,322 |
//! | discovery and naming | `list-children.ts`, `catalog.ts`, `projection.ts`, `descriptor.ts`, `names.ts` | 1,037 |
//! | service seam and depth | `index.ts`, `depth.ts` | 655 |
//! | control, archive, errors | `control.ts`, `archive-admission.ts`, `out-of-process.ts`, `error.ts` | 554 |
//! | results and continuation | `continuation-messages.ts`, `structured.ts`, `assistant-output.ts` | 406 |
//! | in-process fork/spawn shims | `subagent-fork-in-process`, `subagent-spawn-in-process` | 142 |
//!
//! The local crate already covers orchestration in the single-parent case
//! ([`crate::subagent`]), the depth ceiling ([`crate::subagent::depth`]) and its own admission
//! pool ([`crate::subagent::pool`]), interrupt plumbing ([`crate::subagent::interrupt`]), the provider
//! registry ([`crate::subagent::providers`]), the list/wait/send control tools
//! ([`crate::subagent::tools`]), and naming plus catalog traversal ([`crate::subagent::extra`]).
//! That is roughly 3,300 lines against 6,791 — but coverage is not proportional to size, and the
//! single largest untouched block is the **external CLI drivers**: 3,481 lines, 51% of the package,
//! with no local counterpart at all.
//!
//! # Why this driver and not the other three
//!
//! Of the four external drivers, `subagent-codex` is the one whose interesting half is a *pure
//! function of the frames it receives*. `run.ts` (439 lines) spawns a process and shuttles streams;
//! `wire.ts` (711 lines) decides what the frames *mean*, and that decision is the part a port can
//! get wrong silently. The ACP, Claude Code and SDK drivers are shaped the same way but are smaller
//! (618, 587 and 376 lines), and `subagent-codex` is additionally the only one that has to survive a
//! server that *asks the client questions* mid-turn — approvals, permission grants, elicitations —
//! which is where an unattended child goes wrong in practice.
//!
//! So this module ports the decision layer of `wire.ts`: the thread/turn association checks, the
//! unattended-approval policy, the answer-selection rule, and the failure classification. It
//! deliberately does **not** port the transport (`JsonRpcLineTransport` already exists in spirit as
//! [`crate::mcp::client`]'s framing) or the process supervision in `run.ts`, because those are I/O
//! shaped and nothing here needs to invent a second one.
//!
//! # The four jobs this module does
//!
//! 1. **Association.** A JSON-RPC peer can reference any thread or turn id it likes. Every request
//!    and notification is checked against the ids this run actually created, and the two ways that
//!    check can be *premature* rather than wrong — a server that speaks about its turn before
//!    `turn/start` returns — are buffered instead of rejected. See [`CodexWire::on_notification`]
//!    and [`CodexWire::commit_turn`].
//! 2. **Unattended approval.** The child runs without a human, so every interactive request must be
//!    answered by policy: [`CodexWire::on_server_request`] answers each of the five upstream request
//!    methods with the least authority that is still a valid answer, and records a diagnostic
//!    sentence naming the mode, the request, the decision and the reason.
//! 3. **Answer selection.** Codex streams several kinds of `agentMessage` item. Upstream's rule is
//!    `final_answer`, else `partial_answer`, else an unphased message, with commentary ignored and
//!    whitespace-only text treated as no answer. [`CodexWire::collect_output`] is that rule.
//! 4. **Failure classification.** A failed turn carries a `codexErrorInfo` that is either a string
//!    tag or a single-key object with a transport detail. [`failure_facts`] folds both shapes into
//!    one `(stage, category, http status)` triple so a caller can decide between "retry", "the
//!    context is full" and "the sandbox refused" without parsing prose.
//!
//! # Why a state machine rather than a transport wrapper
//!
//! The order of frames is the protocol's real specification, and every ordering hazard is a bug
//! class: a request that arrives before the turn id is known, a terminal notification followed by a
//! late item, an approval that must be deferred until the turn is committed. Expressing the logic as
//! a machine over decoded frames makes each hazard a state transition with a test, instead of a race
//! between two tasks that only reproduces under load. The caller owns the pipe; this module owns the
//! meaning.
//!
//! # What is deliberately not here
//!
//! - **Process spawning and teardown** (`run.ts`). Nothing to decide; ownership of a child process
//!   is already [`crate::subagent::providers`]'s `ChildHandle` contract.
//! - **The `apply()` plugin wiring** in the driver packages. That is Cordis composition, and this
//!   crate's composition happens in the crate root.
//! - **`raceAbort`.** Upstream races a promise against an `AbortSignal`; the Rust equivalent is
//!   `tokio::select!` at the I/O boundary, not a state-machine concern.

use std::fmt;

use serde_json::{Map, Value, json};

use girder_core::ContentBlock;

/// How much authority the app-server child is given for its own actions.
///
/// The three variants are Codex's own permission modes, and each one maps to a distinct
/// `thread/start` parameter set ([`CodexWire::thread_start_params`]). They are *not* a severity
/// ordering that can be derived from a parent setting: `never` and
/// `dangerously-bypass-approvals-and-sandbox` both set `approvalPolicy: never`, and differ only in
/// the sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexPermissionMode {
    /// Never ask; sandbox stays at Codex's default.
    Never,
    /// Interactive approval with an automatic reviewer, inside a workspace-write sandbox.
    ApproveForMe,
    /// Never ask and run without a sandbox.
    DangerouslyBypass,
}

impl CodexPermissionMode {
    /// The mode's wire name, as it appears in diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::ApproveForMe => "approve-for-me",
            Self::DangerouslyBypass => "dangerously-bypass-approvals-and-sandbox",
        }
    }
}

impl fmt::Display for CodexPermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where in a run a failure happened.
///
/// The split exists because the two are handled differently: a `turn-start` failure means the task
/// never reached the model, so retrying is safe, while a `turn` failure may have consumed tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexFailureStage {
    /// Before the turn was accepted by the app-server.
    TurnStart,
    /// During the turn, or while waiting for its terminal notification.
    Turn,
}

impl CodexFailureStage {
    /// The stage name upstream records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnStart => "turn-start",
            Self::Turn => "turn",
        }
    }
}

impl fmt::Display for CodexFailureStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What kind of failure a non-completed turn represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexFailureCategory {
    /// A token, context-window or usage ceiling.
    Limit,
    /// A policy refusal: cyber policy, misalignment, unauthorized, or the sandbox.
    AccessPolicy,
    /// The service behind Codex is overloaded or broke.
    Service,
    /// The connection carrying the protocol failed, at a known HTTP status when one was reported.
    Transport,
    /// Codex rejected the request as malformed or unsupported.
    ProductError,
    /// The turn reported success but produced no answer.
    InvalidResult,
    /// Unrecognised, including the case where `codexErrorInfo` was absent or the wrong shape.
    Unknown,
}

impl CodexFailureCategory {
    /// The category name upstream records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Limit => "limit",
            Self::AccessPolicy => "access-policy",
            Self::Service => "service",
            Self::Transport => "transport",
            Self::ProductError => "product-error",
            Self::InvalidResult => "invalid-result",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for CodexFailureCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The fixed shape upstream attaches to any failed published run.
///
/// `http_status` is present only when the app-server actually reported a numeric one, so a caller
/// can distinguish "the transport broke" from "the transport broke at 503".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodexWireFailureFacts {
    /// Where the failure happened.
    pub stage: CodexFailureStage,
    /// What kind of failure it was.
    pub category: CodexFailureCategory,
    /// The HTTP status the app-server reported, when it reported one.
    pub http_status: Option<u16>,
}

impl CodexWireFailureFacts {
    /// Builds facts for a stage and category with no HTTP status.
    #[must_use]
    pub const fn new(stage: CodexFailureStage, category: CodexFailureCategory) -> Self {
        Self {
            stage,
            category,
            http_status: None,
        }
    }
}

/// Which interactive request the child's app-server made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexUnattendedRequest {
    /// Approval to run one command.
    CommandApproval,
    /// Approval to write one file.
    FileApproval,
    /// A grant of additional turn permissions.
    PermissionGrant,
    /// A question the model wants a human to answer.
    UserInput,
    /// An MCP server's elicitation.
    McpElicitation,
    /// A command Codex declined on its own during the turn.
    CommandExecution,
    /// A file change Codex declined on its own during the turn.
    FileChange,
    /// The sandbox refused an execution.
    SandboxExecution,
}

impl CodexUnattendedRequest {
    /// The request name upstream records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CommandApproval => "command approval",
            Self::FileApproval => "file approval",
            Self::PermissionGrant => "permission grant",
            Self::UserInput => "user input",
            Self::McpElicitation => "MCP elicitation",
            Self::CommandExecution => "command execution",
            Self::FileChange => "file change",
            Self::SandboxExecution => "sandbox execution",
        }
    }
}

impl fmt::Display for CodexUnattendedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the unattended policy decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexUnattendedDecision {
    /// The request was withdrawn, not answered.
    Cancelled,
    /// The request was answered "no".
    Declined,
    /// A permission grant was withheld.
    Denied,
    /// User input was answered with nothing.
    EmptyResponse,
    /// The sandbox failed the execution.
    Failed,
}

impl CodexUnattendedDecision {
    /// The decision name upstream records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Declined => "declined",
            Self::Denied => "denied",
            Self::EmptyResponse => "empty response",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for CodexUnattendedDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The answer to an approval request, once policy has chosen between the server's options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexApprovalDecision {
    /// Withdraw the request.
    Cancel,
    /// Refuse it.
    Decline,
}

impl CodexApprovalDecision {
    /// The decision string the app-server expects.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Decline => "decline",
        }
    }
}

/// Why a frame was rejected or a turn could not be interpreted.
///
/// Every message reproduces upstream's text verbatim, because a caller that already matches on it —
/// a log filter, an operator's runbook — must keep working.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodexWireError {
    /// A frame field had the wrong shape.
    #[error("subagent-codex: app-server returned invalid {label}")]
    InvalidFrame {
        /// What was being read.
        label: String,
    },

    /// An approval request offered nothing the policy is allowed to answer.
    ///
    /// Upstream refuses rather than guessing, because "decline" when the server only offered
    /// "cancel" (or vice versa) is a protocol violation, not a conservative choice.
    #[error("subagent-codex: app-server offered no unattended approval decision")]
    NoUnattendedDecision,

    /// A request referenced a thread this run did not create.
    #[error("subagent-codex: app-server request referenced another thread")]
    ForeignThread,

    /// A request referenced a turn this run did not commit.
    #[error("subagent-codex: app-server request referenced another turn")]
    ForeignTurn,

    /// A notification spoke about a turn before `turn/start` was issued.
    #[error("subagent-codex: app-server referenced a turn before turn/start")]
    TurnBeforeStart,

    /// A notification named a different turn than the one already observed.
    #[error("subagent-codex: app-server referenced conflicting turns")]
    ConflictingTurns,

    /// The committed turn id disagreed with the one observed earlier.
    #[error("subagent-codex: turn/start response did not match the active turn")]
    TurnMismatch,

    /// A server request used a method this driver does not implement.
    ///
    /// The method is rendered as JSON, matching upstream's `JSON.stringify(method)`: a method name is
    /// peer-supplied text, and an unquoted one could carry a newline into an operator's log.
    #[error("subagent-codex: unsupported app-server request {method:?}")]
    UnsupportedRequest {
        /// The method name.
        method: String,
    },

    /// An `agentMessage` item carried a phase this driver does not know.
    #[error("subagent-codex: app-server returned an unknown agent message phase {phase}")]
    UnknownMessagePhase {
        /// The phase, rendered as JSON.
        phase: String,
    },

    /// A terminal notification carried a status outside the protocol's set.
    #[error("subagent-codex: app-server returned invalid terminal turn status {status}")]
    InvalidTerminalStatus {
        /// The status, rendered.
        status: String,
    },

    /// `thread/start` answered with a non-ephemeral thread.
    ///
    /// Retained threads outlive the child and are visible in the operator's Codex history, so a child
    /// that asked for an ephemeral thread and silently got a durable one has leaked.
    #[error("subagent-codex: app-server did not create an ephemeral thread")]
    NotEphemeral,

    /// The turn reported success without producing an answer.
    #[error("subagent-codex: Codex completed without an answer")]
    EmptyAnswer,

    /// The turn ended with a status other than `completed`.
    #[error("subagent-codex: Codex turn ended with status {status}{detail}")]
    TurnFailed {
        /// The terminal status.
        status: String,
        /// `: <category>` when the status was `failed`, empty otherwise.
        detail: String,
    },

    /// A turn was finished before its terminal notification arrived.
    #[error("subagent-codex: Codex turn has not reported a terminal notification")]
    TurnNotTerminal,

    /// A frame was processed after the wire was closed.
    #[error("subagent-codex: app-server protocol stream closed")]
    Closed,
}

impl CodexWireError {
    /// Builds [`Self::InvalidFrame`] for one label.
    fn invalid(label: &str) -> Self {
        Self::InvalidFrame {
            label: label.to_owned(),
        }
    }
}

/// What a finished turn produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexTurnOutcome {
    /// The turn completed and produced a non-empty answer.
    Completed {
        /// The selected answer, as text blocks.
        output: Vec<ContentBlock>,
    },

    /// The turn hit a ceiling; whatever text was observed is retained rather than discarded.
    MaxTokens {
        /// The selected answer, which may be empty.
        output: Vec<ContentBlock>,
    },
}

impl CodexTurnOutcome {
    /// The answer blocks, whichever ending produced them.
    #[must_use]
    pub fn output(&self) -> &[ContentBlock] {
        match self {
            Self::Completed { output } | Self::MaxTokens { output } => output,
        }
    }
}

/// One frame held back because its turn was not committed yet.
#[derive(Clone, Debug)]
struct EarlyNotification {
    method: String,
    params: Map<String, Value>,
    order: u64,
}

/// A diagnostic sentence waiting for the turn id that will rank it.
#[derive(Clone, Debug)]
struct PendingDiagnostic {
    order: u64,
    request: CodexUnattendedRequest,
    decision: CodexUnattendedDecision,
    reason: String,
}

/// One terminal `turn/completed` payload and the observation rank it was received at.
#[derive(Clone, Debug)]
struct Completion {
    params: Map<String, Value>,
    order: u64,
}

/// Reads one JSON object out of a frame field.
///
/// Arrays are rejected: a serialized `[]` in an object position is a protocol error rather than an
/// empty object, and coercing it would turn a broken peer into a silently empty `item`.
fn object<'a>(value: &'a Value, label: &str) -> Result<&'a Map<String, Value>, CodexWireError> {
    value
        .as_object()
        .ok_or_else(|| CodexWireError::invalid(label))
}

/// Reads a non-empty string out of a frame field.
fn field_string(value: &Value, label: &str) -> Result<String, CodexWireError> {
    match value.as_str() {
        Some(text) if !text.is_empty() => Ok(text.to_owned()),
        _ => Err(CodexWireError::invalid(label)),
    }
}

/// Reads a required object member.
fn member<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    label: &str,
) -> Result<&'a Value, CodexWireError> {
    map.get(key).ok_or_else(|| CodexWireError::invalid(label))
}

/// Reads a terminal turn's status, rendered as text when it is not a string.
///
/// A non-string status is kept rather than rejected: the caller's error message must show what the
/// peer actually sent, and an empty status would hide a malformed frame behind an empty name.
fn terminal_status(turn: &Map<String, Value>) -> String {
    match turn.get("status").and_then(Value::as_str) {
        Some(text) => text.to_owned(),
        None => turn
            .get("status")
            .map(ToString::to_string)
            .unwrap_or_default(),
    }
}

/// Chooses between the decisions the app-server offered.
///
/// `cancel` wins over `decline` when both are offered, matching upstream: cancelling withdraws the
/// request entirely, which grants strictly less than declining it.
fn unattended_decision(
    params: &Map<String, Value>,
) -> Result<CodexApprovalDecision, CodexWireError> {
    let Some(available) = params.get("availableDecisions") else {
        return Ok(CodexApprovalDecision::Decline);
    };
    if available.is_null() {
        return Ok(CodexApprovalDecision::Decline);
    }
    let Some(options) = available.as_array() else {
        return Err(CodexWireError::NoUnattendedDecision);
    };
    let has = |wanted: &str| options.iter().any(|option| option.as_str() == Some(wanted));
    if has("cancel") {
        return Ok(CodexApprovalDecision::Cancel);
    }
    if has("decline") {
        return Ok(CodexApprovalDecision::Decline);
    }
    Err(CodexWireError::NoUnattendedDecision)
}

/// Renders the diagnostic sentence for one unattended decision.
///
/// The exact text matters: it is the only place the operator learns *why* a child's command was
/// refused, and upstream's runbook greps for this prefix.
#[must_use]
pub fn unattended_diagnostic(
    mode: CodexPermissionMode,
    request: CodexUnattendedRequest,
    decision: CodexUnattendedDecision,
    reason: &str,
) -> String {
    format!(
        "Codex unattended decision (mode: {mode}; request: {request}; decision: {decision}): {reason}"
    )
}

/// Reads an HTTP status that the protocol is allowed to carry.
///
/// The bound is upstream's: a non-integer, a negative value, or anything above 65535 is not a status
/// and is reported as absent rather than clamped.
fn numeric_http_status(value: &Value) -> Option<u16> {
    if let Some(status) = value.as_u64() {
        return u16::try_from(status).ok();
    }
    let float = value.as_f64()?;
    if float.fract() != 0.0 || !(0.0..=65_535.0).contains(&float) {
        return None;
    }
    // The range check above makes the cast exact.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "guarded to the 0..=65535 integers immediately above"
    )]
    Some(float as u16)
}

/// What a failure detail says, before it is turned into [`CodexWireFailureFacts`].
///
/// Public because a caller that has the `turn/completed` frame in hand — a log reader, a test
/// fixture, a replay — may want the two facts the fixed triple cannot carry: that the ending was a
/// context ceiling (so the text is usable) and that a sandbox refused the work (so a diagnostic is
/// owed). [`CodexWire::finish_turn`] uses both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodexFailureInfo {
    /// What kind of failure it was.
    pub category: CodexFailureCategory,
    /// The HTTP status the app-server reported, when it reported a usable one.
    pub http_status: Option<u16>,
    /// Whether the ceiling that was hit is the context window, which makes the ending usable output.
    pub max_tokens: bool,
    /// Whether the sandbox refused the execution.
    pub sandbox_failure: bool,
}

impl CodexFailureInfo {
    /// The default for anything unrecognised.
    const fn unknown() -> Self {
        Self {
            category: CodexFailureCategory::Unknown,
            http_status: None,
            max_tokens: false,
            sandbox_failure: false,
        }
    }

    /// A category with no modifiers.
    const fn of(category: CodexFailureCategory) -> Self {
        Self {
            category,
            http_status: None,
            max_tokens: false,
            sandbox_failure: false,
        }
    }
}

/// Classifies the single-key error object the app-server sends for transport faults.
///
/// The object is required to have exactly one key. A frame with two keys is ambiguous — the second
/// could be a detail the classifier did not read — so it is `unknown` rather than a guess.
fn object_failure_info(value: &Map<String, Value>) -> CodexFailureInfo {
    if value.len() != 1 {
        return CodexFailureInfo::unknown();
    }
    let Some((category, detail)) = value.iter().next() else {
        return CodexFailureInfo::unknown();
    };
    let Some(fields) = detail.as_object() else {
        return CodexFailureInfo::unknown();
    };
    match category.as_str() {
        "httpConnectionFailed"
        | "responseStreamConnectionFailed"
        | "responseStreamDisconnected"
        | "responseTooManyFailedAttempts" => {
            let status = fields.get("httpStatusCode").and_then(numeric_http_status);
            CodexFailureInfo {
                category: CodexFailureCategory::Transport,
                http_status: status,
                max_tokens: false,
                sandbox_failure: false,
            }
        }
        "activeTurnNotSteerable" => CodexFailureInfo::of(CodexFailureCategory::ProductError),
        _ => CodexFailureInfo::unknown(),
    }
}

/// Classifies the `codexErrorInfo` of a failed turn.
///
/// Two shapes arrive: a bare string tag, and a single-key object carrying a transport detail. Both
/// are folded here so callers never have to know which one they got.
#[must_use]
pub fn failure_info(turn: &Map<String, Value>) -> CodexFailureInfo {
    if turn.get("status").and_then(Value::as_str) != Some("failed") {
        return CodexFailureInfo::unknown();
    }
    let Some(error) = turn.get("error").and_then(Value::as_object) else {
        return CodexFailureInfo::unknown();
    };
    let Some(info) = error.get("codexErrorInfo") else {
        return CodexFailureInfo::unknown();
    };
    if let Some(tag) = info.as_str() {
        return match tag {
            "contextWindowExceeded" => CodexFailureInfo {
                category: CodexFailureCategory::Limit,
                http_status: None,
                max_tokens: true,
                sandbox_failure: false,
            },
            "sessionBudgetExceeded" | "usageLimitExceeded" => {
                CodexFailureInfo::of(CodexFailureCategory::Limit)
            }
            "serverOverloaded" | "internalServerError" => {
                CodexFailureInfo::of(CodexFailureCategory::Service)
            }
            "cyberPolicy" | "misalignmentPolicyViolation" | "unauthorized" => {
                CodexFailureInfo::of(CodexFailureCategory::AccessPolicy)
            }
            "badRequest" | "threadRollbackFailed" | "other" => {
                CodexFailureInfo::of(CodexFailureCategory::ProductError)
            }
            "sandboxError" => CodexFailureInfo {
                category: CodexFailureCategory::AccessPolicy,
                http_status: None,
                max_tokens: false,
                sandbox_failure: true,
            },
            _ => CodexFailureInfo::unknown(),
        };
    }
    match info.as_object() {
        Some(details) => object_failure_info(details),
        None => CodexFailureInfo::unknown(),
    }
}

/// One app-server connection and its single ephemeral thread and turn.
///
/// The type exposes no generic request surface on purpose. Adding another product method has to be a
/// change to this module, because a generic escape hatch would let callers rely on protocol shapes
/// this driver never validated.
// The fields are independent protocol milestones (`turn/start` sent, turn id committed, terminal
// frame seen, stream ended, wire closed), each set by a different frame. Folding them into one enum
// would claim an order the protocol does not guarantee, which is exactly the hazard this module
// exists to model faithfully.
#[derive(Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent protocol milestones, not a state machine with illegal states"
)]
pub struct CodexWire {
    permission_mode: CodexPermissionMode,
    model: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    pending_turn_id: Option<String>,
    turn_started: bool,
    completion: Option<Completion>,
    early: Vec<EarlyNotification>,
    last_final_answer: Option<String>,
    last_partial_answer: Option<String>,
    last_unphased_answer: Option<String>,
    diagnostic: Option<String>,
    failure: Option<CodexWireFailureFacts>,
    diagnostic_order: u64,
    observation_order: u64,
    pending_diagnostic: Option<PendingDiagnostic>,
    input_ended: bool,
    terminal_observed: bool,
    closed: bool,
    initialized: bool,
}

impl CodexWire {
    /// Builds a wire for one run.
    ///
    /// `model` is optional: when absent, `thread/start` omits it and the app-server applies its own
    /// default, which is what a caller that inherits the parent route wants.
    #[must_use]
    pub fn new(permission_mode: CodexPermissionMode, model: Option<String>) -> Self {
        Self {
            permission_mode,
            model,
            thread_id: None,
            turn_id: None,
            pending_turn_id: None,
            turn_started: false,
            completion: None,
            early: Vec::new(),
            last_final_answer: None,
            last_partial_answer: None,
            last_unphased_answer: None,
            diagnostic: None,
            failure: None,
            diagnostic_order: 0,
            observation_order: 0,
            pending_diagnostic: None,
            input_ended: false,
            terminal_observed: false,
            closed: false,
            initialized: false,
        }
    }

    /// The permission mode this run was created with.
    #[must_use]
    pub const fn permission_mode(&self) -> CodexPermissionMode {
        self.permission_mode
    }

    /// Whether the protocol stream ended before a terminal turn notification.
    ///
    /// This is the difference between "the child finished and the pipe then closed" and "the child
    /// died mid-turn", which a supervisor must not conflate.
    #[must_use]
    pub const fn ended_before_terminal(&self) -> bool {
        self.input_ended && !self.terminal_observed
    }

    /// Parses one `initialize` response, confirming the handshake answered with an object.
    ///
    /// Completion is recorded because the handshake is required exactly once per connection, and a
    /// second `initialize` would be a caller error rather than a new session.
    pub fn accept_initialize(&mut self, response: &Value) -> Result<(), CodexWireError> {
        object(response, "initialize response")?;
        self.initialized = true;
        Ok(())
    }

    /// Whether the `initialize` handshake has completed.
    #[must_use]
    pub const fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// The parameters for the required `initialize` request.
    #[must_use]
    pub fn initialize_params() -> Value {
        json!({
            "clientInfo": {
                "name": "deepseek-harness",
                "title": "DeepSeek Harness",
                "version": "0.0.1",
            },
            "capabilities": {
                "experimentalApi": false,
                "requestAttestation": false,
            },
        })
    }

    /// The `initialized` notification's method name.
    #[must_use]
    pub const fn initialized_method() -> &'static str {
        "initialized"
    }

    /// The parameters for the run's private ephemeral `thread/start` request.
    ///
    /// `ephemeral` is not optional and `model` is omitted rather than sent as null: the app-server
    /// treats an explicit null as "no model", which is not the same as "your default".
    #[must_use]
    pub fn thread_start_params(&self, cwd: &str) -> Value {
        let mut params = Map::new();
        params.insert("cwd".to_owned(), Value::String(cwd.to_owned()));
        params.insert("ephemeral".to_owned(), Value::Bool(true));
        if let Some(model) = &self.model {
            params.insert("model".to_owned(), Value::String(model.clone()));
        }
        for (key, value) in self.permission_params() {
            params.insert(key.clone(), value);
        }
        Value::Object(params)
    }

    /// The `thread/start` parameters this run's permission mode implies.
    ///
    /// The table is exact, including which keys are absent: `never` sets no sandbox at all so Codex's
    /// own default applies, and only `approve-for-me` names a reviewer.
    #[must_use]
    pub fn permission_params(&self) -> Map<String, Value> {
        match self.permission_mode {
            CodexPermissionMode::Never => {
                let mut map = Map::new();
                map.insert(
                    "approvalPolicy".to_owned(),
                    Value::String("never".to_owned()),
                );
                map
            }
            CodexPermissionMode::ApproveForMe => {
                let mut map = Map::new();
                map.insert(
                    "approvalPolicy".to_owned(),
                    Value::String("on-request".to_owned()),
                );
                map.insert(
                    "approvalsReviewer".to_owned(),
                    Value::String("auto_review".to_owned()),
                );
                map.insert(
                    "sandbox".to_owned(),
                    Value::String("workspace-write".to_owned()),
                );
                map
            }
            CodexPermissionMode::DangerouslyBypass => {
                let mut map = Map::new();
                map.insert(
                    "approvalPolicy".to_owned(),
                    Value::String("never".to_owned()),
                );
                map.insert(
                    "sandbox".to_owned(),
                    Value::String("danger-full-access".to_owned()),
                );
                map
            }
        }
    }

    /// Adopts the thread id from a `thread/start` response.
    ///
    /// A non-ephemeral thread is refused rather than adopted: the app-server ignored the request, and
    /// running a child in a retained thread would leave its transcript behind in the operator's Codex
    /// history.
    pub fn accept_thread(&mut self, response: &Value) -> Result<(), CodexWireError> {
        let response = object(response, "thread/start response")?;
        let thread = object(
            member(response, "thread", "thread/start thread")?,
            "thread/start thread",
        )?;
        let id = field_string(
            member(thread, "id", "thread/start thread id")?,
            "thread/start thread id",
        )?;
        if thread.get("ephemeral") != Some(&Value::Bool(true)) {
            return Err(CodexWireError::NotEphemeral);
        }
        self.thread_id = Some(id);
        Ok(())
    }

    /// The parameters submitting one text-only task.
    #[must_use]
    pub fn turn_start_params(&self, texts: &[String]) -> Value {
        let input: Vec<Value> = texts
            .iter()
            .map(|text| json!({ "type": "text", "text": text, "text_elements": [] }))
            .collect();
        json!({
            "threadId": self.thread_id.clone().unwrap_or_default(),
            "input": input,
        })
    }

    /// The parameters for a best-effort `turn/interrupt`.
    ///
    /// `None` when there is nothing to interrupt: no thread, no committed turn, or a closed wire. A
    /// caller uses this to avoid sending a request that can only fail.
    #[must_use]
    pub fn interrupt_params(&self) -> Option<Value> {
        let thread_id = self.thread_id.as_ref()?;
        let turn_id = self.turn_id.as_ref()?;
        if self.closed {
            return None;
        }
        Some(json!({ "threadId": thread_id, "turnId": turn_id }))
    }

    /// Marks the turn in flight before `turn/start` is sent.
    ///
    /// Upstream creates its completion slot first for the same reason: the server may speak about the
    /// turn before the start response arrives, and those frames must be buffered rather than reported
    /// as [`CodexWireError::TurnBeforeStart`].
    pub fn begin_turn(&mut self) {
        self.turn_started = true;
        self.terminal_observed = false;
    }

    /// Adopts the turn id from a `turn/start` response and replays buffered frames.
    ///
    /// The replay order is the order the frames were *received*, not the order they are replayed in,
    /// so a diagnostic recorded from an early approval keeps its original rank against later ones.
    pub fn commit_turn(&mut self, response: &Value) -> Result<(), CodexWireError> {
        let response = object(response, "turn/start response")?;
        let turn = object(
            member(response, "turn", "turn/start turn")?,
            "turn/start turn",
        )?;
        let id = field_string(
            member(turn, "id", "turn/start turn id")?,
            "turn/start turn id",
        )?;
        self.commit_turn_id(&id)?;
        // `commit_turn_id` replays the buffered frames; they are drained here so a second commit
        // cannot replay them twice.
        Ok(())
    }

    /// Commits a turn id and flushes whatever was deferred for it.
    fn commit_turn_id(&mut self, id: &str) -> Result<(), CodexWireError> {
        if let Some(pending) = &self.pending_turn_id
            && pending != id
        {
            return Err(CodexWireError::TurnMismatch);
        }
        self.turn_id = Some(id.to_owned());
        if let Some(pending) = self.pending_diagnostic.take() {
            self.record_diagnostic(
                pending.request,
                pending.decision,
                &pending.reason,
                pending.order,
            );
        }
        let early = std::mem::take(&mut self.early);
        for notification in early {
            self.dispatch(
                &notification.method,
                &notification.params,
                Some(notification.order),
            )?;
        }
        Ok(())
    }

    /// Processes one notification frame.
    ///
    /// Unknown methods are ignored: the app-server streams a widening set of item kinds, and a child
    /// must not fail because a newer server mentioned something it does not classify.
    pub fn on_notification(
        &mut self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<(), CodexWireError> {
        if self.closed {
            return Err(CodexWireError::Closed);
        }
        self.dispatch(method, params, None)
    }

    /// Processes one notification, optionally at a recorded observation rank.
    fn dispatch(
        &mut self,
        method: &str,
        params: &Map<String, Value>,
        order: Option<u64>,
    ) -> Result<(), CodexWireError> {
        match method {
            "turn/started" => self.note_turn_started(params),
            "item/completed" => self.note_item_completed(params, order),
            "turn/completed" => self.note_turn_completed(params, order),
            _ => Ok(()),
        }
    }

    /// Whether a frame's thread id is this run's.
    fn is_own_thread(
        &self,
        params: &Map<String, Value>,
        label: &str,
    ) -> Result<bool, CodexWireError> {
        let thread_id = field_string(member(params, "threadId", label)?, label)?;
        Ok(self.thread_id.as_deref() == Some(thread_id.as_str()))
    }

    /// Handles `turn/started`: only learns the provisional turn id.
    fn note_turn_started(&mut self, params: &Map<String, Value>) -> Result<(), CodexWireError> {
        if !self.is_own_thread(params, "turn/started thread id")? {
            return Ok(());
        }
        let turn = object(
            member(params, "turn", "turn/started turn")?,
            "turn/started turn",
        )?;
        if self.turn_started && self.turn_id.is_none() {
            let id = field_string(
                member(turn, "id", "turn/started turn id")?,
                "turn/started turn id",
            )?;
            self.observe_pending_turn_id(&id)?;
        }
        Ok(())
    }

    /// Handles `item/completed`: declined items, then answer selection by phase.
    fn note_item_completed(
        &mut self,
        params: &Map<String, Value>,
        order: Option<u64>,
    ) -> Result<(), CodexWireError> {
        if !self.is_own_thread(params, "item/completed thread id")? {
            return Ok(());
        }
        let turn_id = field_string(
            member(params, "turnId", "item/completed turn id")?,
            "item/completed turn id",
        )?;
        if self.turn_id.is_none() {
            if self.turn_started {
                self.observe_pending_turn_id(&turn_id)?;
                let early_order = self.next_observation_order();
                self.early.push(EarlyNotification {
                    method: "item/completed".to_owned(),
                    params: params.clone(),
                    order: early_order,
                });
            }
            return Ok(());
        }
        if self.turn_id.as_deref() != Some(turn_id.as_str()) {
            return Ok(());
        }
        let item = object(
            member(params, "item", "item/completed item")?,
            "item/completed item",
        )?;
        if self.record_declined_item(item, order) {
            return Ok(());
        }
        if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
            return Ok(());
        }
        let text = field_string(member(item, "text", "agent message")?, "agent message")?;
        match item.get("phase") {
            None | Some(Value::Null) => self.last_unphased_answer = Some(text),
            Some(Value::String(phase)) if phase == "final_answer" => {
                self.last_final_answer = Some(text);
            }
            Some(Value::String(phase)) if phase == "partial_answer" => {
                self.last_partial_answer = Some(text);
            }
            // Commentary is the model narrating its own work; it is never the answer.
            Some(Value::String(phase)) if phase == "commentary" => {}
            Some(phase) => {
                return Err(CodexWireError::UnknownMessagePhase {
                    phase: phase.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Handles `turn/completed`: validates the status and records the terminal notification.
    fn note_turn_completed(
        &mut self,
        params: &Map<String, Value>,
        order: Option<u64>,
    ) -> Result<(), CodexWireError> {
        if !self.is_own_thread(params, "turn/completed thread id")? {
            return Ok(());
        }
        let turn = object(
            member(params, "turn", "turn/completed turn")?,
            "turn/completed turn",
        )?;
        let turn_id = field_string(
            member(turn, "id", "turn/completed turn id")?,
            "turn/completed turn id",
        )?;
        if !self.turn_started {
            return Ok(());
        }
        if self.turn_id.is_none() {
            self.observe_pending_turn_id(&turn_id)?;
            let early_order = self.next_observation_order();
            self.early.push(EarlyNotification {
                method: "turn/completed".to_owned(),
                params: params.clone(),
                order: early_order,
            });
            return Ok(());
        }
        if self.turn_id.as_deref() != Some(turn_id.as_str()) {
            return Ok(());
        }
        let status = terminal_status(turn);
        if !matches!(status.as_str(), "completed" | "interrupted" | "failed") {
            return Err(CodexWireError::InvalidTerminalStatus { status });
        }
        self.terminal_observed = true;
        let resolved_order = order.unwrap_or_else(|| self.next_observation_order());
        self.completion = Some(Completion {
            params: params.clone(),
            order: resolved_order,
        });
        Ok(())
    }

    /// Records a provisional turn id, rejecting a conflicting one.
    fn observe_pending_turn_id(&mut self, id: &str) -> Result<(), CodexWireError> {
        if !self.turn_started {
            return Err(CodexWireError::TurnBeforeStart);
        }
        if let Some(pending) = &self.pending_turn_id
            && pending != id
        {
            return Err(CodexWireError::ConflictingTurns);
        }
        self.pending_turn_id = Some(id.to_owned());
        Ok(())
    }

    /// Validates the thread and turn a server request names.
    ///
    /// Returns `true` when the matching turn is still provisional, so the caller defers its diagnostic
    /// until [`Self::commit_turn`] — the frame was about this run, but its rank is not known yet.
    fn validate_run_ids(
        &mut self,
        params: &Map<String, Value>,
        nullable_turn: bool,
    ) -> Result<bool, CodexWireError> {
        let thread_id = field_string(
            member(params, "threadId", "server request thread id")?,
            "server request thread id",
        )?;
        if self.thread_id.as_deref() != Some(thread_id.as_str()) {
            return Err(CodexWireError::ForeignThread);
        }
        if nullable_turn && params.get("turnId").is_some_and(Value::is_null) {
            return Ok(false);
        }
        let id = field_string(
            member(params, "turnId", "server request turn id")?,
            "server request turn id",
        )?;
        if self.turn_id.is_none() {
            self.observe_pending_turn_id(&id)?;
            return Ok(true);
        }
        if self.turn_id.as_deref() != Some(id.as_str()) {
            return Err(CodexWireError::ForeignTurn);
        }
        Ok(false)
    }

    /// Answers one server request and records its diagnostic.
    ///
    /// Every arm returns the least authority that is still a valid answer: an empty permission grant,
    /// an empty answer set, a refused elicitation. None of them is a refusal to answer, because a
    /// child that leaves a request unanswered blocks the turn.
    pub fn on_server_request(
        &mut self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, CodexWireError> {
        if self.closed {
            return Err(CodexWireError::Closed);
        }
        match method {
            "item/commandExecution/requestApproval" => {
                let provisional = self.validate_run_ids(params, false)?;
                let decision = unattended_decision(params)?;
                self.record_request_diagnostic(
                    provisional,
                    CodexUnattendedRequest::CommandApproval,
                    if decision == CodexApprovalDecision::Cancel {
                        CodexUnattendedDecision::Cancelled
                    } else {
                        CodexUnattendedDecision::Declined
                    },
                    "the provider does not grant interactive approval",
                );
                Ok(json!({ "decision": decision.as_str() }))
            }
            "item/fileChange/requestApproval" => {
                let provisional = self.validate_run_ids(params, false)?;
                let decision = unattended_decision(params)?;
                self.record_request_diagnostic(
                    provisional,
                    CodexUnattendedRequest::FileApproval,
                    if decision == CodexApprovalDecision::Cancel {
                        CodexUnattendedDecision::Cancelled
                    } else {
                        CodexUnattendedDecision::Declined
                    },
                    "the provider does not grant interactive approval",
                );
                Ok(json!({ "decision": decision.as_str() }))
            }
            "item/permissions/requestApproval" => {
                let provisional = self.validate_run_ids(params, false)?;
                self.record_request_diagnostic(
                    provisional,
                    CodexUnattendedRequest::PermissionGrant,
                    CodexUnattendedDecision::Denied,
                    "the provider grants no additional turn permissions",
                );
                Ok(json!({ "permissions": {}, "scope": "turn" }))
            }
            "item/tool/requestUserInput" => {
                let provisional = self.validate_run_ids(params, false)?;
                self.record_request_diagnostic(
                    provisional,
                    CodexUnattendedRequest::UserInput,
                    CodexUnattendedDecision::EmptyResponse,
                    "the provider does not collect interactive answers",
                );
                Ok(json!({ "answers": {} }))
            }
            "mcpServer/elicitation/request" => {
                let provisional = self.validate_run_ids(params, true)?;
                self.record_request_diagnostic(
                    provisional,
                    CodexUnattendedRequest::McpElicitation,
                    CodexUnattendedDecision::Declined,
                    "the provider does not collect interactive MCP input",
                );
                Ok(json!({ "action": "decline", "content": Value::Null, "_meta": Value::Null }))
            }
            _ => Err(CodexWireError::UnsupportedRequest {
                method: method.to_owned(),
            }),
        }
    }

    /// Recognises the two item kinds Codex itself declined during a turn.
    fn record_declined_item(&mut self, item: &Map<String, Value>, order: Option<u64>) -> bool {
        let kind = item.get("type").and_then(Value::as_str);
        let declined = item.get("status").and_then(Value::as_str) == Some("declined");
        if !declined {
            return false;
        }
        let request = match kind {
            Some("commandExecution") => CodexUnattendedRequest::CommandExecution,
            Some("fileChange") => CodexUnattendedRequest::FileChange,
            _ => return false,
        };
        let reason = match kind {
            Some("commandExecution") => {
                "Codex declined the command under the selected permission mode"
            }
            _ => "Codex declined the file change under the selected permission mode",
        };
        let resolved = order.unwrap_or_else(|| self.next_observation_order());
        self.record_diagnostic(request, CodexUnattendedDecision::Declined, reason, resolved);
        true
    }

    /// Defers or records a request diagnostic depending on whether its turn is committed.
    fn record_request_diagnostic(
        &mut self,
        provisional: bool,
        request: CodexUnattendedRequest,
        decision: CodexUnattendedDecision,
        reason: &str,
    ) {
        let order = self.next_observation_order();
        if provisional {
            self.pending_diagnostic = Some(PendingDiagnostic {
                order,
                request,
                decision,
                reason: reason.to_owned(),
            });
            return;
        }
        self.record_diagnostic(request, decision, reason, order);
    }

    /// Records a diagnostic unless a later-ranked one is already held.
    ///
    /// The rank check is what makes the "latest safe fact wins" rule work: frames can arrive out of
    /// order once early notifications are replayed, and a stale decision must not overwrite a newer
    /// one.
    fn record_diagnostic(
        &mut self,
        request: CodexUnattendedRequest,
        decision: CodexUnattendedDecision,
        reason: &str,
        order: u64,
    ) {
        if order < self.diagnostic_order {
            return;
        }
        self.diagnostic_order = order;
        self.diagnostic = Some(unattended_diagnostic(
            self.permission_mode,
            request,
            decision,
            reason,
        ));
    }

    /// Allocates the next observation rank.
    fn next_observation_order(&mut self) -> u64 {
        self.observation_order += 1;
        self.observation_order
    }

    /// The best answer observed so far, preserving exact bytes.
    ///
    /// The precedence is `final_answer`, then `partial_answer`, then an unphased message. A
    /// whitespace-only answer is reported as none, because a parent that received spaces would treat
    /// the child as having answered.
    #[must_use]
    pub fn collect_output(&self) -> Vec<ContentBlock> {
        let selected = self
            .last_final_answer
            .as_ref()
            .or(self.last_partial_answer.as_ref())
            .or(self.last_unphased_answer.as_ref());
        match selected {
            Some(text) if !text.trim().is_empty() => {
                vec![ContentBlock::Text { text: text.clone() }]
            }
            _ => Vec::new(),
        }
    }

    /// The latest safe unattended permission fact observed for this run.
    #[must_use]
    pub fn collect_diagnostic(&self) -> Option<&str> {
        self.diagnostic.as_deref()
    }

    /// The structured failure fact observed for this run.
    ///
    /// Meaningful only after a non-completed return or a rejection from [`Self::finish_turn`]; before
    /// that it is `None`.
    #[must_use]
    pub const fn collect_failure(&self) -> Option<&CodexWireFailureFacts> {
        self.failure.as_ref()
    }

    /// Records the failure facts for this run.
    fn record_failure(&mut self, facts: CodexWireFailureFacts) {
        self.failure = Some(facts);
    }

    /// Records that `turn/start` itself failed, before the app-server accepted any work.
    ///
    /// Upstream does this in the `catch` around its `turn/start` request. The distinction matters to
    /// a supervisor: nothing reached the model, so retrying cannot duplicate side effects, which is
    /// not true of a failure recorded during the turn.
    pub fn record_turn_start_failure(&mut self) {
        self.record_failure(CodexWireFailureFacts::new(
            CodexFailureStage::TurnStart,
            CodexFailureCategory::Unknown,
        ));
    }

    /// Settles the turn once its terminal notification has been observed.
    ///
    /// This is the post-completion half of upstream's `runTurn`, and it is where the three interesting
    /// endings are separated: a ceiling (keep whatever text arrived), a non-`completed` status (fail
    /// with classified facts), and a `completed` status with no answer (an invalid result, not a
    /// success).
    pub fn finish_turn(&mut self) -> Result<CodexTurnOutcome, CodexWireError> {
        let Some(completion) = self.completion.take() else {
            return Err(CodexWireError::TurnNotTerminal);
        };
        let turn = object(
            member(&completion.params, "turn", "turn/completed turn")?,
            "turn/completed turn",
        )?;
        let status = terminal_status(turn);
        if status != "completed" {
            let parsed = failure_info(turn);
            let mut facts = CodexWireFailureFacts::new(CodexFailureStage::Turn, parsed.category);
            facts.http_status = parsed.http_status;
            self.record_failure(facts);
            if parsed.sandbox_failure {
                self.record_diagnostic(
                    CodexUnattendedRequest::SandboxExecution,
                    CodexUnattendedDecision::Failed,
                    "Codex reported a sandbox failure",
                    completion.order,
                );
            }
            if parsed.max_tokens {
                return Ok(CodexTurnOutcome::MaxTokens {
                    output: self.collect_output(),
                });
            }
            let detail = if status == "failed" {
                format!(": {}", parsed.category)
            } else {
                String::new()
            };
            return Err(CodexWireError::TurnFailed { status, detail });
        }
        let output = self.collect_output();
        if output.is_empty() {
            self.record_failure(CodexWireFailureFacts::new(
                CodexFailureStage::Turn,
                CodexFailureCategory::InvalidResult,
            ));
            return Err(CodexWireError::EmptyAnswer);
        }
        Ok(CodexTurnOutcome::Completed { output })
    }

    /// Records that the protocol stream ended.
    ///
    /// Idempotent: a pipe can report end more than once, and the second report must not resurrect
    /// state a teardown already cleared.
    pub fn input_ended(&mut self) {
        if self.input_ended {
            return;
        }
        self.input_ended = true;
    }

    /// Marks the wire closed. Idempotent, matching upstream's `close()`.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Whether the wire has been closed.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }
}
