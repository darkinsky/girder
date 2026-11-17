//! Tests for the `Win32` publication decisions and the verifier concurrency gate.
//!
//! These are the two halves of `session-persistence-jsonl` that the earlier ports recorded as gaps.
//! Neither can be exercised against the real thing here — `MoveFileExW` and `CreateSemaphoreW` need a
//! `windows-sys` binding this crate does not carry — so every test below is deliberately aimed at a
//! *decision* that is portable, and at the exact place where a plausible-looking implementation
//! would be wrong:
//!
//! - **Publication never overwrites and never copies.** The flag word is asserted *by absence*: the
//!   bits that must not be set are checked, because "it works on the other drive now" is what adding
//!   `MOVEFILE_COPY_ALLOWED` sounds like.
//! - **`EEXIST` is adoptable and `EXDEV` is not.** Both leave a winner at the destination name; only
//!   one of them has anything to do with the bytes that were staged.
//! - **A rename is attempted once.** Retrying turns "I lost the name" into an unbounded wait.
//! - **A directory race adopts only a directory.** A file standing at the target name is a refusal,
//!   not something to replace.
//! - **The durable walk never treats a bare drive root as a descendant.** Namespacing `C:\` makes
//!   `stat` fail with `EISDIR`, which is the one path the walk relies on most.
//! - **The lock name is case-folded and length-independent.** Two spellings of one path must name one
//!   kernel object, and a maximal-length path must not exceed the object-name limit.
//! - **At most two verifiers run, in arrival order, and a released slot is handed over rather than
//!   given back.** The third property is the one that reads like a bug and is not.
//! - **A dropped waiter frees its place, and a cancelled run does not release early.**
//!
//! The module is included by path rather than through the crate's re-exports, which is why it names
//! nothing from `girder_core` at all.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::panic
)]

#[path = "../src/persistence_win32_gap.rs"]
mod persistence_win32_gap;

use std::sync::Arc;
use std::time::Duration;

use persistence_win32_gap::{
    Compression, DirectoryCommit, DirectoryState, ExitVerdict, ExpectedPrefix, LOCK_NAME_PREFIX,
    LOCK_SEMAPHORE_COUNT, LOCK_WAIT_MILLISECONDS, LockWait, MAX_CONCURRENT_VERIFIERS,
    MAX_SAFE_INTEGER, MKDIR_STAGING_PREFIX, MOVEFILE_COPY_ALLOWED, MOVEFILE_REPLACE_EXISTING,
    MOVEFILE_WRITE_THROUGH, PUBLISH_ATTEMPTS, PUBLISH_FLAGS, PublishFailure, RequestError,
    ResponseShape, SegmentStep, VerificationRequest, VerifierGate, WAIT_OBJECT_0, WAIT_TIMEOUT,
    classify_lock_wait, classify_publish, classify_response, commit_directory, errno_code,
    is_sha256_hex, judge_exit, lock_semaphore_name, needs_namespaced_path, parse_request,
    plan_segment, publish_flags, sha256_hex, split_windows_path, verifier_gate,
};
use serde_json::json;

// =============================================================================================
// The publication call: flags, verdicts, retries
// =============================================================================================

#[test]
fn publication_never_replaces_an_existing_destination() {
    // The whole contract in one assertion pair: a publish that could overwrite would let two
    // migrators both "succeed" and the loser's bytes silently win.
    assert_eq!(publish_flags() & MOVEFILE_REPLACE_EXISTING, 0);
    assert_eq!(PUBLISH_FLAGS & MOVEFILE_REPLACE_EXISTING, 0);
}

#[test]
fn publication_never_falls_back_to_a_copy() {
    // With no copy fallback a cross-volume publish fails with `EXDEV` instead of emulating the move
    // with a copy that is neither atomic nor durable: a crash mid-copy leaves a partial file under a
    // name that claims to be a complete generation.
    assert_eq!(publish_flags() & MOVEFILE_COPY_ALLOWED, 0);
    assert_eq!(PUBLISH_FLAGS & MOVEFILE_COPY_ALLOWED, 0);
}

