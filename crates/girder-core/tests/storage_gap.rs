//! Tests for the `per-record` storage layout.
//!
//! Upstream reference: `packages/storage/storage-json/tests/per-record-unit.spec.ts` and
//! `format.spec.ts`. The properties pinned here are the ones that make this layout different from
//! `Single`, and each would fail silently rather than loudly if it regressed:
//!
//! - a write touches **one** document and leaves its siblings byte-identical;
//! - a foreign document (malformed, or stamped with an unaccepted version) reads as **absent** and
//!   never bricks the unit — the deliberate opposite of the whole-unit layout;
//! - a batch delete stages before it unlinks, so a rejected batch is unapplied;
//! - an unsafe key is refused rather than escaped into one path with a safe one;
//! - the legacy whole-unit file is migrated once and never modified.
//!
//! # Why the module is included by path
//!
//! `girder-core`'s crate root does not declare `storage_gap`; adding `pub mod storage_gap;` and its
//! `pub use` belongs to the integrator. The module is compiled into this test binary directly, which
//! also means it has no dependency on the rest of the crate today. Once the wiring lands, the `#[path]`
//! attribute should be replaced with `use girder_core::storage_gap::*;`.
//!
//! # Why there are `crate`-root shim modules below
//!
//! The module's doc comments link to `crate::storage::{FileBackend, KvBackend, Layout, Snapshot,
//! UnitDescriptor, is_valid_record_key, is_valid_unit_name}`, `crate::time::civil_from_days`, and
//! `crate::BoxFut`. Those are real items *in the crate*, but a `#[path]` include makes the module a
//! child of this test binary instead, where `crate::storage` does not exist — and under `-D warnings`
//! an unresolved link is an error, not a footnote.
//!
//! The shims below re-export the genuine crate items under exactly the paths the links name, so the
//! documentation keeps pointing at its true destination instead of being diluted to prose. Naming them
//! is also a standing check that the shapes this module claims to mirror still exist under those names.
//! They are test-only artifacts and disappear with the `#[path]` include.

#[path = "../src/storage_gap.rs"]
mod storage_gap;

/// Stands in for the crate's `storage` module, for this module's documentation links.
#[allow(unused_imports)]
mod storage {
    pub use girder_core::{
        FileBackend, KvBackend, Layout, Snapshot, UnitDescriptor, is_valid_record_key,
        is_valid_unit_name,
    };
}

/// Stands in for the crate's `time` module, for this module's documentation links.
#[allow(unused_imports)]
mod time {
    pub use girder_core::civil_from_days;
}

/// Stands in for the crate root's `BoxFut` re-export, for this module's documentation links.
#[allow(unused_imports)]
pub use girder_core::BoxFut;

