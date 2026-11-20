//! Tests for durable session artifacts: naming, header validation, and exclusive publication.
//!
//! The properties here are the ones a stored session depends on, and each is a way for the layer to
//! look correct while being wrong:
//!
//! - **the id encoding is injective and cannot escape the root.** A lossy encoding maps two sessions
//!   onto one file; an unescaped `../` writes outside the session tree. Both are checked from the
//!   side that fails.
//! - **a generation name is canonical or it is not a generation.** `v04`, `V4`, `.v0`, `v5`, and a
//!   mismatched compression suffix all name *something*, and accepting any of them would let one
//!   generation exist twice with a resolver that has to guess.
//! - **a header is evidence.** A seeded session without a cut, a relative `cwd`, a retired policy
//!   field, or an unknown key refuses, because each says the file was written by something whose
//!   invariants this build cannot honour.
//! - **an unknown event type refuses the log unless its writer marked it ignorable.** Silently
//!   skipping a required event reconstructs a wrong session and reports it as a correct one.
//! - **publication is exclusive, and the loser verifies the winner.** Two writers racing must leave
//!   the winner's bytes and one `Published`; the loser either adopts those exact bytes or refuses.
//! - **a moved source is not republished.** Publishing bytes migrated from a revision that no longer
//!   exists installs a generation that never was.
//!
//! The module is included by path rather than through the crate's re-exports, so it compiles against
//! this file as written even though the crate root does not declare it yet. Every path into the crate
//! goes through `girder_core::…` for that reason.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::panic
)]

#[path = "../src/session/extra.rs"]
pub mod session_extra_gap;

use std::path::{Path, PathBuf};

use girder_core::{FormatVersion, SessionEvent};
use serde_json::{Value, json};

use session_extra_gap::{
    Compression, ContractError, ExpectedPrefix, GenerationError, HeaderError, KNOWN_EVENT_TYPES,
    NamingError, Origin, PathPlanError, PublishOutcome, Revision, StorageHeader, StorageMeta,
    assert_contiguous, assert_source_unchanged, assert_stored_id, assert_version,
    decode_generation, digest_hex, encode_generation, encode_segment, generation_log_filename,
    generation_log_path, is_known_event_type, parse_generation_log_filename, parse_header_record,
    plan_generation_paths, project_key, publish_exclusive, read_stable, session_dir, stage,
    validate_stored_records, verify_bytes, verify_file,
};

// ---- fixtures ----------------------------------------------------------------------------------

/// A fresh, empty directory per test, under the temp directory.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-session-extra-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A minimal unseeded header for `id`.
fn header(id: &str) -> StorageHeader {
    StorageHeader::new(id, 1_700_000_000_000)
}

/// The validated metadata for `id`.
fn meta(id: &str) -> StorageMeta {
    StorageMeta::new(header(id), None).expect("an unseeded header needs no cut")
}

/// One `turn/start` row, which is a known event type and needs no extra fields.
fn turn_start_row(turn: u64) -> Value {
    json!({ "type": "turn/start", "turn": turn })
}

// ---- naming: session ids -----------------------------------------------------------------------

#[test]
fn a_plain_id_is_its_own_segment() {
    assert_eq!(encode_segment("s1").as_deref(), Ok("s1"));
    assert_eq!(encode_segment("a-b_c.d").as_deref(), Ok("a-b_c.d"));
}

#[test]
fn the_empty_id_is_refused() {
    assert_eq!(encode_segment(""), Err(NamingError::EmptyId));
}

#[test]
fn dot_and_dot_dot_are_escaped_rather_than_resolved() {
    // `.` and `..` are made of safe characters but are not safe *names*: a segment equal to either
    // would resolve to a directory, so the encoder escapes them whole.
    assert_eq!(encode_segment(".").as_deref(), Ok("~2E"));
    assert_eq!(encode_segment("..").as_deref(), Ok("~2E~2E"));
    assert!(!encode_segment("..").unwrap_or_default().contains('.'));
}

#[test]
fn a_traversal_id_becomes_one_component() {
    let encoded = encode_segment("../../etc/passwd").expect("an id with separators still encodes");
    assert!(!encoded.contains('/'), "encoded as {encoded}");
    assert!(!encoded.contains('\\'), "encoded as {encoded}");
    // The dots survive as data, not as a traversal: the separator is gone, so the whole string is
    // one component beside every other session directory.
    assert!(encoded.contains("~2F"), "encoded as {encoded}");
}

#[test]
fn an_absolute_id_cannot_escape_the_root() {
    let root = Path::new("/sessions");
    let dir = session_dir(root, None, "/etc/passwd").expect("an absolute id still encodes");
    assert!(dir.starts_with(root), "{dir:?}");
    assert!(
        dir.strip_prefix(root)
            .is_ok_and(|rest| rest.components().count() == 2),
        "expected _no-cwd/<segment>, got {dir:?}"
    );
}

