//! Tests for the sandbox policy seam: what a mode means, how it is resolved per call, the escalation
//! ladder and its approval choreography, and the runner facts a confined spawn carries.
//!
//! The properties pinned here are the ones the upstream suites pin, transcribed from
//! `packages/sandbox/sandbox/tests/roots.spec.ts`, `packages/sandbox/sandbox/tests/escalation.spec.ts`,
//! and the policy/local-provider suites:
//!
//! - **One derivation of a mode's meaning.** `read-only` grants nothing; `workspace-write` grants the
//!   workspace, its extra roots, and the platform temp areas, canonical and deduplicated; a symlinked
//!   extra root is refused rather than silently re-pointed.
//! - **Resolution order.** An approved override outranks the session's `sandbox/mode` fold, which
//!   outranks the deployment default, and the session's cwd *is* the workspace boundary.
//! - **The strictly-wider ladder.** Repeating a mode is free; a narrower or unknown target fails closed
//!   before anything asks; and every non-grant outcome has its own verbatim text and code.
//! - **Verbosity of the model-facing texts.** The denial marker is in `shell_gap.rs`; what is here is the
//!   policy paragraph and the escalation description, which a model reads to decide what to do next.
//! - **Fail closed.** A host with no usable runner refuses; no `confine` path returns the caller's argv
//!   unwrapped.
//!
//! Included by path rather than through the crate's re-exports, so these tests compile against the
//! module as written even before the crate root wires it in. That is why the module under test reaches
//! every crate item through `girder_core::` rather than `crate::`: under `#[path]` inclusion, `crate::`
//! names *this test binary*, which has no such item.

#[path = "../src/sandbox/policy.rs"]
pub mod sandbox_gap;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use girder_core::{BoxFut, CancellationToken, SandboxEnforcement, SandboxMode, SessionEvent};
use sandbox_gap::{
    ConfineError, ESCALATION_TARGETS, EscalationApproval, EscalationApprover, EscalationAsk,
    EscalationErrorCode, EscalationOutcome, EscalationRequest, ExecutionPolicy,
    LANDLOCK_LAUNCHER_FAILURE_EXIT, PolicyRequest, PolicyResolver, Runner, SANDBOX_MODES,
    WINDOWS_ACL_RUNNER_FAILURE_EXIT, approve_escalation, assert_temp_root_outside_workspace,
    bwrap_profile_args, canonical_path, configured_runner_denial_signatures, confine,
    denial_signatures, escalation_audit_reason, escalation_display_reason, host_temp_root,
    is_execution_world_path, is_strictly_wider, landlock_grants, parse_mode,
    permissions_for_session, platform_chain, render_policy_context, runner_failure_rules,
    sandbox_permissions_description, seatbelt_profile_args, select_runner, session_override,
    set_sandbox_mode, static_enforcement, temp_root_inside_workspace, unavailable_message,
    validate_escalation_args, validate_runner_config, wider_modes, workspace_writable_roots,
    writable_roots,
};

// The policy source is included directly for focused tests, so provide the library namespace it
// uses for the approval bridge.
mod interaction {
    pub use girder_core::interaction::approval;
}

// ---- helpers ------------------------------------------------------------------------------------

/// Creates a fresh temp directory this test owns.
///
/// Hand-rolled rather than dev-dependency-driven: the crate's dev-dependencies are only `nix` and `tokio`,
/// and one `create_dir_all` over the process's temp dir is all this needs. The pid and a counter keep two
/// concurrent test binaries from colliding.
fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-sandbox-gap-{tag}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("the temp directory can be created");
    // Canonical, so a comparison against the module's canonicalisation is about the policy rather than
    // about `/tmp` being a symlink on this host.
    canonical_path(&dir)
}

/// A `workspace-write` policy rooted at `root`.
fn workspace_policy(root: &Path) -> ExecutionPolicy {
    ExecutionPolicy {
        mode: SandboxMode::WorkspaceWrite,
        workspace_root: root.to_path_buf(),
        additional_writable_roots: None,
        session_id: None,
    }
}

// ---- the mode vocabulary ------------------------------------------------------------------------

#[test]
fn the_mode_vocabulary_is_closed_and_ordered() {
    // Ordered narrowest to widest, which is the order a picker renders and the order the ladder follows.
    assert_eq!(
        SANDBOX_MODES,
        [
            SandboxMode::ReadOnly,
            SandboxMode::WorkspaceWrite,
            SandboxMode::Full
        ]
    );
    // `read-only` is the floor, so nothing escalates to it.
    assert_eq!(
        ESCALATION_TARGETS,
        [SandboxMode::WorkspaceWrite, SandboxMode::Full]
    );
}

#[test]
fn every_mode_round_trips_through_its_wire_name() {
    for mode in SANDBOX_MODES {
        assert_eq!(parse_mode(mode.as_str()), Some(mode));
    }
}

#[test]
fn an_unknown_mode_name_is_not_a_mode() {
    // A UI control, a config file, or a log from a newer build can all spell something else. The parse
    // answers `None` rather than a guess, and the caller decides what that means.
    for name in [
        "",
        "readonly",
        "read only",
        "workspace_write",
        "Danger-Full-Access",
        "full-access",
    ] {
        assert_eq!(parse_mode(name), None, "{name:?} must not parse");
    }
}

#[test]
fn the_ladder_is_strictly_wider_and_totally_ordered() {
    assert_eq!(
        wider_modes(SandboxMode::ReadOnly),
        [SandboxMode::WorkspaceWrite, SandboxMode::Full]
    );
    assert_eq!(
        wider_modes(SandboxMode::WorkspaceWrite),
        [SandboxMode::Full]
    );
    // Nothing is above full access; the empty list is what makes a repeated request a no-op rather than
    // an escalation.
    assert!(wider_modes(SandboxMode::Full).is_empty());

    assert!(is_strictly_wider(
        SandboxMode::ReadOnly,
        SandboxMode::WorkspaceWrite
    ));
    assert!(is_strictly_wider(SandboxMode::ReadOnly, SandboxMode::Full));
    assert!(is_strictly_wider(
        SandboxMode::WorkspaceWrite,
        SandboxMode::Full
    ));
    // Equality is not widening.
    for mode in SANDBOX_MODES {
        assert!(!is_strictly_wider(mode, mode));
    }
    // And nothing goes back down.
    assert!(!is_strictly_wider(
        SandboxMode::WorkspaceWrite,
        SandboxMode::ReadOnly
    ));
    assert!(!is_strictly_wider(
        SandboxMode::Full,
        SandboxMode::WorkspaceWrite
    ));
    assert!(!is_strictly_wider(SandboxMode::Full, SandboxMode::ReadOnly));
}

// ---- roots: what a mode grants ------------------------------------------------------------------

#[test]
fn an_execution_world_path_is_absolute_and_nul_free() {
    assert!(is_execution_world_path("/work"));
    assert!(!is_execution_world_path("work"));
    assert!(!is_execution_world_path(""));
    // A NUL cannot be a filesystem name, so a "grant" containing one would be silently shortened by
    // whatever C API eventually received it.
    assert!(!is_execution_world_path("/work\0/escape"));
}

