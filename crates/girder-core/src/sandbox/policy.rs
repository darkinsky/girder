//! Sandbox policy: what a mode *means*, how it is resolved per call, the escalation ladder and its
//! approval choreography, and the runner facts a confined spawn needs.
//!
//! # Where this comes from
//!
//! Upstream's `packages/sandbox` is four packages totalling 11,427 lines. Excluding the
//! `.spec.ts`/`.e2e.ts` suites, 3,967 lines of production TypeScript remain across 20 files:
//!
//! - `sandbox/src/index.ts` — the `SandboxProvider` service, the `SandboxMode`/`SandboxPolicy`
//!   vocabulary, `ConfinedArgv`, `SandboxEnforcement`, and `RunnerFailureRule`.
//! - `sandbox/src/escalation.ts` — `WIDER_MODES` ([`wider_modes`]), `ESCALATION_TARGETS`
//!   ([`ESCALATION_TARGETS`]), `validateEscalationArgs` ([`validate_escalation_args`]),
//!   `sandboxPermissionsDescription` ([`sandbox_permissions_description`]), and
//!   `approveEscalation` ([`approve_escalation`]).
//! - `sandbox/src/roots.ts` — `canonicalPath` ([`canonical_path`]), `workspaceWritableRoots`
//!   ([`workspace_writable_roots`]), `writableRoots` ([`writable_roots`]), and
//!   `SandboxInvalidRootError` ([`SandboxRootError`]).
//! - `sandbox/src/locales.ts` — `escalationDisplayReason` ([`escalation_display_reason`]), the eight
//!   localized approval sentences.
//! - `sandbox-policy/src/index.ts` — `SandboxPolicyService` ([`PolicyResolver`]), the mode/root/
//!   extra-root resolution order, and `renderPolicyContext` ([`render_policy_context`]).
//! - `sandbox-policy/src/session-mode.ts` — `SANDBOX_MODES` ([`SANDBOX_MODES`]), `setSandboxMode`
//!   ([`set_sandbox_mode`]), and the `sandbox/mode` fold ([`session_override`]).
//! - `sandbox-local/src/profiles.ts` — `bwrapProfileArgs` ([`bwrap_profile_args`]),
//!   `landlockProfileArgs` ([`landlock_grants`]), `seatbeltProfileArgs` ([`seatbelt_profile_args`]).
//! - `sandbox-local/src/index.ts` — `PLATFORM_CHAINS` ([`platform_chain`]), `STATIC_ENFORCEMENT`
//!   ([`static_enforcement`]), `DENIAL_SIGNATURES` ([`denial_signatures`]), `RUNNER_FAILURE_RULES`
//!   ([`runner_failure_rules`]), and `chainVerdict` ([`select_runner`]).
//! - `sandbox-windows-acl/src/path-boundary.ts` — `isTempRootInsideWorkspace`
//!   ([`temp_root_inside_workspace`]) and `assertTempRootOutsideWorkspace`
//!   ([`assert_temp_root_outside_workspace`]).
//!
//! # Why this is the largest remaining gap
//!
//! The local tree already answers three of the package's questions:
//!
//! - **Where does a path really land?** [`crate::PathSandbox`] (`sandbox_path.rs`) resolves symlinks
//!   component by component and classifies a path as inside, outside, or unknown — a *fence*.
//!   Nothing here re-implements it; [`temp_root_inside_workspace`] is a caller of it.
//! - **Which preset did the user pick?** [`crate::credentials::permissions`] maps one named preset onto a sandbox
//!   mode and an approval policy, and [`crate::ShellSandboxInfo`] reports the per-run facts.
//! - **How is a command run and its failure classified?** `exec.rs` spawns under a [`crate::PathPolicy`]
//!   and probes Landlock; `shell_gap.rs` classifies a denial or a broken runner from stderr.
//!
//! What no local module owns is the *meaning of a mode* and the *choreography around a denial*:
//!
//! 1. **Which directories a mode grants.** `PathPolicy` holds prefixes a caller assembled by hand, so
//!    "`workspace-write` means the workspace, the explicit extra roots, and the platform temp areas"
//!    exists nowhere. Two consumers cannot drift because neither is derived from the other — each is
//!    written twice. Upstream keeps this derivation in one module precisely so the Seatbelt profile
//!    and the filesystem fence cannot disagree.
//! 2. **Per-call resolution.** Upstream resolves `approved override → session override → deployment
//!    default`, with the session's own `cwd` as the boundary. Locally the deployment has a default, a
//!    session records `sandbox/mode` events, and *nothing reads them back*: the fold that turns those
//!    events into the effective mode is missing, so a session-scoped switch is write-only.
//! 3. **The escalation ladder.** A denial is not the end of the interaction upstream: the model may
//!    retry the same call once with a strictly wider mode, and the widening is an execution check
//!    gated on a human's approval. Locally [`crate::escalation_hint_marker`] advertises that lever
//!    and nothing implements it, which teaches the model to pull a lever that is not connected.
//!
//! # Design notes
//!
//! - **The mode is per call, not per provider.** Two consumers may confine under different policies at
//!   the same instant, and an approved escalation is a *new* call with a wider policy. Every resolving
//!   function here therefore takes the policy and returns a value; none stores one.
//! - **Widening is checked at execution, never baked into a schema.** The schema's enum is
//!   [`ESCALATION_TARGETS`]; the effective mode is per-call truth. A schema enum pinned to "wider than
//!   this call" would strand a session whose mode sits below the deployment default.
//! - **Fail closed.** A mode that cannot be enforced is an error, never the original argv with a
//!   warning: `confine` refuses rather than passing a command through unconfined.
//!
//! # Deliberately absent
//!
//! The Windows ACL grant materialization (`AclWriteGrant`, SID derivation, private temp directories),
//! the Landlock launcher's flag spelling (owned by the native addon, not by `profiles.ts`), the
//! functional probes themselves, and the session-lifetime registration bookkeeping (a `WeakMap` of
//! disposers upstream, which belongs to whichever local registry owns session state). Each is a
//! separate concern with its own host dependency; [`Runner::WindowsAcl`] and [`Runner::Landlock`]
//! therefore report their facts but refuse to produce an invocation they cannot spell correctly.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use girder_core::{
    BoxFut, PathSandbox, RunnerFailureRule, SandboxEnforcement, SandboxMode, SandboxVerdict,
    SessionEvent,
};

// ---- the mode vocabulary ----------------------------------------------------------------------

/// Every [`SandboxMode`], for option advertisement and for validating an untrusted mode string.
///
/// Upstream: `SANDBOX_MODES` in `packages/sandbox/sandbox-policy/src/session-mode.ts`. Ordered from the
/// narrowest mode to the widest, which is also the order a picker renders and the order
/// [`wider_modes`] follows.
pub const SANDBOX_MODES: [SandboxMode; 3] = [
    SandboxMode::ReadOnly,
    SandboxMode::WorkspaceWrite,
    SandboxMode::Full,
];

/// The closed set of modes a call could ever escalate *to*.
///
/// Upstream: `ESCALATION_TARGETS` in `packages/sandbox/sandbox/src/escalation.ts`. `read-only` is the
/// floor, so nothing escalates to it, and the set is independent of the deployment's default: cutting
/// it down to the modes wider than the default would strand a session whose effective mode sits below
/// that default with no lever at all.
pub const ESCALATION_TARGETS: [SandboxMode; 2] = [SandboxMode::WorkspaceWrite, SandboxMode::Full];

/// Parses an untrusted mode string.
///
/// Upstream: the `SANDBOX_MODES` runtime validation in
/// `packages/sandbox/sandbox-policy/src/session-mode.ts`, which exists because a mode name can arrive
/// from a UI control, a config file, or a replayed log — none of which the type system has vetted.
///
/// Returns `None` for anything outside the closed set, so a caller decides whether an unknown name is an
/// error (a config file) or ignorable (a log written by a newer version).
#[must_use]
pub fn parse_mode(name: &str) -> Option<SandboxMode> {
    match name {
        "read-only" => Some(SandboxMode::ReadOnly),
        "workspace-write" => Some(SandboxMode::WorkspaceWrite),
        "danger-full-access" => Some(SandboxMode::Full),
        _ => None,
    }
}

/// The modes a call whose effective mode is `mode` may escalate to.
///
/// Upstream: `WIDER_MODES` in `packages/sandbox/sandbox/src/escalation.ts`. Empty for the widest mode,
/// because there is nothing above it — and the emptiness is what makes
/// [`approve_escalation`] fail closed rather than treating a repeated request as a widening.
#[must_use]
pub fn wider_modes(mode: SandboxMode) -> &'static [SandboxMode] {
    match mode {
        SandboxMode::ReadOnly => &[SandboxMode::WorkspaceWrite, SandboxMode::Full],
        SandboxMode::WorkspaceWrite => &[SandboxMode::Full],
        SandboxMode::Full => &[],
    }
}