use serde_json::{Value, json};
use storage_gap::{
    Descriptor, GLOBAL_DOCUMENT, Loaded, PerRecordUnit, RECORD_SUFFIX, RecordError, STAGED_SUFFIX,
    format_backup_stamp, global_path, is_safe_record_key, is_valid_unit_name, parse_record,
    record_path, serialize_record, unit_directory,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// A private scratch directory for one test, removed first so a rerun starts clean.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-record-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

fn descriptor(version: u32) -> Descriptor {
    Descriptor::new(
        "sessions",
        version,
        vec!["records".to_owned(), "scratch".to_owned()],
    )
}

/// Every file under `root`, relative to it, sorted — the medium's whole shape.
fn tree(root: &std::path::Path) -> Vec<String> {
    fn walk(base: &std::path::Path, path: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                walk(base, &child, out);
            } else if let Ok(relative) = child.strip_prefix(base) {
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn read(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).expect("document")
}

// ---- name and key rules -----------------------------------------------------------------------

#[test]
fn unit_names_must_be_lowercase_identifiers() {
    for good in ["a", "sessions", "a_1", "abc_def_9"] {
        assert!(is_valid_unit_name(good), "{good}");
    }
    for bad in ["", "A", "1a", "a-b", "a b", "_a", "a.b", "../etc"] {
        assert!(!is_valid_unit_name(bad), "{bad:?}");
    }
}

#[test]
fn record_keys_are_path_segments_only() {
    for good in ["a", "A", "0", "a-b_c", "deadbeef-1234"] {
        assert!(is_safe_record_key(good), "{good}");
    }
    // Every one of these would either escape the table directory or collapse onto another key.
    for bad in ["", ".", "..", "../x", "a/b", "a b", "a.b", "a\nb", "é"] {
        assert!(!is_safe_record_key(bad), "{bad:?}");
    }
}

#[test]
fn a_descriptor_refuses_an_unsafe_unit_name_before_it_becomes_a_path() {
    let root = scratch("bad-unit");
    let error = PerRecordUnit::open(&root, Descriptor::new("../escape", 1, Vec::new()))
        .expect_err("unsafe unit name");
    assert!(
        matches!(&error, RecordError::InvalidName { what, name } if *what == "unit" && name == "../escape"),
        "{error}"
    );
    // Nothing was created: validation precedes any medium access.
    assert!(tree(&root).is_empty());
}

#[test]
fn a_descriptor_refuses_an_unsafe_table_name() {
    let root = scratch("bad-table");
    let error = PerRecordUnit::open(
        &root,
        Descriptor::new("sessions", 1, vec!["../x".to_owned()]),
    )
    .expect_err("unsafe table name");
    assert!(
        matches!(&error, RecordError::InvalidName { what, .. } if *what == "table"),
        "{error}"
    );
}

// ---- document format --------------------------------------------------------------------------

#[test]
fn a_record_document_is_pretty_printed_with_a_trailing_newline() {
    // The format is the backend's reason to exist: a human must be able to read the tree.
    let text = serialize_record(3, &json!({ "title": "hi" })).expect("encode");
    assert!(text.ends_with("}\n"), "{text:?}");
    assert!(text.contains("\n  \"version\": 3"), "{text:?}");
    assert!(text.contains("\n  \"record\": {"), "{text:?}");
    assert_eq!(
        text,
        "{\n  \"record\": {\n    \"title\": \"hi\"\n  },\n  \"version\": 3\n}\n"
    );
}

#[test]
fn a_foreign_record_document_reads_as_absent() {
    let accepted = [2, 1];
    // Accepted stamps decode.
    assert_eq!(
        parse_record("{\"version\":1,\"record\":42}", &accepted),
        Some(json!(42))
    );
    assert_eq!(
        parse_record("{\"version\":2,\"record\":\"x\"}", &accepted),
        Some(json!("x"))
    );
    // Anything else is foreign, and foreign is silence rather than an error.
    for foreign in [
        "",                                 // empty
        "not json",                         // malformed
        "[]",                               // an array is not a document
        "\"text\"",                         // a scalar is not a document
        "{\"record\":1}",                   // no stamp
        "{\"version\":\"1\",\"record\":1}", // non-numeric stamp
        "{\"version\":9,\"record\":1}",     // unaccepted stamp
        "{\"version\":-1,\"record\":1}",    // not a version at all
        "{\"version\":1}",                  // no record member
    ] {
        assert_eq!(parse_record(foreign, &accepted), None, "{foreign:?}");
    }
}

#[test]
fn a_null_record_member_is_a_value_not_an_absence() {
    // Upstream destructures the field and tests it against `undefined`, so a present `null` is data.
    assert_eq!(
        parse_record("{\"version\":1,\"record\":null}", &[1]),
        Some(Value::Null)
    );
}

// ---- the layout itself ------------------------------------------------------------------------

#[test]
fn a_unit_that_was_never_written_reads_as_the_empty_unit() {
    let root = scratch("empty");
    let unit = PerRecordUnit::open(&root, descriptor(1).with_global()).expect("open");
    let loaded = unit.load().expect("load");
    // Every declared table is present, so a caller need not distinguish absent from empty.
    assert_eq!(
        loaded.tables.keys().cloned().collect::<Vec<_>>(),
        vec!["records".to_owned(), "scratch".to_owned()]
    );
    assert!(
        loaded
            .tables
            .values()
            .all(std::collections::BTreeMap::is_empty)
    );
    assert_eq!(loaded.global, None);
    // Opening and loading touch nothing.
    assert!(tree(&root).is_empty());
}

#[test]
fn a_put_materializes_one_document_and_only_that_document() {
    let root = scratch("one-doc");
    let mut unit = PerRecordUnit::open(&root, descriptor(1).with_global()).expect("open");
    unit.put("records", "a", &json!({"n": 1})).expect("put a");
    let sibling = read(&record_path(&root, "sessions", "records", "a"));
    unit.put("records", "b", &json!({"n": 2})).expect("put b");

    // The property the layout exists for: writing `b` did not rewrite `a`.
    assert_eq!(
        read(&record_path(&root, "sessions", "records", "a")),
        sibling
    );
    assert_eq!(
        tree(&root),
        vec![
            "sessions/records/a.json".to_owned(),
            "sessions/records/b.json".to_owned(),
        ]
    );
    let loaded = unit.load().expect("load");
    assert_eq!(loaded.tables["records"]["a"], json!({"n": 1}));
    assert_eq!(loaded.tables["records"]["b"], json!({"n": 2}));
}

#[test]
fn a_record_document_is_stamped_with_the_current_version() {
    let root = scratch("stamp");
    let mut unit = PerRecordUnit::open(&root, descriptor(7)).expect("open");
    unit.put("records", "a", &json!(1)).expect("put");
    let text = read(&record_path(&root, "sessions", "records", "a"));
    assert!(text.contains("\"version\": 7"), "{text}");
}

#[test]
fn the_global_slot_is_its_own_document() {
    let root = scratch("global");
    let mut unit = PerRecordUnit::open(&root, descriptor(1).with_global()).expect("open");
    unit.set_global(&json!({"theme": "dark"}))
        .expect("set global");
    assert_eq!(
        read(&global_path(&root, "sessions")),
        "{\n  \"record\": {\n    \"theme\": \"dark\"\n  },\n  \"version\": 1\n}\n"
    );
    assert_eq!(
        unit.load().expect("load").global,
        Some(json!({"theme": "dark"}))
    );
    // The global document is named exactly `global.json`, because the on-disk name is the format.
    assert_eq!(GLOBAL_DOCUMENT, "global.json");
    assert!(
        unit_directory(&root, "sessions")
            .join("global.json")
            .exists()
    );
}

#[test]
fn a_unit_that_declares_no_global_refuses_to_write_one() {
    let root = scratch("no-global");
    let mut unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    let error = unit.set_global(&json!(1)).expect_err("no global slot");
    assert!(
        matches!(&error, RecordError::NoGlobal { unit } if unit == "sessions"),
        "{error}"
    );
}

#[test]
fn an_undeclared_table_is_refused_rather_than_created() {
    let root = scratch("unknown-table");
    let mut unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    let error = unit
        .put("elsewhere", "a", &json!(1))
        .expect_err("undeclared table");
    assert!(
        matches!(&error, RecordError::UnknownTable { table, .. } if table == "elsewhere"),
        "{error}"
    );
    assert!(tree(&root).is_empty(), "a refusal must create nothing");
}

#[test]
fn an_unsafe_key_is_refused_rather_than_escaped() {
    let root = scratch("unsafe-key");
    let mut unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    for key in ["../escape", "a/b", "a.b", ""] {
        let error = unit.put("records", key, &json!(1)).expect_err("unsafe key");
        assert!(
            matches!(&error, RecordError::UnsafeKey { key: refused, .. } if refused == key),
            "{error}"
        );
    }
    assert!(tree(&root).is_empty());
}

// ---- the per-record contract: a bad document reads as absent --------------------------------

#[test]
fn a_malformed_document_is_skipped_and_the_rest_of_the_unit_survives() {
    let root = scratch("malformed");
    let mut unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    unit.put("records", "good", &json!(1)).expect("put");
    unit.put("records", "bad", &json!(2)).expect("put");
    std::fs::write(
        record_path(&root, "sessions", "records", "bad"),
        "{ truncated",
    )
    .expect("corrupt");

    // One bad file must not brick the unit: this is the whole per-record contract.
    let loaded = unit.load().expect("load");
    assert_eq!(loaded.tables["records"]["good"], json!(1));
    assert!(!loaded.tables["records"].contains_key("bad"));

    // And the unit is still writable, so the bad document is discardable rather than fatal.
    unit.put("records", "bad", &json!(3)).expect("rewrite");
    assert_eq!(
        unit.load().expect("load").tables["records"]["bad"],
        json!(3)
    );
}

#[test]
fn a_document_stamped_with_an_unaccepted_version_is_discarded_not_migrated() {
    let root = scratch("stale-stamp");
    let mut unit = PerRecordUnit::open(&root, descriptor(2)).expect("open");
    unit.put("records", "old", &json!({"n": 1})).expect("put");
    // Rewrite the document under a version this build does not accept.
    std::fs::write(
        record_path(&root, "sessions", "records", "old"),
        "{\n  \"version\": 99,\n  \"record\": {\n    \"n\": 1\n  }\n}\n",
    )
    .expect("restamp");
    assert!(!unit.load().expect("load").tables["records"].contains_key("old"));
}

#[test]
fn a_declared_compatible_version_is_read_without_being_restamped() {
    let root = scratch("compatible");
    let unit = PerRecordUnit::open(&root, descriptor(2).compatible_with(1)).expect("open");
    let path = record_path(&root, "sessions", "records", "kept");
    std::fs::create_dir_all(path.parent().expect("table directory")).expect("table directory");
    std::fs::write(&path, "{\"version\": 1, \"record\": {\"n\": 1}}\n").expect("seed");
    assert_eq!(
        unit.load().expect("load").tables["records"]["kept"],
        json!({"n": 1})
    );
    // An ordinary read must not rewrite the document: untouched records keep their saved stamp, which
    // is why the owner has to vouch that the current schema accepts a mixed generation.
    assert!(read(&path).contains("\"version\": 1"));
}

#[test]
fn a_non_json_file_in_a_table_directory_is_ignored() {
    let root = scratch("residue");
    let mut unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    unit.put("records", "a", &json!(1)).expect("put");
    // Cleanup residue and stray documents are invisible, which is what makes a staged delete safe.
    std::fs::write(
        unit_directory(&root, "sessions")
            .join("records")
            .join(format!("b{STAGED_SUFFIX}")),
        "{\"version\": 1, \"record\": 2}",
    )
    .expect("residue");
    std::fs::write(
        unit_directory(&root, "sessions")
            .join("records")
            .join("notes.txt"),
        "ignored",
    )
    .expect("residue");
    let loaded = unit.load().expect("load");
    assert_eq!(loaded.tables["records"].len(), 1);
    assert!(loaded.tables["records"].contains_key("a"));
}

// ---- deletion ---------------------------------------------------------------------------------

#[test]
fn a_delete_is_idempotent_and_removes_the_document() {
    let root = scratch("delete");
    let mut unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    unit.put("records", "a", &json!(1)).expect("put");
    assert_eq!(unit.delete("records", "a").expect("delete"), 1);
    assert!(unit.load().expect("load").tables["records"].is_empty());
    assert_eq!(tree(&root), Vec::<String>::new());
    // Deleting a key that was never written is a no-op, not a failure.
    assert_eq!(unit.delete("records", "a").expect("delete again"), 0);
}

#[test]
fn a_batch_delete_removes_exactly_its_keys() {
    let root = scratch("batch");
    let mut unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    for key in ["a", "b", "c", "d"] {
        unit.put("records", key, &json!(key)).expect("put");
    }
    // Duplicates in the request are one document, and a missing key is not a failure.
    assert_eq!(
        unit.delete_many("records", &["a", "b", "a", "missing"])
            .expect("batch"),
        2
    );
    let loaded = unit.load().expect("load");
    assert_eq!(
        loaded.tables["records"].keys().cloned().collect::<Vec<_>>(),
        vec!["c".to_owned(), "d".to_owned()]
    );
    // Staged documents are unlinked after a successful batch.
    assert_eq!(
        tree(&root),
        vec![
            "sessions/records/c.json".to_owned(),
            "sessions/records/d.json".to_owned(),
        ]
    );
}

#[test]
fn a_batch_with_one_unsafe_key_is_rejected_before_any_document_is_staged() {
    let root = scratch("batch-unsafe");
    let mut unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    unit.put("records", "a", &json!(1)).expect("put");
    let before = tree(&root);
    let error = unit
        .delete_many("records", &["a", "../escape"])
        .expect_err("unsafe key");
    assert!(
        matches!(&error, RecordError::UnsafeKey { key, .. } if key == "../escape"),
        "{error}"
    );
    // Validation precedes any medium access, so the valid key of the same batch survives.
    assert_eq!(tree(&root), before);
    assert_eq!(unit.load().expect("load").tables["records"]["a"], json!(1));
}

// ---- legacy bootstrap -------------------------------------------------------------------------

#[test]
fn a_legacy_whole_unit_file_is_migrated_once_and_left_untouched() {
    let root = scratch("legacy");
    let legacy = unit_directory(&root, "sessions").with_file_name("sessions.json");
    std::fs::create_dir_all(&root).expect("root");
    let original = "{\n  \"unit\": {\n    \"name\": \"sessions\",\n    \"version\": 1\n  },\n  \"tables\": {\n    \"records\": {\n      \"a\": 1,\n      \"b\": 2\n    }\n  }\n}\n";
    std::fs::write(&legacy, original).expect("legacy");

    let unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    let loaded = unit.load().expect("load");
    assert_eq!(loaded.tables["records"]["a"], json!(1));
    assert_eq!(loaded.tables["records"]["b"], json!(2));
    // The migration writes the new tree and never edits or deletes its source.
    assert_eq!(read(&legacy), original);
    assert_eq!(
        read(&record_path(&root, "sessions", "records", "a")),
        "{\n  \"record\": 1,\n  \"version\": 1\n}\n"
    );
}

#[test]
fn a_legacy_file_stamped_with_an_unaccepted_version_is_left_alone() {
    let root = scratch("legacy-stale");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        unit_directory(&root, "sessions").with_file_name("sessions.json"),
        "{\"unit\":{\"name\":\"sessions\",\"version\":99},\"tables\":{\"records\":{\"a\":1}}}\n",
    )
    .expect("legacy");
    let unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    // Migrating records the owner never vouched for would stamp them current and turn a stale cache
    // into schema failures downstream.
    assert!(unit.load().expect("load").tables["records"].is_empty());
}

#[test]
fn a_legacy_file_belonging_to_another_unit_is_ignored() {
    let root = scratch("legacy-foreign");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        unit_directory(&root, "sessions").with_file_name("sessions.json"),
        "{\"unit\":{\"name\":\"elsewhere\",\"version\":1},\"tables\":{\"records\":{\"a\":1}}}\n",
    )
    .expect("legacy");
    let unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    assert!(unit.load().expect("load").tables["records"].is_empty());
}

