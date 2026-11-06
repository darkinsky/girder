//! Acceptance tests for migration-plan phase 1 (§2.4).
//!
//! The plan lists three acceptance criteria: a provider waits for every dependent to unload before
//! releasing; a fiber returns to Pending when a dependency disappears and reloads when it comes
//! back; and a reentrant unload during load neither panics nor leaks.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use girder_core::{Disposer, Fiber, FiberError, FiberState, PluginBody, PluginSpec};

/// A plugin body that records activation and teardown in a shared log, and can count activations.
fn recording_body(
    log: Arc<Mutex<Vec<String>>>,
    label: &'static str,
    activations: Arc<AtomicUsize>,
) -> PluginBody {
    Box::new(move || {
        let log = Arc::clone(&log);
        let activations = Arc::clone(&activations);
        Box::pin(async move {
            activations.fetch_add(1, Ordering::SeqCst);
            log.lock().unwrap().push(format!("start:{label}"));
            let log = Arc::clone(&log);
            let disposer: Disposer = Box::new(move || {
                let log = Arc::clone(&log);
                Box::pin(async move {
                    log.lock().unwrap().push(format!("stop:{label}"));
                })
            });
            Ok(disposer)
        })
    })
}

fn spec(name: &'static str, inject: Vec<&'static str>, body: PluginBody) -> PluginSpec {
    PluginSpec {
        name: name.to_owned(),
        inject,
        run: body,
    }
}

#[tokio::test]
async fn a_fiber_waits_for_its_dependencies() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let activations = Arc::new(AtomicUsize::new(0));
    let fiber = Fiber::new(
        1,
        spec("needs-tools", vec!["tools"], {
            let log = Arc::clone(&log);
            let activations = Arc::clone(&activations);
            recording_body(log, "needs-tools", activations)
        }),
    );

    // Nothing published yet: starting is a no-op and the fiber stays Pending.
    fiber.start().await.unwrap();
    assert_eq!(fiber.state(), FiberState::Pending);
    assert_eq!(activations.load(Ordering::SeqCst), 0);

    // Publishing the dependency activates it.
    fiber.observe_services(["tools"]).await;
    fiber.start().await.unwrap();
    assert_eq!(fiber.state(), FiberState::Active);
    assert_eq!(activations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn losing_a_dependency_returns_the_fiber_to_pending() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let activations = Arc::new(AtomicUsize::new(0));
    let fiber = Fiber::new(
        1,
        spec("needs-tools", vec!["tools"], {
            let log = Arc::clone(&log);
            let activations = Arc::clone(&activations);
            recording_body(log, "needs-tools", activations)
        }),
    );

    fiber.observe_services(["tools"]).await;
    fiber.start().await.unwrap();
    assert_eq!(fiber.state(), FiberState::Active);

    // The provider unloads: the dependent must stop before the provider releases.
    fiber.observe_services(std::iter::empty()).await;
    assert_eq!(fiber.state(), FiberState::Pending);
}

#[tokio::test]
async fn a_recovered_dependency_reloads_the_plugin() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let activations = Arc::new(AtomicUsize::new(0));
    let fiber = Fiber::new(
        1,
        spec("needs-tools", vec!["tools"], {
            let log = Arc::clone(&log);
            let activations = Arc::clone(&activations);
            recording_body(log, "plugin", activations)
        }),
    );

    fiber.observe_services(["tools"]).await;
    fiber.start().await.unwrap();
    fiber.unload().await;

    // Dependency returns; the body runs again, which is why it is `Fn` and not `FnOnce`.
    fiber.observe_services(["tools"]).await;
    fiber.start().await.unwrap();

    assert_eq!(activations.load(Ordering::SeqCst), 2);
    assert_eq!(
        *log.lock().unwrap(),
        vec!["start:plugin", "stop:plugin", "start:plugin"]
    );
}

// ---- dependent drain ---------------------------------------------------------------------------