#[test]
fn canonical_path_resolves_a_symlink_and_keeps_a_missing_spelling() {
    let root = temp_dir("canonical");
    let target = root.join("real");
    std::fs::create_dir_all(&target).expect("the target directory can be created");
    let link = root.join("link");
    symlink_dir(&target, &link);

    assert_eq!(canonical_path(&link), target);

    // A path that cannot be resolved is returned as spelled rather than invented: a missing root matches
    // nothing until it exists, and a fallback would grant a path the caller never named.
    let missing = root.join("does-not-exist");
    assert_eq!(canonical_path(&missing), missing);
}

/// Creates a directory symlink, or skips the caller when the host refuses.
///
/// Windows needs a privilege for directory symlinks; a test that failed there would be reporting the
/// host's configuration rather than the module's behaviour.
fn symlink_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).expect("a directory symlink can be created");
    }
    #[cfg(windows)]
    {
        let _ = (target, link);
        eprintln!("skipping: directory symlinks need a privilege this host may not grant");
    }
}

#[test]
fn read_only_grants_nothing_even_when_extra_roots_are_carried() {
    let root = temp_dir("ro-extra");
    let extra = temp_dir("ro-extra-dir");
    let policy = ExecutionPolicy {
        mode: SandboxMode::ReadOnly,
        workspace_root: root.clone(),
        additional_writable_roots: Some(vec![extra]),
        session_id: None,
    };
    // A grant list is meaningless when the mode permits no writes; returning the roots anyway would let a
    // careless backend pass them on.
    assert_eq!(
        workspace_writable_roots(&policy).expect("read-only cannot fail"),
        Vec::<PathBuf>::new()
    );
    assert_eq!(
        writable_roots(&policy).expect("read-only cannot fail"),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn full_access_grants_nothing_because_nothing_is_restricted() {
    let root = temp_dir("full");
    let policy = ExecutionPolicy {
        mode: SandboxMode::Full,
        workspace_root: root,
        additional_writable_roots: None,
        session_id: None,
    };
    // `danger-full-access` bypasses confinement rather than granting every root: an allow-list here would
    // be a policy that pretends to restrict.
    assert!(
        workspace_writable_roots(&policy)
            .expect("full access cannot fail")
            .is_empty()
    );
    assert!(
        writable_roots(&policy)
            .expect("full access cannot fail")
            .is_empty()
    );
}

#[test]
fn workspace_write_grants_the_workspace_canonical_and_deduplicated() {
    let root = temp_dir("ws");
    let policy = workspace_policy(&root);
    let roots = workspace_writable_roots(&policy).expect("a plain workspace is grantable");
    assert_eq!(roots, vec![root]);
}

#[test]
fn workspace_write_keeps_a_physical_extra_root_and_drops_its_duplicate() {
    let root = temp_dir("ws-extra");
    let extra = temp_dir("ws-extra-dir");
    let policy = ExecutionPolicy {
        mode: SandboxMode::WorkspaceWrite,
        workspace_root: root.clone(),
        additional_writable_roots: Some(vec![extra.clone(), extra.clone()]),
        session_id: None,
    };
    assert_eq!(
        workspace_writable_roots(&policy).expect("physical roots are grantable"),
        vec![root, extra]
    );
}

#[test]
fn workspace_write_refuses_a_symlinked_extra_root() {
    let root = temp_dir("ws-alias");
    let physical = temp_dir("ws-alias-target");
    let alias = root.join("alias");
    symlink_dir(&physical, &alias);
    if !alias.exists() {
        eprintln!("skipping: this host cannot create the symlink the case needs");
        return;
    }
    let policy = ExecutionPolicy {
        mode: SandboxMode::WorkspaceWrite,
        workspace_root: root,
        additional_writable_roots: Some(vec![alias]),
        session_id: None,
    };
    let error = workspace_writable_roots(&policy).expect_err("a symlinked grant is refused");
    assert_eq!(error.code(), "SANDBOX_INVALID_ROOT");
    assert_eq!(error.subject, "additional writable directory");
    assert!(
        error.detail.contains("use the physical directory path"),
        "the error must name the remedy, got: {}",
        error.detail
    );
}

#[test]
fn workspace_write_refuses_a_missing_or_non_directory_extra_root() {
    let root = temp_dir("ws-bad");
    let missing = root.join("not-there");
    let file = root.join("file.txt");
    std::fs::write(&file, b"x").expect("the file can be written");

    for bad in [missing, file] {
        let policy = ExecutionPolicy {
            mode: SandboxMode::WorkspaceWrite,
            workspace_root: root.clone(),
            additional_writable_roots: Some(vec![bad]),
            session_id: None,
        };
        let error = workspace_writable_roots(&policy).expect_err("an unusable root is refused");
        assert_eq!(error.code(), "SANDBOX_INVALID_ROOT");
    }
}

#[test]
fn write_roots_add_the_platform_temp_areas() {
    let root = temp_dir("ws-temp");
    let writable =
        writable_roots(&workspace_policy(&root)).expect("a plain workspace is grantable");

    assert!(writable.contains(&root), "the workspace itself is writable");
    // `os.tmpdir()` is the real temp area for mkstemp-family tools; omitting it would deny what the mode
    // promises.
    assert!(
        writable.contains(&canonical_path(&std::env::temp_dir())),
        "the platform temp dir must be writable, got {writable:?}"
    );
    if let Some(tmp) = host_temp_root() {
        assert!(
            writable.contains(&canonical_path(tmp)),
            "/tmp must be writable"
        );
    }
    // Deduplicated after canonicalisation: `/tmp` and `TMPDIR=/tmp` coincide on most hosts.
    let unique: std::collections::BTreeSet<&PathBuf> = writable.iter().collect();
    assert_eq!(
        unique.len(),
        writable.len(),
        "writable roots must not repeat: {writable:?}"
    );
}

#[test]
fn the_workspace_grant_precedes_the_temp_areas() {
    // Order is a fact worth keeping: the Seatbelt profile lists the workspace first, which is what makes a
    // diagnostic readable.
    let root = temp_dir("ws-order");
    let writable =
        writable_roots(&workspace_policy(&root)).expect("a plain workspace is grantable");
    assert_eq!(writable.first(), Some(&root));
}

#[test]
fn a_temp_root_inside_the_workspace_is_detected() {
    let root = temp_dir("temp-inside");
    let nested = root.join("tmp");
    std::fs::create_dir_all(&nested).expect("the nested temp dir can be created");

    assert!(temp_root_inside_workspace(&root, &nested).expect("both paths resolve"));
    assert!(temp_root_inside_workspace(&root, &root).expect("both paths resolve"));
}

#[test]
fn a_sibling_temp_root_is_outside_the_workspace_even_with_a_shared_prefix() {
    let parent = temp_dir("temp-outside");
    let work = parent.join("work");
    let work2 = parent.join("work2");
    std::fs::create_dir_all(&work).expect("the workspace can be created");
    std::fs::create_dir_all(&work2).expect("the sibling can be created");

    // Containment is per path segment, so `/work2` is not inside `/work`.
    assert!(!temp_root_inside_workspace(&work, &work2).expect("both paths resolve"));
    assert!(assert_temp_root_outside_workspace(&work, &work2).is_ok());
}

#[test]
fn a_temp_root_inside_the_workspace_is_a_preparation_failure() {
    let root = temp_dir("temp-refuse");
    let nested = root.join("tmp");
    std::fs::create_dir_all(&nested).expect("the nested temp dir can be created");

    let error = assert_temp_root_outside_workspace(&root, &nested)
        .expect_err("a temp root inside the workspace must be refused");
    assert_eq!(error.code(), "SANDBOX_INVALID_ROOT");
    // The message says *why*: the private temp directory would inherit the standing workspace grant.
    assert!(
        error.detail.contains("inherit the workspace grant"),
        "the refusal must explain the inheritance, got: {}",
        error.detail
    );
}

#[test]
fn a_relative_root_is_refused_rather_than_measured() {
    let root = temp_dir("temp-relative");
    let error = temp_root_inside_workspace(&root, Path::new("tmp"))
        .expect_err("a relative temp root cannot be a grant");
    assert_eq!(error.code(), "SANDBOX_INVALID_ROOT");
    let error = temp_root_inside_workspace(Path::new("work"), &root)
        .expect_err("a relative workspace cannot be a boundary");
    assert_eq!(error.code(), "SANDBOX_INVALID_ROOT");
}

#[test]
fn the_root_error_renders_upstreams_message_shape() {
    let error =
        sandbox_gap::SandboxRootError::new("/a", "sandbox workspace root", "expected a directory");
    assert_eq!(
        error.to_string(),
        "Invalid sandbox workspace root \"/a\": expected a directory"
    );
}

// ---- per-call policy resolution ----------------------------------------------------------------

#[test]
fn the_resolver_validates_the_configured_root() {
    let error = PolicyResolver::new(SandboxMode::ReadOnly, "relative/path")
        .expect_err("a relative root must be refused");
    assert_eq!(error.code(), "SANDBOX_INVALID_ROOT");
    assert_eq!(error.subject, "sandbox workspace root");

    let error = PolicyResolver::new(SandboxMode::ReadOnly, "/work\0/escape")
        .expect_err("a NUL byte must be refused");
    assert_eq!(error.code(), "SANDBOX_INVALID_ROOT");
}

#[test]
fn the_resolver_validates_every_extra_root() {
    let resolver =
        PolicyResolver::new(SandboxMode::ReadOnly, "/work").expect("an absolute root is valid");
    let error = resolver
        .with_additional_roots(["relative"])
        .expect_err("a relative extra root must be refused");
    assert_eq!(error.code(), "SANDBOX_INVALID_ROOT");
    assert_eq!(error.subject, "additional writable directory");
}

#[test]
fn the_resolver_detaches_and_deduplicates_its_extra_roots() {
    let resolver = PolicyResolver::new(SandboxMode::WorkspaceWrite, "/work")
        .expect("an absolute root is valid")
        .with_additional_roots(["/extra", "/extra", "/other"])
        .expect("absolute roots are valid");
    assert_eq!(
        resolver.additional_writable_roots(),
        Some(&[PathBuf::from("/extra"), PathBuf::from("/other")][..])
    );
}

#[test]
fn the_deployment_default_is_the_last_resort() {
    let resolver =
        PolicyResolver::new(SandboxMode::ReadOnly, "/work").expect("an absolute root is valid");
    let policy = resolver
        .resolve(&PolicyRequest::default())
        .expect("the fallback root is absolute");

    assert_eq!(policy.mode, SandboxMode::ReadOnly);
    assert_eq!(policy.workspace_root, PathBuf::from("/work"));
    assert_eq!(policy.additional_writable_roots, None);
    assert_eq!(policy.session_id, None);
    assert_eq!(resolver.default_mode(), SandboxMode::ReadOnly);
    assert_eq!(resolver.workspace_root(), Path::new("/work"));
    assert_eq!(resolver.additional_writable_roots(), None);
}

#[test]
fn a_session_override_outranks_the_default_and_an_approved_override_outranks_the_session() {
    let resolver =
        PolicyResolver::new(SandboxMode::ReadOnly, "/fallback").expect("an absolute root is valid");

    let session = PolicyRequest {
        session_override: Some(SandboxMode::WorkspaceWrite),
        ..PolicyRequest::default()
    };
    assert_eq!(
        resolver
            .resolve(&session)
            .expect("the fallback root is absolute")
            .mode,
        SandboxMode::WorkspaceWrite
    );

    let approved = PolicyRequest {
        session_override: Some(SandboxMode::WorkspaceWrite),
        mode: Some(SandboxMode::Full),
        ..PolicyRequest::default()
    };
    assert_eq!(
        resolver
            .resolve(&approved)
            .expect("the fallback root is absolute")
            .mode,
        SandboxMode::Full,
        "an approved escalation is the widest thing a call can carry"
    );
}

#[test]
fn a_session_cwd_is_the_workspace_boundary() {
    let resolver = PolicyResolver::new(SandboxMode::WorkspaceWrite, "/fallback")
        .expect("an absolute root is valid");
    let request = PolicyRequest {
        session_cwd: Some("/sessions/one".to_owned()),
        ..PolicyRequest::default()
    };
    let policy = resolver
        .resolve(&request)
        .expect("an absolute cwd is a usable boundary");
    assert_eq!(policy.workspace_root, PathBuf::from("/sessions/one"));
}

#[test]
fn an_unusable_session_cwd_fails_closed_instead_of_falling_back() {
    let resolver = PolicyResolver::new(SandboxMode::WorkspaceWrite, "/fallback")
        .expect("an absolute root is valid");
    for cwd in ["relative/dir", "/a\0b"] {
        let request = PolicyRequest {
            session_cwd: Some(cwd.to_owned()),
            ..PolicyRequest::default()
        };
        let error = resolver
            .resolve(&request)
            .expect_err("a cwd that cannot be a boundary must not silently use the fallback");
        assert_eq!(error.code(), "SANDBOX_INVALID_ROOT");
        assert_eq!(error.subject, "sandbox workspace root");
    }
}

#[test]
fn the_session_identity_travels_with_the_resolved_policy() {
    let resolver = PolicyResolver::new(SandboxMode::WorkspaceWrite, "/work")
        .expect("an absolute root is valid");
    let id = girder_core::SessionId::new("session-1").expect("a well-formed id");
    let request = PolicyRequest {
        session_id: Some(id.clone()),
        ..PolicyRequest::default()
    };
    assert_eq!(
        resolver
            .resolve(&request)
            .expect("the fallback root is absolute")
            .session_id,
        Some(id)
    );
}

// ---- the session-mode fold ---------------------------------------------------------------------

#[test]
fn a_recorded_mode_replaces_the_deployment_knobs_and_an_unknown_name_does_not() {
    let mut base = girder_core::PermissionState::default();
    base.set_knobs(girder_core::Knobs::unrestricted());
    let events = vec![
        set_sandbox_mode(SandboxMode::ReadOnly, None),
        SessionEvent::ApprovalPolicy {
            policy: "ask".to_owned(),
        },
        SessionEvent::SandboxMode {
            mode: "not-a-mode".to_owned(),
            source: None,
        },
        SessionEvent::ApprovalPolicy {
            policy: "not-a-policy".to_owned(),
        },
    ];
    let (applied, mode) = permissions_for_session(&base, &events);
    assert_eq!(mode, Some(SandboxMode::ReadOnly));
    assert_eq!(applied.knobs().sandbox, SandboxMode::ReadOnly);
    assert_eq!(
        applied.knobs().approval,
        girder_core::Approval::OutsideWorkspace
    );
    let (unchanged, none) = permissions_for_session(&base, &[]);
    assert_eq!(none, None);
    assert_eq!(unchanged.knobs(), base.knobs());
}

#[test]
fn the_last_recorded_mode_wins() {
    let events = vec![
        set_sandbox_mode(SandboxMode::WorkspaceWrite, None),
        set_sandbox_mode(SandboxMode::Full, Some("delegation")),
    ];
    assert_eq!(session_override(&events), Some(SandboxMode::Full));
}

#[test]
fn a_session_that_never_switched_has_no_override() {
    assert_eq!(session_override(&[]), None);
    // Unrelated events carry no mode, and `session_override` reads only the fold's own event type.
    let events = vec![SessionEvent::PlanMode { active: true }];
    assert_eq!(session_override(&events), None);
}

#[test]
fn an_unknown_mode_in_the_log_keeps_the_previous_override() {
    // A log written by a newer build records a mode this one cannot enforce. Dropping the whole override
    // would silently widen the session to the deployment default — the wrong failure direction.
    let events = vec![
        set_sandbox_mode(SandboxMode::ReadOnly, None),
        SessionEvent::SandboxMode {
            mode: "hypothetical-tighter-mode".to_owned(),
            source: None,
        },
    ];
    assert_eq!(session_override(&events), Some(SandboxMode::ReadOnly));
}

#[test]
fn the_mode_switch_is_its_own_event() {
    let event = set_sandbox_mode(SandboxMode::WorkspaceWrite, None);
    assert_eq!(event.type_name(), "sandbox/mode");
    match &event {
        SessionEvent::SandboxMode { mode, source } => {
            assert_eq!(mode, "workspace-write");
            assert_eq!(source, &None);
        }
        other => panic!("expected a sandbox/mode event, got {other:?}"),
    }
    // A delegated seed is marked as such; a runtime switch is not.
    match set_sandbox_mode(SandboxMode::ReadOnly, Some("delegation")) {
        SessionEvent::SandboxMode { source, .. } => {
            assert_eq!(source.as_deref(), Some("delegation"));
        }
        other => panic!("expected a sandbox/mode event, got {other:?}"),
    }
}

// ---- the policy paragraph ----------------------------------------------------------------------

#[test]
fn the_policy_paragraph_names_the_mode_and_the_workspace() {
    let policy = ExecutionPolicy::read_only("/work/ws");
    let text = render_policy_context(&policy);
    assert!(
        text.starts_with("Current DSH file policy: read-only."),
        "got: {text}"
    );
    // The paragraph says what the policy does *not* license, so the model tries the tool instead of
    // refusing a required modification outright.
    assert!(
        text.contains("Do not refuse a required modification"),
        "got: {text}"
    );

    let policy = workspace_policy(Path::new("/work/ws"));
    let text = render_policy_context(&policy);
    assert!(
        text.starts_with("Current DSH file policy: workspace-write."),
        "got: {text}"
    );
    assert!(
        text.contains("[\"/work/ws\"]"),
        "the root is rendered as a JSON array, got: {text}"
    );
    assert!(
        text.contains("Some platform temporary areas may also be writable."),
        "got: {text}"
    );

    let policy = ExecutionPolicy {
        mode: SandboxMode::Full,
        workspace_root: PathBuf::from("/work/ws"),
        additional_writable_roots: None,
        session_id: None,
    };
    let text = render_policy_context(&policy);
    assert!(
        text.starts_with("Current DSH file policy: danger-full-access."),
        "got: {text}"
    );
    assert!(
        text.contains("does not restrict file modifications"),
        "got: {text}"
    );
}

#[test]
fn extra_writable_roots_appear_in_the_paragraph_with_the_windows_caveat() {
    let policy = ExecutionPolicy {
        mode: SandboxMode::WorkspaceWrite,
        workspace_root: PathBuf::from("/work/ws"),
        additional_writable_roots: Some(vec![
            PathBuf::from("/data/one"),
            PathBuf::from("/data/two"),
        ]),
        session_id: None,
    };
    let text = render_policy_context(&policy);
    assert!(
        text.contains("Additional writable directories: [\"/data/one\",\"/data/two\"]."),
        "got: {text}"
    );
    // Windows process confinement ignores extra roots; a paragraph that did not say so would promise
    // something only the file tools honour.
    assert!(text.contains("not sandboxed processes"), "got: {text}");
}

#[test]
fn an_empty_extra_root_list_is_omitted_from_the_paragraph() {
    let policy = ExecutionPolicy {
        mode: SandboxMode::WorkspaceWrite,
        workspace_root: PathBuf::from("/work/ws"),
        additional_writable_roots: Some(Vec::new()),
        session_id: None,
    };
    assert!(!render_policy_context(&policy).contains("Additional writable directories"));
}

// ---- escalation: argument validation -----------------------------------------------------------

#[test]
fn escalation_arguments_are_accepted_together_or_not_at_all() {
    assert!(validate_escalation_args(None, None).is_ok());
    assert!(
        validate_escalation_args(Some("workspace-write"), Some("the workspace needs it")).is_ok()
    );
}

#[test]
fn an_unpaired_escalation_argument_is_invalid() {
    let error = validate_escalation_args(Some("workspace-write"), None)
        .expect_err("sandbox_permissions without a justification is malformed");
    assert_eq!(error.code, EscalationErrorCode::InvalidArgument);
    assert_eq!(
        error.message,
        "invalid escalation: sandbox_permissions requires a justification"
    );

    let error = validate_escalation_args(None, Some("orphan reason"))
        .expect_err("a justification driving nothing is malformed");
    assert_eq!(error.code, EscalationErrorCode::InvalidArgument);
    assert_eq!(
        error.message,
        "invalid escalation: justification is only valid together with sandbox_permissions"
    );
}

#[test]
fn a_blank_justification_is_invalid() {
    for blank in ["", "   ", "\t\n"] {
        let error = validate_escalation_args(Some("workspace-write"), Some(blank))
            .expect_err("a blank justification cannot drive an approval prompt");
        assert_eq!(error.code, EscalationErrorCode::InvalidArgument);
        assert_eq!(
            error.message,
            "invalid justification: expected a non-empty sentence"
        );
    }
}

// ---- escalation: the model-facing texts --------------------------------------------------------

#[test]
fn the_permissions_description_names_the_family_subject() {
    assert_eq!(
        sandbox_permissions_description("command"),
        "The narrowest wider sandbox mode for a one-shot retry of the exact command the sandbox just \
         denied; the retry asks the user for approval."
    );
    assert!(sandbox_permissions_description("operation").contains("the exact operation"));
}

#[test]
fn the_audit_reason_carries_the_mode_and_the_verbatim_justification() {
    assert_eq!(
        escalation_audit_reason(
            SandboxMode::WorkspaceWrite,
            "the user asked to write in the workspace"
        ),
        "escalate sandbox to workspace-write: the user asked to write in the workspace"
    );
    // The reason is part of the grant's identity, so the target mode is always in it.
    assert_eq!(
        escalation_audit_reason(SandboxMode::Full, "needs /etc"),
        "escalate sandbox to danger-full-access: needs /etc"
    );
}

#[test]
fn every_locale_carries_the_mode_and_the_untranslated_justification() {
    let reason = escalation_display_reason(SandboxMode::WorkspaceWrite, "the user asked to write");
    assert_eq!(
        reason.en,
        "Allow this operation with workspace-write permissions: the user asked to write"
    );
    assert_eq!(
        reason.zh,
        "允许本次操作使用 workspace-write 权限：the user asked to write"
    );
    assert_eq!(
        reason.ja,
        "workspace-write 権限でこの操作を許可します：the user asked to write"
    );
    assert_eq!(
        reason.es,
        "Permitir esta operación con permisos workspace-write: the user asked to write"
    );
    assert_eq!(
        reason.fr,
        "Autoriser cette opération avec les permissions workspace-write : the user asked to write"
    );
    assert_eq!(
        reason.de,
        "Diesen Vorgang mit workspace-write-Berechtigungen zulassen: the user asked to write"
    );
    assert_eq!(
        reason.ru,
        "Разрешить эту операцию с правами workspace-write: the user asked to write"
    );
    assert_eq!(
        reason.pt,
        "Permitir esta operação com permissões workspace-write: the user asked to write"
    );
}

#[test]
fn a_locale_tag_selects_its_language_and_falls_back_to_english() {
    let reason = escalation_display_reason(SandboxMode::Full, "reason");
    assert_eq!(reason.for_locale("zh-Hans"), reason.zh);
    assert_eq!(reason.for_locale("pt_BR"), reason.pt);
    assert_eq!(reason.for_locale("en-US"), reason.en);
    // A locale this build does not speak still gets a readable prompt rather than nothing.
    assert_eq!(reason.for_locale("xx"), reason.en);
}

// ---- escalation: the approval choreography -----------------------------------------------------

/// An approver that records what it was asked and answers with a fixed outcome.
struct ScriptedApprover {
    outcome: EscalationOutcome,
    seen: Arc<Mutex<Vec<EscalationAsk>>>,
}

impl ScriptedApprover {
    fn new(outcome: EscalationOutcome) -> Self {
        Self {
            outcome,
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn asked(&self) -> Arc<Mutex<Vec<EscalationAsk>>> {
        Arc::clone(&self.seen)
    }
}

impl EscalationApprover for ScriptedApprover {
    fn request(&self, ask: EscalationAsk) -> BoxFut<EscalationOutcome> {
        let seen = Arc::clone(&self.seen);
        let outcome = self.outcome;
        Box::pin(async move {
            seen.lock()
                .expect("the test's sink is not poisoned")
                .push(ask);
            outcome
        })
    }
}

/// A request whose effective mode is `read-only`, escalating to `requested`.
fn escalation_request(
    requested: SandboxMode,
    effective: SandboxMode,
    subject: &str,
) -> EscalationRequest {
    EscalationRequest {
        requested_mode: requested,
        justification: "the user asked to write in the workspace".to_owned(),
        effective_mode: effective,
        subject: subject.to_owned(),
    }
}

/// The approval ingredients, with an agent and no cancellation.
fn approval<'a>(approver: &'a dyn EscalationApprover, call_id: &'a str) -> EscalationApproval<'a> {
    EscalationApproval {
        approver: Some(approver),
        agent: Some("agent-1"),
        call_id,
        tool_name: "bash",
        signal: None,
    }
}

#[tokio::test]
async fn a_grant_returns_the_requested_mode_and_asks_with_the_audit_reason() {
    let approver = ScriptedApprover::new(EscalationOutcome::AllowedOnce);
    let seen = approver.asked();
    let request = escalation_request(
        SandboxMode::WorkspaceWrite,
        SandboxMode::ReadOnly,
        "command",
    );

    let granted = approve_escalation(&request, &approval(&approver, "call-1"))
        .await
        .expect("an allowed-once outcome grants");

    assert_eq!(granted, SandboxMode::WorkspaceWrite);
    let asks = seen.lock().expect("the sink is not poisoned");
    assert_eq!(asks.len(), 1);
    assert_eq!(
        asks[0].reason,
        "escalate sandbox to workspace-write: the user asked to write in the workspace"
    );
    assert_eq!(
        asks[0].display_reason.en,
        "Allow this operation with workspace-write permissions: the user asked to write in the workspace"
    );
    assert_eq!(asks[0].call_id, "call-1");
    assert_eq!(asks[0].tool_name, "bash");
}

#[tokio::test]
async fn repeating_the_effective_mode_needs_no_approval_at_all() {
    for mode in SANDBOX_MODES {
        let approver = ScriptedApprover::new(EscalationOutcome::Rejected);
        let seen = approver.asked();
        let request = escalation_request(mode, mode, "command");
        let granted = approve_escalation(&request, &approval(&approver, "call-1"))
            .await
            .expect("repeating a mode is not an escalation");
        assert_eq!(granted, mode);
        // A repeat must not even consult the approver: a prompt the user never needed is a prompt that
        // teaches them to click through.
        assert!(
            seen.lock().expect("the sink is not poisoned").is_empty(),
            "no approval may be requested when {mode:?} is repeated"
        );
    }
}

#[tokio::test]
async fn a_narrower_target_fails_closed_without_asking() {
    for (requested, effective) in [
        (SandboxMode::ReadOnly, SandboxMode::WorkspaceWrite),
        (SandboxMode::ReadOnly, SandboxMode::Full),
        (SandboxMode::WorkspaceWrite, SandboxMode::Full),
    ] {
        let approver = ScriptedApprover::new(EscalationOutcome::AllowedOnce);
        let seen = approver.asked();
        let request = escalation_request(requested, effective, "command");
        let error = approve_escalation(&request, &approval(&approver, "call-1"))
            .await
            .expect_err("a narrower target is not an escalation");
        assert_eq!(error.code, EscalationErrorCode::InvalidArgument);
        assert!(
            error
                .message
                .contains("is not strictly wider than this call's current"),
            "got: {}",
            error.message
        );
        assert!(
            error.message.contains(effective.as_str()),
            "the message names the effective mode"
        );
        assert!(seen.lock().expect("the sink is not poisoned").is_empty());
    }
}

#[tokio::test]
async fn a_missing_approver_fails_closed_rather_than_widening_silently() {
    let request = escalation_request(
        SandboxMode::WorkspaceWrite,
        SandboxMode::ReadOnly,
        "command",
    );
    let ingredients = EscalationApproval {
        approver: None,
        agent: Some("agent-1"),
        call_id: "call-1",
        tool_name: "bash",
        signal: None,
    };
    let error = approve_escalation(&request, &ingredients)
        .await
        .expect_err("widening without an approval service must fail closed");
    assert_eq!(error.code, EscalationErrorCode::Unavailable);
    assert_eq!(
        error.message,
        "sandbox escalation to \"workspace-write\" requires approval, but no approval service is composed"
    );
}

#[tokio::test]
async fn an_agentless_call_fails_closed_with_its_own_text() {
    let approver = ScriptedApprover::new(EscalationOutcome::AllowedOnce);
    let request = escalation_request(
        SandboxMode::WorkspaceWrite,
        SandboxMode::ReadOnly,
        "command",
    );
    let ingredients = EscalationApproval {
        approver: Some(&approver),
        agent: None,
        call_id: "call-1",
        tool_name: "bash",
        signal: None,
    };
    let error = approve_escalation(&request, &ingredients)
        .await
        .expect_err("an agentless call has nobody to route the ask through");
    assert_eq!(error.code, EscalationErrorCode::Unavailable);
    assert_eq!(
        error.message,
        "sandbox escalation to \"workspace-write\" requires approval, but the call has no agent to route \
         it through"
    );
}

#[tokio::test]
async fn each_non_grant_outcome_has_its_own_text_and_code() {
    let cases = [
        (
            EscalationOutcome::Rejected,
            EscalationErrorCode::Rejected,
            "the user rejected escalating this operation to \"workspace-write\"; it stays denied, so stop \
             and explain instead of working around it",
        ),
        (
            EscalationOutcome::Cancelled,
            EscalationErrorCode::Cancelled,
            "approval for escalating to \"workspace-write\" was cancelled",
        ),
        (
            EscalationOutcome::Unavailable,
            EscalationErrorCode::Unavailable,
            "sandbox escalation to \"workspace-write\" requires approval, but no approval channel is \
             available",
        ),
    ];
    for (outcome, code, message) in cases {
        let approver = ScriptedApprover::new(outcome);
        let request = escalation_request(
            SandboxMode::WorkspaceWrite,
            SandboxMode::ReadOnly,
            "operation",
        );
        let error = approve_escalation(&request, &approval(&approver, "call-1"))
            .await
            .expect_err("a non-grant outcome must not widen the call");
        assert_eq!(error.code, code, "outcome {outcome:?}");
        assert_eq!(error.message, message, "outcome {outcome:?}");
    }
}

#[tokio::test]
async fn a_rejection_names_the_family_subject() {
    let approver = ScriptedApprover::new(EscalationOutcome::Rejected);
    let request = escalation_request(
        SandboxMode::WorkspaceWrite,
        SandboxMode::ReadOnly,
        "command",
    );
    let error = approve_escalation(&request, &approval(&approver, "call-1"))
        .await
        .expect_err("a rejected escalation stays denied");
    assert!(
        error.message.contains("escalating this command to"),
        "the shell family says `command`, got: {}",
        error.message
    );
}

#[tokio::test]
async fn a_cancelled_call_reports_its_own_cancellation() {
    let approver = ScriptedApprover::new(EscalationOutcome::Cancelled);
    let request = escalation_request(
        SandboxMode::WorkspaceWrite,
        SandboxMode::ReadOnly,
        "command",
    );
    let token = CancellationToken::new();
    token.cancel();
    let ingredients = EscalationApproval {
        approver: Some(&approver),
        agent: Some("agent-1"),
        call_id: "call-1",
        tool_name: "bash",
        signal: Some(&token),
    };
    let error = approve_escalation(&request, &ingredients)
        .await
        .expect_err("the call was cancelled, so it does not escalate");
    // Upstream rethrows the abort reason so the registry records the call as aborted; here the distinction
    // rides on the variant and the shared code.
    assert!(error.is_call_cancellation());
    assert_eq!(error.code, EscalationErrorCode::CancelledByCall);
    assert_eq!(error.code.as_str(), "SANDBOX_ESCALATION_CANCELLED");
}

#[tokio::test]
async fn an_un_cancelled_call_reports_a_plain_cancelled_approval() {
    let approver = ScriptedApprover::new(EscalationOutcome::Cancelled);
    let request = escalation_request(
        SandboxMode::WorkspaceWrite,
        SandboxMode::ReadOnly,
        "command",
    );
    let token = CancellationToken::new();
    let ingredients = EscalationApproval {
        approver: Some(&approver),
        agent: Some("agent-1"),
        call_id: "call-1",
        tool_name: "bash",
        signal: Some(&token),
    };
    let error = approve_escalation(&request, &ingredients)
        .await
        .expect_err("an unanswered prompt is not a grant");
    assert!(!error.is_call_cancellation());
    assert_eq!(error.code, EscalationErrorCode::Cancelled);
}

#[test]
fn the_escalation_codes_are_the_upstream_strings() {
    assert_eq!(
        EscalationErrorCode::InvalidArgument.as_str(),
        "SANDBOX_ESCALATION_INVALID_ARGUMENT"
    );
    assert_eq!(
        EscalationErrorCode::Unavailable.as_str(),
        "SANDBOX_ESCALATION_UNAVAILABLE"
    );
    assert_eq!(
        EscalationErrorCode::Rejected.as_str(),
        "SANDBOX_ESCALATION_REJECTED"
    );
    assert_eq!(
        EscalationErrorCode::Cancelled.as_str(),
        "SANDBOX_ESCALATION_CANCELLED"
    );
}

// ---- the runner ladder -------------------------------------------------------------------------

#[test]
fn each_platform_has_its_chain_in_preference_order() {
    assert_eq!(platform_chain("linux"), [Runner::Bwrap, Runner::Landlock]);
    assert_eq!(platform_chain("darwin"), [Runner::Seatbelt]);
    assert_eq!(platform_chain("win32"), [Runner::WindowsAcl]);
    // A platform with no chain fails closed at `confine`.
    for unsupported in ["freebsd", "aix", ""] {
        assert!(platform_chain(unsupported).is_empty());
    }
}

#[test]
fn a_sole_candidate_is_selected_without_a_probe() {
    let mut probes = 0;
    let selected = select_runner(platform_chain("darwin"), |_| {
        probes += 1;
        None
    })
    .expect("darwin's sole rung is selected without arbitration");
    assert_eq!(selected.runner, Runner::Seatbelt);
    assert_eq!(selected.enforcement, SandboxEnforcement::Full);
    // Probing would spend a process on a choice that has no alternative.
    assert_eq!(probes, 0);
}

#[test]
fn several_candidates_are_probed_in_chain_order() {
    let asked = std::cell::RefCell::new(Vec::new());
    let selected = select_runner(platform_chain("linux"), |runner| {
        asked.borrow_mut().push(runner);
        // bwrap is unusable here; the Landlock launcher reports partial enforcement for an older ABI.
        match runner {
            Runner::Bwrap => None,
            _ => Some(SandboxEnforcement::Partial),
        }
    })
    .expect("the first usable rung wins");
    assert_eq!(selected.runner, Runner::Landlock);
    assert_eq!(selected.enforcement, SandboxEnforcement::Partial);
    assert_eq!(*asked.borrow(), vec![Runner::Bwrap, Runner::Landlock]);
}

#[test]
fn a_chain_with_no_usable_rung_selects_nothing() {
    assert!(select_runner(platform_chain("linux"), |_| None).is_none());
    assert!(select_runner(&[], |_| Some(SandboxEnforcement::Full)).is_none());
}

#[test]
fn the_windows_rung_advertises_partial_enforcement() {
    // NTFS hard links alias a granted file to a path outside it, reads stay unconfined, and an
    // AppContainer-protected tree is unreadable to the child: the backend must not promise the absolute.
    assert_eq!(
        static_enforcement(Runner::WindowsAcl),
        SandboxEnforcement::Partial
    );
    for runner in [Runner::Bwrap, Runner::Landlock, Runner::Seatbelt] {
        assert_eq!(static_enforcement(runner), SandboxEnforcement::Full);
    }
}

#[test]
fn each_runner_publishes_its_own_denial_dialect() {
    // EROFS text under bwrap's read-only binds, EACCES under Landlock, EPERM under Seatbelt.
    assert_eq!(denial_signatures(Runner::Bwrap), ["read-only file system"]);
    assert_eq!(denial_signatures(Runner::Landlock), ["permission denied"]);
    assert_eq!(
        denial_signatures(Runner::Seatbelt),
        ["operation not permitted"]
    );
    // A cross-backend union would claim denials a given backend never produces.
    assert!(!denial_signatures(Runner::Bwrap).contains(&"operation not permitted"));
    assert!(denial_signatures(Runner::WindowsAcl).contains(&"access is denied"));
    assert_eq!(
        configured_runner_denial_signatures(),
        ["read-only file system", "permission denied"]
    );
}

#[test]
fn the_landlock_rule_is_exit_gated_and_excludes_its_partial_enforcement_notice() {
    let rules = runner_failure_rules(Runner::Landlock);
    assert_eq!(rules.len(), 1);
    assert_eq!(
        rules[0].allowed_exit_codes,
        Some(vec![LANDLOCK_LAUNCHER_FAILURE_EXIT])
    );
    assert_eq!(rules[0].fatal_signatures, ["landlock-run: "]);
    // The launcher self-reports partial enforcement at every confined run; without the exclusion a working
    // run would be read as a broken runner.
    assert_eq!(
        rules[0].informational_lines,
        ["landlock-run: partial enforcement (older Landlock ABI)"]
    );
}

#[test]
fn the_windows_rule_is_exit_gated_so_printed_text_is_not_mistaken_for_a_failure() {
    let rules = runner_failure_rules(Runner::WindowsAcl);
    assert_eq!(
        rules[0].allowed_exit_codes,
        Some(vec![WINDOWS_ACL_RUNNER_FAILURE_EXIT])
    );
    assert_eq!(rules[0].fatal_signatures, ["windows-acl-run: "]);
}

#[test]
fn the_signature_only_rules_reserve_no_exit_code() {
    // bwrap's public contract does not reserve exit 1 and sandbox-exec publishes no failure status, so
    // gating on an exit code would invent a contract.
    for runner in [Runner::Bwrap, Runner::Seatbelt] {
        let rules = runner_failure_rules(runner);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].allowed_exit_codes, None);
        assert!(rules[0].informational_lines.is_empty());
    }
    assert_eq!(
        runner_failure_rules(Runner::Bwrap)[0].fatal_signatures,
        ["bwrap: "]
    );
    assert_eq!(
        runner_failure_rules(Runner::Seatbelt)[0].fatal_signatures,
        ["sandbox-exec: "]
    );
}

#[test]
fn a_configured_runner_pairs_with_its_failure_signatures() {
    let runner = vec!["my-runner".to_owned()];
    let signatures = vec!["my-runner: ".to_owned()];
    assert!(validate_runner_config(&runner, &signatures).is_ok());
    // Neither half alone describes anything usable.
    assert_eq!(
        validate_runner_config(&runner, &[]),
        Err("sandbox-local: runnerCommand requires at least one runnerFailureSignatures entry")
    );
    assert_eq!(
        validate_runner_config(&[], &signatures),
        Err("sandbox-local: runnerFailureSignatures requires runnerCommand")
    );
    // A blank or multi-line signature cannot identify a fatal stderr line.
    for bad in [
        String::new(),
        "   ".to_owned(),
        "a\nb".to_owned(),
        "a\rb".to_owned(),
    ] {
        assert_eq!(
            validate_runner_config(&runner, std::slice::from_ref(&bad)),
            Err(
                "sandbox-local: runnerFailureSignatures entries must be non-empty single-line strings"
            )
        );
    }
}

#[test]
fn the_landlock_launcher_is_named_where_a_single_program_name_suffices() {
    assert_eq!(Runner::Bwrap.program(), Some("bwrap"));
    assert_eq!(Runner::Landlock.program(), Some("landlock-run"));
    assert_eq!(Runner::Seatbelt.program(), Some("sandbox-exec"));
    // The Windows invocation is a node process plus a resolved runner entry, so a single name would be a
    // lie.
    assert_eq!(Runner::WindowsAcl.program(), None);
}

// ---- profiles ----------------------------------------------------------------------------------

#[test]
fn the_bwrap_profile_binds_the_roots_only_under_workspace_write() {
    let root = temp_dir("bwrap-ro");
    let read_only =
        bwrap_profile_args(&ExecutionPolicy::read_only(&root)).expect("read-only has no roots");
    assert_eq!(
        read_only,
        [
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--unshare-pid",
            "--proc",
            "/proc",
            "--die-with-parent"
        ]
    );

    let root = temp_dir("bwrap-ws");
    let profile =
        bwrap_profile_args(&workspace_policy(&root)).expect("a plain workspace is grantable");
    // `workspace-write` gets a fresh `/tmp` mount rather than a bind of the host's temp tree.
    let tmpfs = profile
        .windows(2)
        .position(|pair| pair == ["--tmpfs", "/tmp"]);
    assert!(
        tmpfs.is_some(),
        "workspace-write must mount a private /tmp: {profile:?}"
    );
    let spelled = root.to_string_lossy().into_owned();
    let bind = profile
        .windows(3)
        .position(|triple| triple == ["--bind", spelled.as_str(), spelled.as_str()]);
    assert!(
        bind.is_some(),
        "the workspace must be bound writable: {profile:?}"
    );
}

#[test]
fn the_landlock_grants_always_allow_dev_null() {
    let root = temp_dir("landlock");
    // `/dev/null` is granted even under read-only, because a denied sink breaks ordinary commands for a
    // reason that is not the user's policy.
    let grants =
        landlock_grants(&ExecutionPolicy::read_only(&root)).expect("read-only has no roots");
    assert_eq!(grants.read_only, ["/"]);
    assert_eq!(grants.read_write, ["/dev/null"]);

    let root = temp_dir("landlock-ws");
    let grants = landlock_grants(&workspace_policy(&root)).expect("a plain workspace is grantable");
    assert!(grants.read_write.contains(&"/tmp".to_owned()));
    assert!(
        grants
            .read_write
            .contains(&root.to_string_lossy().into_owned())
    );
}

#[test]
fn the_seatbelt_profile_denies_writes_then_reallows_the_roots() {
    let root = temp_dir("seatbelt");
    let args =
        seatbelt_profile_args(&workspace_policy(&root)).expect("a plain workspace is grantable");
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], "-p");
    let profile = &args[1];
    assert!(
        profile.starts_with("(version 1) (allow default) (deny file-write*)"),
        "got: {profile}"
    );
    assert!(
        profile.contains("(allow file-write* (literal \"/dev/null\"))"),
        "a denied sink would break ordinary commands, got: {profile}"
    );
    assert!(
        profile.contains(&format!("(subpath \"{}\")", root.to_string_lossy())),
        "the workspace must be re-allowed, got: {profile}"
    );
}

