//! Tests for permission presets.
//!
//! The property that matters is that a switch actually changes enforcement: a preset that recorded a name
//! while the sandbox still permitted writes would leave a user believing they are protected.

use std::path::Path;

use girder_core::{
    Approval, Knobs, PermissionCatalogue, PermissionError, PermissionState, Preset, SandboxMode,
};

// ---- approval policy --------------------------------------------------------------------------

#[test]
fn always_requires_approval_for_every_write() {
    let policy = Approval::Always;

    assert!(policy.requires_approval(Path::new("/work"), Path::new("/work/a")));
    assert!(policy.requires_approval(Path::new("/work"), Path::new("/etc/hosts")));
}

#[test]
fn never_requires_approval_for_any_write() {
    let policy = Approval::Never;

    assert!(!policy.requires_approval(Path::new("/work"), Path::new("/etc/hosts")));
}

#[test]
fn outside_workspace_requires_approval_only_outside() {
    // The policy is interpreted in one place, so execution and a UI preview cannot disagree.
    let policy = Approval::OutsideWorkspace;

    assert!(!policy.requires_approval(Path::new("/work"), Path::new("/work/a/b")));
    assert!(policy.requires_approval(Path::new("/work"), Path::new("/etc/hosts")));
}

#[test]
fn outside_workspace_requires_approval_when_a_path_climbs_out() {
    let policy = Approval::OutsideWorkspace;

    assert!(policy.requires_approval(Path::new("/work"), Path::new("/work/../etc/passwd")));
}

#[test]
fn the_approval_names_are_stable() {
    assert_eq!(Approval::Always.as_str(), "always");
    assert_eq!(Approval::OutsideWorkspace.as_str(), "outside-workspace");
    assert_eq!(Approval::Never.as_str(), "never");
}

// ---- the bundles ------------------------------------------------------------------------------

#[test]
fn the_bundles_pair_a_mode_with_a_policy() {
    // Read-only needs no prompt because nothing can change.
    assert_eq!(
        Knobs::read_only(),
        Knobs::new(SandboxMode::ReadOnly, Approval::Never)
    );
    assert_eq!(
        Knobs::workspace(),
        Knobs::new(SandboxMode::WorkspaceWrite, Approval::OutsideWorkspace)
    );
    assert_eq!(
        Knobs::unrestricted(),
        Knobs::new(SandboxMode::Full, Approval::Never)
    );
}

// ---- presets ----------------------------------------------------------------------------------

#[test]
fn a_preset_needs_a_name_and_a_description() {
    // One with no name cannot be selected; one with no description cannot be chosen knowingly.
    assert!(Preset::new("", "does something", Knobs::read_only()).is_none());
    assert!(Preset::new("read-only", "  ", Knobs::read_only()).is_none());
}

#[test]
fn the_builtin_presets_cover_the_three_intents() {
    let presets = Preset::builtin();
    let names: Vec<&str> = presets.iter().map(|preset| preset.name.as_str()).collect();

    assert_eq!(
        names,
        vec!["read-only", "workspace-write", "danger-full-access"]
    );
}

#[test]
fn the_builtin_bundles_are_distinct() {
    // Two presets sharing a bundle is allowed, but these three must differ or the choice is empty.
    let presets = Preset::builtin();
    assert_ne!(presets[0].knobs, presets[1].knobs);
    assert_ne!(presets[1].knobs, presets[2].knobs);
}

// ---- the catalogue ----------------------------------------------------------------------------

#[test]
fn the_catalogue_lists_every_preset() {
    let catalogue = PermissionCatalogue::builtin();

    assert_eq!(catalogue.entries().len(), 3);
    assert_eq!(catalogue.available().len(), 3);
}

#[test]
fn a_preset_can_be_marked_unavailable_without_being_forgotten() {
    // A hosted deployment may not want to offer "danger-full-access" while still knowing how to apply it.
    let mut catalogue = PermissionCatalogue::builtin();
    catalogue.set_available("danger-full-access", false);

    assert_eq!(catalogue.available().len(), 2);
    assert!(catalogue.get("danger-full-access").is_some(), "still known");
}

