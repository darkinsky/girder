//! Tests for the extension registry.
//!
//! The properties that matter are about authority rather than data: an unapproved extension must
//! never reach the runtime, a missing service must fail before starting rather than after, and a
//! stopped extension must actually stop.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{
    ExtensionDefinition, ExtensionError, ExtensionId, ExtensionRegistry, ExtensionRun,
    ExtensionRuntime, ExtensionStatus,
};

/// A runtime that records starts and stops without running anything.
#[derive(Default)]
struct Counting {
    starts: Arc<AtomicU32>,
    stops: Arc<AtomicU32>,
    fail: bool,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn start_count(&self) -> u32 {
        self.starts.load(Ordering::SeqCst)
    }

    fn stop_count(&self) -> u32 {
        self.stops.load(Ordering::SeqCst)
    }
}

impl ExtensionRuntime for Counting {
    fn start(
        &self,
        definition: &ExtensionDefinition,
        _available: &[String],
    ) -> girder_core::BoxFut<Result<ExtensionRun, ExtensionError>> {
        let fail = self.fail;
        let id = definition.id.clone();
        self.starts.fetch_add(1, Ordering::SeqCst);
        let stops = Arc::clone(&self.stops);
        Box::pin(async move {
            if fail {
                return Err(ExtensionError::LoadFailed {
                    id: id.to_string(),
                    reason: "synthetic failure".to_owned(),
                });
            }
            Ok(ExtensionRun {
                id,
                stop: Box::new(move || {
                    stops.fetch_add(1, Ordering::SeqCst);
                }),
            })
        })
    }
}

fn definition(id: &str) -> ExtensionDefinition {
    ExtensionDefinition::new(
        ExtensionId::new(id).expect("valid"),
        format!("Extension {id}"),
        "// source",
    )
    .expect("valid definition")
}

fn registry_with(id: &str) -> ExtensionRegistry {
    let registry = ExtensionRegistry::new();
    registry.define(definition(id)).expect("defined");
    registry
}

// ---- definitions ------------------------------------------------------------------------------

#[test]
fn an_empty_name_or_source_is_refused() {
    // An unnamed extension cannot be reported in a permission prompt.
    let id = ExtensionId::new("e1").expect("valid");
    assert!(ExtensionDefinition::new(id.clone(), "", "// source").is_none());
    assert!(ExtensionDefinition::new(id, "name", "  ").is_none());
}

#[test]
fn a_defined_extension_starts_unapproved_and_stopped() {
    // Approval is the fact that makes running it legitimate, so a fresh one has none.
    let registry = registry_with("e1");

    assert_eq!(registry.status("e1"), Some(ExtensionStatus::Stopped));
    assert!(!registry.definition("e1").expect("present").is_approved());
}

#[test]
fn a_duplicate_id_is_refused() {
    // Replacing silently would swap the source behind an approval a human gave for different code.
    let registry = registry_with("e1");
    let error = registry.define(definition("e1")).expect_err("duplicate");

    assert!(matches!(error, ExtensionError::Duplicate { .. }));
}

#[test]
fn ids_are_sorted() {
    let registry = ExtensionRegistry::new();
    for id in ["zeta", "alpha"] {
        registry.define(definition(id)).expect("defined");
    }

    assert_eq!(registry.ids(), vec!["alpha", "zeta"]);
}

#[test]
fn missing_services_are_reported_in_order() {
    let definition = definition("e1").requiring("tools").requiring("events");
    let available = vec!["events".to_owned()];

    assert_eq!(definition.missing_services(&available), vec!["tools"]);
}

#[test]
fn an_extension_with_all_its_services_reports_none_missing() {
    let definition = definition("e1").requiring("tools");
    let available = vec!["tools".to_owned()];

    assert!(definition.missing_services(&available).is_empty());
}

// ---- approval ---------------------------------------------------------------------------------

#[tokio::test]
async fn an_unapproved_extension_never_reaches_the_runtime() {
    // A model that could activate an extension on its own could widen its own capabilities.
    let registry = registry_with("e1");
    let runtime = Counting::new();

    let error = registry
        .activate("e1", &[], runtime.as_ref())
        .await
        .expect_err("not approved");

    assert!(matches!(error, ExtensionError::NotApproved { .. }));
    assert_eq!(runtime.start_count(), 0, "the runtime must not be called");
}

#[tokio::test]
async fn approving_lets_it_start() {
    let registry = registry_with("e1");
    registry.approve("e1", "human").expect("approved");
    let runtime = Counting::new();

    let status = registry
        .activate("e1", &[], runtime.as_ref())
        .await
        .expect("started");

    assert_eq!(status, ExtensionStatus::Running);
    assert_eq!(runtime.start_count(), 1);
    assert_eq!(
        registry.definition("e1").unwrap().approved_by.as_deref(),
        Some("human")
    );
}

#[test]
fn approving_an_unknown_extension_is_an_error() {
    let registry = ExtensionRegistry::new();
    assert!(matches!(
        registry.approve("nope", "human"),
        Err(ExtensionError::NotFound { .. })
    ));
}