#[test]
fn encoding_is_injective_over_the_cases_that_collide_in_a_lossy_scheme() {
    // Each pair would collide under a lossy encoding: `~` versus an escape, one non-ASCII character
    // versus its bytes, and a separator versus the escaped separator.
    let pairs = [
        ("~2E", "~"),
        ("a/b", "a~2Fb"),
        ("ü", "~C3~BC"),
        ("a b", "a~20b"),
        ("x:y", "x~3Ay"),
        (".", "~2E~2E"),
    ];
    for (left, right) in pairs {
        assert_ne!(
            encode_segment(left).ok(),
            encode_segment(right).ok(),
            "{left} and {right} must not share a segment"
        );
    }
}

#[test]
fn a_nul_in_an_id_is_escaped_not_emitted() {
    let encoded = encode_segment("a\0b").expect("a NUL still encodes");
    assert!(!encoded.contains('\0'));
    assert_eq!(encoded, "a~00b");
}

// ---- naming: project keys and directories ------------------------------------------------------

#[test]
fn a_project_key_is_bounded_and_dash_wrapped() {
    let key = project_key("/home/me/work").expect("an absolute path has a key");
    assert_eq!(key, "--home-me-work--");
}

#[test]
fn a_root_only_path_still_gets_a_key() {
    assert_eq!(project_key("/").as_deref(), Ok("--root--"));
}

#[test]
fn a_long_project_key_is_bounded() {
    let deep = format!("/{}", "a".repeat(400));
    let key = project_key(&deep).expect("a long path still keys");
    assert!(key.len() <= 255, "key is {} bytes", key.len());
    assert!(key.starts_with("--") && key.ends_with("--"));
}

#[test]
fn an_empty_project_path_is_refused() {
    assert_eq!(project_key(""), Err(NamingError::EmptyCwd));
}

#[test]
fn a_relative_cwd_is_refused_rather_than_grouped_by_the_readers_directory() {
    let err = session_dir(Path::new("/root"), Some("work"), "s1").expect_err("relative cwd");
    assert_eq!(
        err,
        NamingError::RelativeCwd {
            cwd: "work".to_owned()
        }
    );
}

#[test]
fn a_session_without_a_cwd_lives_under_a_named_directory() {
    let dir = session_dir(Path::new("/root"), None, "s1").expect("no cwd");
    assert_eq!(dir, Path::new("/root/_no-cwd/s1"));
}

#[test]
fn a_session_with_a_cwd_lives_under_its_project_key() {
    let dir = session_dir(Path::new("/root"), Some("/home/me/work"), "s1").expect("absolute cwd");
    assert_eq!(dir, Path::new("/root/--home-me-work--/s1"));
}

// ---- naming: generation files ------------------------------------------------------------------

#[test]
fn version_zero_keeps_the_original_name() {
    assert_eq!(
        generation_log_filename(FormatVersion::V0, Compression::Plain),
        "session.jsonl"
    );
    assert_eq!(
        generation_log_filename(FormatVersion::V0, Compression::Zstd),
        "session.jsonl.zstd"
    );
}

#[test]
fn later_generations_carry_their_number() {
    assert_eq!(
        generation_log_filename(FormatVersion::V3, Compression::Zstd),
        "session.v3.jsonl.zstd"
    );
    assert_eq!(
        generation_log_filename(FormatVersion::V4, Compression::Zstd),
        "session.v4.jsonl.zstd"
    );
}

#[test]
fn a_canonical_name_parses_back_to_its_generation() {
    for version in FormatVersion::ALL {
        for compression in [Compression::Plain, Compression::Zstd] {
            let name = generation_log_filename(version, compression);
            assert_eq!(
                parse_generation_log_filename(&name, compression),
                Some(version),
                "{name}"
            );
        }
    }
}

#[test]
fn noncanonical_generation_names_are_refused() {
    for name in [
        "session.v04.jsonl.zstd",    // leading zero: one generation, two names
        "session.V4.jsonl.zstd",     // uppercase: not the canonical spelling
        "session.v0.jsonl.zstd",     // version zero is spelled without a tag
        "session.v5.jsonl.zstd",     // a generation this build does not know
        "session.v.jsonl.zstd",      // no digits
        "session.v4.jsonl",          // suffix disagrees with the configured encoding
        "other.v4.jsonl.zstd",       // not a session log at all
        "session.v4.jsonl.zstd.tmp", // a staging file is not a generation
    ] {
        assert_eq!(
            parse_generation_log_filename(name, Compression::Zstd),
            None,
            "{name} must not parse as a generation"
        );
    }
}

#[test]
fn a_plain_name_does_not_parse_under_the_zstd_encoding() {
    assert_eq!(
        parse_generation_log_filename("session.v4.jsonl", Compression::Zstd),
        None
    );
    assert_eq!(
        parse_generation_log_filename("session.v4.jsonl", Compression::Plain),
        Some(FormatVersion::V4)
    );
}

#[test]
fn a_generation_path_lives_inside_the_session_directory() {
    let path = generation_log_path(
        Path::new("/root"),
        Some("/w"),
        "s1",
        FormatVersion::V4,
        Compression::Zstd,
    )
    .expect("a path");
    assert_eq!(path, Path::new("/root/--w--/s1/session.v4.jsonl.zstd"));
}

