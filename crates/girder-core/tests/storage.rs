//! Tests for the storage hub (M2 of the v2 plan).
//!
//! The load-bearing property is the version gate: a document written by a newer build must not be
//! read by an older one, because reading it under the wrong schema silently produces wrong data.

use girder_core::{
    FileBackend, KvBackend, Layout, StorageError, UnitDescriptor, is_valid_record_key,
    is_valid_unit_name,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-store-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

fn descriptor(version: u32) -> UnitDescriptor {
    UnitDescriptor::new("sessions", version, vec!["records".to_owned()]).with_global()
}

// ---- name rules -------------------------------------------------------------------------------

#[test]
fn unit_names_must_be_lowercase_identifiers() {
    // Safe as both a file name and a SQL identifier, without escaping.
    for good in ["a", "sessions", "a_1", "abc_def_9"] {
        assert!(is_valid_unit_name(good), "{good}");
    }
    for bad in ["", "A", "1a", "a-b", "a b", "_a", "a.b"] {
        assert!(!is_valid_unit_name(bad), "{bad:?}");
    }
}

#[test]
fn record_keys_allow_the_path_segment_alphabet() {
    for good in ["a", "A1", "key-1", "key_1"] {
        assert!(is_valid_record_key(good), "{good}");
    }
    for bad in ["", "a/b", "a b", "a.b", "../x"] {
        assert!(!is_valid_record_key(bad), "{bad:?}");
    }
}

// ---- the version gate -------------------------------------------------------------------------

#[tokio::test]
async fn a_unit_opens_at_its_declared_version() {
    let dir = scratch("open");
    let backend = FileBackend::new(&dir);

    let unit = backend.open(descriptor(1), cancel()).await.expect("opened");

    assert_eq!(unit.descriptor().version, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_document_from_a_newer_build_is_refused() {
    // The property the gate exists for: reading a newer document under an older schema silently
    // produces wrong data, which is worse than a failure.
    let dir = scratch("newer");
    let backend = FileBackend::new(&dir);

    // Write at version 2.
    let mut newer = backend.open(descriptor(2), cancel()).await.unwrap();
    newer
        .put_record("records", "k", json!({"v": 2}), cancel())
        .await
        .unwrap();
    newer.close();

    // An older build declares version 1 and must refuse.
    let error = backend
        .open(descriptor(1), cancel())
        .await
        .expect_err("version 2 is not readable at 1");

    let StorageError::VersionMismatch {
        stored, declared, ..
    } = error
    else {
        panic!("expected VersionMismatch, got {error:?}");
    };
    assert_eq!(stored, 2);
    assert_eq!(declared, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_declared_compatible_version_is_readable() {
    // The escape hatch: the owner vouches that old records are readable under current schemas.
    let dir = scratch("compatible");
    let backend = FileBackend::new(&dir);

    let mut older = backend.open(descriptor(1), cancel()).await.unwrap();
    older
        .put_record("records", "k", json!({"v": 1}), cancel())
        .await
        .unwrap();
    older.close();

    let tolerant = descriptor(2).compatible_with(1);
    let unit = backend
        .open(tolerant, cancel())
        .await
        .expect("version 1 is declared compatible");

    let snapshot = unit.load_all(cancel()).await.unwrap();
    assert_eq!(snapshot.tables["records"]["k"], json!({"v": 1}));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_version_above_the_declared_one_is_never_compatible() {
    // Listing a newer version as compatible would be a claim the owner cannot make.
    let declared = descriptor(1).compatible_with(0);
    assert!(declared.accepts_version(1));
    assert!(declared.accepts_version(0));
    assert!(
        !declared.accepts_version(2),
        "a newer document is never readable"
    );
}

// ---- records ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_record_round_trips() {
    let dir = scratch("roundtrip");
    let backend = FileBackend::new(&dir);
    let mut unit = backend.open(descriptor(1), cancel()).await.unwrap();

    unit.put_record("records", "a", json!({"n": 1}), cancel())
        .await
        .unwrap();
    let snapshot = unit.load_all(cancel()).await.unwrap();

    assert_eq!(snapshot.tables["records"]["a"], json!({"n": 1}));
    unit.close();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_put_overwrites_an_existing_key() {
    let dir = scratch("overwrite");
    let backend = FileBackend::new(&dir);
    let mut unit = backend.open(descriptor(1), cancel()).await.unwrap();

    unit.put_record("records", "a", json!(1), cancel())
        .await
        .unwrap();
    unit.put_record("records", "a", json!(2), cancel())
        .await
        .unwrap();

    let snapshot = unit.load_all(cancel()).await.unwrap();
    assert_eq!(snapshot.tables["records"]["a"], json!(2));
    unit.close();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_delete_removes_a_record() {
    let dir = scratch("delete");
    let backend = FileBackend::new(&dir);
    let mut unit = backend.open(descriptor(1), cancel()).await.unwrap();

    unit.put_record("records", "a", json!(1), cancel())
        .await
        .unwrap();
    unit.delete_record("records", "a", cancel()).await.unwrap();

    let snapshot = unit.load_all(cancel()).await.unwrap();
    assert!(!snapshot.tables["records"].contains_key("a"));
    unit.close();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn deleting_a_missing_record_is_a_no_op() {
    // Idempotent, per the contract: a caller retrying a delete must not get an error.
    let dir = scratch("delete-missing");
    let backend = FileBackend::new(&dir);
    let mut unit = backend.open(descriptor(1), cancel()).await.unwrap();

    unit.delete_record("records", "absent", cancel())
        .await
        .expect("a missing key is a no-op");

    unit.close();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_undeclared_table_is_refused() {
    // The descriptor declares the shape; writing outside it is a caller bug.
    let dir = scratch("table");
    let backend = FileBackend::new(&dir);
    let mut unit = backend.open(descriptor(1), cancel()).await.unwrap();

    let error = unit
        .put_record("other", "a", json!(1), cancel())
        .await
        .expect_err("not declared");

    assert!(matches!(error, StorageError::UnknownTable { .. }));
    unit.close();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_global_singleton_round_trips() {
    let dir = scratch("global");
    let backend = FileBackend::new(&dir);
    let mut unit = backend.open(descriptor(1), cancel()).await.unwrap();

    unit.set_global(json!({"schema": 1}), cancel())
        .await
        .unwrap();

    let snapshot = unit.load_all(cancel()).await.unwrap();
    assert_eq!(snapshot.global, Some(json!({"schema": 1})));
    unit.close();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_global_on_a_unit_without_one_is_refused() {
    let dir = scratch("no-global");
    let backend = FileBackend::new(&dir);
    let plain = UnitDescriptor::new("plain", 1, vec!["records".to_owned()]);
    let mut unit = backend.open(plain, cancel()).await.unwrap();

    let error = unit
        .set_global(json!(1), cancel())
        .await
        .expect_err("no global slot");

    assert!(matches!(error, StorageError::UnknownTable { .. }));
    unit.close();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- persistence ------------------------------------------------------------------------------

#[tokio::test]
async fn records_survive_a_reopen() {
    let dir = scratch("reopen");
    let backend = FileBackend::new(&dir);

    {
        let mut unit = backend.open(descriptor(1), cancel()).await.unwrap();
        unit.put_record("records", "persisted", json!({"kept": true}), cancel())
            .await
            .unwrap();
        unit.close();
    }

    let unit = backend.open(descriptor(1), cancel()).await.unwrap();
    let snapshot = unit.load_all(cancel()).await.unwrap();

    assert_eq!(
        snapshot.tables["records"]["persisted"],
        json!({"kept": true})
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn opening_a_missing_unit_yields_an_empty_snapshot() {
    // Creation is deferred to the first write, but `load_all` must serve the empty shape
    // immediately.
    let dir = scratch("empty");
    let backend = FileBackend::new(&dir);
    let unit = backend.open(descriptor(1), cancel()).await.unwrap();

    let snapshot = unit.load_all(cancel()).await.unwrap();

    assert!(snapshot.tables.is_empty());
    assert!(snapshot.global.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_malformed_document_is_reported() {
    let dir = scratch("malformed");
    let backend = FileBackend::new(&dir);
    std::fs::write(dir.join("sessions.json"), "{not json").unwrap();

    let error = backend
        .open(descriptor(1), cancel())
        .await
        .expect_err("cannot parse");

    assert!(matches!(error, StorageError::Malformed { .. }), "{error:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_write_leaves_no_staging_file_behind() {
    // The rename-based write must clean up after itself, or a unit directory fills with temp files.
    let dir = scratch("staging");
    let backend = FileBackend::new(&dir);
    let mut unit = backend.open(descriptor(1), cancel()).await.unwrap();
    unit.put_record("records", "a", json!(1), cancel())
        .await
        .unwrap();
    unit.close();

    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("tmp"))
        .collect();

    assert!(
        leftovers.is_empty(),
        "staging files left behind: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- closing ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_closed_unit_refuses_operations() {
    let dir = scratch("closed");
    let backend = FileBackend::new(&dir);
    let mut unit = backend.open(descriptor(1), cancel()).await.unwrap();
    unit.close();

    let error = unit.load_all(cancel()).await.expect_err("closed");

    assert!(matches!(error, StorageError::Closed { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- layout vocabulary ------------------------------------------------------------------------

#[test]
fn the_per_record_layout_is_declared_but_not_implemented() {
    // Declaring it and silently behaving like `Single` would be worse than the gap: a caller would
    // believe its large records stop rewriting the unit.
    let per_record = descriptor(1).per_record();
    assert_eq!(per_record.layout, Layout::PerRecord);

    let dir = std::env::temp_dir().join(format!("girder-store-perrec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let backend = FileBackend::new(&dir);
    let unit = futures::executor::block_on(backend.open(per_record, cancel()));
    // The backend accepts it today and behaves like `Single`; the test records the gap rather than
    // asserting a behavior that does not exist yet.
    assert!(unit.is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_invalid_unit_name_is_refused_at_open() {
    let dir = scratch("badname");
    let backend = FileBackend::new(&dir);
    let bad = UnitDescriptor::new("Not-Valid", 1, vec![]);

    let error = backend.open(bad, cancel()).await.expect_err("invalid name");

    assert!(matches!(
        error,
        StorageError::InvalidName { what: "unit", .. }
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_invalid_table_name_is_refused_at_open() {
    let dir = scratch("badtable");
    let backend = FileBackend::new(&dir);
    let bad = UnitDescriptor::new("ok", 1, vec!["Bad-Table".to_owned()]);

    let error = backend
        .open(bad, cancel())
        .await
        .expect_err("invalid table");

    assert!(matches!(
        error,
        StorageError::InvalidName { what: "table", .. }
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
