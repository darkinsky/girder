//! The shell tool's model-facing result contract: rendering, the exit-status marker grammar,
//! and sandbox denial/runner-failure classification.
//!
//! # Where this comes from
//!
//! Upstream keeps this logic in `packages/shell` (4,861 lines of production TypeScript across 20
//! files, once the `.spec.ts`/`.e2e.ts` suites are excluded):
//!
//! - `shell/src/render.ts` — [`parse_exit_status`] (upstream `parseExitStatus`), the inverse of the
//!   `[exit code: N]` / `[killed by signal: X]` markers the tool renderers append.
//! - `tool-bash/src/render.ts` — `renderResult` ([`render_result`]), `renderPromoted`
//!   ([`render_promoted`]), `renderJobRead` ([`render_job_read`]), and the private `streamText`
//!   ([`stream_text`]). `tool-pwsh/src/render.ts` is the same contract for PowerShell, and the Web
//!   terminal card mirrors it.
//! - `tool-bash/src/background.ts` — `ringDelta` ([`ring_delta`]), the stdout-then-`[stderr]`
//!   sectioning of one consuming job read.
//! - `sandbox/sandbox/src/escalation.ts` — `sandboxDenialMarker` ([`sandbox_denial_marker`]) and
//!   `escalationHintMarker` ([`escalation_hint_marker`]).
//! - `sandbox/sandbox/src/diagnostics.ts` — `matchesSignature` ([`matches_signature`]), and
//!   `bash-sandbox`/`pwsh-sandbox`'s `classifyDenial` ([`classify_denial`]) and
//!   `classifyRunnerFailure` ([`classify_runner_failure`]).
//!
//! # Why this gap
//!
//! The local crate already runs commands and bounds their output:
//! `shell.rs` resolves a [`Specification`](crate::ShellSpecification)
//! (program, filtered environment, deadline, sandbox mode, retention),
//! `exec.rs` spawns and classifies failures, `persistent_shell.rs` keeps a long-lived shell, and the
//! `terminal*.rs` family owns the TTY side. What none of them own is the *text the model reads*.
//! `ExecOutput::to_text` renders `[exit {code}]` — a spelling the renderer above never emits, with no
//! stderr section, no timeout/stopped/signal facts, and no truncation notice — and it cannot be
//! parsed back: there is no inverse.
//!
//! That matters because the exit status is the one fact a terminal presentation must recover from
//! rendered text alone: replay retains the rendered content, not the original run result. The marker
//! grammar is therefore a contract with two ends, and this module owns both.
//!
//! The second gap is classification. Zero lines of the local tree match a sandbox denial against a
//! backend's stderr dialect, or distinguish "the runner never ran the command" from "the command
//! failed". Upstream spends paragraphs on that distinction because the two call for opposite
//! responses: a denial is retried once with wider permissions, a broken runner is a sandbox bug and
//! retrying it is pointless.
//!
//! # The marker contract
//!
//! [`render_result`] appends markers after the body, and [`parse_exit_status`] reads back only the
//! last of them. Any marker may stay in the body when nothing renders it as its own pill (`timed out`,
//! `stopped`, the denial notice); a *consumed* marker is removed from the body, because leaving it
//! there would show the exit twice.
//!
//! The consumed markers require a leading newline and the end of the text. That is what keeps a
//! command whose output merely *ends* with marker-like text from being read as a real exit status —
//! `[exit code: 5]` as an entire one-line body is output, not a failure.
//!
//! # Deliberately absent
//!
//! The executor lifecycle itself (`bash-local`, the persistent shells, the sandbox runner wrap), the
//! `DSH_*` environment registry (`shell-env`), and the job outcome mapping (`background.ts`'s
//! `processOutcome`) are separate concerns with local homes or their own gaps. This module is the
//! projection layer only: pure functions over already-settled facts, no I/O, no clock, no process.

use girder_core::SandboxMode;