// ---- path planning -----------------------------------------------------------------------------

#[test]
fn a_planned_pair_shares_one_directory() {
    let dir = Path::new("/root/p/s1");
    let plan = plan_generation_paths(
        &dir.join("session.v3.jsonl.zstd"),
        FormatVersion::V3,
        &dir.join("session.v4.jsonl.zstd"),
        FormatVersion::V4,
        Compression::Zstd,
    )
    .expect("v3 to v4 in one directory");
    assert_eq!(plan.source, dir.join("session.v3.jsonl.zstd"));
    assert_eq!(plan.current, dir.join("session.v4.jsonl.zstd"));
}

#[test]
fn a_noncanonical_source_name_is_refused_before_any_read() {
    let dir = Path::new("/root/p/s1");
    let err = plan_generation_paths(
        &dir.join("session.jsonl.zstd"),
        FormatVersion::V3,
        &dir.join("session.v4.jsonl.zstd"),
        FormatVersion::V4,
        Compression::Zstd,
    )
    .expect_err("v3 must be spelled .v3");
    assert!(matches!(err, PathPlanError::NonCanonicalName { .. }));
}

#[test]
fn two_directories_for_one_session_are_refused() {
    let err = plan_generation_paths(
        &Path::new("/root/p/s1").join("session.v3.jsonl.zstd"),
        FormatVersion::V3,
        &Path::new("/root/p/s2").join("session.v4.jsonl.zstd"),
        FormatVersion::V4,
        Compression::Zstd,
    )
    .expect_err("one session, one directory");
    assert!(matches!(err, PathPlanError::DifferentDirectories { .. }));
}

#[test]
fn migrating_a_generation_to_itself_is_refused() {
    let dir = Path::new("/root/p/s1");
    let err = plan_generation_paths(
        &dir.join("session.v4.jsonl.zstd"),
        FormatVersion::V4,
        &dir.join("session.v4.jsonl.zstd"),
        FormatVersion::V4,
        Compression::Zstd,
    )
    .expect_err("not an upgrade");
    assert!(matches!(err, PathPlanError::NotAnUpgrade { .. }));
}

// ---- header ------------------------------------------------------------------------------------

#[test]
fn a_minimal_header_round_trips() {
    let record = meta("s1").record();
    assert_eq!(record["type"], json!("session"));
    assert_eq!(record["version"], json!(4));
    assert_eq!(record["isSeeded"], json!(false));
    assert_eq!(record["delegationDepth"], json!(0));
    // Optional fields are omitted rather than written as null.
    assert!(record.get("cwd").is_none());
    let parsed = parse_header_record(&record).expect("its own output parses");
    assert_eq!(parsed.header, header("s1"));
}

#[test]
fn every_optional_header_field_round_trips() {
    let mut stored = header("s1");
    stored.cwd = Some("/home/me/work".to_owned());
    stored.parent_session = Some("parent".to_owned());
    stored.agent_preset = Some("default".to_owned());
    stored.origin = Some(Origin::Subagent);
    stored.delegation_depth = 2;
    let meta = StorageMeta::new(stored.clone(), None).expect("unseeded");
    let parsed = parse_header_record(&meta.record()).expect("its own output parses");
    assert_eq!(parsed.header, stored);
}

#[test]
fn a_header_that_is_not_an_object_is_refused() {
    assert_eq!(
        parse_header_record(&json!([1, 2])).expect_err("an array is not a header"),
        HeaderError::NotAnObject
    );
}

#[test]
fn a_missing_required_key_is_named() {
    let mut record = meta("s1").record();
    record.as_object_mut().expect("object").remove("createdAt");
    assert_eq!(
        parse_header_record(&record).expect_err("no createdAt"),
        HeaderError::MissingKey { key: "createdAt" }
    );
}

#[test]
fn an_unknown_key_refuses_rather_than_being_ignored() {
    let mut record = meta("s1").record();
    record
        .as_object_mut()
        .expect("object")
        .insert("futureField".to_owned(), json!(true));
    assert_eq!(
        parse_header_record(&record).expect_err("an unknown key"),
        HeaderError::UnknownKey {
            key: "futureField".to_owned()
        }
    );
}

#[test]
fn a_retired_policy_field_dates_the_log() {
    for retired in ["sandboxMode", "approvalPolicy"] {
        let mut record = meta("s1").record();
        record
            .as_object_mut()
            .expect("object")
            .insert(retired.to_owned(), json!("workspace-write"));
        assert_eq!(
            parse_header_record(&record).expect_err("a retired field"),
            HeaderError::RetiredKey { key: retired }
        );
    }
}

#[test]
fn a_relative_cwd_is_refused() {
    let mut record = meta("s1").record();
    record
        .as_object_mut()
        .expect("object")
        .insert("cwd".to_owned(), json!("work"));
    assert_eq!(
        parse_header_record(&record).expect_err("a relative cwd"),
        HeaderError::RelativeCwd {
            cwd: "work".to_owned()
        }
    );
}

