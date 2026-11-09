//! Tests for the subagent provider seam (M4 of the v2 plan).
//!
//! The property that matters is that a request is refused **before** a child exists. Once a child has
//! a session and possibly a process, unwinding a refusal is far worse than refusing up front.

use std::sync::Arc;

use girder_core::{
    ChildHandle, Depth, ProviderRegistry, SessionId, StartError, StartRequest,
    SubagentCapabilities, SubagentProvider,
};

/// A backend that records that it was called and reports the capabilities it was built with.
struct TestProvider {
    name: String,
    capabilities: SubagentCapabilities,
    started: Arc<std::sync::atomic::AtomicU32>,
}

impl TestProvider {
    fn new(name: &str, capabilities: SubagentCapabilities) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_owned(),
            capabilities,
            started: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        })
    }
}

impl SubagentProvider for TestProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> SubagentCapabilities {
        self.capabilities
    }

    fn start(
        &self,
        _request: StartRequest,
        parent: &SessionId,
        depth: Depth,
    ) -> girder_core::BoxFut<Result<ChildHandle, StartError>> {
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let parent = parent.to_string();
        Box::pin(async move {
            Ok(ChildHandle {
                session_id: SessionId::new(format!("{parent}-child")).expect("valid"),
                provider: "test".to_owned(),
                depth,
                stop: Box::new(|| {}),
            })
        })
    }
}

fn parent() -> SessionId {
    SessionId::new("parent-1").expect("valid")
}

// ---- capabilities -----------------------------------------------------------------------------

#[test]
fn a_full_backend_declares_every_capability() {
    let caps = SubagentCapabilities::full();

    assert!(caps.agent_options);
    assert!(caps.agent_preset);
    assert!(caps.output_schema);
    assert!(caps.depth_limit);
    assert!(caps.steering);
}

#[test]
fn a_one_shot_backend_does_not_declare_steering() {
    // An external execution has no message channel back into the child.
    let caps = SubagentCapabilities::one_shot();

    assert!(!caps.steering);
    assert!(caps.depth_limit);
}

#[test]
fn a_plain_request_needs_no_capability() {
    // A backend with none of the four declared can still run a task.
    let request = StartRequest::new("do something");

    assert!(request.needs().is_empty());
}

#[test]
fn a_request_lists_what_it_needs() {
    let request = StartRequest::new("x")
        .with_preset("reviewer")
        .with_output_schema(serde_json::json!({"type": "object"}))
        .with_max_depth(2);

    let needs = request.needs();
    assert!(needs.contains(&"agent_preset"));
    assert!(needs.contains(&"output_schema"));
    assert!(needs.contains(&"depth_limit"));
}

// ---- the registry -----------------------------------------------------------------------------

#[test]
fn a_duplicate_name_is_refused() {
    let mut registry = ProviderRegistry::new();
    registry
        .register(TestProvider::new("a", SubagentCapabilities::full()))
        .expect("registered");

    let error = registry
        .register(TestProvider::new("a", SubagentCapabilities::full()))
        .expect_err("duplicate");

    assert_eq!(error, "a");
}

#[test]
fn names_are_sorted() {
    let mut registry = ProviderRegistry::new();
    for name in ["zeta", "alpha"] {
        registry
            .register(TestProvider::new(name, SubagentCapabilities::full()))
            .unwrap();
    }

    assert_eq!(registry.names(), vec!["alpha", "zeta"]);
}

#[tokio::test]
async fn an_unknown_provider_is_named_in_the_error() {
    let registry = ProviderRegistry::new();
    let error = registry
        .start("nope", StartRequest::new("x"), &parent(), Depth::root(), 3)
        .await
        .expect_err("unknown");

    let StartError::UnknownProvider { name } = error else {
        panic!("expected UnknownProvider, got {error:?}");
    };
    assert_eq!(name, "nope");
}

// ---- capability checking ----------------------------------------------------------------------

#[test]
fn a_request_for_an_undeclared_capability_is_refused() {
    // A caller that assumed the capability would send a request the backend silently ignores.
    let caps = SubagentCapabilities {
        output_schema: false,
        ..SubagentCapabilities::full()
    };
    let request = StartRequest::new("x").with_output_schema(serde_json::json!({}));

    let error = ProviderRegistry::check("backend", caps, &request).expect_err("unsupported");

    let StartError::Unsupported { name, capability } = error else {
        panic!("expected Unsupported, got {error:?}");
    };
    assert_eq!(name, "backend");
    assert_eq!(capability, "output_schema");
}

#[test]
fn a_request_within_the_capabilities_is_accepted() {
    let caps = SubagentCapabilities::full();
    let request = StartRequest::new("x").with_max_depth(2);

    ProviderRegistry::check("backend", caps, &request).expect("supported");
}