// ---- services ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_service_fails_before_starting() {
    // Failing at the first call would surface the problem in whatever the extension was doing.
    let registry = ExtensionRegistry::new();
    registry
        .define(definition("e1").requiring("tools"))
        .expect("defined");
    registry.approve("e1", "human").expect("approved");
    let runtime = Counting::new();

    let error = registry
        .activate("e1", &[], runtime.as_ref())
        .await
        .expect_err("missing service");

    let ExtensionError::MissingServices { missing, .. } = error else {
        panic!("expected MissingServices, got {error:?}");
    };
    assert_eq!(missing, vec!["tools"]);
    assert_eq!(runtime.start_count(), 0, "nothing was started");
    assert!(matches!(
        registry.status("e1"),
        Some(ExtensionStatus::Failed { .. })
    ));
}

// ---- activation and stopping ------------------------------------------------------------------

#[tokio::test]
async fn activating_twice_is_refused() {
    let registry = registry_with("e1");
    registry.approve("e1", "human").expect("approved");
    let runtime = Counting::new();
    registry
        .activate("e1", &[], runtime.as_ref())
        .await
        .expect("started");

    let error = registry
        .activate("e1", &[], runtime.as_ref())
        .await
        .expect_err("already running");

    assert!(matches!(error, ExtensionError::AlreadyRunning { .. }));
}

#[tokio::test]
async fn deactivating_calls_the_stopper() {
    // The bug this test exists for: `activate` once discarded the returned run, leaving an
    // extension that could never be stopped.
    let registry = registry_with("e1");
    registry.approve("e1", "human").expect("approved");
    let runtime = Counting::new();
    registry
        .activate("e1", &[], runtime.as_ref())
        .await
        .expect("started");

    let status = registry.deactivate("e1").expect("stopped");

    assert_eq!(status, ExtensionStatus::Stopped);
    assert_eq!(runtime.stop_count(), 1, "the stopper must be called");
}

#[tokio::test]
async fn deactivating_twice_calls_the_stopper_once() {
    let registry = registry_with("e1");
    registry.approve("e1", "human").expect("approved");
    let runtime = Counting::new();
    registry
        .activate("e1", &[], runtime.as_ref())
        .await
        .expect("started");

    registry.deactivate("e1").expect("stopped");
    registry.deactivate("e1").expect("stopped again");

    assert_eq!(runtime.stop_count(), 1, "a second stop is a no-op");
}

#[tokio::test]
async fn removing_a_running_extension_stops_it_first() {
    // Otherwise its tools stay registered and its timers alive with nothing left that could stop
    // them.
    let registry = registry_with("e1");
    registry.approve("e1", "human").expect("approved");
    let runtime = Counting::new();
    registry
        .activate("e1", &[], runtime.as_ref())
        .await
        .expect("started");

    registry.remove("e1").expect("removed");

    assert_eq!(runtime.stop_count(), 1);
    assert!(registry.ids().is_empty());
}

#[tokio::test]
async fn a_runtime_failure_marks_the_extension_failed() {
    let registry = registry_with("e1");
    registry.approve("e1", "human").expect("approved");
    let runtime = Arc::new(Counting {
        starts: Arc::new(AtomicU32::new(0)),
        stops: Arc::new(AtomicU32::new(0)),
        fail: true,
    });

    let error = registry
        .activate("e1", &[], runtime.as_ref())
        .await
        .expect_err("load failed");

    assert!(matches!(error, ExtensionError::LoadFailed { .. }));
    assert!(matches!(
        registry.status("e1"),
        Some(ExtensionStatus::Failed { .. })
    ));
    assert!(!registry.running().contains(&"e1".to_owned()));
}

#[test]
fn stopping_an_extension_that_was_never_started_is_a_success() {
    // The caller wanted it stopped, and it is.
    let registry = registry_with("e1");
    assert_eq!(
        registry.deactivate("e1").expect("stopped"),
        ExtensionStatus::Stopped
    );
}

#[test]
fn deactivating_an_unknown_extension_is_an_error() {
    let registry = ExtensionRegistry::new();
    assert!(matches!(
        registry.deactivate("nope"),
        Err(ExtensionError::NotFound { .. })
    ));
}

// ---- listing ----------------------------------------------------------------------------------

#[tokio::test]
async fn running_lists_only_started_extensions() {
    let registry = ExtensionRegistry::new();
    for id in ["a", "b"] {
        registry.define(definition(id)).expect("defined");
        registry.approve(id, "human").expect("approved");
    }
    let runtime = Counting::new();
    registry
        .activate("a", &[], runtime.as_ref())
        .await
        .expect("started");

    assert_eq!(registry.running(), vec!["a"]);
}

#[test]
fn status_wire_names_are_stable() {
    assert_eq!(ExtensionStatus::Stopped.as_str(), "stopped");
    assert_eq!(ExtensionStatus::Running.as_str(), "running");
    assert_eq!(
        ExtensionStatus::Failed {
            reason: "x".to_owned()
        }
        .as_str(),
        "failed"
    );
}

#[test]
fn the_debug_form_lists_ids_not_runs() {
    let registry = registry_with("alpha");
    let text = format!("{registry:?}");

    assert!(text.contains("alpha"));
    assert!(text.contains("Registry"));
}