/// Whether `requested` is strictly wider than `effective`.
///
/// Upstream: the `WIDER_MODES[effectiveMode].includes(mode)` execution check in
/// `approveEscalation`, `packages/sandbox/sandbox/src/escalation.ts`. Equality is *not* widening — a
/// call may repeat its own mode for free — which is why this answers `false` for it and
/// [`approve_escalation`] handles the repeat before consulting the ladder.
#[must_use]
pub fn is_strictly_wider(effective: SandboxMode, requested: SandboxMode) -> bool {
    wider_modes(effective).contains(&requested)
}

// ---- roots: what a mode grants ----------------------------------------------------------------

/// Why a path cannot be used as a sandbox grant.
///
/// Upstream: `SandboxInvalidRootError` in `packages/sandbox/sandbox/src/roots.ts`, carrying the
/// `SANDBOX_INVALID_ROOT` code. The three facts upstream interpolates into the message are kept
/// separate here so a caller can render its own: the path as the caller spelled it, the role the path
/// plays in the policy, and what is wrong with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxRootError {
    /// The rejected path, as the caller spelled it.
    pub path: String,
    /// The role of the path in the policy, as the message names it.
    pub subject: &'static str,
    /// Why the path cannot be enforced.
    pub detail: String,
}

impl SandboxRootError {
    /// Builds the error for one rejected path.
    #[must_use]
    pub fn new(path: impl Into<String>, subject: &'static str, detail: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            subject,
            detail: detail.into(),
        }
    }

    /// The stable code a consumer routes on.
    ///
    /// Upstream: the `SANDBOX_INVALID_ROOT` code passed to `HarnessError`, which travels through
    /// `tool/result` so a caller can tell a policy refusal from a command failure.
    // `&self` rather than an associated function: the code is a fact *about this error* to every caller
    // that routes on it, and a uniform `error.code()` across the crate's error types is what lets a
    // consumer write one match arm per family.
    #[allow(clippy::unused_self)]
    #[must_use]
    pub fn code(&self) -> &'static str {
        "SANDBOX_INVALID_ROOT"
    }
}

impl std::fmt::Display for SandboxRootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Upstream: `Invalid ${subject} ${JSON.stringify(path)}: ${detail}`.
        write!(
            f,
            "Invalid {} \"{}\": {}",
            self.subject, self.path, self.detail
        )
    }
}

impl std::error::Error for SandboxRootError {}

/// The role a workspace root plays in a policy message.
const SUBJECT_WORKSPACE_ROOT: &str = "sandbox workspace root";
/// The role an operator-configured extra directory plays in a policy message.
const SUBJECT_ADDITIONAL_ROOT: &str = "additional writable directory";

/// Whether a path is usable as an execution-world grant: absolute and free of NUL bytes.
///
/// Upstream: the private `isExecutionWorldPath` in
/// `packages/sandbox/sandbox-policy/src/index.ts`. Both halves matter: a relative root means a different
/// directory per process, and a NUL cannot be a filesystem name, so a "grant" containing one would be
/// silently shortened by whatever C API eventually received it.
#[must_use]
pub fn is_execution_world_path(path: &str) -> bool {
    Path::new(path).is_absolute() && !path.contains('\0')
}

/// Resolves a path to the identity the enforcement layer actually compares.
///
/// Upstream: `canonicalPath` in `packages/sandbox/sandbox/src/roots.ts`. Resolution replaces symlinks
/// (`/tmp` *is* `/private/tmp` on darwin, and an as-spelled grant would match nothing) and collapses
/// `..` the way `chdir`/`spawn` do.
///
/// A path that cannot be resolved is returned **as spelled**, not invented and not dropped: a missing
/// root matches nothing until it exists, which is the conservative outcome, while a fallback would grant
/// a path the caller never named.
#[must_use]
pub fn canonical_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The hard-coded platform temp root that `workspace-write` promises.
///
/// Upstream: the `canonicalPath('/tmp')` term of `writableRoots` in
/// `packages/sandbox/sandbox/src/roots.ts`, which is unconditional because on a non-Unix host the
/// spelling matches no real path anyway. Here it is absent on Windows, where `/tmp` cannot be opened:
/// the effective allow-list is identical, and a grant that cannot be enforced is omitted rather than
/// carried as a string that silently never matches.
// The `Option` is the contract, not an artefact: on a Unix host there *is* a `/tmp` and on Windows there
// is not, and folding that into a bare `Path` would hide a platform fact behind a constant that lies on
// one of the two.
#[allow(clippy::unnecessary_wraps)]
#[must_use]
pub fn host_temp_root() -> Option<&'static Path> {
    #[cfg(windows)]
    {
        None
    }
    #[cfg(not(windows))]
    {
        Some(Path::new("/tmp"))
    }
}

/// The complete file-effect policy resolved for one capability call.
///
/// Upstream: `SandboxExecutionPolicy` in `packages/sandbox/sandbox/src/index.ts`. The workspace root is
/// carried even under modes that do not consume it, so a caller can resolve the policy once before
/// choosing an enforcement path — and so the session's identity travels with the decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionPolicy {
    /// The file-effect mode this execution runs under.
    pub mode: SandboxMode,
    /// The absolute root `workspace-write` may write under.
    pub workspace_root: PathBuf,
    /// Extra physical directories `workspace-write` may also write under, or `None` for none.
    ///
    /// Held in their execution-world spelling: a local host must not canonicalize a remote path. The
    /// enforcing host validates each physical directory before granting it, on every call.
    pub additional_writable_roots: Option<Vec<PathBuf>>,
    /// Opaque identity of the calling session, when there is one.
    ///
    /// Backends key per-session state off it (Windows ACL gives each live session/workspace pair a
    /// private temp directory and SID); it is absent for agentless calls, which fall back to per-call
    /// state.
    pub session_id: Option<girder_core::SessionId>,
}

impl ExecutionPolicy {
    /// A read-only policy rooted at `workspace_root`.
    #[must_use]
    pub fn read_only(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            mode: SandboxMode::ReadOnly,
            workspace_root: workspace_root.into(),
            additional_writable_roots: None,
            session_id: None,
        }
    }

    /// The extra roots as a slice, empty when none are configured.
    ///
    /// Distinct from the field so a consumer does not have to spell `map_or` for the ordinary case.
    #[must_use]
    pub fn additional_roots(&self) -> &[PathBuf] {
        self.additional_writable_roots
            .as_deref()
            .unwrap_or_default()
    }
}

/// Resolves one granted root to the physical directory the enforcement layer compares.
///
/// Upstream: the private `additionalRoot` in `packages/sandbox/sandbox/src/roots.ts`. An alias that
/// differs under the host's path comparison is refused rather than quietly canonicalized: a grant
/// spelled through a symlink would be re-resolved by the enforcement layer to something the operator did
/// not name, and on a hostile tree that is the difference between granting one directory and granting
/// whatever the link points at later.
fn additional_root(path: &Path) -> Result<PathBuf, SandboxRootError> {
    let spelled = path.to_string_lossy();
    let physical = std::fs::canonicalize(path).map_err(|error| {
        SandboxRootError::new(
            spelled.as_ref(),
            SUBJECT_ADDITIONAL_ROOT,
            format!("cannot read the directory: {error}"),
        )
    })?;
    if !physical.is_dir() {
        return Err(SandboxRootError::new(
            spelled.as_ref(),
            SUBJECT_ADDITIONAL_ROOT,
            "expected an existing directory",
        ));
    }
    if girder_core::normalize_path(path) != physical {
        return Err(SandboxRootError::new(
            spelled.as_ref(),
            SUBJECT_ADDITIONAL_ROOT,
            format!(
                "use the physical directory path \"{}\"; symlinked grants are not supported",
                physical.to_string_lossy()
            ),
        ));
    }
    Ok(physical)
}

/// The workspace-owned writable roots: the policy's workspace plus its explicit extra directories.
///
/// Upstream: `workspaceWritableRoots` in `packages/sandbox/sandbox/src/roots.ts`. Canonical and
/// deduplicated, because both the filesystem fence and the POSIX process backends compare resolved
/// paths. This derivation is deliberately separate from each backend's *platform temp* policy: bwrap and
/// Landlock add their own temp grants, which is a per-runner fact rather than part of the mode's
/// workspace meaning.
///
/// Empty under every mode but `workspace-write` — a grant list is meaningless when the mode permits no
/// writes, and returning the roots anyway would let a careless backend pass them on.
pub fn workspace_writable_roots(
    policy: &ExecutionPolicy,
) -> Result<Vec<PathBuf>, SandboxRootError> {
    if policy.mode != SandboxMode::WorkspaceWrite {
        return Ok(Vec::new());
    }
    let mut roots: Vec<PathBuf> = Vec::with_capacity(policy.additional_roots().len() + 1);
    roots.push(canonical_path(&policy.workspace_root));
    for root in policy.additional_roots() {
        roots.push(additional_root(root)?);
    }
    Ok(deduplicated(roots))
}