#[test]
fn a_plain_request_is_accepted_by_a_backend_with_no_capabilities() {
    let caps = SubagentCapabilities::default();

    ProviderRegistry::check("bare", caps, &StartRequest::new("x"))
        .expect("a plain task needs nothing");
}

#[test]
fn the_first_missing_capability_is_reported() {
    // Deterministic so an error message does not change between runs.
    let caps = SubagentCapabilities::default();
    let request = StartRequest::new("x").with_preset("p");

    let error = ProviderRegistry::check("b", caps, &request).expect_err("unsupported");
    let StartError::Unsupported { capability, .. } = error else {
        panic!("expected Unsupported");
    };
    assert_eq!(capability, "agent_preset");
}

// ---- refusal before a child exists ------------------------------------------------------------

#[tokio::test]
async fn a_refused_request_never_reaches_the_backend() {
    // The whole point of checking first: a refusal costs nothing.
    let caps = SubagentCapabilities {
        output_schema: false,
        ..SubagentCapabilities::full()
    };
    let provider = TestProvider::new("limited", caps);
    let started = Arc::clone(&provider.started);
    let mut registry = ProviderRegistry::new();
    registry.register(provider).unwrap();

    let error = registry
        .start(
            "limited",
            StartRequest::new("x").with_output_schema(serde_json::json!({})),
            &parent(),
            Depth::root(),
            3,
        )
        .await
        .expect_err("refused");

    assert!(matches!(error, StartError::Unsupported { .. }));
    assert_eq!(
        started.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the backend must not have been called"
    );
}

#[tokio::test]
async fn a_start_beyond_the_ceiling_is_refused_before_the_backend() {
    let provider = TestProvider::new("full", SubagentCapabilities::full());
    let started = Arc::clone(&provider.started);
    let mut registry = ProviderRegistry::new();
    registry.register(provider).unwrap();

    let error = registry
        .start(
            "full",
            StartRequest::new("x"),
            &parent(),
            Depth::root().child().child(),
            1,
        )
        .await
        .expect_err("too deep");

    let StartError::TooDeep { depth, ceiling } = error else {
        panic!("expected TooDeep, got {error:?}");
    };
    assert_eq!((depth, ceiling), (2, 1));
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 0);
}

// ---- a successful start -----------------------------------------------------------------------

#[tokio::test]
async fn a_successful_start_returns_a_handle() {
    let provider = TestProvider::new("full", SubagentCapabilities::full());
    let started = Arc::clone(&provider.started);
    let mut registry = ProviderRegistry::new();
    registry.register(provider).unwrap();

    let handle = registry
        .start(
            "full",
            StartRequest::new("task"),
            &parent(),
            Depth::root(),
            3,
        )
        .await
        .expect("started");

    assert_eq!(handle.session_id.as_str(), "parent-1-child");
    assert_eq!(handle.depth.get(), 0);
    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_handle_carries_a_stop() {
    // Ownership transferred with the handle: the caller no longer needs the provider to stop it.
    let mut registry = ProviderRegistry::new();
    registry
        .register(TestProvider::new("full", SubagentCapabilities::full()))
        .unwrap();
    let handle = registry
        .start("full", StartRequest::new("x"), &parent(), Depth::root(), 3)
        .await
        .unwrap();

    handle.stop();
}

#[tokio::test]
async fn two_backends_coexist_and_the_named_one_runs() {
    // An in-process fork and a remote agent satisfy the same need with different tradeoffs.
    let fork = TestProvider::new("fork", SubagentCapabilities::full());
    let remote = TestProvider::new("remote", SubagentCapabilities::one_shot());
    let fork_started = Arc::clone(&fork.started);
    let remote_started = Arc::clone(&remote.started);
    let mut registry = ProviderRegistry::new();
    registry.register(fork).unwrap();
    registry.register(remote).unwrap();

    registry
        .start(
            "remote",
            StartRequest::new("x"),
            &parent(),
            Depth::root(),
            3,
        )
        .await
        .expect("started");

    assert_eq!(fork_started.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(remote_started.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn a_debug_registry_lists_names_not_handles() {
    let mut registry = ProviderRegistry::new();
    registry
        .register(TestProvider::new("alpha", SubagentCapabilities::full()))
        .unwrap();

    let text = format!("{registry:?}");
    assert!(text.contains("alpha"));
    assert!(text.contains("ProviderRegistry"));
}

#[test]
fn a_handle_debug_does_not_print_the_stop_closure() {
    // A closure has no useful debug form; naming it is enough.
    let handle = ChildHandle {
        session_id: SessionId::new("c1").expect("valid"),
        provider: "p".to_owned(),
        depth: Depth::root(),
        stop: Box::new(|| {}),
    };

    let text = format!("{handle:?}");
    assert!(text.contains("ChildHandle"));
    assert!(text.contains("c1"));
}