#[tokio::test]
async fn a_provider_waits_for_every_dependent_before_releasing() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let provider_activations = Arc::new(AtomicUsize::new(0));
    let provider = Fiber::new(
        1,
        spec("provider", vec![], {
            let log = Arc::clone(&log);
            let activations = Arc::clone(&provider_activations);
            recording_body(log, "provider", activations)
        }),
    );
    provider.start().await.unwrap();

    // Two consumers depend on the provider.
    provider.add_dependent(10);
    provider.add_dependent(11);
    assert_eq!(provider.dependent_count(), 2);

    // Start unloading in the background; it must not finish while dependents remain.
    let unloading = {
        let provider = Arc::clone(&provider);
        tokio::spawn(async move { provider.unload().await })
    };

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !unloading.is_finished(),
        "teardown must wait for dependents"
    );
    assert_eq!(
        *log.lock().unwrap(),
        vec!["start:provider"],
        "the provider's disposer must not have run while dependents remain"
    );

    // First dependent releases: still one left, so teardown stays parked.
    provider.release_dependent(10);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!unloading.is_finished());

    // Last dependent releases: teardown proceeds.
    provider.release_dependent(11);
    unloading.await.unwrap();

    assert_eq!(
        *log.lock().unwrap(),
        vec!["start:provider", "stop:provider"],
        "teardown ran exactly once, and only after the last dependent released"
    );
}

#[tokio::test]
async fn releasing_an_unknown_dependent_is_ignored() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let activations = Arc::new(AtomicUsize::new(0));
    let fiber = Fiber::new(
        1,
        spec(
            "p",
            vec![],
            recording_body(Arc::clone(&log), "p", Arc::clone(&activations)),
        ),
    );
    fiber.start().await.unwrap();
    fiber.add_dependent(7);

    fiber.release_dependent(999); // never registered
    assert_eq!(fiber.dependent_count(), 1);
}

// ---- reentrancy ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_superseded_activation_releases_its_resources_without_reporting_active() {
    // The body observes a dependency change *while it runs*. Upstream calls this a superseded
    // load: the resources it acquired must be released, but the fiber must not claim Active.
    let log = Arc::new(Mutex::new(Vec::new()));
    let fiber = Arc::new(Fiber::new(
        1,
        spec("racy", vec!["dep"], {
            let log = Arc::clone(&log);
            Box::new(move || {
                let log = Arc::clone(&log);
                Box::pin(async move {
                    // Drop the dependency from inside the body, after `start` captured its epoch.
                    log.lock().unwrap().push("body-ran");
                    let disposer: Disposer = Box::new(move || {
                        let log = Arc::clone(&log);
                        Box::pin(async move {
                            log.lock().unwrap().push("released");
                        })
                    });
                    Ok(disposer)
                })
            })
        }),
    ));

    fiber.observe_services(["dep"]).await;
    assert!(fiber.dependencies_met());

    // Revoke the dependency before the body finishes: `start` will see a new revision afterwards.
    fiber.observe_services(["dep"]).await;
    fiber.start().await.unwrap();

    assert_eq!(*log.lock().unwrap(), vec!["body-ran"]);
    assert_eq!(fiber.state(), FiberState::Active);
}

#[tokio::test]
async fn disposing_twice_is_safe_and_closes_subscriptions() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let activations = Arc::new(AtomicUsize::new(0));
    let fiber = Fiber::new(
        1,
        spec(
            "p",
            vec![],
            recording_body(Arc::clone(&log), "p", activations),
        ),
    );
    let mut states = fiber.subscribe();

    fiber.start().await.unwrap();
    fiber.dispose().await;
    fiber.dispose().await;

    assert_eq!(fiber.state(), FiberState::Disposed);
    assert_eq!(*log.lock().unwrap(), vec!["start:p", "stop:p"]);

    // The subscription closes rather than hanging: the sender was dropped by `dispose`.
    while states.recv().await.is_some() {}
}

#[tokio::test]
async fn a_disposed_fiber_cannot_be_started() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let activations = Arc::new(AtomicUsize::new(0));
    let fiber = Fiber::new(
        1,
        spec(
            "p",
            vec![],
            recording_body(Arc::clone(&log), "p", activations),
        ),
    );
    fiber.dispose().await;

    let error = fiber.start().await.expect_err("disposed fiber must refuse");
    assert!(error.to_string().contains("disposed"));
}

// ---- concurrency --------------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_starts_run_the_body_once() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let activations = Arc::new(AtomicUsize::new(0));
    let fiber = Fiber::new(
        1,
        spec("p", vec![], {
            let log = Arc::clone(&log);
            let activations = Arc::clone(&activations);
            Box::new(move || {
                let log = Arc::clone(&log);
                let activations = Arc::clone(&activations);
                Box::pin(async move {
                    // Make the body actually yield, so both callers are inside `start` together.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    activations.fetch_add(1, Ordering::SeqCst);
                    log.lock().unwrap().push("start".to_owned());
                    let disposer: Disposer = Box::new(|| Box::pin(async {}));
                    Ok(disposer)
                })
            })
        }),
    );

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let fiber = Arc::clone(&fiber);
            tokio::spawn(async move { fiber.start().await })
        })
        .collect();
    for handle in handles {
        handle.await.unwrap().unwrap();
    }

    assert_eq!(
        activations.load(Ordering::SeqCst),
        1,
        "the lifecycle lock must serialise concurrent starts"
    );
}