#[test]
fn an_existing_empty_table_directory_suppresses_the_bootstrap() {
    let root = scratch("legacy-suppressed");
    // The unit was materialized with a declared table directory, even though it holds nothing. That
    // presence is what says "this tree is authoritative", so the legacy file must not resurrect.
    std::fs::create_dir_all(unit_directory(&root, "sessions").join("records")).expect("table dir");
    std::fs::write(
        unit_directory(&root, "sessions").with_file_name("sessions.json"),
        "{\"unit\":{\"name\":\"sessions\",\"version\":1},\"tables\":{\"records\":{\"a\":1}}}\n",
    )
    .expect("legacy");
    let unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    assert!(unit.load().expect("load").tables["records"].is_empty());
    // And nothing was migrated onto the medium.
    assert!(!record_path(&root, "sessions", "records", "a").exists());
}

#[test]
fn a_malformed_legacy_file_reads_as_the_empty_unit() {
    let root = scratch("legacy-malformed");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(
        unit_directory(&root, "sessions").with_file_name("sessions.json"),
        "{ not json",
    )
    .expect("legacy");
    let unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    // Not ours to interpret or delete.
    assert!(unit.load().expect("load").tables["records"].is_empty());
}

// ---- lifecycle --------------------------------------------------------------------------------

#[test]
fn a_closed_unit_reports_closed_from_every_operation() {
    let root = scratch("closed");
    let mut unit = PerRecordUnit::open(&root, descriptor(1).with_global()).expect("open");
    unit.close();
    unit.close(); // idempotent
    assert!(unit.is_closed());
    for error in [
        unit.load().expect_err("load"),
        unit.put("records", "a", &json!(1)).expect_err("put"),
        unit.delete("records", "a").expect_err("delete"),
        unit.delete_many("records", &["a"]).expect_err("batch"),
        unit.set_global(&json!(1)).expect_err("global"),
        unit.backup("records", "a", 0, 0).expect_err("backup"),
    ] {
        assert!(
            matches!(&error, RecordError::Closed { unit } if unit == "sessions"),
            "{error}"
        );
    }
}