#[test]
fn read_only_profiles_grant_only_the_required_sink() {
    let root = temp_dir("seatbelt-ro");
    let args =
        seatbelt_profile_args(&ExecutionPolicy::read_only(&root)).expect("read-only has no roots");
    let profile = &args[1];
    assert!(profile.contains("(literal \"/dev/null\")"));
    // No subtree grant at all: under read-only the only permitted write is the sink.
    assert!(
        !profile.contains("(subpath"),
        "read-only must not re-allow a subtree, got: {profile}"
    );
}

#[test]
fn an_sbpl_string_escapes_a_quote_and_a_backslash() {
    // A path containing a quote would otherwise end the literal early and change the profile's meaning.
    let policy = ExecutionPolicy {
        mode: SandboxMode::WorkspaceWrite,
        workspace_root: PathBuf::from("/work/we\"ird"),
        additional_writable_roots: None,
        session_id: None,
    };
    let args = seatbelt_profile_args(&policy)
        .expect("the root is not inspected under a non-existent path");
    assert!(
        args[1].contains(r#"(subpath "/work/we\"ird")"#),
        "got: {}",
        args[1]
    );
}

// ---- confine: fail closed ----------------------------------------------------------------------

#[test]
fn confine_wraps_the_callers_argv_behind_a_separator() {
    let root = temp_dir("confine-bwrap");
    let policy = workspace_policy(&root);
    let argv = vec!["bash".to_owned(), "-c".to_owned(), "echo hi".to_owned()];
    let confined =
        confine(&argv, &policy, &[Runner::Bwrap], |_| None).expect("bwrap can be wrapped");

    // The separator is the boundary between the runner's profile and the caller's argv; without it a
    // caller's `--ro-bind` would be parsed as another profile flag.
    let separator = confined
        .argv
        .iter()
        .position(|arg| arg == "--")
        .expect("a separator exists");
    assert_eq!(&confined.argv[separator + 1..], argv.as_slice());
    assert_eq!(confined.argv[0], "bwrap");
    assert_eq!(confined.enforcement, SandboxEnforcement::Full);
    // The dialect travels with the wrap, because only the wrap knows which backend was in force.
    assert_eq!(confined.denial_signatures, ["read-only file system"]);
    assert_eq!(confined.runner_failure_rules.len(), 1);
}

#[test]
fn confine_refuses_a_host_with_no_usable_runner() {
    let root = temp_dir("confine-none");
    let argv = vec!["bash".to_owned()];
    let error = confine(&argv, &workspace_policy(&root), &[], |_| {
        Some(SandboxEnforcement::Full)
    })
    .expect_err("a host with no chain must not run the command unconfined");
    assert_eq!(error.code(), "SANDBOX_UNAVAILABLE");
    assert!(matches!(
        error,
        ConfineError::Unavailable {
            mode: SandboxMode::WorkspaceWrite
        }
    ));
}

#[test]
fn confine_refuses_when_every_rung_proves_unusable() {
    let root = temp_dir("confine-probes");
    let argv = vec!["bash".to_owned()];
    let error = confine(
        &argv,
        &workspace_policy(&root),
        platform_chain("linux"),
        |_| None,
    )
    .expect_err("no rung passed its probe, so nothing may confine");
    assert_eq!(error.code(), "SANDBOX_UNAVAILABLE");
    assert!(
        error
            .to_string()
            .contains("refusing to run the command unconfined")
    );
    assert!(
        error.to_string().contains("danger-full-access"),
        "the message names the remedy"
    );
}

#[test]
fn confine_propagates_an_unenforceable_root() {
    let root = temp_dir("confine-bad-root");
    let policy = ExecutionPolicy {
        mode: SandboxMode::WorkspaceWrite,
        workspace_root: root,
        additional_writable_roots: Some(vec![PathBuf::from("relative")]),
        session_id: None,
    };
    let argv = vec!["bash".to_owned()];
    let error = confine(&argv, &policy, &[Runner::Bwrap], |_| None)
        .expect_err("an unenforceable root must refuse the run");
    assert_eq!(error.code(), "SANDBOX_INVALID_ROOT");
}

#[test]
fn confine_refuses_backends_whose_invocation_it_cannot_spell() {
    // Emitting a guessed profile would either fail every run or — if the launcher parsed it as a different
    // grant — confine the wrong tree, so the refusal is the honest answer.
    for (runner, chain) in [
        (Runner::Landlock, vec![Runner::Landlock]),
        (Runner::WindowsAcl, vec![Runner::WindowsAcl]),
    ] {
        let root = temp_dir("confine-unsupported");
        let argv = vec!["bash".to_owned()];
        let error = confine(&argv, &workspace_policy(&root), &chain, |_| {
            Some(SandboxEnforcement::Full)
        })
        .expect_err("an unspellable invocation is refused");
        assert!(
            matches!(error, ConfineError::Unsupported { runner: refused, .. } if refused == runner)
        );
        assert_eq!(error.code(), "SANDBOX_PREPARATION_FAILED");
    }
}

#[test]
fn no_confine_path_returns_the_callers_own_argv() {
    // The property the seam must never trade away: a refusal, never a silent passthrough.
    let root = temp_dir("confine-passthrough");
    let argv = vec!["bash".to_owned(), "-c".to_owned(), "true".to_owned()];
    for chain in [
        vec![],
        vec![Runner::Bwrap],
        vec![Runner::Seatbelt],
        vec![Runner::Landlock],
        vec![Runner::WindowsAcl],
        platform_chain("linux").to_vec(),
    ] {
        if let Ok(confined) = confine(&argv, &workspace_policy(&root), &chain, |_| None) {
            assert_ne!(
                confined.argv, argv,
                "a confined result must add the runner: {chain:?}"
            );
        }
    }
}

#[test]
fn the_unavailable_message_names_every_platform_remedy() {
    let message = unavailable_message(SandboxMode::ReadOnly);
    assert!(message.starts_with("sandbox mode \"read-only\" is requested"));
    for remedy in [
        "bubblewrap",
        "Landlock",
        "sandbox-exec",
        "restricted-token",
        "danger-full-access",
    ] {
        assert!(
            message.contains(remedy),
            "{remedy} must be named in: {message}"
        );
    }
}

#[test]
fn the_confine_error_codes_are_the_upstream_strings() {
    assert_eq!(
        ConfineError::Unavailable {
            mode: SandboxMode::Full
        }
        .code(),
        "SANDBOX_UNAVAILABLE"
    );
    let roots = ConfineError::Roots(sandbox_gap::SandboxRootError::new("/a", "x", "y"));
    assert_eq!(roots.code(), "SANDBOX_INVALID_ROOT");
    assert_eq!(
        ConfineError::Unsupported {
            runner: Runner::Landlock,
            detail: "d"
        }
        .code(),
        "SANDBOX_PREPARATION_FAILED"
    );
}

// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod sandbox {
    pub use super::sandbox_gap as policy;
}
