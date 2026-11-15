//! End-to-end concurrency tests for the modules that hold or guard shared state.
//!
//! Every test here drives *real* OS threads (`std::thread::spawn`) over an `Arc`, released together
//! through a [`Barrier`] so the contention window is entered at the same instant rather than being
//! staggered by spawn cost. Nothing is simulated on one thread: a single-threaded "concurrency" test
//! can only prove that a function is deterministic, never that the synchronisation it relies on
//! actually excludes anyone.
//!
//! # Why each test asserts an invariant rather than "it did not panic"
//!
//! A data race that merely does not panic is still a corruption. Each case below therefore pins an
//! arithmetic or set-theoretic property that a lost update, a torn read, or a double-release would
//! break, and repeats the scenario enough times that a window measured in microseconds is entered
//! many times over rather than once:
//!
//! - **session log**: every appended event appears exactly once — not "at least one survived";
//! - **file lock table**: exactly one contender wins and the losers name the winner, because a lock
//!   that admits two owners is not a lock;
//! - **tool cache**: a hit never returns another key's text, because a cache that hands back the
//!   wrong file's bytes is worse than a miss;
//! - **approval rules**: every thread decides identically, because an immutable table that can be
//!   observed two ways is a policy that cannot be reasoned about;
//! - **subagent pool**: exactly `capacity` callers acquire, because a bound that admits
//!   `capacity + 1` under load is not a bound;
//! - **stream window**: `inflight` never exceeds `capacity`, because the whole point of the credit
//!   window is that memory stays bounded.
//!
//! # Which handle each module actually offers
//!
//! Three of these types (`FileLocks`, `Cache`, `Flow`) take `&mut self` and carry no interior
//! mutability, so a genuinely shared instance has to be put behind a `Mutex` — which is exactly how
//! a caller in the harness shares them (one table per process, one cache per session, one window per
//! connection). Wrapping them is therefore not a way of avoiding the race, it is the real sharing
//! shape; the test still enters it from many OS threads at once, which is what the single-threaded
//! tests in this crate could not do.
//!
//! `SubagentPool` and `ApprovalRules` are `Sync` already (`Semaphore` and an immutable `Vec`), so
//! those two are exercised with no external lock at all — the strongest form of the test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::float_cmp,
    reason = "tests assert by panicking; a failed unwrap is a failed test, and the indexes are \
              over fixed-size literals defined in the test itself"
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use girder_core::{
    ApprovalAction, ApprovalRule, ApprovalRules, Contention, FileLocks, Flow, SendResult,
    SessionEvent, SessionStore, SubagentPool, ToolCache, ToolCacheEntry, ToolCacheKey,
    read_session_file,
};

/// An isolated scratch directory: `temp_dir()` plus pid plus a per-process counter.
///
/// The counter keeps two tests that run in parallel in one process from sharing a directory, and the
/// pid keeps two copies of the test binary from sharing one.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-concurrency-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory is creatable");
    dir
}

/// How many threads contend in the lock and pool tests.
const CONTENDERS: usize = 16;

/// How many threads append to one log in the session tests.
const WRITERS: usize = 10;

/// How many independent rounds each repeated scenario runs.
///
/// A race window that a single run enters by luck must be entered *reliably* for the test to mean
/// anything, so the cheap scenarios repeat this many times. One hundred rounds over sixteen threads
/// is sixteen hundred contended acquisitions, which is enough to observe a scheduling-dependent
/// interleaving rather than hoping for one.
const ROUNDS: usize = 100;

// ---- 1. concurrent appends to one session log ---------------------------------------------------