#[test]
fn close_is_idempotent_and_always_available() {
    let root = scratch("close-idempotent");
    let mut unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    assert!(!unit.is_closed());
    unit.close();
    unit.close();
    assert!(unit.is_closed());
}

// NOTE: `RecordError::UnknownState` is the one arm of this module no test reaches. It is set only when
// a failed batch delete cannot restore documents it had already staged, which needs the *restoration*
// rename to fail after the staging rename succeeded — a fault the public API cannot induce
// deterministically, because both operate on the same directory within one call. Testing it would mean
// racing the filesystem, which would pin a flake instead of the behaviour. The gate itself is covered:
// every operation routes through `assert_open`, which the closed-unit test exercises for both arms.

#[test]
fn a_rejected_batch_leaves_the_unit_usable() {
    let root = scratch("not-poisoned");
    let mut unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    unit.put("records", "a", &json!(1)).expect("put");
    // A rejection is a rejection, not a state change: the unit must still read and write afterwards.
    assert!(unit.delete_many("records", &["../bad"]).is_err());
    assert_eq!(unit.load().expect("load").tables["records"]["a"], json!(1));
    unit.put("records", "b", &json!(2)).expect("still writable");
    assert_eq!(unit.load().expect("load").tables["records"].len(), 2);
}

// ---- backup -----------------------------------------------------------------------------------

