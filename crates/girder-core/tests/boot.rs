//! Tests for boot.
//!
//! The interesting cases are the negative ones: a missing essential component must abort, a missing
//! optional one must not, and a credential must never appear in a log.

use girder_core::{BootConfig, BootError, BootOutcome, boot};

fn with_key() -> BootConfig {
    BootConfig {
        api_key: Some("test-key".to_owned()),
        ..BootConfig::default()
    }
}

// ---- the happy path ---------------------------------------------------------------------------

#[test]
fn a_configured_boot_mounts_every_component() {
    let booted = boot(&with_key()).expect("boots");

    assert!(
        booted
            .report
            .outcome("provider")
            .is_some_and(BootOutcome::is_mounted)
    );
    assert!(
        booted
            .report
            .outcome("tools")
            .is_some_and(BootOutcome::is_mounted)
    );
    assert!(
        booted
            .report
            .outcome("events")
            .is_some_and(BootOutcome::is_mounted)
    );
    assert!(booted.provider.is_some());
    assert!(booted.report.failures().is_empty());
}

#[test]
fn the_default_model_is_used_when_none_is_configured() {
    let booted = boot(&with_key()).expect("boots");

    assert_eq!(booted.model, girder_core::DEFAULT_MODEL);
}

#[test]
fn a_configured_model_wins() {
    let config = BootConfig {
        model: Some("deepseek-reasoner".to_owned()),
        ..with_key()
    };

    assert_eq!(boot(&config).expect("boots").model, "deepseek-reasoner");
}

// ---- a missing essential component -------------------------------------------------------------

#[test]
fn a_missing_credential_does_not_abort_the_boot() {
    // A system with no provider is still inspectable: it can list tools and be asked why. Aborting
    // would make "which part is missing" a question answered only by reading the source.
    let booted = boot(&BootConfig::default()).expect("boots without a provider");

    assert!(booted.provider.is_none());
    assert!(matches!(
        booted.report.outcome("provider"),
        Some(BootOutcome::Skipped { .. })
    ));
    assert_eq!(
        booted.report.outcome("provider"),
        Some(&BootOutcome::Skipped {
            reason: "no credential configured".to_owned()
        })
    );
}

#[test]
fn a_runtime_without_a_provider_is_refused() {
    // Failing at startup is easier to diagnose than failing on the first prompt.
    let booted = boot(&BootConfig::default()).expect("boots");
    let error = booted.into_runtime(None).expect_err("no provider");

    let BootError::RequiredFailed { component, .. } = error else {
        panic!("expected RequiredFailed, got {error:?}");
    };
    assert_eq!(component, "provider");
}

#[test]
fn a_runtime_with_a_provider_is_built() {
    let booted = boot(&with_key()).expect("boots");

    let runtime = booted.into_runtime(Some(4)).expect("runtime");

    assert!(std::sync::Arc::strong_count(&runtime) >= 1);
}

// ---- optional components ----------------------------------------------------------------------

#[test]
fn a_missing_session_directory_uses_memory() {
    // In-memory is the right default for a test and a poor one for anything else, so it is recorded
    // as a skip rather than silently chosen.
    let booted = boot(&with_key()).expect("boots");

    assert!(matches!(
        booted.report.outcome("sessions"),
        Some(BootOutcome::Skipped { .. })
    ));
}

#[test]
fn a_session_directory_is_created() {
    let directory = std::env::temp_dir().join(format!("girder-boot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let config = BootConfig {
        session_dir: Some(directory.clone()),
        ..with_key()
    };

    let booted = boot(&config).expect("boots");

    assert!(directory.is_dir(), "the directory must exist");
    assert!(
        booted
            .report
            .outcome("sessions")
            .is_some_and(BootOutcome::is_mounted)
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn an_unusable_session_directory_aborts() {
    // A configured directory that cannot be created is an error, not something to fall back from:
    // falling back would silently write to memory and lose the log.
    let config = BootConfig {
        session_dir: Some(std::path::PathBuf::from("/proc/girder-cannot-write-here")),
        ..with_key()
    };

    let error = boot(&config).expect_err("unwritable directory");

    assert!(
        matches!(error, BootError::RequiredFailed { component, .. } if component == "sessions")
    );
}

// ---- the report -------------------------------------------------------------------------------

#[test]
fn the_report_names_every_component() {
    let booted = boot(&with_key()).expect("boots");
    let names: Vec<String> = booted
        .report
        .entries()
        .into_iter()
        .map(|(name, _)| name)
        .collect();

    assert_eq!(names, vec!["events", "provider", "sessions", "tools"]);
}

#[test]
fn the_report_renders_one_line_per_component() {
    // A log where one part's outcome appears far from the next is a log nobody reads.
    let booted = boot(&with_key()).expect("boots");
    let rendered = booted.report.render();

    assert_eq!(rendered.lines().count(), 4);
    assert!(rendered.contains("provider: mounted"));
}

#[test]
fn a_skip_renders_differently_from_a_failure() {
    // A deployment that does not want a component is a decision, not a fault.
    let booted = boot(&BootConfig::default()).expect("boots");
    let rendered = booted.report.render();

    assert!(rendered.contains("provider: skipped"));
    assert!(!rendered.contains("FAILED"));
}

#[test]
fn failures_are_listed_separately() {
    let booted = boot(&with_key()).expect("boots");

    assert!(booted.report.failures().is_empty());
}

// ---- the credential ---------------------------------------------------------------------------

#[test]
fn the_credential_never_appears_in_debug_output() {
    // A credential in a startup log is a credential in every log aggregator the deployment has.
    let config = with_key();
    let rendered = format!("{config:?}");

    assert!(!rendered.contains("test-key"), "{rendered}");
    assert!(rendered.contains("redacted"), "{rendered}");
}

#[test]
fn the_credential_never_appears_in_the_report() {
    let booted = boot(&with_key()).expect("boots");
    let rendered = booted.report.render();

    assert!(!rendered.contains("test-key"), "{rendered}");
}

#[test]
fn the_debug_output_of_a_booted_system_leaks_nothing() {
    let booted = boot(&with_key()).expect("boots");
    let rendered = format!("{booted:?}");

    assert!(!rendered.contains("test-key"), "{rendered}");
}

// ---- configuration from the environment -------------------------------------------------------

#[test]
fn an_empty_environment_yields_a_default_config() {
    // The test cannot clear the process environment safely, so this asserts the shape rather than
    // the values: every field is optional.
    let config = BootConfig::default();

    assert!(config.provider.is_none());
    assert!(config.model.is_none());
    assert!(config.api_key.is_none());
    assert!(config.max_turns.is_none());
}

#[test]
fn a_default_config_boots_without_a_provider() {
    // The property that matters: booting never panics on a bare configuration.
    let booted = boot(&BootConfig::default()).expect("boots");
    assert!(booted.provider.is_none());
}