/// One captured stream, as the subprocess seam records it.
///
/// Upstream: `CollectedOutput` in `packages/subprocess/subprocess/src/types.ts`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CollectedOutput {
    /// The collected text — the *tail* of the stream when [`truncated`](Self::truncated).
    pub text: String,
    /// Whether bytes were dropped from [`text`](Self::text).
    pub truncated: bool,
    /// A file holding the *complete* stream, when truncated and available.
    pub spill_path: Option<String>,
}

impl CollectedOutput {
    /// Unbounded text, kept whole.
    pub fn complete(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            truncated: false,
            spill_path: None,
        }
    }

    /// A truncated tail with its full-stream spill file, when one was written.
    pub fn truncated_tail(text: impl Into<String>, spill_path: Option<String>) -> Self {
        Self {
            text: text.into(),
            truncated: true,
            spill_path,
        }
    }
}

/// How completely a sandbox backend enforced the requested mode.
///
/// Upstream: `SandboxEnforcement = 'full' | 'partial'` in `packages/sandbox/sandbox/src/index.ts`.
/// Distinct from [`Enforcement`](crate::Enforcement), which names *which facility* enforces
/// (advisory vs kernel); this names whether the facility covers every promised file effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxEnforcement {
    /// Every promised file effect is governed.
    Full,
    /// An active backend or older kernel ABI cannot govern every promised file effect. A caller
    /// requiring an absolute boundary must not treat this as [`Full`](Self::Full).
    Partial,
}

/// Sandbox facts for one run, present only when a confining executor handled it.
///
/// Upstream: `ShellSandboxInfo` in `packages/shell/shell/src/types.ts`. The facts are reported
/// independently of exit status so a caller can tell a command failure from a policy denial and from
/// a broken runner — three cases with three different correct responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellSandboxInfo {
    /// The mode the command actually ran under.
    pub mode: SandboxMode,
    /// Whether the sandbox denied a file operation.
    pub denied: bool,
    /// How completely the selected runner enforced the requested mode.
    pub enforcement: Option<SandboxEnforcement>,
    /// Whether the runner failed before the command could run.
    pub runner_failed: bool,
}

impl ShellSandboxInfo {
    /// A confined run that was neither denied nor broken.
    pub fn confined(mode: SandboxMode) -> Self {
        Self {
            mode,
            denied: false,
            enforcement: None,
            runner_failed: false,
        }
    }

    /// A confined run whose file operation the sandbox refused.
    pub fn denied(mode: SandboxMode) -> Self {
        Self {
            denied: true,
            ..Self::confined(mode)
        }
    }

    /// A confined run whose runner failed before the command ran.
    ///
    /// Upstream ranks this above a denial in every rendering, because the command *did not run*: the
    /// denial dialect it may also match is a side effect of the broken runner, not a policy decision.
    pub fn runner_failed(mode: SandboxMode) -> Self {
        Self {
            runner_failed: true,
            ..Self::confined(mode)
        }
    }
}

/// A settled foreground run, in the shape the renderer consumes.
///
/// Upstream: `ShellRunResult` (plus the `stopped?` field the tools layer adds) in
/// `packages/shell/shell/src/types.ts`. Not the same type as [`ShellRunResult`](crate::ShellRunResult),
/// which is the executor's own result: this one carries the per-stream truncation facts, the
/// terminating signal, and the sandbox facts a rendering needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOutcome {
    /// The exit code, or `None` when the process was signalled — or when preparation expired before a
    /// process existed, which is why a reported `None` is not by itself "killed by a signal".
    pub exit_code: Option<i32>,
    /// The terminating signal name, when the process died from one.
    pub signal: Option<String>,
    /// Whether the executor's own deadline was the first cause to cut the command short.
    pub timed_out: bool,
    /// Why an outside kill stopped the call, when one did (the human stopping its job).
    pub stopped: Option<String>,
    /// The effective deadline, echoed into the `timed out` marker.
    pub timeout_ms: u64,
    /// Captured standard output.
    pub stdout: CollectedOutput,
    /// Captured standard error.
    pub stderr: CollectedOutput,
    /// Sandbox facts, absent for an unconfined run.
    pub sandbox: Option<ShellSandboxInfo>,
}