#[test]
fn a_backup_moves_the_document_out_of_the_readable_set() {
    let root = scratch("backup");
    let mut unit = PerRecordUnit::open(&root, descriptor(1)).expect("open");
    unit.put("records", "a", &json!(1)).expect("put");
    // 2026-11-16T14:04:00Z at UTC+08:00 is 2026-11-16 22:04 local.
    let millis = 1_794_837_840_000;
    let moved = unit.backup("records", "a", millis, 480).expect("backup");
    assert!(
        moved.to_string_lossy().ends_with(".json.bak.202611162204"),
        "{moved:?}"
    );
    // The moved file no longer ends in `.json`, so every later read ignores it while the bytes stay.
    assert!(!record_path(&root, "sessions", "records", "a").exists());
    assert!(moved.exists());
    assert!(unit.load().expect("load").tables["records"].is_empty());
}

#[test]
fn a_backup_stamp_is_local_time_with_a_four_digit_year() {
    // 1970-01-01T00:00:00Z at UTC+0 is the epoch itself.
    assert_eq!(format_backup_stamp(0, 0).as_deref(), Some("197001010000"));
    // Six hours east of UTC, from the same instant: the date rolls forward.
    assert_eq!(format_backup_stamp(0, 360).as_deref(), Some("197001010600"));
    // Five hours west: the previous evening.
    assert_eq!(
        format_backup_stamp(0, -300).as_deref(),
        Some("196912311900")
    );
    // A leap day, and the last minute of a year.
    assert_eq!(
        format_backup_stamp(1_709_164_800_000, 0).as_deref(),
        Some("202402290000")
    );
    assert_eq!(
        format_backup_stamp(1_735_689_599_000, 0).as_deref(),
        Some("202412312359")
    );
    // The offset is applied in minutes, not hours: one minute past midnight, one minute east.
    assert_eq!(format_backup_stamp(0, 1).as_deref(), Some("197001010001"));
    // A year outside four digits is refused rather than emitted in a form that sorts wrongly.
    assert_eq!(format_backup_stamp(253_402_300_800_000, 0), None);
    assert_eq!(format_backup_stamp(i64::MAX, 0), None);
}