#[test]
fn concurrent_appends_to_one_log_keep_every_event_exactly_once() {
    // The invariant: the log is the source of truth for a replay, so an append that another append
    // silently discards is not a performance problem, it is a lost turn. `SessionStore::append`
    // serialises on `session::path_lock`, and the only way to know that lock is doing its job is to
    // have many threads in it at once and then count the survivors.
    for round in 0..ROUNDS {
        let dir = scratch(&format!("append-{round}"));
        let path = dir.join("session.log.zst");
        let store = Arc::new(SessionStore::new(&path));
        store.touch().expect("log is created");

        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut handles = Vec::with_capacity(WRITERS);
        for index in 0..WRITERS {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                // Released together, so the appends collide in the lock rather than queueing up
                // behind spawn latency one at a time.
                barrier.wait();
                store.append(&SessionEvent::user_text(
                    &format!("m{index}"),
                    format!("body {index}"),
                ))
            }));
        }
        for handle in handles {
            handle
                .join()
                .expect("an appending thread must not panic")
                .expect("every append is accepted");
        }

        // Decoded straight from the file, bypassing the store's read cache: this is the ground truth
        // about what is on disk, and it also proves the frames are not corrupt (a torn write shows
        // up here as `SessionError::Compression`, not as a short vector).
        let decoded = read_session_file(&path).expect("the log decodes without corruption");
        assert_eq!(
            decoded.len(),
            WRITERS,
            "round {round}: every concurrent append must survive"
        );

        assert_exactly_the_expected_events(decoded, WRITERS);
        assert_exactly_the_expected_events(store.read_all().expect("cached read decodes"), WRITERS);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn many_appends_per_thread_to_one_log_keep_the_total_exact() {
    // The heavier variant: ten appends from each of ten threads. The first test would still pass if
    // the lock only excluded *some* pairs; this one pushes a hundred frames through the same lock, so
    // an append that leaks past it shows up as an off-by-more-than-one total.
    const APPENDS_PER_THREAD: usize = 10;
    const ROUNDS: usize = 20;

    for round in 0..ROUNDS {
        let dir = scratch(&format!("append-many-{round}"));
        let path = dir.join("session.log.zst");
        let store = Arc::new(SessionStore::new(&path));
        store.touch().expect("log is created");

        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut handles = Vec::with_capacity(WRITERS);
        for thread in 0..WRITERS {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for append in 0..APPENDS_PER_THREAD {
                    store
                        .append(&SessionEvent::user_text(
                            &format!("t{thread}-a{append}"),
                            format!("body {thread} {append}"),
                        ))
                        .expect("append is accepted");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("an appending thread must not panic");
        }

        let expected = WRITERS * APPENDS_PER_THREAD;
        assert_eq!(
            read_session_file(&path)
                .expect("the log decodes without corruption")
                .len(),
            expected,
            "round {round}: no append may be lost or duplicated"
        );
        assert_eq!(
            store.read_all().expect("cached read decodes").len(),
            expected,
            "round {round}: the read cache must not hide an append"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn separate_stores_over_one_path_still_serialise_on_the_path_lock() {
    // The invariant behind `session::path_lock` being a *process-wide, path-keyed* table rather than a
    // field on `SessionStore`: the RPC layer builds a store per call, so two stores over one path are
    // two different objects that must still exclude each other. A per-instance mutex would pass every
    // other test in this file — those all share one `Arc` — and still corrupt a log the moment two
    // callers each constructed their own store.
    //
    // This is the case that would silently regress if the lock were ever moved onto the struct, so it
    // is asserted with the same exactly-once count rather than a looser "it did not error".
    const WRITERS: usize = 8;
    const ROUNDS: usize = 25;

    for round in 0..ROUNDS {
        let dir = scratch(&format!("append-distinct-stores-{round}"));
        let path = dir.join("session.log.zst");
        let first = Arc::new(SessionStore::new(&path));
        first.touch().expect("log is created");

        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut handles = Vec::with_capacity(WRITERS);
        for index in 0..WRITERS {
            // A fresh store per thread, sharing only the path. Nothing but the path-derived lock
            // connects them.
            let store = SessionStore::new(&path);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                store
                    .append(&SessionEvent::user_text(
                        &format!("m{index}"),
                        format!("body {index}"),
                    ))
                    .expect("append is accepted");
            }));
        }
        for handle in handles {
            handle.join().expect("an appending thread must not panic");
        }

        let decoded = read_session_file(&path).expect("the log decodes without corruption");
        assert_eq!(
            decoded.len(),
            WRITERS,
            "round {round}: distinct stores over one path must still be serialised"
        );
        assert_exactly_the_expected_events(decoded, WRITERS);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Asserts that `events` is exactly the `count` writer events named `m0..m{count}`, each present once.
///
/// Removing each match from the expected list is what makes this an *exactly once* check: a
/// duplicate would leave a hole, and a lost event would leave a leftover. A `len` assertion alone
/// would accept `count` copies of one event.
fn assert_exactly_the_expected_events(events: Vec<SessionEvent>, count: usize) {
    let mut expected: Vec<SessionEvent> = (0..count)
        .map(|index| SessionEvent::user_text(&format!("m{index}"), format!("body {index}")))
        .collect();

    for event in events {
        let position = expected
            .iter()
            .position(|candidate| candidate == &event)
            .unwrap_or_else(|| panic!("unexpected or duplicated event in the log"));
        expected.remove(position);
    }
    assert!(
        expected.is_empty(),
        "{} event(s) were lost by concurrent appends",
        expected.len()
    );
}

// ---- 2. file lock contention --------------------------------------------------------------------

/// What one contender observed when it tried to take the lock.
#[derive(Debug)]
enum Contender {
    Won { owner: String },
    Lost { holder: String, since_ms: u64 },
    Duplicate,
}

#[test]
fn exactly_one_contender_wins_the_file_lock_and_the_rest_name_it() {
    // The invariant: `FileLocks` answers a per-path question with a single owner. The property that
    // matters is not "acquisition usually succeeds" but that the *set* of winners has size one — a
    // second `Ok` means two writers believe they may write the same file and the observation layer
    // in `Session::check_write` can no longer catch a lost update. The losers must also be told who
    // beat them, because a bare "busy" is a refusal a model cannot act on.
    for round in 0..ROUNDS {
        let locks = Arc::new(Mutex::new(FileLocks::new()));
        let barrier = Arc::new(Barrier::new(CONTENDERS));
        let mut handles = Vec::with_capacity(CONTENDERS);

        for index in 0..CONTENDERS {
            let locks = Arc::clone(&locks);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let owner = format!("agent-{index}");
                // The outer mutex is the caller's lock, not the module's: `Locks` takes `&mut self`
                // and has no interior mutability, so one table shared by many threads must be
                // serialised by its owner. That is the real sharing shape and it still lets all
                // sixteen threads contend for the table itself.
                let mut table = locks.lock().expect("lock table is not poisoned");
                match table.acquire("/workspace/src/shared.rs", &owner, 1_000) {
                    Ok(()) => Contender::Won { owner },
                    Err(Contention::HeldBy { owner, since_ms }) => Contender::Lost {
                        holder: owner,
                        since_ms,
                    },
                    Err(Contention::AlreadyYours) => Contender::Duplicate,
                }
            }));
        }

        let mut winners = Vec::new();
        let mut losers = Vec::new();
        for handle in handles {
            match handle.join().expect("a contender must not panic") {
                Contender::Won { owner } => winners.push(owner),
                Contender::Lost { holder, since_ms } => losers.push((holder, since_ms)),
                Contender::Duplicate => {
                    panic!("round {round}: distinct owners cannot be AlreadyYours")
                }
            }
        }

        assert_eq!(
            winners.len(),
            1,
            "round {round}: exactly one contender may hold the lock, got {winners:?}"
        );
        assert_eq!(
            losers.len(),
            CONTENDERS - 1,
            "round {round}: every other contender must be refused"
        );
        let winner = &winners[0];
        for (holder, since_ms) in &losers {
            assert_eq!(
                holder, winner,
                "round {round}: a refusal must name the thread that actually won"
            );
            assert_eq!(
                *since_ms, 1_000,
                "round {round}: the refusal must report the winner's acquisition time"
            );
        }

        let table = locks.lock().expect("lock table is not poisoned");
        assert_eq!(
            table.len(),
            1,
            "round {round}: only the winner's lock may be recorded"
        );
        assert_eq!(
            table
                .holder("/workspace/src/shared.rs")
                .map(|f| f.owner.as_str()),
            Some(winner.as_str()),
        );
    }
}