#[test]
fn publication_is_write_through() {
    // Windows exposes no parent-directory fsync through Node, so the rename *is* the durability
    // primitive: without this flag the entry update is not guaranteed durable.
    assert_eq!(publish_flags(), MOVEFILE_WRITE_THROUGH);
    assert_eq!(
        publish_flags() & MOVEFILE_WRITE_THROUGH,
        MOVEFILE_WRITE_THROUGH
    );
}

#[test]
fn a_failed_rename_is_not_retried() {
    // Upstream calls `MoveFileExW` exactly once. The retry lives one layer up, where `EEXIST` means
    // "somebody else won" and the winner is verified — retrying the rename itself would turn a lost
    // race into an unbounded wait inside the publisher.
    assert_eq!(PUBLISH_ATTEMPTS, 1);
}

#[test]
fn the_two_exist_codes_are_indistinguishable_to_a_publisher() {
    // Which of 80 and 183 `MoveFileExW` reports depends on whether the existing object is a file or
    // a directory. A caller that branched on the code would handle a directory race on one and not
    // the other.
    assert_eq!(classify_publish(80), PublishFailure::AlreadyExists);
    assert_eq!(classify_publish(183), PublishFailure::AlreadyExists);
    assert_eq!(PublishFailure::AlreadyExists.errno(), "EEXIST");
}

#[test]
fn both_not_found_codes_mean_the_staging_object_vanished() {
    assert_eq!(classify_publish(2), PublishFailure::NotFound);
    assert_eq!(classify_publish(3), PublishFailure::NotFound);
}

#[test]
fn the_errno_mapping_is_upstreams_table() {
    // Reproduced exactly, so code that classifies a POSIX failure by `isEEXIST`/`isENOENT` keeps
    // working on Windows. The `_ => "EIO"` tail matters: guessing a more specific errno for an
    // unknown code would let a caller take a recovery path the failure does not justify.
    assert_eq!(errno_code(2), "ENOENT");
    assert_eq!(errno_code(3), "ENOENT");
    assert_eq!(errno_code(5), "EACCES");
    assert_eq!(errno_code(17), "EXDEV");
    assert_eq!(errno_code(32), "EBUSY");
    assert_eq!(errno_code(80), "EEXIST");
    assert_eq!(errno_code(123), "EINVAL");
    assert_eq!(errno_code(183), "EEXIST");
    assert_eq!(errno_code(87), "EIO");
    assert_eq!(errno_code(0), "EIO");
}

#[test]
fn every_publish_failure_agrees_with_its_errno() {
    // Each classifier arm and each errno arm are written twice; this is what keeps the two tables
    // from drifting, which would make `is_adoptable` and the errno disagree about the same event.
    let failures = [
        (PublishFailure::AlreadyExists, "EEXIST"),
        (PublishFailure::CrossDevice, "EXDEV"),
        (PublishFailure::Busy, "EBUSY"),
        (PublishFailure::NotFound, "ENOENT"),
        (PublishFailure::Denied, "EACCES"),
        (PublishFailure::InvalidName, "EINVAL"),
        (PublishFailure::Other, "EIO"),
    ];
    for (failure, expected) in failures {
        assert_eq!(failure.errno(), expected, "{failure:?}");
    }
}

#[test]
fn only_losing_the_name_is_adoptable() {
    // An `EXDEV` also leaves something at the destination name, but it is unrelated to the bytes
    // that were staged — adopting it would publish a stranger's generation under our name.
    assert!(PublishFailure::AlreadyExists.is_adoptable());
    for failure in [
        PublishFailure::CrossDevice,
        PublishFailure::Busy,
        PublishFailure::NotFound,
        PublishFailure::Denied,
        PublishFailure::InvalidName,
        PublishFailure::Other,
    ] {
        assert!(!failure.is_adoptable(), "{failure:?} must not be adoptable");
    }
}

// =============================================================================================
// The durable-directory walk
// =============================================================================================

#[test]
fn the_walk_creates_only_what_is_missing() {
    assert_eq!(
        plan_segment(DirectoryState::Directory),
        SegmentStep::Descend
    );
    assert_eq!(plan_segment(DirectoryState::Absent), SegmentStep::Create);
}