/// Which stream a [`Chunk`] came from.
///
/// Upstream: the `channel` field of `JobChunk`, `'stdout' | 'stderr'`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Captured standard output.
    Stdout,
    /// Captured standard error.
    Stderr,
}

/// One ordered piece of a job's bounded output ring.
///
/// Upstream: `JobChunk` in `packages/jobs`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// The stream this piece belongs to.
    pub channel: Channel,
    /// The captured text.
    pub text: String,
}

impl Chunk {
    /// A standard-output chunk.
    pub fn stdout(text: impl Into<String>) -> Self {
        Self {
            channel: Channel::Stdout,
            text: text.into(),
        }
    }

    /// A standard-error chunk.
    pub fn stderr(text: impl Into<String>) -> Self {
        Self {
            channel: Channel::Stderr,
            text: text.into(),
        }
    }
}

/// The recovery is not a clean zero: either a numeric exit or a signal name.
///
/// Upstream: the union half of `ParsedExitStatus` (`{ exitCode } | { signal }`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParsedExit {
    /// A numeric exit code.
    Code(i32),
    /// The signal name from a `killed by signal` marker.
    Signal(String),
}

impl ParsedExit {
    /// The exit code, when the status was numeric.
    pub fn code(&self) -> Option<i32> {
        match self {
            Self::Code(code) => Some(*code),
            Self::Signal(_) => None,
        }
    }
}

/// A rendered result split back into its body and its structured exit status.
///
/// Upstream: `ParsedExitStatus` in `packages/shell/shell/src/render.ts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedExitStatus {
    /// The rendered text with the consumed exit marker removed.
    pub body: String,
    /// The recovered status.
    pub exit: ParsedExit,
}

/// The notice a truncation appends to a stream's text.
///
/// Upstream: the private `streamText` helper in `packages/shell/tool-bash/src/render.ts`.
/// Unbounded output is returned verbatim; a truncated one says so and names the file that holds the
/// whole stream, so a reader that needs omitted bytes knows where to look.
pub fn stream_text(output: &CollectedOutput) -> String {
    if !output.truncated {
        return output.text.clone();
    }
    let spill = output.spill_path.as_deref().unwrap_or("(unavailable)");
    format!("{}\n[output truncated; full output: {spill}]", output.text)
}

/// The marker a sandbox denial appends, naming the mode that refused the write.
///
/// Upstream: `sandboxDenialMarker` in `packages/sandbox/sandbox/src/escalation.ts`.
pub fn sandbox_denial_marker(mode: SandboxMode) -> String {
    format!("[sandbox: file access denied under {} mode]", mode.as_str())
}

/// The same-turn escalation hint that rides a denial when the composition advertises escalation.
///
/// Upstream: `escalationHintMarker` in `packages/sandbox/sandbox/src/escalation.ts`. `subject` is the
/// capability family's noun for the denied action — `command` for the shell tools.
pub fn escalation_hint_marker(subject: &str) -> String {
    format!(
        "[sandbox: escalation available — retry this exact {subject} once with sandbox_permissions \
         (the narrowest wider mode that suffices) + justification; the approval prompt asks the user]"
    )
}

/// The notice that a sandbox *runner* failed, so the command never ran.
///
/// Upstream: the `runnerFailed` branch of `sandboxNotes` in
/// `packages/shell/tool-bash/src/background.ts` and of `renderJobRead` in
/// `packages/shell/tool-bash/src/render.ts`. It says outright that this is not a command failure,
/// because the model's otherwise-correct reflex is to retry the command.
pub fn runner_failure_notice(mode: SandboxMode) -> String {
    format!(
        "[sandbox: the sandbox runner itself failed under {} mode — the command did not run; this is \
         a sandbox problem, not a command failure]",
        mode.as_str()
    )
}