#[test]
fn a_mistyped_field_is_reported_with_the_field_name() {
    let mut record = meta("s1").record();
    record
        .as_object_mut()
        .expect("object")
        .insert("isSeeded".to_owned(), json!("yes"));
    assert_eq!(
        parse_header_record(&record).expect_err("a string isSeeded"),
        HeaderError::WrongType {
            key: "isSeeded",
            expected: "a boolean"
        }
    );
}

#[test]
fn a_negative_or_fractional_number_is_not_a_safe_count() {
    let mut record = meta("s1").record();
    record
        .as_object_mut()
        .expect("object")
        .insert("delegationDepth".to_owned(), json!(-1));
    assert_eq!(
        parse_header_record(&record).expect_err("negative depth"),
        HeaderError::NotANonNegativeInteger {
            key: "delegationDepth"
        }
    );
}

#[test]
fn an_unknown_origin_is_refused() {
    let mut record = meta("s1").record();
    record
        .as_object_mut()
        .expect("object")
        .insert("origin".to_owned(), json!("human"));
    assert!(matches!(
        parse_header_record(&record).expect_err("an unknown origin"),
        HeaderError::WrongType { key: "origin", .. }
    ));
}

#[test]
fn a_seeded_header_without_a_cut_is_refused() {
    let mut stored = header("s1");
    stored.is_seeded = true;
    assert_eq!(
        StorageMeta::new(stored, None).expect_err("no cut"),
        HeaderError::SeededWithoutCut
    );
}

#[test]
fn an_unseeded_header_with_a_cut_is_refused() {
    assert_eq!(
        StorageMeta::new(header("s1"), Some(3)).expect_err("a cut without a seed"),
        HeaderError::CutWithoutSeed { cut: 3 }
    );
}

#[test]
fn a_seeded_header_keeps_its_exact_cut() {
    let mut stored = header("s1");
    stored.is_seeded = true;
    let meta = StorageMeta::new(stored, Some(7)).expect("a seeded header with its cut");
    assert_eq!(meta.inherited_event_count, 7);
}

#[test]
fn a_header_from_another_session_is_refused() {
    assert_eq!(
        assert_stored_id("s2", &header("s1")).expect_err("ids differ"),
        HeaderError::IdMismatch {
            requested: "s2".to_owned(),
            stored: "s1".to_owned()
        }
    );
    assert!(assert_stored_id("s1", &header("s1")).is_ok());
}

#[test]
fn a_newer_generation_says_to_upgrade_the_harness() {
    let mut stored = header("s1");
    stored.version = 5;
    let refusal = assert_version(&stored).expect_err("v5 is not readable here");
    assert!(refusal.is_newer());
    assert!(refusal.message().contains("newer harness"), "{}", refusal);
    assert!(!refusal.message().contains("corrupt"));
}

#[test]
fn an_older_generation_says_this_build_cannot_open_it() {
    let mut stored = header("s1");
    stored.version = 2;
    let refusal = assert_version(&stored).expect_err("v2 is not current here");
    assert!(!refusal.is_newer());
    assert!(refusal.message().contains("older than the supported"));
}

#[test]
fn the_current_generation_is_accepted() {
    assert!(assert_version(&header("s1")).is_ok());
}

// ---- storage contract --------------------------------------------------------------------------

#[test]
fn every_known_event_type_covers_the_real_vocabulary() {
    // A row for each type the enum has must be known here, and the list must not claim a type the
    // enum lacks: both directions are checked by the count and by the specific spellings.
    assert_eq!(KNOWN_EVENT_TYPES.len(), 18);
    for name in KNOWN_EVENT_TYPES {
        assert!(is_known_event_type(name), "{name}");
    }
    assert!(!is_known_event_type("image/offload"));
    assert!(!is_known_event_type("assistant/chunk"));
}

#[test]
fn a_known_row_set_validates() {
    let rows = vec![turn_start_row(1), json!({ "type": "turn/end", "turn": 1 })];
    assert!(validate_stored_records(&rows).is_ok());
}

#[test]
fn a_non_object_row_is_refused() {
    assert_eq!(
        validate_stored_records(&[json!("row")]).expect_err("a string is not a row"),
        ContractError::NotAnObject { index: 0 }
    );
}

#[test]
fn a_row_without_a_type_is_refused() {
    assert_eq!(
        validate_stored_records(&[json!({ "turn": 1 })]).expect_err("no type"),
        ContractError::MissingType { index: 0 }
    );
}

#[test]
fn an_unknown_required_row_refuses_the_log_and_names_the_sequence() {
    let rows = vec![
        turn_start_row(1),
        json!({ "type": "assistant/chunk", "seq": 4 }),
    ];
    let err = validate_stored_records(&rows).expect_err("an unknown type");
    assert_eq!(
        err,
        ContractError::UnknownEventType {
            type_name: "assistant/chunk".to_owned(),
            seq: Some(4)
        }
    );
    // The message must tell the user which way to move: this is a vocabulary refusal, not damage.
    assert!(err.to_string().contains("newer harness"), "{err}");
}