#[test]
fn a_file_where_a_directory_belongs_stops_the_walk() {
    // Upstream's `assertDirectory` throws `ENOTDIR`; restated as a decision, the walk must refuse.
    // Replacing the occupant would destroy a file nobody asked to touch.
    assert_eq!(plan_segment(DirectoryState::Other), SegmentStep::Refuse);
}

#[test]
fn a_lost_directory_race_is_success_when_the_winner_is_a_directory() {
    // The directory this call was asked to ensure now exists, durably, because somebody else's
    // `MOVEFILE_WRITE_THROUGH` is what made it.
    assert_eq!(
        commit_directory(Err(PublishFailure::AlreadyExists), true),
        DirectoryCommit::Adopted
    );
    assert!(DirectoryCommit::Adopted.is_durable());
}

#[test]
fn a_lost_directory_race_is_refused_when_the_winner_is_not_a_directory() {
    assert_eq!(
        commit_directory(Err(PublishFailure::AlreadyExists), false),
        DirectoryCommit::NotADirectory
    );
    assert!(!DirectoryCommit::NotADirectory.is_durable());
}

#[test]
fn a_published_directory_is_durable() {
    assert_eq!(commit_directory(Ok(()), false), DirectoryCommit::Published);
    assert!(DirectoryCommit::Published.is_durable());
}

#[test]
fn the_winner_check_is_not_consulted_for_other_failures() {
    // Upstream only stats the target inside the `EEXIST` arm. If this function consulted
    // `winner_is_directory` for every failure, a cross-volume or denied publish would be reported as
    // a satisfied directory just because the name happened to hold one.
    assert_eq!(
        commit_directory(Err(PublishFailure::CrossDevice), true),
        DirectoryCommit::Failed(PublishFailure::CrossDevice)
    );
    assert_eq!(
        commit_directory(Err(PublishFailure::Denied), true),
        DirectoryCommit::Failed(PublishFailure::Denied)
    );
    assert!(!DirectoryCommit::Failed(PublishFailure::Busy).is_durable());
}

#[test]
fn the_staging_prefix_shares_no_basename_with_the_target() {
    // A legal 255-byte final component plus this prefix plus a random suffix would break the
    // per-component limit, so the staging name is generated in the parent and shares only its
    // directory with the target.
    assert_eq!(MKDIR_STAGING_PREFIX, ".dsh-mkdir-");
    assert!(MKDIR_STAGING_PREFIX.starts_with('.'));
}

// ---- path splitting --------------------------------------------------------------------------

#[test]
fn a_drive_root_is_a_root_and_nothing_else() {
    let split = split_windows_path("C:\\");
    assert_eq!(split.root, "C:\\");
    assert!(split.segments.is_empty());
}

#[test]
fn a_drive_path_splits_into_its_components() {
    let split = split_windows_path("C:\\Users\\ann\\sessions\\s1");
    assert_eq!(split.root, "C:\\");
    assert_eq!(split.segments, ["Users", "ann", "sessions", "s1"]);
}

#[test]
fn separators_mix_and_repeats_collapse() {
    // Upstream splits on `[\\/]+`, so a mixed or doubled separator is not an empty component.
    let split = split_windows_path("C:\\Users//ann\\\\sessions/");
    assert_eq!(split.root, "C:\\");
    assert_eq!(split.segments, ["Users", "ann", "sessions"]);
}

#[test]
fn a_rooted_path_without_a_drive_keeps_its_separator() {
    let split = split_windows_path("\\sessions\\s1");
    assert_eq!(split.root, "\\");
    assert_eq!(split.segments, ["sessions", "s1"]);
}

#[test]
fn a_unc_root_is_the_server_and_share() {
    // A UNC path with no share names nothing, so the share belongs to the root rather than becoming
    // a segment the walk would try to create.
    let split = split_windows_path("\\\\server\\share\\sessions\\s1");
    assert_eq!(split.root, "\\\\server\\share\\");
    assert_eq!(split.segments, ["sessions", "s1"]);
}