#[test]
fn a_backup_stamp_agrees_with_the_crate_time_module() {
    // The stamp formatter carries a private copy of `civil_from_days` because the module compiles
    // without a crate root. This pins the copy against the real one across a wide corpus, so the
    // duplication cannot drift.
    let mut millis = 0i64;
    let mut checked = 0u32;
    while millis < 4_102_444_800_000 {
        // Every 41 days, a prime stride that walks all months, leap years, and weekdays.
        let days = millis.div_euclid(86_400_000);
        let (crate_year, crate_month, crate_day) = girder_core::civil_from_days(days);
        let stamp = format_backup_stamp(millis, 0).expect("stamp");
        assert_eq!(
            &stamp[..8],
            format!("{crate_year:04}{crate_month:02}{crate_day:02}"),
            "{millis}"
        );
        assert_eq!(&stamp[8..], "0000");
        millis += 41 * 86_400_000;
        checked += 1;
    }
    assert!(checked > 30, "corpus too small: {checked}");
}

// ---- the shape adapter ------------------------------------------------------------------------

#[test]
fn the_descriptor_and_snapshot_shapes_mirror_the_hub_vocabulary() {
    // The wiring swap is a field-by-field move, so the shapes must line up exactly. This test is the
    // contract for that move: it fails if either side gains or loses a field.
    let descriptor = Descriptor::new("sessions", 3, vec!["records".to_owned()])
        .with_global()
        .compatible_with(2);
    assert_eq!(descriptor.accepted_versions(), vec![3, 2]);

    let hub = girder_core::UnitDescriptor::new("sessions", 3, vec!["records".to_owned()])
        .with_global()
        .per_record()
        .compatible_with(2);
    assert_eq!(hub.name, descriptor.name);
    assert_eq!(hub.version, descriptor.version);
    assert_eq!(hub.tables, descriptor.tables);
    assert_eq!(hub.has_global, descriptor.has_global);
    assert_eq!(hub.compatible_versions, descriptor.compatible_versions);
    assert_eq!(hub.layout, girder_core::Layout::PerRecord);

    let loaded = Loaded {
        tables: std::collections::BTreeMap::from([(
            "records".to_owned(),
            std::collections::BTreeMap::from([("a".to_owned(), json!(1))]),
        )]),
        global: Some(json!({"g": 1})),
    };
    let snapshot = girder_core::Snapshot {
        tables: loaded.tables.clone(),
        global: loaded.global.clone(),
    };
    assert_eq!(snapshot.tables, loaded.tables);
    assert_eq!(snapshot.global, loaded.global);
}