/// The roots one confined execution may **write** under.
///
/// Upstream: `writableRoots` in `packages/sandbox/sandbox/src/roots.ts`. `workspace-write` allows the
/// policy's workspace root, its explicit extra directories, the host `/tmp`, and the per-user platform
/// temp directory (`std::env::temp_dir()` — the real temp area for `mkstemp`-family tools, so omitting
/// it would deny what the mode promises).
pub fn writable_roots(policy: &ExecutionPolicy) -> Result<Vec<PathBuf>, SandboxRootError> {
    if policy.mode != SandboxMode::WorkspaceWrite {
        return Ok(Vec::new());
    }
    let mut roots = workspace_writable_roots(policy)?;
    if let Some(temp) = host_temp_root() {
        roots.push(canonical_path(temp));
    }
    roots.push(canonical_path(&std::env::temp_dir()));
    Ok(deduplicated(roots))
}

/// Keeps the first occurrence of each root, preserving order.
///
/// A `BTreeSet` would sort, and the order is a fact worth keeping: the Seatbelt profile lists the
/// workspace before the temp areas, which is what makes a diagnostic readable. Deduplication is by
/// resolved path, so `/tmp` and `TMPDIR=/tmp` collapse.
fn deduplicated(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut out = Vec::with_capacity(roots.len());
    for root in roots {
        if seen.insert(root.clone()) {
            out.push(root);
        }
    }
    out
}

/// Whether `workspace_root` contains `temp_root`, or is the same directory.
///
/// Upstream: `isTempRootInsideWorkspace` (via the private `containsDirectory`) in
/// `packages/sandbox/sandbox-windows-acl/src/path-boundary.ts`. Both sides are resolved first, and
/// containment is decided per path segment, so `/work2` is not inside `/work`.
///
/// # Errors
///
/// Returns [`SandboxRootError`] when either path is not absolute, or when the fence cannot tell where
/// the path lands (a symlink loop, an unreadable directory). "Cannot tell" is an error rather than a
/// `false`: a caller that treated it as "outside" would create a private temp directory inside a tree
/// it had just failed to measure, and inherit the standing workspace grant.
pub fn temp_root_inside_workspace(
    workspace_root: &Path,
    temp_root: &Path,
) -> Result<bool, SandboxRootError> {
    let workspace = workspace_root.to_string_lossy();
    if !workspace_root.is_absolute() {
        return Err(SandboxRootError::new(
            workspace.as_ref(),
            SUBJECT_WORKSPACE_ROOT,
            "expected an absolute path",
        ));
    }
    let temp = temp_root.to_string_lossy();
    if !temp_root.is_absolute() {
        return Err(SandboxRootError::new(
            temp.as_ref(),
            "platform temp root",
            "expected an absolute path",
        ));
    }
    let sandbox = PathSandbox::new(workspace_root.to_path_buf()).map_err(|error| {
        SandboxRootError::new(
            workspace.as_ref(),
            SUBJECT_WORKSPACE_ROOT,
            error.to_string(),
        )
    })?;
    match sandbox.check(&temp) {
        SandboxVerdict::Inside => Ok(true),
        SandboxVerdict::Outside { .. } => Ok(false),
        SandboxVerdict::Unknown { detail } => Err(SandboxRootError::new(
            temp.as_ref(),
            "platform temp root",
            format!("cannot resolve the directory: {detail}"),
        )),
    }
}

/// Refuses a temp root that lies inside the workspace.
///
/// Upstream: `assertTempRootOutsideWorkspace` in
/// `packages/sandbox/sandbox-windows-acl/src/path-boundary.ts`, whose message is the
/// `SANDBOX_INVALID_TEMP_ROOT` preparation failure in `sandbox-local`. A private temp directory created
/// below the workspace would inherit the standing workspace capability, which merges two grants that are
/// supposed to be independent — the workspace one standing per workspace, the temp one revocable per
/// session.
pub fn assert_temp_root_outside_workspace(
    workspace_root: &Path,
    temp_root: &Path,
) -> Result<(), SandboxRootError> {
    if temp_root_inside_workspace(workspace_root, temp_root)? {
        let workspace = workspace_root.to_string_lossy();
        let temp = temp_root.to_string_lossy();
        return Err(SandboxRootError::new(
            temp.as_ref(),
            "platform temp root",
            format!(
                "must be outside the workspace: workspace={workspace}; temp={temp}. \
                 A private temp directory there would inherit the workspace grant"
            ),
        ));
    }
    Ok(())
}

// ---- per-call policy resolution ---------------------------------------------------------------

/// A session's contribution to one policy resolution.
///
/// Upstream reads these three facts straight off a live `Session` (its immutable `cwd`, its `sandbox/mode`
/// projection state, and its id). The local [`Session`](girder_core::Session) seam carries events rather
/// than a header, so the fold is exposed as [`session_override`] and the caller hands the result in here.
/// Keeping them as an explicit input is also what makes the resolution order testable without a
/// filesystem session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyRequest {
    /// The session's working directory, which becomes the `workspace-write` boundary.
    pub session_cwd: Option<String>,
    /// The session's last recorded mode, from [`session_override`].
    pub session_override: Option<SandboxMode>,
    /// An approved explicit mode, which outranks the session's override.
    pub mode: Option<SandboxMode>,
    /// The calling session's identity, carried onto the resolved policy.
    pub session_id: Option<girder_core::SessionId>,
}

/// The deployment's sandbox policy: its default mode, fallback root, and shared extra directories.
///
/// Upstream: `SandboxPolicyService` (`ctx.sandboxPolicy`) in
/// `packages/sandbox/sandbox-policy/src/index.ts`, minus the parts that need a Cordis context — the
/// system-prompt contribution (rendered here by [`render_policy_context`]) and the per-session
/// registration map (see the module note on what is deliberately absent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyResolver {
    default_mode: SandboxMode,
    workspace_root: PathBuf,
    additional_writable_roots: Option<Vec<PathBuf>>,
}

impl PolicyResolver {
    /// Builds a deployment policy.
    ///
    /// The root is kept in its execution-world spelling rather than canonicalized: the enforcing host is
    /// the one that resolves filesystem identity, and a local `realpath` must not rewrite a remote path.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxRootError`] when the root is relative or contains a NUL byte.
    pub fn new(
        default_mode: SandboxMode,
        workspace_root: impl Into<PathBuf>,
    ) -> Result<Self, SandboxRootError> {
        let workspace_root = workspace_root.into();
        let spelled = workspace_root.to_string_lossy();
        if !is_execution_world_path(&spelled) {
            return Err(SandboxRootError::new(
                spelled.as_ref(),
                SUBJECT_WORKSPACE_ROOT,
                "must be an absolute execution-world path without NUL bytes",
            ));
        }
        Ok(Self {
            default_mode,
            workspace_root,
            additional_writable_roots: None,
        })
    }

    /// Adds the operator-configured extra writable directories.
    ///
    /// Upstream: `resolveAdditionalWritableRoots` in
    /// `packages/sandbox/sandbox-policy/src/index.ts`. The list is validated (absolute, no NUL),
    /// deduplicated, and detached; each entry is still re-validated by the enforcing host on every call,
    /// because a directory can disappear between configuration and use.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxRootError`] for a relative path or one containing a NUL byte.
    pub fn with_additional_roots<I, S>(mut self, roots: I) -> Result<Self, SandboxRootError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut kept: Vec<PathBuf> = Vec::new();
        for root in roots {
            let root = root.as_ref();
            if !is_execution_world_path(root) {
                return Err(SandboxRootError::new(
                    root,
                    SUBJECT_ADDITIONAL_ROOT,
                    "must be an absolute execution-world path without NUL bytes",
                ));
            }
            kept.push(PathBuf::from(root));
        }
        kept.dedup();
        self.additional_writable_roots = Some(deduplicated(kept));
        Ok(self)
    }

    /// The deployment default mode — the fallback beneath a session override.
    #[must_use]
    pub fn default_mode(&self) -> SandboxMode {
        self.default_mode
    }

    /// The absolute `workspace-write` fallback root for agentless calls.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// The frozen extra writable directories every resolved policy shares, or `None` for none.
    #[must_use]
    pub fn additional_writable_roots(&self) -> Option<&[PathBuf]> {
        self.additional_writable_roots.as_deref()
    }

    /// Resolves the complete policy for one capability call.
    ///
    /// Upstream: `SandboxPolicyService.resolve` in
    /// `packages/sandbox/sandbox-policy/src/index.ts`. Precedence is `request.mode` (an approved
    /// escalation) over `request.session_override` over [`Self::default_mode`]. The session's cwd, when
    /// present, *is* the workspace-write boundary; the configured root is the fallback for agentless
    /// calls and sessions without a cwd.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxRootError`] when the session's recorded cwd is relative or contains a NUL byte —
    /// the one case where the caller's selected path cannot be enforced without changing it.
    pub fn resolve(&self, request: &PolicyRequest) -> Result<ExecutionPolicy, SandboxRootError> {
        let workspace_root = match request.session_cwd.as_deref() {
            None => self.workspace_root.clone(),
            Some(cwd) => {
                if !is_execution_world_path(cwd) {
                    return Err(SandboxRootError::new(
                        cwd,
                        SUBJECT_WORKSPACE_ROOT,
                        "the Session working directory must be an absolute execution-world path without NUL bytes",
                    ));
                }
                PathBuf::from(cwd)
            }
        };
        Ok(ExecutionPolicy {
            mode: request
                .mode
                .or(request.session_override)
                .unwrap_or(self.default_mode),
            workspace_root,
            additional_writable_roots: self.additional_writable_roots.clone(),
            session_id: request.session_id.clone(),
        })
    }
}