/// Shapes one finished run into the text the model sees.
///
/// Upstream: `renderResult` in `packages/shell/tool-bash/src/render.ts` (and its PowerShell twin).
///
/// The body is stdout, then a marked `[stderr]` section, then `(no output)` when both are empty. The
/// markers follow in a fixed order: sandbox denial (with the escalation hint when `escalation_modes`
/// is non-empty), the timeout, an outside stop, then the terminating fact — the signal when there was
/// one, otherwise a non-zero exit code.
///
/// A non-zero exit is *reported, not errored*: the model decides how to react. The exit marker stays
/// last because [`parse_exit_status`] anchors there.
///
/// A run with no signal and no exit code (preparation expired before a process existed) renders the
/// code as the literal `null`, which is upstream's behaviour and is why the marker is anchored on the
/// text rather than on a number.
pub fn render_result(outcome: &RunOutcome, escalation_modes: &[SandboxMode]) -> String {
    let out = stream_text(&outcome.stdout);
    let err = stream_text(&outcome.stderr);

    let mut body = out;
    if !err.is_empty() {
        // A single newline between sections; stdout usually already ends with one.
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("[stderr]\n");
        body.push_str(&err);
    }
    if body.is_empty() {
        body.push_str("(no output)");
    }

    let mut markers: Vec<String> = Vec::new();
    if let Some(sandbox) = &outcome.sandbox {
        if sandbox.denied {
            markers.push(sandbox_denial_marker(sandbox.mode));
            // The hint lives at the decision point, before the final exit marker.
            if !escalation_modes.is_empty() {
                markers.push(escalation_hint_marker("command"));
            }
        }
    }
    // A command may trap SIGTERM and exit 0 after the deadline; still report the interruption.
    if outcome.timed_out {
        markers.push(format!("[timed out after {}ms]", outcome.timeout_ms));
    }
    // A kill from outside the call is not a command failure, and the reason tells the model so.
    if let Some(reason) = &outcome.stopped {
        markers.push(format!("[stopped: {reason}]"));
    }
    if let Some(signal) = &outcome.signal {
        markers.push(format!("[killed by signal: {signal}]"));
    } else if outcome.exit_code != Some(0) {
        let code = outcome
            .exit_code
            .map_or_else(|| "null".to_owned(), |code| code.to_string());
        markers.push(format!("[exit code: {code}]"));
    }

    if markers.is_empty() {
        return body;
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&markers.join("\n"));
    body
}

/// Shapes a foreground call that stopped waiting into the text the model sees.
///
/// Upstream: `renderPromoted` in `packages/shell/tool-bash/src/render.ts`. The output captured so far
/// is followed by the still-running marker, which names the job the call became so the model can
/// follow up through the job tools.
pub fn render_promoted(job_id: &str, timeout_ms: u64, output: &str) -> String {
    let body = if output.is_empty() {
        String::new()
    } else if output.ends_with('\n') {
        output.to_owned()
    } else {
        format!("{output}\n")
    };
    format!("{body}[still running after {timeout_ms}ms; moved to background job {job_id}]")
}