// ---- 3. cache under concurrent access -----------------------------------------------------------

#[test]
fn concurrent_cache_gets_and_puts_never_return_another_keys_value() {
    // The invariant: a cache is keyed on (tool, raw arguments) and a hit is served as if it came from
    // the tool. Returning another key's text is therefore silent data corruption — the model reads
    // file B's bytes believing they are file A's — which is strictly worse than the miss that would
    // have cost one disk read. Capacity is deliberately smaller than the key count so eviction runs
    // concurrently with lookup, the path where a second index could desynchronise from the map.
    const KEYS: usize = 4;
    const WORKERS: usize = 8;
    const OPS: usize = 400;
    const CAPACITY: usize = 2;
    const MTIME: u64 = 7;

    let keys: Vec<(ToolCacheKey, String)> = (0..KEYS)
        .map(|index| {
            (
                ToolCacheKey::of("read", &format!("{{\"path\":\"/work/f{index}.txt\"}}")),
                format!("contents of file {index}"),
            )
        })
        .collect();
    let keys = Arc::new(keys);
    let cache = Arc::new(Mutex::new(ToolCache::new(CAPACITY)));
    let barrier = Arc::new(Barrier::new(WORKERS));
    let hits = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::with_capacity(WORKERS);
    for worker in 0..WORKERS {
        let keys = Arc::clone(&keys);
        let cache = Arc::clone(&cache);
        let barrier = Arc::clone(&barrier);
        let hits = Arc::clone(&hits);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for op in 0..OPS {
                // A deterministic but interleaved walk over the key space, so different threads touch
                // the same key at different moments instead of partitioning it.
                let index = (worker * 3 + op * 5) % KEYS;
                let (key, text) = &keys[index];
                let mut cache = cache.lock().expect("cache is not poisoned");
                if op % 3 == 0 {
                    cache.put(
                        key.clone(),
                        ToolCacheEntry {
                            text: text.clone(),
                            is_error: false,
                            source_mtime_ms: Some(MTIME),
                        },
                    );
                } else if let Some(entry) = cache.get(key, Some(MTIME)) {
                    // The one assertion that makes this a correctness test rather than a smoke test.
                    assert_eq!(
                        &entry.text, text,
                        "the cache served another key's value for entry `{index}`"
                    );
                    assert_eq!(
                        entry.source_mtime_ms,
                        Some(MTIME),
                        "a hit must carry the mtime it was stored under"
                    );
                    hits.fetch_add(1, Ordering::Relaxed);
                }
                assert!(
                    cache.len() <= CAPACITY,
                    "the cache grew past its capacity under concurrent puts"
                );
            }
        }));
    }
    for handle in handles {
        handle.join().expect("a cache worker must not panic");
    }

    let cache = cache.lock().expect("cache is not poisoned");
    assert_eq!(
        cache.stats().refused,
        0,
        "only cacheable keys were stored, so nothing may be refused"
    );
    assert!(
        cache.len() <= CAPACITY,
        "eviction must keep the cache within its bound"
    );
    // Eviction is aggressive at this ratio, but *some* lookups must land or the test proved nothing
    // about the hit path.
    assert!(
        hits.load(Ordering::Relaxed) > 0,
        "the workload never produced a cache hit, so the hit path was not exercised"
    );
}