/// The session's last recorded sandbox mode — the `sandbox/mode` fold.
///
/// Upstream: the `sandboxMode` projection unit registered in
/// `packages/sandbox/sandbox-policy/src/index.ts`, whose `apply` is
/// `event.type === 'sandbox/mode' ? event.data.mode : state`. The last such event wins, which is what
/// makes an override survive a restart by replay and keeps two sessions from seeing each other's state.
///
/// A mode name the local closed set does not contain is skipped, keeping whatever came before it: a log
/// written by a newer build records a mode this one cannot enforce, and dropping the *whole* override
/// would silently widen the session to the deployment default — the failure direction a sandbox must not
/// have. `None` means the session never switched, so the deployment default applies.
#[must_use]
pub fn session_override(events: &[SessionEvent]) -> Option<SandboxMode> {
    let mut state = None;
    for event in events {
        if let SessionEvent::SandboxMode { mode, .. } = event {
            if let Some(parsed) = parse_mode(mode) {
                state = Some(parsed);
            }
        }
    }
    state
}

/// The one write path for a session's sandbox-mode override.
///
/// Upstream: `setSandboxMode` in `packages/sandbox/sandbox-policy/src/session-mode.ts`. The switch *is*
/// its event: nothing mutates mode state out of band, so a replay reconstructs the same mode the
/// enforcing consumers resolved. `source` marks an override seeded into a child at delegation, as opposed
/// to a runtime switch, and is `None` for the latter.
#[must_use]
pub fn set_sandbox_mode(mode: SandboxMode, source: Option<&str>) -> SessionEvent {
    SessionEvent::SandboxMode {
        mode: mode.as_str().to_owned(),
        source: source.map(str::to_owned),
    }
}

/// Renders the policy paragraph a session's model requests carry.
///
/// Upstream: the private `renderPolicyContext` in
/// `packages/sandbox/sandbox-policy/src/index.ts`, contributed to the cache-safe runtime-context
/// snapshot so replay reconstructs the same mode and root without rewriting the stable system prompt.
///
/// The text is **verbatim upstream**, including the product name in `Current DSH file policy`, because it
/// is a contract with the model in the same way [`crate::sandbox_denial_marker`] is: the paragraph is
/// replayed, snapshotted, and matched by the wording the model was trained against. Each branch also says
/// what the policy does *not* license — a `read-only` paragraph that only said "read-only" would teach a
/// model to refuse a required modification instead of trying the tool and following the denial guidance.
#[must_use]
pub fn render_policy_context(policy: &ExecutionPolicy) -> String {
    match policy.mode {
        SandboxMode::ReadOnly => "Current DSH file policy: read-only. Any available operation enforced by \
             the DSH file sandbox cannot modify files in the standing mode. Do not refuse a required \
             modification from this policy alone: try an available tool normally and follow any denial \
             and escalation guidance it returns."
            .to_owned(),
        SandboxMode::WorkspaceWrite => {
            let mut text = format!(
                "Current DSH file policy: workspace-write. Any available operation enforced by the DSH \
                 file sandbox may modify files under the session workspace: {}.",
                json_string_array([policy.workspace_root.as_path()])
            );
            if let Some(additional) = policy.additional_writable_roots.as_deref() {
                if !additional.is_empty() {
                    let _ = write!(
                        text,
                        " Additional writable directories: {}. On Windows, these additional grants \
                         apply to file tools only, not sandboxed processes.",
                        json_string_array(additional)
                    );
                }
            }
            let _ = write!(
                text,
                " Some platform temporary areas may also be writable."
            );
            text
        }
        SandboxMode::Full => "Current DSH file policy: danger-full-access. The DSH file sandbox does not \
             restrict file modifications by available operations."
            .to_owned(),
    }
}

/// Renders paths the way upstream's `JSON.stringify(roots)` does.
///
/// `["\/a","\/b"]` exactly, because the paragraph above is replayed and a formatting difference would
/// make two snapshots of the same policy compare unequal.
fn json_string_array<I, P>(paths: I) -> String
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let quoted: Vec<String> = paths
        .into_iter()
        .map(|path| {
            serde_json::Value::String(path.as_ref().to_string_lossy().into_owned()).to_string()
        })
        .collect();
    format!("[{}]", quoted.join(","))
}

// ---- escalation: the strictly-wider ladder ----------------------------------------------------

/// Every [`EscalationError`] code.
///
/// Upstream: `SANDBOX_ESCALATION_ERROR_CODES` in `packages/sandbox/sandbox/src/escalation.ts`, plus
/// [`Self::CancelledByCall`] — see its note on the one place the local vocabulary has to add a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalationErrorCode {
    /// The escalation fields are unpaired, the justification is blank, or the target is not strictly
    /// wider than the call's effective mode; the model corrects the request.
    InvalidArgument,
    /// The composition cannot ask for approval: no approver, no agent, or no approval channel.
    Unavailable,
    /// The user declined the escalation; the operation stays denied.
    Rejected,
    /// The approval request ended without a decision while the call itself was not cancelled.
    Cancelled,
    /// The approval ended *because* the call was cancelled.
    ///
    /// Upstream this is not an `EscalationError` at all: `approveEscalation` rethrows the abort signal's
    /// reason so the registry records the tool call as aborted rather than failed. Rust callers here have
    /// one error type, so the distinction is carried as a variant mapping to the same
    /// `SANDBOX_ESCALATION_CANCELLED` code — a caller that needs to re-raise its own cancellation asks
    /// [`EscalationError::is_call_cancellation`].
    CancelledByCall,
}

impl EscalationErrorCode {
    /// The stable code a consumer routes on, spelled as upstream spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "SANDBOX_ESCALATION_INVALID_ARGUMENT",
            Self::Unavailable => "SANDBOX_ESCALATION_UNAVAILABLE",
            Self::Rejected => "SANDBOX_ESCALATION_REJECTED",
            Self::Cancelled | Self::CancelledByCall => "SANDBOX_ESCALATION_CANCELLED",
        }
    }
}

/// A `sandbox_permissions` request that did not produce a grant; nothing executed.
///
/// Upstream: `SandboxEscalationError` in `packages/sandbox/sandbox/src/escalation.ts`. The message is the
/// model-facing explanation *and* the safe next step, which is why the texts below are pinned verbatim:
/// a model reads them to decide what to do next, so a paraphrase changes behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EscalationError {
    /// The failure class.
    pub code: EscalationErrorCode,
    /// The model-facing explanation and next step.
    pub message: String,
}

impl EscalationError {
    /// Builds the error for one failure class.
    fn new(code: EscalationErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Whether this error exists only because the tool call itself was cancelled.
    ///
    /// A caller that holds the call's cancellation token should re-raise its own cancellation instead of
    /// reporting a sandbox failure: the escalation did not fail, the turn stopped.
    #[must_use]
    pub fn is_call_cancellation(&self) -> bool {
        self.code == EscalationErrorCode::CancelledByCall
    }
}

impl std::fmt::Display for EscalationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EscalationError {}

/// The closed outcome vocabulary of one escalation ask.
///
/// Upstream: `EscalationOutcome` in `packages/sandbox/sandbox/src/escalation.ts`, structurally identical
/// to the approval seam's own outcome so an approval service's return value is assignable without the
/// sandbox importing the approval package. [`Approved`](Self::Approved) is deliberately *not* called
/// `Allowed` forever: it grants one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalationOutcome {
    /// Approved for this call only.
    AllowedOnce,
    /// The user declined.
    Rejected,
    /// The ask ended without a decision, and the call was not itself cancelled.
    Cancelled,
    /// No approval channel exists in this composition.
    Unavailable,
}