#[test]
fn a_unc_root_without_a_trailing_separator_is_still_a_root() {
    let split = split_windows_path("\\\\server\\share");
    assert_eq!(split.root, "\\\\server\\share");
    assert!(split.segments.is_empty());
}

#[test]
fn an_extended_length_prefix_is_not_read_as_a_server_named_question_mark() {
    // `\\?\C:\x` is a namespaced drive path, not a UNC path whose server is `?`.
    let split = split_windows_path("\\\\?\\C:\\sessions\\s1");
    assert_eq!(split.root, "\\\\?\\C:\\");
    assert_eq!(split.segments, ["sessions", "s1"]);
}

#[test]
fn an_extended_length_unc_path_keeps_its_unc_root() {
    let split = split_windows_path("\\\\?\\UNC\\server\\share\\sessions");
    assert_eq!(split.root, "\\\\?\\UNC\\server\\share\\");
    assert_eq!(split.segments, ["sessions"]);
}

#[test]
fn a_relative_path_has_no_root() {
    // Upstream cannot produce one — `resolve` has already run — but a total answer beats a panic on
    // an input the type system does not exclude.
    let split = split_windows_path("sessions\\s1");
    assert_eq!(split.root, "");
    assert_eq!(split.segments, ["sessions", "s1"]);
}

#[test]
fn a_bare_drive_root_is_not_namespaced() {
    // `toNamespacedPath('C:\\')` is `\\?\C:\`, which Node rejects with `EISDIR` from `stat` — the one
    // path the walk relies on most would fail for a reason unrelated to the caller's problem.
    assert!(!needs_namespaced_path("C:\\"));
    assert!(!needs_namespaced_path("\\"));
    assert!(!needs_namespaced_path("\\\\server\\share"));
}

#[test]
fn a_descendant_is_namespaced() {
    assert!(needs_namespaced_path("C:\\Users"));
    assert!(needs_namespaced_path("C:\\Users\\ann\\sessions"));
    assert!(needs_namespaced_path("\\\\server\\share\\sessions"));
}

// =============================================================================================
// The named-semaphore lock: name derivation and the wait verdict
// =============================================================================================