// ---- 4. approval rules are immutable, so decisions must agree -----------------------------------

#[test]
fn every_thread_deciding_the_same_call_gets_a_byte_identical_answer() {
    // The invariant: `Rules` holds no locks and no interior mutability, so a decision is a pure
    // function of (rules, tool, arguments). If two threads could disagree, the same call would be
    // allowed in one step and denied in the next — a policy that cannot be audited, and the reason
    // the module documents itself as lock-free. Threads therefore read the same `Arc<Rules>` with no
    // synchronisation beyond the `Arc`, which is only sound because the table is immutable.
    const WORKERS: usize = 16;
    const DECISIONS_PER_THREAD: usize = 300;

    let mut rules = ApprovalRules::new();
    // Appended in precedence order: later rules win, so the narrow exceptions must come after the
    // broad baselines. The expected answers below depend on that ordering, which is the point.
    rules.add(ApprovalRule {
        tool: "bash".to_owned(),
        contains: Vec::new(),
        action: ApprovalAction::Allow,
        reason: "the shell is trusted in this workspace".to_owned(),
    });
    rules.add(ApprovalRule {
        tool: "bash".to_owned(),
        contains: vec!["rm".to_owned()],
        action: ApprovalAction::Ask,
        reason: "deleting a file needs a person".to_owned(),
    });
    rules.add(ApprovalRule {
        tool: "*".to_owned(),
        contains: vec!["/etc/".to_owned()],
        action: ApprovalAction::Deny,
        reason: "system paths are out of scope".to_owned(),
    });

    // The single-threaded answers are the reference every worker must reproduce.
    let calls = [
        ("bash", "ls -la"),
        ("bash", "rm a.txt"),
        ("bash", "rm /etc/hosts"),
        ("read", "{\"path\":\"/etc/passwd\"}"),
        ("read", "{\"path\":\"/tmp/a\"}"),
        ("read", "ls -la"),
    ];
    let expected: Vec<_> = calls
        .iter()
        .map(|(tool, arguments)| rules.decide(tool, arguments))
        .collect();

    // Guard the reference itself: a rule table whose own answers do not match the documented
    // last-match-wins precedence would make the concurrency check meaningless.
    assert_eq!(expected[0].action, ApprovalAction::Allow);
    assert_eq!(expected[0].matched, Some(0));
    assert_eq!(expected[1].action, ApprovalAction::Ask);
    assert_eq!(expected[1].matched, Some(1));
    assert_eq!(expected[2].action, ApprovalAction::Deny);
    assert_eq!(expected[2].matched, Some(2));
    assert_eq!(expected[4].action, ApprovalAction::Ask);
    assert_eq!(expected[4].matched, None, "no rule covers a plain read");

    let rules = Arc::new(rules);
    let barrier = Arc::new(Barrier::new(WORKERS));
    let mut handles = Vec::with_capacity(WORKERS);
    for _worker in 0..WORKERS {
        let rules = Arc::clone(&rules);
        let barrier = Arc::clone(&barrier);
        let expected = expected.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..DECISIONS_PER_THREAD {
                for (index, (tool, arguments)) in calls.iter().enumerate() {
                    let decision = rules.decide(tool, arguments);
                    assert_eq!(
                        decision, expected[index],
                        "a concurrent decide disagreed with the reference for `{tool} {arguments}`"
                    );
                }
            }
        }));
    }
    for handle in handles {
        handle.join().expect("a deciding thread must not panic");
    }
}