#[test]
fn an_unknown_but_ignorable_row_is_tolerated() {
    let rows = vec![json!({ "type": "future/thing", "seq": 1, "ignorable": true })];
    assert!(
        validate_stored_records(&rows).is_ok(),
        "a writer that marked a row ignorable promised nothing needed it"
    );
}

#[test]
fn an_unknown_row_marked_not_ignorable_still_refuses() {
    let rows = vec![json!({ "type": "future/thing", "ignorable": false })];
    assert!(matches!(
        validate_stored_records(&rows).expect_err("explicitly required"),
        ContractError::UnknownEventType { .. }
    ));
}

#[test]
fn the_retired_request_header_fallback_is_a_version_refusal_not_a_vocabulary_one() {
    let rows = vec![json!({
        "type": "request/header",
        "seq": 2,
        "data": { "reason": "fallback" }
    })];
    let err = validate_stored_records(&rows).expect_err("the retired delta codec shape");
    assert_eq!(err, ContractError::RetiredReason { seq: Some(2) });
    assert!(err.to_string().contains("pre-release"), "{err}");
}

#[test]
fn a_request_header_with_another_reason_is_only_unknown_vocabulary() {
    let rows = vec![json!({
        "type": "request/header",
        "data": { "reason": "full" }
    })];
    assert!(matches!(
        validate_stored_records(&rows).expect_err("retired type"),
        ContractError::UnknownEventType { .. }
    ));
}

#[test]
fn a_contiguous_batch_continues_the_cursor() {
    assert!(assert_contiguous(&[4, 5, 6], 4).is_ok());
    assert!(assert_contiguous(&[], 9).is_ok());
}

#[test]
fn a_batch_that_starts_elsewhere_is_refused() {
    assert_eq!(
        assert_contiguous(&[3, 4], 4).expect_err("starts one early"),
        ContractError::OutOfSequence {
            index: 0,
            expected: 4,
            found: 3
        }
    );
}

#[test]
fn a_batch_with_a_hole_is_refused_at_the_hole() {
    assert_eq!(
        assert_contiguous(&[4, 6], 4).expect_err("a hole"),
        ContractError::OutOfSequence {
            index: 1,
            expected: 5,
            found: 6
        }
    );
}

// ---- encoding and decoding ---------------------------------------------------------------------

#[test]
fn a_generation_round_trips_through_plaintext() {
    let bytes =
        encode_generation(&meta("s1"), &[turn_start_row(1)], Compression::Plain).expect("encode");
    let decoded = verify_bytes(&bytes, Compression::Plain).expect("decode");
    assert_eq!(decoded.meta.header.id, "s1");
    assert_eq!(decoded.rows.len(), 1);
    // One record per line, each terminated: that is what makes a torn tail decidable from bytes.
    let text = decode_generation(&bytes, Compression::Plain).expect("text");
    assert!(text.ends_with('\n'));
    assert_eq!(text.lines().count(), 2);
}

#[test]
fn a_generation_round_trips_through_zstd() {
    let rows = vec![turn_start_row(1), turn_start_row(2)];
    let bytes = encode_generation(&meta("s1"), &rows, Compression::Zstd).expect("encode zstd");
    assert_ne!(&bytes[..4], b"{\"ty", "the bytes really are compressed");
    let decoded = verify_bytes(&bytes, Compression::Zstd).expect("decode zstd");
    assert_eq!(decoded.rows, rows);
}

#[test]
fn an_empty_file_is_not_a_log() {
    assert!(matches!(
        verify_bytes(&[], Compression::Plain).expect_err("nothing to read"),
        GenerationError::HeaderMissing
    ));
}

#[test]
fn a_log_that_does_not_end_on_a_record_is_torn() {
    let bytes =
        encode_generation(&meta("s1"), &[turn_start_row(1)], Compression::Plain).expect("encode");
    let cut = bytes.len().saturating_sub(3);
    let torn = &bytes[..cut];
    assert!(matches!(
        verify_bytes(torn, Compression::Plain).expect_err("torn mid-record"),
        GenerationError::TornTail
    ));
}

#[test]
fn a_row_that_is_not_json_names_the_line() {
    let mut bytes = encode_generation(&meta("s1"), &[], Compression::Plain).expect("encode");
    bytes.extend_from_slice(b"not json\n");
    let err = verify_bytes(&bytes, Compression::Plain).expect_err("bad row");
    assert!(
        matches!(err, GenerationError::CorruptJson { line: 2, .. }),
        "{err}"
    );
}

#[test]
fn a_generation_with_a_refused_header_is_refused_as_a_whole() {
    let stored = StorageHeader {
        version: 4,
        cwd: Some("relative".to_owned()),
        ..header("s1")
    };
    let bytes = encode_generation(
        &StorageMeta {
            header: stored,
            inherited_event_count: 0,
        },
        &[],
        Compression::Plain,
    )
    .expect("encode");
    assert!(matches!(
        verify_bytes(&bytes, Compression::Plain).expect_err("relative cwd"),
        GenerationError::Header(HeaderError::RelativeCwd { .. })
    ));
}