#[test]
fn the_restated_name_rules_agree_with_the_hub() {
    // The two restatements exist only because this module must compile without a crate root. Pinning
    // them here means the duplication cannot drift into two different definitions of a valid name.
    let corpus = [
        "",
        "a",
        "A",
        "1a",
        "_a",
        "a-b",
        "a_b",
        "a.b",
        "a b",
        "../x",
        "sessions",
        "Sessions",
        "abc_def_9",
        "é",
        "a\nb",
        "-",
        "a-",
        "0",
    ];
    for name in corpus {
        assert_eq!(
            is_valid_unit_name(name),
            girder_core::is_valid_unit_name(name),
            "unit name rule disagrees on {name:?}"
        );
        assert_eq!(
            is_safe_record_key(name),
            girder_core::is_valid_record_key(name),
            "record key rule disagrees on {name:?}"
        );
    }
}

#[test]
fn the_documented_suffixes_are_the_ones_on_disk() {
    assert_eq!(RECORD_SUFFIX, ".json");
    // A staged document must not be readable, which is exactly why it cannot end in RECORD_SUFFIX.
    assert!(!STAGED_SUFFIX.ends_with(RECORD_SUFFIX));
    assert_eq!(STAGED_SUFFIX, ".json.deleting");
}

#[test]
fn the_unit_exposes_the_descriptor_and_directory_it_was_opened_with() {
    let root = scratch("introspect");
    let opened = descriptor(4).with_global().compatible_with(3);
    let unit = PerRecordUnit::open(&root, opened.clone()).expect("open");
    // Both accessors report what the caller supplied, so a host can attribute a unit without keeping
    // its own copy of either value.
    assert_eq!(unit.descriptor(), &opened);
    assert_eq!(unit.directory(), unit_directory(&root, "sessions"));
    assert!(unit.directory().ends_with("sessions"));
}