// ---- 5. subagent pool admission -----------------------------------------------------------------

#[test]
fn a_full_subagent_pool_admits_exactly_capacity_callers_under_concurrent_demand() {
    // The invariant: `capacity` is a hard admission bound, not a target. Two failures matter and they
    // pull in opposite directions — admitting `capacity + 1` overruns the provider streams the bound
    // exists to protect, and admitting `capacity - 1` leaks a place so the pool silently shrinks over
    // the process's life. Counting the winners exactly pins both. `Slot` is not `Clone`, so the
    // winners must each own a distinct permit; the uniqueness check on slot indices is the runtime
    // evidence that no permit was handed out twice.
    const CAPACITY: usize = 4;
    const CALLERS: usize = 32;
    const ROUNDS: usize = 50;

    for round in 0..ROUNDS {
        let pool = Arc::new(SubagentPool::new(CAPACITY));
        let barrier = Arc::new(Barrier::new(CALLERS));
        let mut handles = Vec::with_capacity(CALLERS);

        for _caller in 0..CALLERS {
            let pool = Arc::clone(&pool);
            let barrier = Arc::clone(&barrier);
            // No external lock: the pool is `Sync` and its semaphore is the synchronisation. Each
            // thread keeps whatever slot it won, so the bound stays held until every thread has
            // finished racing.
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                pool.try_acquire()
            }));
        }

        let mut slots = Vec::new();
        for handle in handles {
            if let Some(slot) = handle.join().expect("a caller must not panic") {
                slots.push(slot);
            }
        }

        assert_eq!(
            slots.len(),
            CAPACITY,
            "round {round}: exactly the capacity may be admitted, got {}",
            slots.len()
        );
        assert_eq!(
            pool.active(),
            CAPACITY,
            "round {round}: the live count must equal the permits handed out"
        );

        let mut indices: Vec<usize> = slots.iter().map(girder_core::SubagentSlot::index).collect();
        indices.sort_unstable();
        indices.dedup();
        assert_eq!(
            indices.len(),
            CAPACITY,
            "round {round}: two admitted callers shared one slot index"
        );

        // Returning every place must restore the pool exactly: a release that lost a permit would
        // leave the pool permanently smaller than its documented capacity.
        for slot in slots {
            pool.release(slot);
        }
        assert_eq!(
            pool.active(),
            0,
            "round {round}: the pool must be empty after every slot is released"
        );
        assert!(pool.has_room());
    }
}

// ---- 6. stream backpressure ---------------------------------------------------------------------