/// One escalation ask, as the approver receives it.
///
/// Upstream: the `request` parameter of `EscalationApprover.request` in
/// `packages/sandbox/sandbox/src/escalation.ts`. The `reason` is self-contained for the audit trail
/// (`approval/asked` stores it, and the target mode is part of the grant's identity), while
/// `display_reason` carries the localized sentence a person reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EscalationAsk {
    /// The tool name recorded on the approval request.
    pub tool_name: String,
    /// The tool-call id the prompt attaches to.
    pub call_id: String,
    /// The audit reason, verbatim.
    pub reason: String,
    /// The localized sentence presented to the person.
    pub display_reason: EscalationDisplayReason,
}

/// The seam an escalation asks through.
///
/// Upstream: `EscalationApprover` in `packages/sandbox/sandbox/src/escalation.ts` — a minimal structural
/// shape rather than the approval service type, so this module never depends on the approval or agent
/// packages. The tool layer (which owns the agent, the call id, and the tool name) closes over its own
/// approval request and hands the closure down; [`approve_escalation`] only judges it.
///
/// Returning an [`EscalationOutcome`] rather than a `Result` is upstream's shape: a denial is an answer,
/// not a failure, and the four outcomes call for four different responses.
pub trait EscalationApprover: Send + Sync {
    /// Asks a person to approve one action, resolving to a closed outcome.
    fn request(&self, ask: EscalationAsk) -> BoxFut<EscalationOutcome>;
}

/// Turns the turn's [`crate::interaction::approval::Approver`] into an escalation approver.
///
/// The two traits stay separate: a tool approval and a sandbox widening are different questions, and
/// this is the one place that asks the same person both of them.
#[derive(Clone)]
pub struct ApproverBridge {
    inner: std::sync::Arc<dyn crate::interaction::approval::Approver>,
}

impl std::fmt::Debug for ApproverBridge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApproverBridge")
            .finish_non_exhaustive()
    }
}

impl ApproverBridge {
    /// Wraps the person this turn already asks.
    #[must_use]
    pub fn new(inner: std::sync::Arc<dyn crate::interaction::approval::Approver>) -> Self {
        Self { inner }
    }
}

impl EscalationApprover for ApproverBridge {
    fn request(&self, ask: EscalationAsk) -> BoxFut<EscalationOutcome> {
        let inner = std::sync::Arc::clone(&self.inner);
        Box::pin(async move {
            let Some(request) = crate::interaction::approval::Request::new(
                ask.tool_name,
                ask.display_reason.for_locale("en"),
            ) else {
                return EscalationOutcome::Unavailable;
            };
            let request = request.with_detail(ask.reason);
            match inner.ask(request).await {
                Ok(decision) if decision.is_approved() => EscalationOutcome::AllowedOnce,
                Ok(_) => EscalationOutcome::Rejected,
                Err(crate::interaction::approval::ApprovalError::NoApprover) => {
                    EscalationOutcome::Unavailable
                }
                Err(_) => EscalationOutcome::Cancelled,
            }
        })
    }
}

/// The approval ingredients an escalating tool hands [`approve_escalation`].
///
/// Upstream: `EscalationApproval` in `packages/sandbox/sandbox/src/escalation.ts`: the requester (or
/// `None` when none is composed), the calling agent (or `None` for an agent-less execution), and the
/// call's identity. The tool layer holds all of these; this module only judges them.
pub struct EscalationApproval<'a> {
    /// The approval requester, or `None` when no approval service is composed.
    pub approver: Option<&'a dyn EscalationApprover>,
    /// The calling agent's identity, or `None` for an agent-less execution, which fails closed.
    ///
    /// An escalation is routed *through* an agent so a person's decision is attributed to the
    /// conversation that asked. Without one there is nobody to ask on behalf of, and asking anyway would
    /// produce an approval no session owns.
    pub agent: Option<&'a str>,
    /// The tool-call id the approval prompt attaches to.
    pub call_id: &'a str,
    /// The tool name recorded on the approval request.
    pub tool_name: &'a str,
    /// The tool-execution cancellation the approval rides, when the caller has one.
    pub signal: Option<&'a girder_core::CancellationToken>,
}

/// One escalation request, as [`approve_escalation`] judges it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EscalationRequest {
    /// The requested target mode, pinned to [`ESCALATION_TARGETS`] by the tool schema when advertised.
    pub requested_mode: SandboxMode,
    /// The model's one-sentence reason, shown verbatim to the person inside the audit reason.
    pub justification: String,
    /// The call's effective mode. Repeating it needs no approval, so it is also the no-op case.
    pub effective_mode: SandboxMode,
    /// The family's noun for the escalated action in user-facing texts (`command` for bash,
    /// `operation` for a filesystem mutation).
    pub subject: String,
}

/// Validates the escalation argument pairing a tool schema cannot express.
///
/// Upstream: `validateEscalationArgs` in `packages/sandbox/sandbox/src/escalation.ts`. `sandbox_permissions`
/// and `justification` travel together — an approval prompt without a reason, or a reason driving
/// nothing, is a malformed ask — and the justification must be a non-empty sentence.
///
/// # Errors
///
/// Returns [`EscalationErrorCode::InvalidArgument`] with upstream's verbatim text for each violation.
pub fn validate_escalation_args(
    sandbox_permissions: Option<&str>,
    justification: Option<&str>,
) -> Result<(), EscalationError> {
    if sandbox_permissions.is_some() && justification.is_none() {
        return Err(EscalationError::new(
            EscalationErrorCode::InvalidArgument,
            "invalid escalation: sandbox_permissions requires a justification",
        ));
    }
    if justification.is_some() && sandbox_permissions.is_none() {
        return Err(EscalationError::new(
            EscalationErrorCode::InvalidArgument,
            "invalid escalation: justification is only valid together with sandbox_permissions",
        ));
    }
    if justification.is_some_and(|text| text.trim().is_empty()) {
        return Err(EscalationError::new(
            EscalationErrorCode::InvalidArgument,
            "invalid justification: expected a non-empty sentence",
        ));
    }
    Ok(())
}

/// The model-facing `sandbox_permissions` parameter description, which carries the escalation rules for
/// every enforcing family.
///
/// Upstream: `sandboxPermissionsDescription` in `packages/sandbox/sandbox/src/escalation.ts`. `subject` is
/// the family's noun for the denied action: `command` for the shell tools, `operation` for a filesystem
/// mutation.
#[must_use]
pub fn sandbox_permissions_description(subject: &str) -> String {
    format!(
        "The narrowest wider sandbox mode for a one-shot retry of the exact {subject} the sandbox just \
         denied; the retry asks the user for approval."
    )
}

/// The audit reason an escalation ask carries.
///
/// Upstream: the `reason` field of the request `approveEscalation` builds in
/// `packages/sandbox/sandbox/src/escalation.ts`.
#[must_use]
pub fn escalation_audit_reason(requested: SandboxMode, justification: &str) -> String {
    format!(
        "escalate sandbox to {}: {justification}",
        requested.as_str()
    )
}

/// One localized approval sentence, per locale.
///
/// Upstream: `escalationDisplayReason` in `packages/sandbox/sandbox/src/locales.ts`. Eight locales, each
/// interpolating the mode and the model's justification verbatim — a translated justification would put
/// words in the model's mouth on the one screen a person uses to decide whether to trust it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EscalationDisplayReason {
    /// English.
    pub en: String,
    /// Chinese.
    pub zh: String,
    /// Japanese.
    pub ja: String,
    /// Spanish.
    pub es: String,
    /// French.
    pub fr: String,
    /// German.
    pub de: String,
    /// Russian.
    pub ru: String,
    /// Portuguese.
    pub pt: String,
}

impl EscalationDisplayReason {
    /// The sentence for one locale tag, falling back to English.
    ///
    /// A fallback rather than an error: an unknown locale is a client that speaks something this build
    /// does not, and showing the English sentence is better than showing nothing on an approval prompt.
    #[must_use]
    pub fn for_locale(&self, locale: &str) -> &str {
        // A BCP-47 tag may carry a region (`zh-Hans`, `pt-BR`); the language subtag is what selects.
        let language = locale
            .split(['-', '_'])
            .next()
            .unwrap_or(locale)
            .to_ascii_lowercase();
        match language.as_str() {
            "zh" => &self.zh,
            "ja" => &self.ja,
            "es" => &self.es,
            "fr" => &self.fr,
            "de" => &self.de,
            "ru" => &self.ru,
            "pt" => &self.pt,
            _ => &self.en,
        }
    }
}

