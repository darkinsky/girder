//! Tests for the storage quota: measuring a session directory and choosing what to give up.
//!
//! Three properties carry the module, and each one has a test that fails if it is swapped for the
//! tempting alternative:
//!
//! 1. **Eviction is by age.** A size-ordered implementation passes every "does it fit" assertion and
//!    still deletes the newest session on a disk full of old ones, so the age tests state the
//!    expected victim *names* rather than only the resulting byte total.
//! 2. **One pass frees enough.** Freeing exactly the overflow leaves the directory on its ceiling, so
//!    the next write evicts again. `evict_fraction` is checked by asserting that the leftover is under
//!    the ceiling by the configured margin, not merely at or under it.
//! 3. **Nothing is deleted unnecessarily.** A directory inside its quota — including one exactly *at*
//!    it — must come back with an empty victim list, because a policy that trimmed on every pass
//!    would delete data to celebrate a budget it was not over.
//!
//! The module is included by path rather than through the crate root, so these tests compile against
//! it as written even before the crate root wires it in — the same arrangement `tests/usage_quota.rs`
//! uses. That is also why the tests never touch the filesystem: every input is a constructed
//! [`Footprint`], and none of the arithmetic here needs a disk.

#[path = "../src/storage_quota.rs"]
mod storage_quota;

use storage_quota::{Footprint, Quota, evict, is_oversized, usage};

/// A footprint with the given id, total size, and modification time.
fn footprint(session: &str, bytes: u64, modified_ms: u64) -> Footprint {
    Footprint {
        session: session.to_owned(),
        log_bytes: bytes,
        sidecar_bytes: 0,
        modified_ms,
    }
}

/// A quota with no extra headroom, so the arithmetic is the plain overflow.
fn exact(max_bytes: u64) -> Quota {
    Quota {
        max_bytes,
        evict_fraction: 0.0,
    }
}

/// A quota that frees `fraction` of the ceiling in addition to the overflow.
fn with_headroom(max_bytes: u64, fraction: f64) -> Quota {
    Quota {
        max_bytes,
        evict_fraction: fraction,
    }
}

// ---- usage -------------------------------------------------------------------------------------

#[test]
fn usage_sums_logs_and_sidecars() {
    let footprints = [
        Footprint {
            session: "a".to_owned(),
            log_bytes: 1_000,
            sidecar_bytes: 24,
            modified_ms: 100,
        },
        Footprint {
            session: "b".to_owned(),
            log_bytes: 2_000,
            sidecar_bytes: 0,
            modified_ms: 300,
        },
    ];

    let measured = usage(&footprints);

    assert_eq!(
        measured.total_bytes, 3_024,
        "a sidecar counts, and a session without one counts as zero rather than being skipped"
    );
    assert_eq!(measured.sessions, 2);
    assert_eq!(measured.oldest_ms, Some(100));
    assert_eq!(measured.newest_ms, Some(300));
}

#[test]
fn total_is_the_sum_of_the_two_parts() {
    let sample = Footprint {
        session: "a".to_owned(),
        log_bytes: 7,
        sidecar_bytes: 35,
        modified_ms: 0,
    };

    assert_eq!(sample.total(), 42);
}

#[test]
fn usage_of_nothing_is_zero_and_has_no_bounds() {
    let measured = usage(&[]);

    assert_eq!(measured.total_bytes, 0);
    assert_eq!(measured.sessions, 0);
    // `None` rather than `Some(0)`: zero is the epoch, so a sentinel would make a fresh install
    // indistinguishable from a clock bug.
    assert_eq!(measured.oldest_ms, None);
    assert_eq!(measured.newest_ms, None);
}

#[test]
fn one_session_is_its_own_oldest_and_newest() {
    let measured = usage(&[footprint("only", 10, 42)]);

    assert_eq!(measured.oldest_ms, Some(42));
    assert_eq!(measured.newest_ms, Some(42));
}

// ---- the ceiling is not crossed ----------------------------------------------------------------

#[test]
fn a_directory_under_its_quota_is_left_alone() {
    let footprints = [footprint("old", 10, 1), footprint("new", 10, 2)];

    assert_eq!(
        evict(&footprints, &exact(1_000)),
        Vec::<String>::new(),
        "there is room, so no session is a candidate no matter how much headroom the quota asks for"
    );
    assert!(
        evict(&footprints, &with_headroom(1_000, 0.9)).is_empty(),
        "the fraction applies to a crossed ceiling, not to an idle one"
    );
}