#[test]
fn entries_are_sorted() {
    let catalogue = PermissionCatalogue::builtin();
    let entries = catalogue.entries();
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry.preset.name.as_str())
        .collect();

    assert_eq!(
        names,
        vec!["danger-full-access", "read-only", "workspace-write"]
    );
}

// ---- state ------------------------------------------------------------------------------------

#[test]
fn a_fresh_state_uses_the_workspace_bundle() {
    // A fresh session must not be able to write outside its workspace without asking.
    let state = PermissionState::default();

    assert_eq!(state.knobs(), Knobs::workspace());
    assert!(state.preset().is_none(), "nothing was chosen yet");
}

#[test]
fn applying_a_preset_changes_the_knobs() {
    // The property: enforcement follows the choice rather than the name.
    let catalogue = PermissionCatalogue::builtin();
    let mut state = PermissionState::default();

    let knobs = state.apply(&catalogue, "read-only").expect("applied");

    assert_eq!(knobs, Knobs::read_only());
    assert_eq!(state.knobs(), Knobs::read_only(), "execution reads these");
    assert_eq!(state.preset(), Some("read-only"));
}

#[test]
fn applying_the_unrestricted_preset_actually_unrestricts() {
    let catalogue = PermissionCatalogue::builtin();
    let mut state = PermissionState::default();

    state
        .apply(&catalogue, "danger-full-access")
        .expect("applied");

    assert_eq!(state.knobs().sandbox, SandboxMode::Full);
    assert_eq!(state.knobs().approval, Approval::Never);
}

#[test]
fn an_unknown_preset_is_refused() {
    let catalogue = PermissionCatalogue::builtin();
    let mut state = PermissionState::default();

    let error = state.apply(&catalogue, "nope").expect_err("unknown");

    assert!(matches!(error, PermissionError::Unknown { .. }));
}

#[test]
fn an_unavailable_preset_is_refused_distinctly() {
    // The name is real and the deployment refused it, which a user reports differently from a typo.
    let mut catalogue = PermissionCatalogue::builtin();
    catalogue.set_available("danger-full-access", false);
    let mut state = PermissionState::default();

    let error = state
        .apply(&catalogue, "danger-full-access")
        .expect_err("unavailable");

    assert!(matches!(error, PermissionError::Unavailable { .. }));
}

#[test]
fn a_refused_switch_leaves_the_previous_knobs() {
    // A half-applied switch would leave enforcement between two policies.
    let catalogue = PermissionCatalogue::builtin();
    let mut state = PermissionState::default();
    state.apply(&catalogue, "read-only").expect("applied");

    let _ = state.apply(&catalogue, "nope");

    assert_eq!(state.knobs(), Knobs::read_only());
    assert_eq!(state.preset(), Some("read-only"));
}

#[test]
fn setting_knobs_by_hand_clears_the_recorded_preset() {
    // A caller that changed one knob has left the preset it came from, and keeping the name would report
    // an intent the values no longer match.
    let catalogue = PermissionCatalogue::builtin();
    let mut state = PermissionState::default();
    state.apply(&catalogue, "workspace-write").expect("applied");

    state.set_knobs(Knobs::new(SandboxMode::Full, Approval::Always));

    assert!(state.preset().is_none());
    assert_eq!(state.knobs().approval, Approval::Always);
}

#[test]
fn the_recorded_preset_survives_a_bundle_that_matches_another() {
    // Two presets can share a bundle; the recorded name is what distinguishes them, so it cannot be
    // derived from the knobs.
    let mut catalogue = PermissionCatalogue::builtin();
    // A preset whose bundle equals "read-only" but means something else.
    catalogue.add(
        Preset::new("review-only", "Read only, for review.", Knobs::read_only()).expect("valid"),
        true,
    );
    let mut state = PermissionState::default();

    state.apply(&catalogue, "review-only").expect("applied");

    assert_eq!(state.preset(), Some("review-only"), "not `read-only`");
    assert_eq!(state.knobs(), Knobs::read_only(), "the same bundle");
}

#[test]
fn the_catalogue_accepts_a_deployment_specific_preset() {
    let mut catalogue = PermissionCatalogue::builtin();
    catalogue.add(
        Preset::new("custom", "A deployment's own choice.", Knobs::workspace()).expect("valid"),
        true,
    );

    assert_eq!(catalogue.available().len(), 4);
}