#[test]
fn a_generation_holding_an_unknown_required_row_is_refused() {
    let bytes = encode_generation(
        &meta("s1"),
        &[json!({ "type": "future/thing" })],
        Compression::Plain,
    )
    .expect("encode");
    assert!(matches!(
        verify_bytes(&bytes, Compression::Plain).expect_err("unknown type"),
        GenerationError::Contract(ContractError::UnknownEventType { .. })
    ));
}

#[test]
fn a_generation_compressed_as_zstd_does_not_decode_as_plaintext() {
    let bytes =
        encode_generation(&meta("s1"), &[turn_start_row(1)], Compression::Zstd).expect("encode");
    // Reading a compressed artifact as plaintext must not yield a plausible log: it yields either a
    // decode error or a header refusal, never a valid generation.
    assert!(verify_bytes(&bytes, Compression::Plain).is_err());
}

// ---- identity and stability --------------------------------------------------------------------

#[test]
fn a_revision_changes_when_the_file_does() {
    let dir = scratch("revision");
    let path = dir.join("session.jsonl");
    std::fs::write(&path, b"a\n").expect("write");
    let first = session_extra_gap::identity(&path).expect("identity");
    std::fs::write(&path, b"a longer body\n").expect("rewrite");
    let second = session_extra_gap::identity(&path).expect("identity");
    assert!(!first.may_be_unchanged(second));
    assert_ne!(Revision::of(first), Revision::of(second));
}

#[test]
fn a_revision_is_stable_for_an_untouched_file() {
    let dir = scratch("revision-stable");
    let path = dir.join("session.jsonl");
    std::fs::write(&path, b"a\n").expect("write");
    assert_eq!(
        Revision::of(session_extra_gap::identity(&path).expect("identity")),
        Revision::of(session_extra_gap::identity(&path).expect("identity"))
    );
}

#[test]
fn a_stable_read_returns_the_bytes_and_their_identity() {
    let dir = scratch("stable");
    let path = dir.join("session.jsonl");
    std::fs::write(&path, b"body\n").expect("write");
    let stable = read_stable(&path).expect("a stable read");
    assert_eq!(stable.bytes, b"body\n");
    assert_eq!(
        stable.identity,
        session_extra_gap::identity(&path).expect("identity")
    );
}

#[test]
fn a_moved_source_is_detected_before_publication() {
    let dir = scratch("moved");
    let source = dir.join("session.v3.jsonl");
    std::fs::write(&source, b"old\n").expect("write");
    let witnessed = session_extra_gap::identity(&source).expect("identity");
    std::fs::write(&source, b"old and more\n").expect("rewrite");
    assert!(matches!(
        assert_source_unchanged(&source, witnessed).expect_err("the source moved"),
        GenerationError::SourceChanged { .. }
    ));
}

#[test]
fn an_untouched_source_passes_its_check() {
    let dir = scratch("unmoved");
    let source = dir.join("session.v3.jsonl");
    std::fs::write(&source, b"old\n").expect("write");
    let witnessed = session_extra_gap::identity(&source).expect("identity");
    assert!(assert_source_unchanged(&source, witnessed).is_ok());
}

// ---- publication -------------------------------------------------------------------------------

#[test]
fn a_staged_generation_is_a_synced_temporary() {
    let dir = scratch("stage");
    let staged = stage(&dir, b"generation").expect("stage");
    assert!(staged.path.starts_with(&dir));
    assert!(staged.path.to_string_lossy().ends_with(".tmp"));
    assert_eq!(std::fs::read(&staged.path).expect("read"), b"generation");
    assert_eq!(staged.bytes, 10);
    assert_eq!(staged.digest, digest_hex(b"generation"));
}

#[test]
fn staging_twice_in_one_directory_uses_two_names() {
    let dir = scratch("stage-two");
    let first = stage(&dir, b"one").expect("first");
    let second = stage(&dir, b"two").expect("second");
    assert_ne!(first.path, second.path);
    assert_eq!(std::fs::read(&first.path).expect("read"), b"one");
    assert_eq!(std::fs::read(&second.path).expect("read"), b"two");
}

#[test]
fn publication_creates_the_current_generation() {
    let dir = scratch("publish");
    let current = dir.join("session.v4.jsonl");
    let staged = stage(&dir, b"generation").expect("stage");
    let outcome = publish_exclusive(&staged, &current).expect("publish");
    assert!(outcome.is_published());
    assert_eq!(std::fs::read(&current).expect("read"), b"generation");
    // The staged link is cleaned up: one file is the generation, not two names for it.
    assert!(!staged.path.exists());
}

#[test]
fn a_second_publication_of_the_same_bytes_is_adopted_not_rewritten() {
    let dir = scratch("publish-adopt");
    let current = dir.join("session.v4.jsonl");
    let first = stage(&dir, b"generation").expect("first stage");
    let published = publish_exclusive(&first, &current).expect("first publish");
    let second = stage(&dir, b"generation").expect("second stage");
    let adopted = publish_exclusive(&second, &current).expect("second publish");
    assert!(published.is_published());
    assert!(
        matches!(adopted, PublishOutcome::Adopted { .. }),
        "the loser must adopt, not republish: {adopted:?}"
    );
    assert_eq!(published.identity(), adopted.identity());
    assert_eq!(std::fs::read(&current).expect("read"), b"generation");
}