#[test]
fn exactly_at_the_ceiling_is_not_over_it() {
    // The boundary is inclusive, the same as every other size limit in this crate: a ceiling of 100
    // allows 100 bytes. Evicting here would make the stated maximum unattainable by one session.
    let footprints = [footprint("a", 60, 1), footprint("b", 40, 2)];

    assert_eq!(evict(&footprints, &exact(100)), Vec::<String>::new());
}

#[test]
fn an_empty_directory_evicts_nothing() {
    assert_eq!(evict(&[], &exact(0)), Vec::<String>::new());
    assert_eq!(evict(&[], &with_headroom(1_000, 0.5)), Vec::<String>::new());
}

// ---- eviction is by age, not by size -----------------------------------------------------------

#[test]
fn the_oldest_session_goes_first_even_when_it_is_the_smallest() {
    // The newest session is by far the largest. A size-ordered implementation would delete it — and
    // free the most bytes per delete — which is exactly the mistake this test exists to catch.
    let footprints = [
        footprint("newest-and-huge", 10_000, 300),
        footprint("oldest-and-tiny", 100, 100),
        footprint("middle", 100, 200),
    ];

    // 10_200 bytes against a 10_150-byte ceiling: 50 bytes over, and the 100-byte oldest session is
    // the smallest thing that can be given up.
    let victims = evict(&footprints, &exact(10_150));

    assert_eq!(
        victims,
        vec!["oldest-and-tiny".to_owned()],
        "only the overflow needs freeing, and the oldest session is the one that pays"
    );
    assert!(
        !victims.contains(&"newest-and-huge".to_owned()),
        "a recent session is never chosen over an older one of the same footprint"
    );
}

#[test]
fn victims_come_out_in_age_order() {
    // Deliberately supplied newest-first: the output order must come from the timestamps, not from
    // the caller's slice, or two scans of the same disk would produce different deletion trails.
    let footprints = [
        footprint("c", 400, 30),
        footprint("b", 400, 20),
        footprint("a", 400, 10),
    ];

    // 1_200 bytes against a 100-byte ceiling, so all three go — and in age order.
    let victims = evict(&footprints, &exact(100));

    assert_eq!(
        victims,
        vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        "oldest, then next-oldest, until the directory fits"
    );
}

#[test]
fn ties_are_broken_by_name() {
    // One import can write several sessions inside the same millisecond. Without a tiebreak the
    // choice would depend on the order the caller listed the directory.
    let footprints = [footprint("zeta", 50, 7), footprint("alpha", 50, 7)];

    let victims = evict(&footprints, &exact(50));

    assert_eq!(victims, vec!["alpha".to_owned()]);
}

// ---- one pass frees enough ---------------------------------------------------------------------

#[test]
fn the_fraction_frees_more_than_the_overflow_in_one_pass() {
    // Ten sessions of 100 bytes: 1_000 bytes against a 900-byte ceiling, so the overflow is 100.
    // Evicting only the overflow would leave exactly 900 — on the ceiling — and the next append
    // would evict again. A tenth of the ceiling makes the target 810.
    let footprints: Vec<Footprint> = (0..10_u64)
        .map(|i| footprint(&format!("s{i}"), 100, i))
        .collect();

    let victims = evict(&footprints, &with_headroom(900, 0.1));

    assert_eq!(
        victims,
        vec!["s0".to_owned(), "s1".to_owned()],
        "the target is 810, so two sessions (leaving 800) are enough; the plain overflow would \
         have stopped at 900"
    );
}

#[test]
fn without_a_fraction_only_the_overflow_is_freed() {
    // The same input with `evict_fraction == 0.0` stops as soon as the ceiling is met. This is the
    // behaviour the fraction exists to avoid, stated as a test so the difference between the two
    // configurations is recorded rather than implied.
    let footprints: Vec<Footprint> = (0..10_u64)
        .map(|i| footprint(&format!("s{i}"), 100, i))
        .collect();

    let victims = evict(&footprints, &exact(900));

    assert_eq!(
        victims,
        vec!["s0".to_owned()],
        "one 100-byte session brings 1_000 down to exactly 900"
    );
}