/// Shapes the one job read a foreground call embeds when it stops waiting.
///
/// Upstream: `renderJobRead` in `packages/shell/tool-bash/src/render.ts`. The delta is the output
/// produced since the model's cursor; when the cursor fell behind the ring, `lossy` says bytes were
/// evicted unread and `spill_paths` names the files that still hold the complete streams.
///
/// A broken runner outranks a denial: the command never ran, so a denial-shaped line is a symptom.
pub fn render_job_read(
    delta: &str,
    lossy: bool,
    spill_paths: &[String],
    sandbox: Option<&ShellSandboxInfo>,
    escalation_modes: &[SandboxMode],
) -> String {
    let mut notices: Vec<String> = Vec::new();
    if lossy {
        let paths = if spill_paths.is_empty() {
            "(unavailable)".to_owned()
        } else {
            spill_paths.join(", ")
        };
        notices.push(format!(
            "[some output was dropped from memory; full output: {paths}]"
        ));
    }
    if let Some(sandbox) = sandbox {
        if sandbox.runner_failed {
            notices.push(runner_failure_notice(sandbox.mode));
        } else if sandbox.denied {
            notices.push(sandbox_denial_marker(sandbox.mode));
            if !escalation_modes.is_empty() {
                notices.push(escalation_hint_marker("command"));
            }
        }
    }
    if notices.is_empty() {
        return delta.to_owned();
    }

    let mut out = String::from(delta);
    if !delta.is_empty() && !delta.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&notices.join("\n"));
    out
}

/// Renders one consuming ring read: stdout chunks in order, then every stderr chunk in one `[stderr]`
/// section.
///
/// Upstream: `ringDelta` in `packages/shell/tool-bash/src/background.ts`. Sectioning rather than
/// interleaving is what makes the output a foreground call hands over read exactly like the
/// `job_output` reads that follow it: chunk order within the ring is a buffering artefact, not the
/// order the two streams were written in.
pub fn ring_delta(chunks: &[Chunk]) -> String {
    let mut out = String::new();
    let mut err = String::new();
    for chunk in chunks {
        match chunk.channel {
            Channel::Stdout => out.push_str(&chunk.text),
            Channel::Stderr => err.push_str(&chunk.text),
        }
    }
    if err.is_empty() {
        return out;
    }
    let separator = if out.is_empty() || out.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{out}{separator}[stderr]\n{err}")
}

/// Splits a rendered result into its body and the exit status its marker reported.
///
/// Upstream: `parseExitStatus` in `packages/shell/shell/src/render.ts`.
///
/// A killed marker yields [`ParsedExit::Signal`]; otherwise a numeric marker yields
/// [`ParsedExit::Code`]; absent both the status is a clean exit 0. The consumed marker is removed from
/// the body because a terminal presentation shows the exit status as its own pill, and leaving the
/// marker would render the exit twice. Markers no pill shows — the timeout, a stop, a denial — stay in
/// the body.
///
/// This is the inverse of [`render_result`], and the reason it exists is replay: replay retains the
/// rendered content, not the original [`RunOutcome`], so the exit pill must be recoverable from text.
///
/// The marker must start a line and end the text, so output that merely *ends* with marker-like text
/// stays in the body. A digit run that does not fit an `i32` is not treated as a marker either: the
/// renderer can only produce a marker for an exit code that fits, so such a line is output.
pub fn parse_exit_status(text: &str) -> ParsedExitStatus {
    if let Some(line_start) = text.rfind('\n') {
        let line = &text[line_start + 1..];
        if let Some(name) = line
            .strip_prefix("[killed by signal: ")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            // Upstream's `[^\]\n]+`: non-empty, and no `]` may appear in the name.
            if !name.is_empty() && !name.contains(']') {
                return ParsedExitStatus {
                    body: text[..line_start].to_owned(),
                    exit: ParsedExit::Signal(name.to_owned()),
                };
            }
        }
        if let Some(digits) = line
            .strip_prefix("[exit code: ")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                if let Ok(code) = digits.parse::<i32>() {
                    return ParsedExitStatus {
                        body: text[..line_start].to_owned(),
                        exit: ParsedExit::Code(code),
                    };
                }
            }
        }
    }
    ParsedExitStatus {
        body: text.to_owned(),
        exit: ParsedExit::Code(0),
    }
}