#[test]
fn a_competing_publication_with_different_bytes_is_refused() {
    let dir = scratch("publish-conflict");
    let current = dir.join("session.v4.jsonl");
    let first = stage(&dir, b"generation A").expect("first stage");
    publish_exclusive(&first, &current).expect("first publish");
    let second = stage(&dir, b"generation B").expect("second stage");
    let err = publish_exclusive(&second, &current).expect_err("different bytes");
    assert!(
        matches!(err, GenerationError::TargetConflict { .. }),
        "{err}"
    );
    // The winner's bytes are untouched: the loser did not clobber them.
    assert_eq!(std::fs::read(&current).expect("read"), b"generation A");
}

#[test]
fn a_target_shorter_than_the_staging_is_refused() {
    let dir = scratch("publish-short");
    let current = dir.join("session.v4.jsonl");
    std::fs::write(&current, b"short").expect("pre-existing target");
    let staged = stage(&dir, b"a much longer generation").expect("stage");
    assert!(matches!(
        publish_exclusive(&staged, &current).expect_err("too short"),
        GenerationError::PrefixTooShort { .. }
    ));
}

#[test]
fn a_target_that_begins_with_the_staged_bytes_is_adopted_with_its_tail() {
    // The append-tail case: a live writer already extended the winner, and that continuation is a
    // valid generation rather than a disagreement.
    let dir = scratch("publish-tail");
    let current = dir.join("session.v4.jsonl");
    std::fs::write(&current, b"generationextended").expect("pre-existing target");
    let staged = stage(&dir, b"generation").expect("stage");
    let outcome = publish_exclusive(&staged, &current).expect("adopt with its tail");
    assert!(matches!(outcome, PublishOutcome::Adopted { .. }));
    assert_eq!(
        std::fs::read(&current).expect("read"),
        b"generationextended"
    );
}

#[test]
fn a_staged_file_from_another_directory_cannot_be_published() {
    let dir = scratch("publish-cross");
    let other = scratch("publish-cross-other");
    let current = dir.join("session.v4.jsonl");
    let staged = stage(&other, b"generation").expect("stage elsewhere");
    assert!(matches!(
        publish_exclusive(&staged, &current).expect_err("different directories"),
        GenerationError::TargetConflict { .. }
    ));
}

#[test]
fn publication_into_a_missing_directory_creates_it() {
    let dir = scratch("publish-missing").join("nested").join("session");
    let current = dir.join("session.v4.jsonl");
    let staged = stage(&dir, b"generation").expect("stage creates the directory");
    assert!(publish_exclusive(&staged, &current).is_ok());
    assert!(current.exists());
}

// ---- verification against a file ---------------------------------------------------------------

#[test]
fn a_verified_file_reports_its_count_and_digest() {
    let dir = scratch("verify");
    let path = dir.join("session.v4.jsonl");
    let bytes =
        encode_generation(&meta("s1"), &[turn_start_row(1)], Compression::Plain).expect("encode");
    std::fs::write(&path, &bytes).expect("write");
    let verified = verify_file(&path, Compression::Plain, "s1", 1, None).expect("verify");
    assert_eq!(verified.digest, digest_hex(&bytes));
    assert_eq!(verified.bytes, bytes.len());
    let decoded = verified.decoded.expect("a full verification decodes");
    assert_eq!(decoded.meta.header.id, "s1");
}

#[test]
fn a_file_holding_another_session_is_refused() {
    let dir = scratch("verify-id");
    let path = dir.join("session.v4.jsonl");
    let bytes = encode_generation(&meta("s1"), &[], Compression::Plain).expect("encode");
    std::fs::write(&path, &bytes).expect("write");
    assert!(matches!(
        verify_file(&path, Compression::Plain, "s2", 0, None).expect_err("wrong id"),
        GenerationError::IdMismatch { .. }
    ));
}

#[test]
fn a_file_with_the_wrong_event_count_is_refused() {
    let dir = scratch("verify-count");
    let path = dir.join("session.v4.jsonl");
    let bytes =
        encode_generation(&meta("s1"), &[turn_start_row(1)], Compression::Plain).expect("encode");
    std::fs::write(&path, &bytes).expect("write");
    assert!(matches!(
        verify_file(&path, Compression::Plain, "s1", 2, None).expect_err("count"),
        GenerationError::CountMismatch {
            found: 1,
            expected: 2
        }
    ));
}

#[test]
fn a_verified_prefix_proves_the_head_without_decoding_the_tail() {
    let dir = scratch("verify-prefix");
    let path = dir.join("session.v4.jsonl");
    let head =
        encode_generation(&meta("s1"), &[turn_start_row(1)], Compression::Plain).expect("encode");
    let mut extended = head.clone();
    extended.extend_from_slice(b"{\"type\":\"turn/end\"}\n");
    std::fs::write(&path, &extended).expect("write");

    let prefix = ExpectedPrefix::of(&head);
    let verified = verify_file(&path, Compression::Plain, "s1", 0, Some(&prefix))
        .expect("the head is the verified prefix");
    assert_eq!(verified.bytes, extended.len());
    assert!(
        verified.decoded.is_none(),
        "a prefix verification claims nothing about the tail"
    );
}