/// Builds the localized approval sentences for one escalation.
///
/// Upstream: `escalationDisplayReason` in `packages/sandbox/sandbox/src/locales.ts`, whose eight
/// dictionaries are checked against each other by `satisfies typeof en` — a key missing from one would
/// not compile. The Rust equivalent is this single constructor: every locale is filled here, so there is
/// no dictionary to forget.
#[must_use]
pub fn escalation_display_reason(
    mode: SandboxMode,
    justification: &str,
) -> EscalationDisplayReason {
    let mode = mode.as_str();
    EscalationDisplayReason {
        en: format!("Allow this operation with {mode} permissions: {justification}"),
        zh: format!("允许本次操作使用 {mode} 权限：{justification}"),
        ja: format!("{mode} 権限でこの操作を許可します：{justification}"),
        es: format!("Permitir esta operación con permisos {mode}: {justification}"),
        fr: format!("Autoriser cette opération avec les permissions {mode} : {justification}"),
        de: format!("Diesen Vorgang mit {mode}-Berechtigungen zulassen: {justification}"),
        ru: format!("Разрешить эту операцию с правами {mode}: {justification}"),
        pt: format!("Permitir esta operação com permissões {mode}: {justification}"),
    }
}

/// Resolves a sandbox permission request **before execution**.
///
/// Upstream: `approveEscalation` in `packages/sandbox/sandbox/src/escalation.ts`. Repeating the call's
/// effective mode returns it without approval. A strictly wider mode requires approval and applies only to
/// this call. Narrower or unsupported targets, a missing approver or agent for a widening, and every
/// non-grant outcome fail closed — *before* anything executes, which is the whole point of the ordering:
/// a request checked after the command ran would be an audit of a confession.
///
/// The checks are ordered so the cheapest and most correctable failure comes first: arguing about the
/// ladder needs no approval channel, and a reaped rejection must never be reported as "no approver".
///
/// # Errors
///
/// Returns [`EscalationError`] whose [`code`](EscalationError::code) distinguishes an invalid request, a
/// composition that cannot ask, a user's denial, and a cancellation.
pub async fn approve_escalation(
    request: &EscalationRequest,
    approval: &EscalationApproval<'_>,
) -> Result<SandboxMode, EscalationError> {
    let requested = request.requested_mode;
    let effective = request.effective_mode;
    if requested == effective {
        return Ok(effective);
    }
    // Strict widening is an EXECUTION check against the call's effective mode, deliberately not a schema
    // constraint: the enum is the closed target vocabulary, while the effective mode is per-call truth.
    if !is_strictly_wider(effective, requested) {
        return Err(EscalationError::new(
            EscalationErrorCode::InvalidArgument,
            format!(
                "sandbox escalation to \"{}\" is not strictly wider than this call's current \"{}\" mode",
                requested.as_str(),
                effective.as_str()
            ),
        ));
    }
    let Some(approver) = approval.approver else {
        return Err(EscalationError::new(
            EscalationErrorCode::Unavailable,
            format!(
                "sandbox escalation to \"{}\" requires approval, but no approval service is composed",
                requested.as_str()
            ),
        ));
    };
    if approval.agent.is_none() {
        return Err(EscalationError::new(
            EscalationErrorCode::Unavailable,
            format!(
                "sandbox escalation to \"{}\" requires approval, but the call has no agent to route it \
                 through",
                requested.as_str()
            ),
        ));
    }
    let outcome = approver
        .request(EscalationAsk {
            tool_name: approval.tool_name.to_owned(),
            call_id: approval.call_id.to_owned(),
            reason: escalation_audit_reason(requested, &request.justification),
            display_reason: escalation_display_reason(requested, &request.justification),
        })
        .await;
    match outcome {
        // The ladder above proved `requested` is strictly wider; the schema pinned it to the closed set.
        EscalationOutcome::AllowedOnce => Ok(requested),
        EscalationOutcome::Rejected => Err(EscalationError::new(
            EscalationErrorCode::Rejected,
            format!(
                "the user rejected escalating this {} to \"{}\"; it stays denied, so stop and explain \
                 instead of working around it",
                request.subject,
                requested.as_str()
            ),
        )),
        EscalationOutcome::Cancelled => {
            // A cancelled call keeps its own cancellation, so the registry records it as aborted rather
            // than as a sandbox failure.
            if approval
                .signal
                .is_some_and(girder_core::CancellationToken::is_cancelled)
            {
                return Err(EscalationError::new(
                    EscalationErrorCode::CancelledByCall,
                    "the tool call was cancelled while awaiting sandbox escalation approval",
                ));
            }
            Err(EscalationError::new(
                EscalationErrorCode::Cancelled,
                format!(
                    "approval for escalating to \"{}\" was cancelled",
                    requested.as_str()
                ),
            ))
        }
        EscalationOutcome::Unavailable => Err(EscalationError::new(
            EscalationErrorCode::Unavailable,
            format!(
                "sandbox escalation to \"{}\" requires approval, but no approval channel is available",
                requested.as_str()
            ),
        )),
    }
}

// ---- the runner: which argv confines, and what its stderr means --------------------------------

/// A confining backend this build can name.
///
/// Upstream: the `SelectedRunner['runner']` union in `packages/sandbox/sandbox-local/src/index.ts`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runner {
    /// Bubblewrap, the Linux chain's first rung.
    Bwrap,
    /// The Landlock launcher, the Linux chain's second rung.
    Landlock,
    /// `sandbox-exec` with an SBPL profile, darwin's sole rung.
    Seatbelt,
    /// The ACL restricted-token runner, Windows' sole rung.
    WindowsAcl,
}

impl Runner {
    /// The runner's name, as upstream spells it in configuration and diagnostics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bwrap => "bwrap",
            Self::Landlock => "landlock",
            Self::Seatbelt => "seatbelt",
            Self::WindowsAcl => "windows-acl",
        }
    }

    /// The executable that establishes confinement, where one name is enough to start it.
    ///
    /// `None` for [`Self::WindowsAcl`]: its invocation is a `node` process plus a runner entry, resolved
    /// against an installation (built `lib/runner.js`, else the TypeScript source through `tsx`), so a
    /// single program name would be a lie.
    #[must_use]
    pub fn program(self) -> Option<&'static str> {
        match self {
            Self::Bwrap => Some("bwrap"),
            Self::Landlock => Some("landlock-run"),
            Self::Seatbelt => Some("sandbox-exec"),
            Self::WindowsAcl => None,
        }
    }
}

/// The Linux launcher's reserved failure exit.
///
/// Upstream: `LAUNCHER_FAILURE_EXIT` from `@deepseek-ai/node-addon-system/landlock-run`, used as the
/// exit gate of the Landlock runner-failure rule.
pub const LANDLOCK_LAUNCHER_FAILURE_EXIT: i32 = 125;

/// The Windows ACL runner's reserved failure exit.
///
/// Upstream: `WINDOWS_ACL_RUNNER_FAILURE_EXIT` in `packages/sandbox/sandbox-local/src/index.ts`, distinct
/// from the Landlock launcher's 125.
pub const WINDOWS_ACL_RUNNER_FAILURE_EXIT: i32 = 127;

/// The runner chain for one platform, in preference order.
///
/// Upstream: `PLATFORM_CHAINS` in `packages/sandbox/sandbox-local/src/index.ts`. Selection is by platform
/// *first*, probes second: a chain is probed only when it has more than one candidate, because probing
/// arbitrates a choice and does not re-validate one that has no alternative. A platform with no chain
/// fails closed at [`confine`].
///
/// Linux prefers `bwrap` (its mount profile is closest to the mode vocabulary) over the Landlock
/// launcher; darwin and Windows each have exactly one candidate.
#[must_use]
pub fn platform_chain(os: &str) -> &'static [Runner] {
    match os {
        "linux" => &[Runner::Bwrap, Runner::Landlock],
        "darwin" => &[Runner::Seatbelt],
        "win32" => &[Runner::WindowsAcl],
        _ => &[],
    }
}

/// The enforcement completeness a rung claims when selected **without** a probe.
///
/// Upstream: `STATIC_ENFORCEMENT` in `packages/sandbox/sandbox-local/src/index.ts`. `bwrap` and Seatbelt
/// govern every promised file effect by construction, so their claims are profile facts. The Windows
/// rung reports `partial`: NTFS hard links still alias a granted workspace file to a path outside it,
/// reads stay unconfined, and a tree another `AppContainer` tool has protected is unreadable to the child —
/// so the backend must not advertise the absolute promise.
#[must_use]
pub fn static_enforcement(runner: Runner) -> SandboxEnforcement {
    match runner {
        Runner::Bwrap | Runner::Landlock | Runner::Seatbelt => SandboxEnforcement::Full,
        Runner::WindowsAcl => SandboxEnforcement::Partial,
    }
}