/// Whether a non-zero exit's stderr matches one of a backend's denial signatures.
///
/// Upstream: `matchesSignature` in `packages/sandbox/sandbox/src/diagnostics.ts`.
///
/// Signatures are the *selected backend's* dialect — `EROFS` text under bwrap's read-only binds,
/// `EACCES` under Landlock, `EPERM` under Seatbelt — rather than a cross-backend union, which would
/// claim denials a given backend never produces. Matching is case-insensitive, and exit status alone
/// never proves a denial: a clean exit or a signal death is not one.
pub fn matches_signature(exit_code: Option<i32>, stderr: &str, signatures: &[&str]) -> bool {
    if matches!(exit_code, None | Some(0)) {
        return false;
    }
    let lowered = stderr.to_lowercase();
    signatures
        .iter()
        .any(|signature| lowered.contains(&signature.to_lowercase()))
}

/// Whether a failed run is a sandbox denial rather than a command failure.
///
/// Upstream: `classifyDenial` in `packages/shell/bash-sandbox/src/helpers.ts` (and the pwsh twin),
/// which is exactly [`matches_signature`] over the settled result's stderr.
pub fn classify_denial(exit_code: Option<i32>, stderr: &str, signatures: &[&str]) -> bool {
    matches_signature(exit_code, stderr, signatures)
}

/// Evidence that identifies a sandbox runner failing before it executed the command.
///
/// Upstream: `RunnerFailureRule` in `packages/sandbox/sandbox/src/index.ts`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunnerFailureRule {
    /// Non-zero exit codes this rule may match; `None` permits any non-zero exit.
    pub allowed_exit_codes: Option<Vec<i32>>,
    /// Non-empty substrings identifying a fatal runner diagnostic on one stderr line.
    pub fatal_signatures: Vec<String>,
    /// Benign stderr lines excluded by exact full-line equality before fatal matching.
    pub informational_lines: Vec<String>,
}

/// The stderr line that proved a runner failure, kept for the infrastructure error's detail.
///
/// Upstream: the `RunnerFailureMatch` interface in `packages/shell/pwsh-sandbox/src/helpers.ts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerFailureMatch {
    /// The original stderr line that matched a fatal signature, unmodified.
    pub detail: String,
}

/// Classifies one settled process against a backend's structured runner-failure rules.
///
/// Upstream: `classifyRunnerFailure` in `packages/shell/pwsh-sandbox/src/helpers.ts` (behaviourally
/// identical to the `@deepseek-ai/dsh-sandbox` original the bash consumer imports).
///
/// Every rule requires a non-zero exit, its optional exit-code gate, and a fatal signature on one
/// stderr line *after* exact informational lines are excluded — a rule that would otherwise match the
/// runner's ordinary "sandbox ready" chatter does not. Rules are tried in order, and within a rule the
/// first offending line wins, so the reported detail is the fatal line rather than an informational
/// one that merely preceded it.
///
/// A blank signature carries no evidence and is ignored, but a valid signature beside it stays active.
pub fn classify_runner_failure(
    exit_code: Option<i32>,
    stderr: &str,
    rules: &[RunnerFailureRule],
) -> Option<RunnerFailureMatch> {
    let code = exit_code.filter(|code| *code != 0)?;
    let lines: Vec<&str> = stderr
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();

    for rule in rules {
        if let Some(allowed) = &rule.allowed_exit_codes {
            if !allowed.contains(&code) {
                continue;
            }
        }
        let informational: Vec<String> = rule
            .informational_lines
            .iter()
            .map(|line| line.to_lowercase())
            .collect();
        let fatal: Vec<String> = rule
            .fatal_signatures
            .iter()
            .filter(|signature| !signature.trim().is_empty())
            .map(|signature| signature.to_lowercase())
            .collect();

        for line in &lines {
            let lowered = line.to_lowercase();
            if informational.contains(&lowered) {
                continue;
            }
            if fatal.iter().any(|signature| lowered.contains(signature)) {
                return Some(RunnerFailureMatch {
                    detail: (*line).to_owned(),
                });
            }
        }
    }
    None
}