#[test]
fn a_prefix_that_does_not_match_is_refused() {
    let dir = scratch("verify-prefix-bad");
    let path = dir.join("session.v4.jsonl");
    std::fs::write(&path, b"generation B").expect("write");
    let prefix = ExpectedPrefix::of(b"generation A");
    assert!(matches!(
        verify_file(&path, Compression::Plain, "s1", 0, Some(&prefix)).expect_err("different head"),
        GenerationError::TargetConflict { .. }
    ));
}

#[test]
fn a_prefix_longer_than_the_file_is_refused() {
    let dir = scratch("verify-prefix-long");
    let path = dir.join("session.v4.jsonl");
    std::fs::write(&path, b"short").expect("write");
    let prefix = ExpectedPrefix::of(b"a longer generation than the file");
    assert!(matches!(
        verify_file(&path, Compression::Plain, "s1", 0, Some(&prefix))
            .expect_err("file is shorter"),
        GenerationError::PrefixTooShort { .. }
    ));
}

// ---- end to end --------------------------------------------------------------------------------

#[test]
fn a_migration_publishes_a_readable_current_generation() {
    // The whole path: plan the pair, read the historical generation, encode the current one, stage,
    // publish, and verify what is now on disk.
    let root = scratch("e2e");
    let dir = session_dir(&root, Some("/home/me/work"), "s1").expect("session directory");
    std::fs::create_dir_all(&dir).expect("create the session directory");

    let legacy = StorageMeta {
        header: StorageHeader {
            version: FormatVersion::V3.as_number(),
            ..header("s1")
        },
        inherited_event_count: 0,
    };
    let old_bytes = encode_generation(&legacy, &[turn_start_row(1)], Compression::Plain)
        .expect("encode the historical generation");
    let source = dir.join(generation_log_filename(
        FormatVersion::V3,
        Compression::Plain,
    ));
    std::fs::write(&source, &old_bytes).expect("write the historical generation");

    let current = dir.join(generation_log_filename(
        FormatVersion::V4,
        Compression::Plain,
    ));
    let plan = plan_generation_paths(
        &source,
        FormatVersion::V3,
        &current,
        FormatVersion::V4,
        Compression::Plain,
    )
    .expect("one session's two generations");

    let historical = read_stable(&plan.source).expect("read the source");
    assert_source_unchanged(&plan.source, historical.identity).expect("the source has not moved");

    let new_bytes = encode_generation(&meta("s1"), &[turn_start_row(1)], Compression::Plain)
        .expect("encode the current generation");
    let staged = stage(
        plan.current.parent().expect("a session directory"),
        &new_bytes,
    )
    .expect("stage");
    let outcome = publish_exclusive(&staged, &plan.current).expect("publish");
    assert!(outcome.is_published());

    let verified = verify_file(&plan.current, Compression::Plain, "s1", 1, None)
        .expect("the published generation verifies");
    assert_eq!(verified.digest, digest_hex(&new_bytes));
    // The historical generation is still there: a migration that deleted its source could not be
    // retried, and could not be inspected after a failure.
    assert!(plan.source.exists());
}

#[test]
fn a_session_event_type_name_is_the_tag_the_log_writes() {
    // The known-type list is only correct if it spells types the way `SessionEvent` serializes them.
    // One representative event is enough to catch a rename: the tag is derived from the derive, not
    // from this module.
    let event = SessionEvent::TurnStart { turn: 1 };
    let text = serde_json::to_string(&event).expect("serialize");
    let value: Value = serde_json::from_str(&text).expect("parse");
    let tag = value
        .get("type")
        .and_then(Value::as_str)
        .expect("a tagged event");
    assert!(is_known_event_type(tag), "{tag} must be a known type");
}

/// The digest helper is exercised against the published FIPS vectors, so a transcription error in
/// the round constants cannot pass as a working hash.
#[test]
fn sha256_matches_the_published_vectors() {
    assert_eq!(
        digest_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        digest_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        digest_hex(b"The quick brown fox jumps over the lazy dog"),
        "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
    );
    // A length that crosses the 64-byte block boundary and the padding boundary.
    assert_eq!(
        digest_hex(&[b'a'; 64]),
        "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
    );
}

#[test]
fn a_path_built_for_a_hostile_id_still_lands_under_the_root() {
    let root = PathBuf::from("/sessions");
    for id in ["../x", "..", ".", "/abs", "a/b", "~", "a\0b"] {
        let dir = session_dir(&root, Some("/w"), id).expect("every string encodes");
        assert!(dir.starts_with(&root), "{id} escaped to {dir:?}");
        let depth = dir
            .strip_prefix(&root)
            .map_or(usize::MAX, |rest| rest.components().count());
        assert_eq!(depth, 2, "{id} produced {dir:?}");
    }
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::session_extra_gap as extra;
}