/// The denial dialect a runner's kernel speaks.
///
/// Upstream: `DENIAL_SIGNATURES` in `packages/sandbox/sandbox-local/src/index.ts` — the case-insensitive
/// stderr substrings a *denied file effect* produces under that backend: `EROFS` text under bwrap's
/// read-only binds, `EACCES` under Landlock, `EPERM` under Seatbelt. A consumer that inferred denials from
/// a failed run must match against exactly one backend's list rather than a cross-backend union: the union
/// claims denials a given backend never produces, which turns a program's own "permission denied" output
/// into a fabricated policy decision.
#[must_use]
pub fn denial_signatures(runner: Runner) -> &'static [&'static str] {
    match runner {
        Runner::Bwrap => &["read-only file system"],
        Runner::Landlock => &["permission denied"],
        Runner::Seatbelt => &["operation not permitted"],
        // pwsh/.NET: "Access to the path '...' is denied."; cmd: "Access is denied."; Node EACCES:
        // "permission denied"; EPERM: "operation not permitted".
        Runner::WindowsAcl => &[
            "access is denied",
            "access to the path",
            "permission denied",
            "operation not permitted",
        ],
    }
}

/// The denial dialect of an operator-supplied runner.
///
/// Upstream: the `runnerCommand` entry of `DENIAL_SIGNATURES` in
/// `packages/sandbox/sandbox-local/src/index.ts`. A configured runner asserts full enforcement and speaks
/// the intersection the operator can rely on, since this build cannot know which backend it wraps.
#[must_use]
pub fn configured_runner_denial_signatures() -> &'static [&'static str] {
    &["read-only file system", "permission denied"]
}

/// The structured runner-failure rules a backend publishes.
///
/// Upstream: `RUNNER_FAILURE_RULES` in `packages/sandbox/sandbox-local/src/index.ts`. A *runner failure*
/// means the command never ran, which is the opposite conclusion from a denial, so each rule is as narrow
/// as its backend's contract allows:
///
/// - bwrap's fatal paths exit 1 but its public contract does not reserve that status, so the rule is
///   signature-only.
/// - The Landlock launcher reserves exit 125 plus a fatal line, and self-reports partial enforcement on
///   stderr at every confined run — that line is excluded so a working run is never read as a failure.
/// - `sandbox-exec` publishes no launcher-failure status, so the rule is signature-only.
/// - The Windows ACL runner prints `windows-acl-run: <detail>` and exits 127; the rule is exit-gated so a
///   confined command that merely *prints* the signature is not misclassified as "did not run".
#[must_use]
pub fn runner_failure_rules(runner: Runner) -> Vec<RunnerFailureRule> {
    match runner {
        Runner::Bwrap => vec![RunnerFailureRule {
            fatal_signatures: vec!["bwrap: ".to_owned()],
            ..RunnerFailureRule::default()
        }],
        Runner::Landlock => vec![RunnerFailureRule {
            allowed_exit_codes: Some(vec![LANDLOCK_LAUNCHER_FAILURE_EXIT]),
            fatal_signatures: vec!["landlock-run: ".to_owned()],
            informational_lines: vec![
                "landlock-run: partial enforcement (older Landlock ABI)".to_owned(),
            ],
        }],
        Runner::Seatbelt => vec![RunnerFailureRule {
            fatal_signatures: vec!["sandbox-exec: ".to_owned()],
            ..RunnerFailureRule::default()
        }],
        Runner::WindowsAcl => vec![RunnerFailureRule {
            allowed_exit_codes: Some(vec![WINDOWS_ACL_RUNNER_FAILURE_EXIT]),
            fatal_signatures: vec!["windows-acl-run: ".to_owned()],
            ..RunnerFailureRule::default()
        }],
    }
}

/// Validates an operator-supplied runner configuration.
///
/// Upstream: the constructor checks of `LocalSandboxProvider` in
/// `packages/sandbox/sandbox-local/src/index.ts`. The pairing is required in both directions: a configured
/// runner without failure signatures cannot be classified when it refuses its profile (its exit status
/// would be reported as a command failure), and signatures without a runner describe nothing.
///
/// # Errors
///
/// Returns the message upstream throws, as a `&'static str` so a caller can quote it.
pub fn validate_runner_config(
    runner_command: &[String],
    failure_signatures: &[String],
) -> Result<(), &'static str> {
    if runner_command.is_empty() && !failure_signatures.is_empty() {
        return Err("sandbox-local: runnerFailureSignatures requires runnerCommand");
    }
    if !runner_command.is_empty() && failure_signatures.is_empty() {
        return Err(
            "sandbox-local: runnerCommand requires at least one runnerFailureSignatures entry",
        );
    }
    if failure_signatures
        .iter()
        .any(|signature| signature.trim().is_empty() || signature.contains(['\r', '\n']))
    {
        return Err(
            "sandbox-local: runnerFailureSignatures entries must be non-empty single-line strings",
        );
    }
    Ok(())
}

/// The bwrap profile arguments for one policy.
///
/// Upstream: `bwrapProfileArgs` in `packages/sandbox/sandbox-local/src/profiles.ts`. A read-only bind of
/// `/` plus the mode's writable roots as `--bind`: the profile *is* the mode, expressed as mounts.
///
/// `workspace-write` adds `--tmpfs /tmp` — a fresh mount rather than a bind of the host's temp tree, so
/// writes there land in an ephemeral filesystem that disappears with the process, which is why this
/// dialect does not grant the platform temp roots by path.
///
/// # Errors
///
/// Returns [`SandboxRootError`] from [`workspace_writable_roots`], re-validated on every call.
pub fn bwrap_profile_args(policy: &ExecutionPolicy) -> Result<Vec<String>, SandboxRootError> {
    let mut args: Vec<String> = vec![
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--unshare-pid",
        "--proc",
        "/proc",
        "--die-with-parent",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if policy.mode == SandboxMode::WorkspaceWrite {
        args.extend(["--tmpfs", "/tmp"].into_iter().map(str::to_owned));
        for root in workspace_writable_roots(policy)? {
            let root = root.to_string_lossy().into_owned();
            args.push("--bind".to_owned());
            args.push(root.clone());
            args.push(root);
        }
    }
    Ok(args)
}

/// The Landlock launcher's allow-list grants for one policy.
///
/// Upstream: `landlockProfileArgs` in `packages/sandbox/sandbox-local/src/profiles.ts`, which delegates the
/// flag spelling to `grantArgs` in the native `landlock-run` addon. This type is that call's *input*: the
/// read-only and read-write lists, canonical and deduplicated, which is the part the policy owns and the
/// part a test can pin. The argv spelling stays behind [`confine`]'s refusal, because a launcher flag
/// guessed here would either fail every run or — worse — be parsed as a different grant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LandlockGrants {
    /// Paths mounted read-only. Always the whole filesystem: the mode is expressed as an exception.
    pub read_only: Vec<String>,
    /// Paths whose writes are permitted.
    pub read_write: Vec<String>,
}

/// The Landlock grants for one policy.
///
/// Upstream: `landlockProfileArgs` in `packages/sandbox/sandbox-local/src/profiles.ts`. `/dev/null` is
/// granted even under `read-only`, because a denied sink breaks ordinary commands for a reason that is not
/// the user's policy; `workspace-write` adds the host's `/tmp` (a real directory here, unlike bwrap's
/// ephemeral mount) and the workspace-owned roots.
///
/// # Errors
///
/// Returns [`SandboxRootError`] from [`workspace_writable_roots`].
pub fn landlock_grants(policy: &ExecutionPolicy) -> Result<LandlockGrants, SandboxRootError> {
    let mut read_write = vec!["/dev/null".to_owned()];
    if policy.mode == SandboxMode::WorkspaceWrite {
        read_write.push("/tmp".to_owned());
        for root in workspace_writable_roots(policy)? {
            read_write.push(root.to_string_lossy().into_owned());
        }
    }
    Ok(LandlockGrants {
        read_only: vec!["/".to_owned()],
        read_write,
    })
}

/// Quotes one path as an SBPL string literal.
///
/// Upstream: the private `sbplString` in `packages/sandbox/sandbox-local/src/profiles.ts`. A backslash and
/// a double quote are the two characters that could end the literal early; escaping only those is exactly
/// what upstream does, and escaping more would change the path's meaning.
fn sbpl_string(path: &Path) -> String {
    let spelled = path.to_string_lossy();
    let escaped = spelled.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// The `sandbox-exec` arguments — including the SBPL profile — for one policy.
///
/// Upstream: `seatbeltProfileArgs` in `packages/sandbox/sandbox-local/src/profiles.ts`. The profile denies
/// every write, then re-allows `/dev/null` by exact path and the mode's writable roots by subtree. The
/// roots come from [`writable_roots`] — the shared derivation, not a local copy — so the Seatbelt grant and
/// the in-process filesystem fence cannot drift into "the write tool cannot write `/tmp` but bash can".
///
/// # Errors
///
/// Returns [`SandboxRootError`] from [`writable_roots`].
pub fn seatbelt_profile_args(policy: &ExecutionPolicy) -> Result<Vec<String>, SandboxRootError> {
    let mut forms = vec![
        "(version 1)".to_owned(),
        "(allow default)".to_owned(),
        "(deny file-write*)".to_owned(),
        format!(
            "(allow file-write* (literal {}))",
            sbpl_string(Path::new("/dev/null"))
        ),
    ];
    let roots = writable_roots(policy)?;
    if !roots.is_empty() {
        let subtrees: Vec<String> = roots
            .iter()
            .map(|root| format!("(subpath {})", sbpl_string(root)))
            .collect();
        forms.push(format!("(allow file-write* {})", subtrees.join(" ")));
    }
    Ok(vec!["-p".to_owned(), forms.join(" ")])
}

/// A runner selected for this host, with the enforcement it achieves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectedRunner {
    /// Which backend confines.
    pub runner: Runner,
    /// How completely that backend enforces the policy's file effects.
    pub enforcement: SandboxEnforcement,
}