#[test]
fn sha256_matches_the_fips_vectors() {
    // The name is only correct if the digest is, and a transcription error in the round constants
    // still *looks* like a hash. These are the FIPS 180-4 / NIST examples.
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

#[test]
fn sha256_handles_the_padding_boundaries() {
    // 55, 56 and 64 bytes are where the length field forces an extra block. A hash that is wrong only
    // at these lengths would still pass the short vectors above.
    for length in [55_usize, 56, 63, 64, 65] {
        let bytes = vec![b'a'; length];
        let digest = sha256_hex(&bytes);
        assert!(is_sha256_hex(&digest), "length {length} produced {digest}");
        assert_eq!(digest.len(), 64);
    }
    assert_eq!(
        sha256_hex(&[b'a'; 64]),
        "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
    );
}

#[test]
fn only_lowercase_hex_of_the_right_length_is_a_digest() {
    // The digest is compared byte-for-byte against one this crate produced, which is always
    // lowercase; accepting uppercase would turn a formatting difference into a verification failure
    // three layers deeper.
    assert!(is_sha256_hex(&sha256_hex(b"x")));
    assert!(!is_sha256_hex(&"a".repeat(63)));
    assert!(!is_sha256_hex(&"a".repeat(65)));
    assert!(!is_sha256_hex(&"A".repeat(64)));
    assert!(!is_sha256_hex(&"g".repeat(64)));
    assert!(!is_sha256_hex(""));
}

#[test]
fn the_lock_name_is_scoped_and_hashed() {
    let name = lock_semaphore_name("C:\\Users\\ann\\sessions\\s1\\session.lock");
    assert!(name.starts_with(LOCK_NAME_PREFIX));
    assert!(name.starts_with("Local\\"));
    let suffix = name.strip_prefix(LOCK_NAME_PREFIX).expect("the prefix");
    assert!(is_sha256_hex(suffix), "suffix was {suffix}");
}

#[test]
fn the_lock_name_is_case_folded() {
    // Windows paths are case-insensitive, so two spellings of one file must name one kernel object.
    // Without the fold, a second writer using different casing would create a *different* semaphore
    // and both would believe they hold the lock.
    let upper = lock_semaphore_name("C:\\Users\\Ann\\Sessions\\S1\\session.lock");
    let lower = lock_semaphore_name("c:\\users\\ann\\sessions\\s1\\session.lock");
    let mixed = lock_semaphore_name("C:\\USERS\\ann\\SESSIONS\\s1\\SESSION.LOCK");
    assert_eq!(upper, lower);
    assert_eq!(mixed, lower);
}

#[test]
fn different_paths_get_different_lock_names() {
    let first = lock_semaphore_name("C:\\Users\\ann\\sessions\\s1\\session.lock");
    let second = lock_semaphore_name("C:\\Users\\ann\\sessions\\s2\\session.lock");
    assert_ne!(first, second);
}

#[test]
fn the_lock_name_length_is_independent_of_the_path_length() {
    // The kernel object-name limit is 32,767 characters. Hashing makes a maximal legal path and a
    // twelve-character one produce the same 64-character suffix, so the limit is unreachable.
    let short = lock_semaphore_name("C:\\s1");
    let long = lock_semaphore_name(&format!("C:\\{}", "a".repeat(32_000)));
    assert_eq!(short.len(), long.len());
    assert_eq!(short.len(), LOCK_NAME_PREFIX.len() + 64);
}

#[test]
fn the_lock_wait_is_zero_and_single_count() {
    // Zero timeout is the contract: contention must become an error the caller reports, never a wait,
    // because a blocking acquire against a live but wedged holder is an unbounded hang.
    assert_eq!(LOCK_WAIT_MILLISECONDS, 0);
    assert_eq!(LOCK_SEMAPHORE_COUNT, 1);
}

#[test]
fn the_lock_wait_codes_classify() {
    assert_eq!(classify_lock_wait(WAIT_OBJECT_0), LockWait::Acquired);
    assert_eq!(classify_lock_wait(0x0000_0102), LockWait::Contended);
    assert_eq!(classify_lock_wait(WAIT_TIMEOUT), LockWait::Contended);
    assert_eq!(classify_lock_wait(0x0000_0080), LockWait::Failed(0x80));
    assert_eq!(classify_lock_wait(u32::MAX), LockWait::Failed(u32::MAX));
}

// =============================================================================================
// The worker protocol
// =============================================================================================

fn a_valid_payload() -> serde_json::Value {
    json!({
        "path": "/tmp/s1/session.v2.jsonl",
        "compression": "none",
        "expectedId": "s1",
        "expectedEventCount": 3,
    })
}

#[test]
fn a_valid_payload_parses() {
    let request = parse_request(&a_valid_payload()).expect("a valid payload");
    assert_eq!(
        request,
        VerificationRequest {
            path: "/tmp/s1/session.v2.jsonl".to_owned(),
            compression: Compression::None,
            expected_id: "s1".to_owned(),
            expected_event_count: 3,
            expected_prefix: None,
        }
    );
}

#[test]
fn a_payload_with_a_prefix_parses() {
    let digest = sha256_hex(b"prefix");
    let mut payload = a_valid_payload();
    payload["compression"] = json!("zstd");
    payload["expectedPrefix"] = json!({ "bytes": 12, "digest": digest });
    let request = parse_request(&payload).expect("a valid payload");
    assert_eq!(request.compression, Compression::Zstd);
    assert_eq!(
        request.expected_prefix,
        Some(ExpectedPrefix {
            bytes: 12,
            digest: sha256_hex(b"prefix"),
        })
    );
}

#[test]
fn a_non_object_is_not_a_request() {
    for payload in [json!(null), json!(7), json!("x"), json!([1])] {
        assert_eq!(parse_request(&payload), Err(RequestError::NotAnObject));
    }
    assert_eq!(
        RequestError::NotAnObject.message(),
        "migration verifier request must be an object"
    );
}

#[test]
fn a_missing_or_mistyped_field_is_malformed() {
    let cases = [
        json!({ "compression": "none", "expectedId": "s1", "expectedEventCount": 3 }),
        json!({ "path": 7, "compression": "none", "expectedId": "s1", "expectedEventCount": 3 }),
        json!({ "path": "p", "compression": "gzip", "expectedId": "s1", "expectedEventCount": 3 }),
        json!({ "path": "p", "expectedId": "s1", "expectedEventCount": 3 }),
        json!({ "path": "p", "compression": "none", "expectedEventCount": 3 }),
        json!({ "path": "p", "compression": "none", "expectedId": "s1" }),
        json!({ "path": "p", "compression": "none", "expectedId": 7, "expectedEventCount": 3 }),
    ];
    for payload in cases {
        assert_eq!(
            parse_request(&payload),
            Err(RequestError::Malformed),
            "{payload}"
        );
    }
    assert_eq!(
        RequestError::Malformed.message(),
        "migration verifier request is malformed"
    );
}

#[test]
fn a_negative_or_unsafe_event_count_is_malformed() {
    // `Number.isSafeInteger` is what upstream checks, and it is not "fits in a double": `2^53` is a
    // double but not an exact integer sequence, so a length comparison against it is meaningless.
    assert_eq!(MAX_SAFE_INTEGER, 9_007_199_254_740_991);
    let mut negative = a_valid_payload();
    negative["expectedEventCount"] = json!(-1);
    assert_eq!(parse_request(&negative), Err(RequestError::Malformed));

    let mut unsafe_count = a_valid_payload();
    unsafe_count["expectedEventCount"] = json!(MAX_SAFE_INTEGER + 1);
    assert_eq!(parse_request(&unsafe_count), Err(RequestError::Malformed));

    let mut fraction = a_valid_payload();
    fraction["expectedEventCount"] = json!(1.5);
    assert_eq!(parse_request(&fraction), Err(RequestError::Malformed));

    let mut largest = a_valid_payload();
    largest["expectedEventCount"] = json!(MAX_SAFE_INTEGER);
    assert_eq!(
        parse_request(&largest).map(|request| request.expected_event_count),
        Ok(MAX_SAFE_INTEGER)
    );
}

#[test]
fn a_malformed_prefix_is_malformed() {
    let cases = [
        json!(null),
        json!(7),
        json!({}),
        json!({ "bytes": 1 }),
        json!({ "digest": sha256_hex(b"x") }),
        json!({ "bytes": -1, "digest": sha256_hex(b"x") }),
        json!({ "bytes": 1, "digest": "not-a-digest" }),
        json!({ "bytes": 1, "digest": sha256_hex(b"x").to_uppercase() }),
        json!({ "bytes": MAX_SAFE_INTEGER + 1, "digest": sha256_hex(b"x") }),
    ];
    for prefix in cases {
        let mut payload = a_valid_payload();
        payload["expectedPrefix"] = prefix.clone();
        assert_eq!(
            parse_request(&payload),
            Err(RequestError::Malformed),
            "prefix {prefix}"
        );
    }
}

#[test]
fn the_compression_spelling_is_exact() {
    assert_eq!(Compression::parse("none"), Some(Compression::None));
    assert_eq!(Compression::parse("zstd"), Some(Compression::Zstd));
    assert_eq!(Compression::parse("None"), None);
    assert_eq!(Compression::parse("zst"), None);
    assert_eq!(Compression::None.as_str(), "none");
    assert_eq!(Compression::Zstd.as_str(), "zstd");
}

#[test]
fn a_response_is_classified_by_its_boolean_ok() {
    // The `ok` field must be a *boolean*: a protocol violation reported as a verification failure
    // would send a caller looking for corruption that is not there.
    assert_eq!(
        classify_response(&json!({ "ok": true, "result": {} })),
        ResponseShape::Success
    );
    assert_eq!(
        classify_response(&json!({ "ok": false, "message": "bad" })),
        ResponseShape::Failure
    );
    assert_eq!(
        classify_response(&json!({ "ok": "true" })),
        ResponseShape::Invalid
    );
    assert_eq!(
        classify_response(&json!({ "ok": 1 })),
        ResponseShape::Invalid
    );
    assert_eq!(classify_response(&json!({})), ResponseShape::Invalid);
    assert_eq!(classify_response(&json!(null)), ResponseShape::Invalid);
    assert_eq!(classify_response(&json!("ok")), ResponseShape::Invalid);
}

#[test]
fn success_needs_both_a_result_and_a_zero_exit() {
    // Nothing downstream should have to know which of the two arrived last, so neither half alone is
    // enough.
    assert_eq!(judge_exit(true, 0), ExitVerdict::Resolve);
    assert_eq!(judge_exit(true, 0).message(), None);
}

#[test]
fn exiting_before_reporting_is_a_failure_naming_the_code() {
    let verdict = judge_exit(false, 0);
    assert_eq!(verdict, ExitVerdict::BeforeResult(0));
    assert_eq!(
        verdict.message().as_deref(),
        Some("migration verifier exited before reporting a result (code 0)")
    );
    assert_eq!(judge_exit(false, 3), ExitVerdict::BeforeResult(3));
}

#[test]
fn exiting_non_zero_after_reporting_still_fails() {
    // It produced a result and then failed; upstream refuses both rather than preferring the result.
    let verdict = judge_exit(true, 2);
    assert_eq!(verdict, ExitVerdict::AfterResult(2));
    assert_eq!(
        verdict.message().as_deref(),
        Some("migration verifier exited after reporting a result (code 2)")
    );
}

// =============================================================================================
// The concurrency gate
// =============================================================================================

#[test]
fn the_gate_is_bounded_at_two() {
    // Memory, not parallelism: each verifier decodes a whole generation, so N of them is N copies
    // resident at once.
    assert_eq!(MAX_CONCURRENT_VERIFIERS, 2);
    assert_eq!(verifier_gate().capacity(), MAX_CONCURRENT_VERIFIERS);
    assert_eq!(VerifierGate::new().capacity(), MAX_CONCURRENT_VERIFIERS);
}

#[test]
fn the_process_gate_is_one_object() {
    // Two separately constructed gates would each allow two verifiers — double the memory bound the
    // constant exists to enforce.
    assert!(std::ptr::eq(verifier_gate(), verifier_gate()));
}

#[tokio::test]
async fn two_verifiers_run_and_the_third_waits() {
    let gate = VerifierGate::new();
    let first = gate.acquire().await;
    let second = gate.acquire().await;
    assert_eq!(gate.active(), 2);
    assert_eq!(gate.waiting(), 0);

    let third = tokio::time::timeout(Duration::from_millis(20), gate.acquire()).await;
    assert!(
        third.is_err(),
        "a third verifier must not start while two run"
    );
    // The timed-out acquire's future was dropped, so its queue entry is gone: a cancelled waiter must
    // not delay the next one until a release happens to pop it.
    assert_eq!(gate.waiting(), 0);

    drop(first);
    assert_eq!(gate.active(), 1);
    drop(second);
    assert_eq!(gate.active(), 0);
}

#[tokio::test]
async fn a_released_slot_is_handed_over_rather_than_given_back() {
    // The property that reads like a leak: with a waiter present, `active` is left alone and the
    // waiter is granted directly. Decrement-then-wake would let a third caller take the freed slot
    // between the two steps, so the waiter that was promised it would queue again behind a newcomer.
    let gate = Arc::new(VerifierGate::with_capacity(1));
    let first = gate.acquire().await;
    assert_eq!(gate.active(), 1);

    let waiter_gate = Arc::clone(&gate);
    let waiter = tokio::spawn(async move {
        let _permit = waiter_gate.acquire().await;
        waiter_gate.active()
    });
    // Let the waiter reach the queue.
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(gate.waiting(), 1);

    drop(first);
    // The slot went straight to the waiter: active stayed at one the whole time.
    assert_eq!(gate.active(), 1);
    assert_eq!(waiter.await.expect("the waiter task"), 1);
    assert_eq!(gate.active(), 0);
}

#[tokio::test]
async fn waiters_are_served_in_arrival_order() {
    // A `shift()` from the head: a burst of migrations must not starve the oldest request.
    let gate = Arc::new(VerifierGate::with_capacity(1));
    let held = gate.acquire().await;

    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for index in 0..3_u32 {
        let gate = Arc::clone(&gate);
        let order = Arc::clone(&order);
        handles.push(tokio::spawn(async move {
            let permit = gate.acquire().await;
            order.lock().expect("the order log").push(index);
            tokio::time::sleep(Duration::from_millis(5)).await;
            drop(permit);
        }));
        // Stagger the arrivals so the queue order is not a coin flip.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(gate.waiting(), 3);

    drop(held);
    for handle in handles {
        handle.await.expect("a waiter task");
    }
    assert_eq!(*order.lock().expect("the order log"), vec![0, 1, 2]);
}

#[tokio::test]
async fn dropping_a_waiter_removes_its_place() {
    // A cancelled verification must not consume the permit a release hands it: if its entry survived,
    // the slot would be spent on a receiver nobody is waiting on and the next real waiter would wait
    // forever.
    let gate = Arc::new(VerifierGate::with_capacity(1));
    let held = gate.acquire().await;

    let cancelled = tokio::time::timeout(Duration::from_millis(20), gate.acquire()).await;
    assert!(cancelled.is_err());
    assert_eq!(gate.waiting(), 0);

    // The one slot is still the held one, and it can still be handed over to a real waiter.
    let waiter = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            let _permit = gate.acquire().await;
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(gate.waiting(), 1);
    drop(held);
    waiter.await.expect("the waiter task");
    assert_eq!(gate.active(), 0);
}

#[tokio::test]
async fn a_permit_releases_on_scope_exit_even_when_the_body_returns_early() {
    // Upstream releases in `finally`. RAII is the Rust spelling, and it cannot be forgotten on an
    // early return — including the panic path.
    let gate = VerifierGate::new();
    let outcome = async {
        let _permit = gate.acquire().await;
        if true {
            return Err::<(), &str>("early");
        }
        Ok(())
    }
    .await;
    assert_eq!(outcome, Err("early"));
    assert_eq!(gate.active(), 0);
    assert_eq!(gate.waiting(), 0);
}

#[tokio::test]
async fn a_cancelled_run_does_not_release_early() {
    // The permit is released when the operation actually stops, not when cancellation is requested:
    // the worker still holds its generation in memory until then.
    let gate = Arc::new(VerifierGate::with_capacity(1));
    let running = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            let _permit = gate.acquire().await;
            tokio::time::sleep(Duration::from_millis(60)).await;
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(gate.active(), 1);

    // Cancelling a *waiter* changes nothing about the running holder's slot.
    let blocked = tokio::time::timeout(Duration::from_millis(10), gate.acquire()).await;
    assert!(blocked.is_err());
    assert_eq!(gate.active(), 1);

    running.abort();
    let _ = running.await;
    // Aborting a task drops its permit, which is the same as the body finishing.
    assert_eq!(gate.active(), 0);
}

#[tokio::test]
async fn a_capacity_of_zero_is_treated_as_one() {
    // A zero-capacity gate would deny every verification forever; clamping keeps the failure mode
    // out of the type rather than out of a caller's discipline.
    let gate = VerifierGate::with_capacity(0);
    assert_eq!(gate.capacity(), 1);
    let permit = gate.acquire().await;
    assert_eq!(gate.active(), 1);
    drop(permit);
    assert_eq!(gate.active(), 0);
}

#[tokio::test]
async fn the_gate_survives_a_panicking_holder() {
    // The state behind the lock is a counter and a queue; a panic unwinds through `release` via the
    // permit's `Drop`, and the mutex is recovered rather than poisoning every later verification.
    let gate = Arc::new(VerifierGate::with_capacity(1));
    let panicking = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            let _permit = gate.acquire().await;
            panic!("a verifier panicked");
        })
    };
    assert!(panicking.await.is_err());
    assert_eq!(gate.active(), 0);

    let after = gate.acquire().await;
    assert_eq!(gate.active(), 1);
    drop(after);
    assert_eq!(gate.active(), 0);
}