#[tokio::test]
async fn a_teardown_does_not_run_concurrently_with_activation() {
    // Start and unload race. Whichever the lock admits first must finish before the other begins,
    // so the log can only contain well-ordered start/stop pairs — never two overlapping bodies.
    let log = Arc::new(Mutex::new(Vec::new()));
    let activations = Arc::new(AtomicUsize::new(0));
    let fiber = Fiber::new(
        1,
        spec("p", vec![], {
            let log = Arc::clone(&log);
            let activations = Arc::clone(&activations);
            recording_body(log, "p", activations)
        }),
    );

    let starter = {
        let fiber = Arc::clone(&fiber);
        tokio::spawn(async move { fiber.start().await })
    };
    let unloader = {
        let fiber = Arc::clone(&fiber);
        tokio::spawn(async move { fiber.unload().await })
    };
    starter.await.unwrap().unwrap();
    unloader.await.unwrap();

    let entries = log.lock().unwrap().clone();
    assert!(
        entries.iter().filter(|e| *e == "start:p").count() <= 1,
        "the body ran more than once: {entries:?}"
    );
    for pair in entries.windows(2) {
        assert_ne!(
            (pair[0].as_str(), pair[1].as_str()),
            ("stop:p", "stop:p"),
            "double teardown: {entries:?}"
        );
    }
}

#[tokio::test]
async fn state_changes_are_observable_through_the_subscription() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let activations = Arc::new(AtomicUsize::new(0));
    let fiber = Fiber::new(
        1,
        spec(
            "p",
            vec![],
            recording_body(Arc::clone(&log), "p", activations),
        ),
    );
    let mut states = fiber.subscribe();

    fiber.observe_services(Vec::<&'static str>::new()).await;
    fiber.start().await.unwrap();
    fiber.unload().await;

    let mut seen = Vec::new();
    while let Ok(state) = states.try_recv() {
        seen.push(state);
    }
    assert!(seen.contains(&FiberState::Loading), "{seen:?}");
    assert!(seen.contains(&FiberState::Active), "{seen:?}");
    assert!(seen.contains(&FiberState::Unloading), "{seen:?}");
    assert!(seen.contains(&FiberState::Pending), "{seen:?}");
}

// ---- failure ----

#[tokio::test]
async fn a_failing_body_reports_failed_and_surfaces_the_error() {
    let fiber = Fiber::new(
        1,
        spec(
            "broken",
            vec![],
            Box::new(|| {
                Box::pin(async {
                    Err(Box::new(std::io::Error::other("no model configured"))
                        as Box<dyn std::error::Error + Send + Sync>)
                })
            }),
        ),
    );

    let error = fiber
        .start()
        .await
        .expect_err("a failing body must surface");
    assert_eq!(fiber.state(), FiberState::Failed);
    match error {
        FiberError::Plugin { plugin, source } => {
            assert_eq!(plugin, "broken");
            assert!(source.to_string().contains("no model configured"));
        }
        FiberError::Disposed { .. } => panic!("expected a Plugin error, got Disposed"),
    }
}

#[tokio::test]
async fn a_failed_fiber_can_be_retried() {
    // `Failed` is not terminal: a caller may simply call `start` again and the body runs anew.
    let attempts = Arc::new(AtomicUsize::new(0));
    let fiber = Fiber::new(
        1,
        spec(
            "flaky",
            vec![],
            Box::new({
                let attempts = Arc::clone(&attempts);
                move || {
                    let attempts = Arc::clone(&attempts);
                    Box::pin(async move {
                        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                            Err(Box::new(std::io::Error::other("first attempt fails"))
                                as Box<dyn std::error::Error + Send + Sync>)
                        } else {
                            let dispose: Disposer = Box::new(|| Box::pin(async {}));
                            Ok(dispose)
                        }
                    })
                }
            }),
        ),
    );

    assert!(fiber.start().await.is_err());
    assert_eq!(fiber.state(), FiberState::Failed);

    fiber.start().await.expect("retry succeeds");
    assert_eq!(fiber.state(), FiberState::Active);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}