/// A confined execution: the argv to spawn instead of the caller's, plus the facts a consumer classifies
/// with.
///
/// Upstream: `ConfinedArgv` in `packages/sandbox/sandbox/src/index.ts`. Everything a consumer needs to
/// interpret a failed run travels with the wrap, because the wrap is the only thing that knows which
/// dialect was in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfinedArgv {
    /// The wrapped argv: runner, profile, separator, then the caller's argv.
    pub argv: Vec<String>,
    /// How completely the selected backend enforces the policy's file effects.
    pub enforcement: SandboxEnforcement,
    /// The selected backend's denial dialect, for inferring a denial from stderr.
    pub denial_signatures: Vec<String>,
    /// The selected backend's structured runner-failure rules.
    pub runner_failure_rules: Vec<RunnerFailureRule>,
}

/// Selects the runner that confines commands, once, for the provider's lifetime.
///
/// Upstream: `chainVerdict` in `packages/sandbox/sandbox-local/src/index.ts`. A sole candidate is selected
/// **without** a probe — its execution-time refusal is still the fail-closed end, and probing would spend a
/// process on a choice that has no alternative. Several candidates are probed in chain order and the first
/// usable one wins.
///
/// `probe` is the functional probe: it runs the candidate's own trivial confined command and reports the
/// enforcement it achieved, or `None` when the rung is unusable. Injecting it keeps the decision pure and
/// testable, and keeps process spawning out of this module.
///
/// `None` means this host has no usable rung, which [`confine`] turns into the fail-closed
/// `SANDBOX_UNAVAILABLE` refusal.
pub fn select_runner<F>(chain: &[Runner], mut probe: F) -> Option<SelectedRunner>
where
    F: FnMut(Runner) -> Option<SandboxEnforcement>,
{
    let first = *chain.first()?;
    if chain.len() == 1 {
        // A sole candidate needs no arbitration; its refusal at execution time still fails closed.
        return Some(SelectedRunner {
            runner: first,
            enforcement: static_enforcement(first),
        });
    }
    chain.iter().find_map(|runner| {
        probe(*runner).map(|enforcement| SelectedRunner {
            runner: *runner,
            enforcement,
        })
    })
}

/// Why a command could not be confined.
///
/// Upstream: `SandboxUnavailableError` (`SANDBOX_UNAVAILABLE`) and `SandboxPreparationError` in
/// `packages/sandbox/sandbox/src/index.ts`, plus the root errors the profiles re-raise. Nothing here runs a
/// command unconfined: every variant is a refusal, which is the one property the seam must never trade
/// away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfineError {
    /// The mode was requested but no backend on this host is usable.
    Unavailable {
        /// The mode that could not be enforced.
        mode: SandboxMode,
    },
    /// A root the policy names cannot be enforced.
    Roots(SandboxRootError),
    /// The selected backend is usable but this module cannot spell its invocation safely.
    Unsupported {
        /// The backend that refused to be wrapped.
        runner: Runner,
        /// Why, in one sentence, for the operator.
        detail: &'static str,
    },
}

impl ConfineError {
    /// The stable code a consumer routes on.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "SANDBOX_UNAVAILABLE",
            Self::Roots(error) => error.code(),
            Self::Unsupported { .. } => "SANDBOX_PREPARATION_FAILED",
        }
    }
}

impl std::fmt::Display for ConfineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { mode } => f.write_str(&unavailable_message(*mode)),
            Self::Roots(error) => write!(f, "{error}"),
            Self::Unsupported { runner, detail } => {
                write!(f, "sandbox runner \"{}\": {detail}", runner.as_str())
            }
        }
    }
}

impl std::error::Error for ConfineError {}

impl From<SandboxRootError> for ConfineError {
    fn from(error: SandboxRootError) -> Self {
        Self::Roots(error)
    }
}

/// The fail-closed refusal text for a mode with no usable backend.
///
/// Upstream: the `SandboxUnavailableError` message in `packages/sandbox/sandbox/src/index.ts`, verbatim. It
/// names the remedy per platform and the only sanctioned alternative, because a caller that reads only
/// "refusing to run" would otherwise try to work around the sandbox — the one response the message must
/// pre-empt.
#[must_use]
pub fn unavailable_message(mode: SandboxMode) -> String {
    format!(
        "sandbox mode \"{}\" is requested but no sandbox backend is usable on this host; refusing to run \
         the command unconfined. Install bubblewrap or run a Landlock-enforcing kernel (Linux), ensure \
         sandbox-exec is usable (macOS), or ensure the ACL restricted-token runner can start (Windows) — \
         otherwise switch the consumer to danger-full-access.",
        mode.as_str()
    )
}

/// Wraps `argv` so it executes confined under `policy`.
///
/// Upstream: `SandboxProvider.confine` (`LocalSandboxProvider.confine`) in
/// `packages/sandbox/sandbox-local/src/index.ts`. The caller spawns the returned argv *in place of* its
/// own; `argv` is the exact argv it was about to spawn (program plus arguments), never a shell string.
///
/// The root is re-resolved on every call: `--bind` over a directory that has since been replaced by a
/// symlink would mount something the operator never granted, so the strict derivation runs here rather
/// than at configuration time.
///
/// # Errors
///
/// Returns [`ConfineError::Unavailable`] when this host has no usable rung,
/// [`ConfineError::Roots`] when a granted root cannot be enforced, and [`ConfineError::Unsupported`] for a
/// backend whose invocation this module will not guess (see [`Runner::Landlock`] and
/// [`Runner::WindowsAcl`]).
pub fn confine<F>(
    argv: &[String],
    policy: &ExecutionPolicy,
    chain: &[Runner],
    probe: F,
) -> Result<ConfinedArgv, ConfineError>
where
    F: FnMut(Runner) -> Option<SandboxEnforcement>,
{
    let selected =
        select_runner(chain, probe).ok_or(ConfineError::Unavailable { mode: policy.mode })?;
    let (prefix, enforcement) = runner_prefix(selected, policy)?;
    let mut wrapped = prefix;
    wrapped.push("--".to_owned());
    wrapped.extend(argv.iter().cloned());
    Ok(ConfinedArgv {
        argv: wrapped,
        enforcement,
        denial_signatures: denial_signatures(selected.runner)
            .iter()
            .map(|signature| (*signature).to_owned())
            .collect(),
        runner_failure_rules: runner_failure_rules(selected.runner),
    })
}

/// The runner invocation prefix (program, profile, separator excluded) for one policy.
///
/// Upstream: the private `runnerArgv` in `packages/sandbox/sandbox-local/src/index.ts`.
fn runner_prefix(
    selected: SelectedRunner,
    policy: &ExecutionPolicy,
) -> Result<(Vec<String>, SandboxEnforcement), ConfineError> {
    match selected.runner {
        Runner::Bwrap => {
            let mut argv = vec!["bwrap".to_owned()];
            argv.extend(bwrap_profile_args(policy)?);
            Ok((argv, SandboxEnforcement::Full))
        }
        Runner::Seatbelt => {
            let mut argv = vec!["sandbox-exec".to_owned()];
            argv.extend(seatbelt_profile_args(policy)?);
            Ok((argv, SandboxEnforcement::Full))
        }
        // Both refusals are honest rather than lazy. The Landlock launcher's grant flags belong to the
        // native addon (`grantArgs`), and the ACL runner's argv carries SIDs materialized by a
        // preparation step this module does not own. Emitting a guess would either fail every run or, if
        // the launcher parsed it as a different grant, confine the wrong tree — so the caller is told to
        // use a backend whose spelling is known.
        Runner::Landlock => Err(ConfineError::Unsupported {
            runner: Runner::Landlock,
            detail: "the launcher's grant flags are owned by the native landlock-run addon; \
                     use bwrap, or pass the grants from `landlock_grants` to the launcher directly",
        }),
        Runner::WindowsAcl => Err(ConfineError::Unsupported {
            runner: Runner::WindowsAcl,
            detail: "the restricted-token runner needs workspace and private-temp SIDs materialized \
                     before it can be spawned; use a backend whose preparation this build performs",
        }),
    }
}