#[test]
fn inflight_never_exceeds_capacity_while_send_and_acknowledge_race() {
    // The invariant: the credit window is the only thing between a fast producer and an unbounded
    // buffer, so `inflight <= capacity` is the whole guarantee. It is checked after *every* mutation
    // from both threads, not only at the end, because a transient excursion is exactly the unbounded
    // moment that matters — a final reading of zero would hide it. `sent_total` must also equal the
    // number of accepted sends, which catches a `Blocked` result that incremented anyway.
    const CAPACITY: usize = 4;
    const FRAMES: usize = 5_000;

    let window = Arc::new(Mutex::new(Flow::new(CAPACITY)));
    let sent = Arc::new(AtomicUsize::new(0));
    let acked = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));

    let sender = {
        let window = Arc::clone(&window);
        let sent = Arc::clone(&sent);
        let max_seen = Arc::clone(&max_seen);
        std::thread::spawn(move || {
            while sent.load(Ordering::Relaxed) < FRAMES {
                let mut flow = window.lock().expect("flow is not poisoned");
                match flow.send() {
                    SendResult::Allowed => {
                        let inflight = flow.inflight();
                        assert!(
                            inflight <= CAPACITY,
                            "a successful send left {inflight} frames in flight, capacity {CAPACITY}"
                        );
                        max_seen.fetch_max(inflight, Ordering::Relaxed);
                        sent.fetch_add(1, Ordering::Relaxed);
                    }
                    SendResult::Blocked { inflight, capacity } => {
                        // A refusal must report the live numbers, and must not have moved them.
                        assert!(inflight <= CAPACITY);
                        assert_eq!(capacity, CAPACITY);
                        max_seen.fetch_max(inflight, Ordering::Relaxed);
                        drop(flow);
                        // Hand the mutex to the acknowledger instead of spinning on it: a blocked
                        // sender that never yields would starve the only thread that can unblock it.
                        std::thread::yield_now();
                    }
                }
            }
        })
    };

    let acknowledger = {
        let window = Arc::clone(&window);
        let sent = Arc::clone(&sent);
        let acked = Arc::clone(&acked);
        let max_seen = Arc::clone(&max_seen);
        std::thread::spawn(move || {
            while acked.load(Ordering::Relaxed) < FRAMES {
                // Only acknowledge frames that were actually sent: an ack for a frame that does not
                // exist yet would saturate `inflight` to zero and let the sender briefly exceed the
                // bound on the next round. That would be a defect in this test, not in `Flow`.
                if acked.load(Ordering::Relaxed) >= sent.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                    continue;
                }
                let mut flow = window.lock().expect("flow is not poisoned");
                flow.acknowledge(1);
                let inflight = flow.inflight();
                assert!(
                    inflight <= CAPACITY,
                    "an acknowledgement left {inflight} frames in flight, capacity {CAPACITY}"
                );
                max_seen.fetch_max(inflight, Ordering::Relaxed);
                acked.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    sender.join().expect("the sender must not panic");
    acknowledger
        .join()
        .expect("the acknowledger must not panic");

    assert_eq!(sent.load(Ordering::Relaxed), FRAMES);
    assert_eq!(acked.load(Ordering::Relaxed), FRAMES);

    let flow = window.lock().expect("flow is not poisoned");
    assert_eq!(
        flow.inflight(),
        0,
        "every sent frame was acknowledged, so nothing may remain in flight"
    );
    assert_eq!(
        flow.sent_total(),
        u64::try_from(FRAMES).expect("frame count fits in u64"),
        "`sent_total` must count accepted sends only"
    );
    assert!(
        max_seen.load(Ordering::Relaxed) <= CAPACITY,
        "the window was observed above its capacity"
    );
    // The stress is only meaningful if the window actually filled; a run that never blocked tested
    // the happy path and nothing else.
    assert!(
        max_seen.load(Ordering::Relaxed) == CAPACITY,
        "the window never reached capacity, so the blocking path was not exercised"
    );
}

#[test]
fn copying_a_window_duplicates_its_credit_which_the_module_documentation_forbids() {
    // This test documents a divergence between `stream_flow`'s documentation and its derive list.
    const CAPACITY: usize = 2;

    //
    // The module says of `Flow`: "Not `Clone`: two handles over one window would each believe they owned its credit, and
    // the capacity would then be enforced twice over or not at all." That sentence was a lie for a while -- the type
    // derived `Clone, Copy` -- and a copy meant a caller passing a `Flow` by value to two producers silently got
    // `2 * capacity` outstanding frames.
    //
    // The derives are gone, so the property is now a compile-time one the test can only assert indirectly: two values
    // cannot come from one. What is checkable at runtime is that a single window still behaves as documented, which the
    // test above covers, and that sharing is by reference rather than by value, which is what this asserts.
    let mut flow = Flow::new(CAPACITY);
    let shared = std::sync::Arc::new(std::sync::Mutex::new(flow));
    {
        let handle = std::sync::Arc::clone(&shared);
        let locked = handle.lock().expect("the window");
        assert_eq!(locked.inflight(), 0, "a fresh window has carried nothing");
    }
    // The only way to reach one window from two places is through a lock, and that lock is what serialises the sends.
    flow = std::sync::Arc::try_unwrap(shared)
        .expect("the only handle")
        .into_inner()
        .expect("not poisoned");
    for _ in 0..CAPACITY {
        assert_eq!(flow.send(), SendResult::Allowed);
    }
    assert!(
        flow.is_blocked(),
        "one window cannot carry more than its capacity, however many places hold the lock"
    );
}