#[test]
fn a_full_fraction_empties_the_directory() {
    // A fraction of 1.0 asks for everything to go, which is the configuration for a wipe rather than
    // a trim. It must terminate rather than loop looking for a target it has already passed.
    let footprints = [footprint("a", 10, 1), footprint("b", 10, 2)];

    assert_eq!(
        evict(&footprints, &with_headroom(10, 1.0)),
        vec!["a".to_owned(), "b".to_owned()]
    );
}

#[test]
fn an_out_of_range_fraction_is_clamped_rather_than_trusted() {
    // The fraction is a tuning knob from configuration, so it arrives unvalidated. A negative value
    // must not make the target exceed the ceiling, and a value above one must not ask for more than
    // the directory holds.
    let footprints = [footprint("a", 100, 1), footprint("b", 100, 2)];

    assert_eq!(
        evict(&footprints, &with_headroom(100, -2.5)),
        vec!["a".to_owned()],
        "a negative fraction frees the overflow and no more"
    );
    assert_eq!(
        evict(&footprints, &with_headroom(100, 9.0)),
        vec!["a".to_owned(), "b".to_owned()],
        "a fraction above one clamps to one, so every session is a victim"
    );
    assert_eq!(
        evict(&footprints, &with_headroom(100, f64::NAN)),
        vec!["a".to_owned()],
        "NaN is normalized, not propagated: a NaN target would never be reached"
    );
}

// ---- oversized sessions ------------------------------------------------------------------------

#[test]
fn a_session_over_the_per_session_cap_is_flagged() {
    let big = footprint("big", 2_000, 1);

    assert!(is_oversized(&big, 1_000));
    assert!(
        !is_oversized(&big, 2_000),
        "the cap is inclusive: exactly the maximum is admissible"
    );
    assert!(!is_oversized(&footprint("small", 10, 1), 1_000));
}

#[test]
fn a_sidecar_can_push_a_session_over_the_cap() {
    let sample = Footprint {
        session: "with-sidecars".to_owned(),
        log_bytes: 990,
        sidecar_bytes: 20,
        modified_ms: 1,
    };

    assert!(
        is_oversized(&sample, 1_000),
        "the cap applies to the whole footprint, not only to the log"
    );
}

// ---- scale -------------------------------------------------------------------------------------

#[test]
fn ten_thousand_sessions_are_summed_and_trimmed_in_age_order() {
    // The size of a real deployment, and the case where a quadratic implementation or an unstable
    // sort would actually show. Each session is 1_000 bytes with a sidecar, so the directory holds
    // 10_000 * 1_024 bytes.
    let footprints: Vec<Footprint> = (0..10_000_u64)
        .map(|i| Footprint {
            session: format!("session-{i:05}"),
            log_bytes: 1_000,
            sidecar_bytes: 24,
            modified_ms: i,
        })
        .collect();

    let measured = usage(&footprints);
    assert_eq!(measured.sessions, 10_000);
    assert_eq!(measured.total_bytes, 10_000 * 1_024);
    assert_eq!(measured.oldest_ms, Some(0));
    assert_eq!(measured.newest_ms, Some(9_999));

    // Ceiling of half the directory plus a tenth of headroom: 5_120_000 - 512_000 = 4_608_000.
    let quota = with_headroom(5_120_000, 0.1);
    let victims = evict(&footprints, &quota);

    // 1_024 bytes each: 5_000 sessions alone exceed the ceiling, so at least 5_000 must go; the
    // headroom target of 4_608_000 needs 500 more.
    assert_eq!(victims.len(), 5_500);
    assert_eq!(victims.first().map(String::as_str), Some("session-00000"));
    assert_eq!(victims.last().map(String::as_str), Some("session-05499"));

    let freed = 1_024_u64 * victims.len() as u64;
    assert!(
        measured.total_bytes - freed <= 4_608_000,
        "the survivors must fit under the post-eviction target"
    );
}

#[test]
fn a_single_session_over_the_ceiling_is_evicted_even_at_the_smallest_quota() {
    // `max_bytes == 0` is a hard stop rather than "unlimited": nothing may be stored, so the one
    // session present is a victim. Reading it as unlimited would invert a configured stop into an
    // open door.
    let footprints = [footprint("only", 1, 0)];

    assert_eq!(evict(&footprints, &exact(0)), vec!["only".to_owned()]);
}
