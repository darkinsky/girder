//! The dependent-drain wake-up under concurrency.
//!
//! Phase 1 replaces upstream's spin loop with a [`Notify`](tokio::sync::Notify). That has a real
//! race: a wake delivered between registering interest and awaiting the notification is only safe
//! because `notified()` is registered *before* the condition is checked. A lost wake-up would hang
//! teardown forever, and a hang is exactly what a unit test cannot catch by asserting `Err` — it
//! times out or blocks.
//!
//! So this drives the window hard: many dependents releasing concurrently, many rounds, with a
//! timeout that fails the round instead of hanging the run.

use std::sync::Arc;
use std::time::Duration;

use girder_core::{Disposer, Fiber, FiberState, PluginSpec};

/// Rounds to run. High enough that a lost wake-up shows up within a CI run, low enough that the
/// test stays fast — the whole file finishes in well under a second on a laptop.
const ROUNDS: usize = 200;

/// Concurrent dependents per round.
const DEPENDENTS: u64 = 16;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_dependents_all_drain() {
    for round in 0..ROUNDS {
        // Fresh body per round so each fiber owns its own disposer.
        let fiber = Fiber::new(
            1,
            PluginSpec {
                name: "p".to_owned(),
                inject: vec![],
                run: Box::new(|| {
                    Box::pin(async {
                        let dispose: Disposer = Box::new(|| Box::pin(async {}));
                        Ok(dispose)
                    })
                }),
            },
        );
        fiber.start().await.unwrap();

        for uid in 0..DEPENDENTS {
            fiber.add_dependent(uid);
        }

        let unloader = {
            let fiber = Arc::clone(&fiber);
            tokio::spawn(async move { fiber.unload().await })
        };

        let releasers: Vec<_> = (0..DEPENDENTS)
            .map(|uid| {
                let fiber = Arc::clone(&fiber);
                tokio::spawn(async move { fiber.release_dependent(uid) })
            })
            .collect();
        for releaser in releasers {
            releaser.await.unwrap();
        }

        // Completion is the assertion: if a notification were lost, teardown would never finish.
        tokio::time::timeout(Duration::from_secs(2), unloader)
            .await
            .unwrap_or_else(|_| panic!("round {round}: teardown hung, so a wake-up was lost"))
            .unwrap();
        assert_eq!(fiber.state(), FiberState::Pending);
    }
}
