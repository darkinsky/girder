//! Tests for the filesystem seam (M2 of the v2 plan).
//!
//! The interesting properties are the guards: a write must refuse when the caller read an older
//! version, and a create must refuse when the target exists. Without those, a read-modify-write
//! silently discards someone else's change.

use girder_core::{FileSystem, FsError, FsTarget, FsType, LocalFileSystem, read_file_via_fs};
use tokio_util::sync::CancellationToken;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-fs-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

// ---- stat ------------------------------------------------------------------------------------

#[tokio::test]
async fn stat_reports_a_file() {
    let dir = scratch("stat");
    let path = dir.join("a.txt");
    std::fs::write(&path, "hello").unwrap();

    let info = LocalFileSystem
        .stat(&FsTarget::from_path(&path), cancel())
        .await
        .unwrap()
        .expect("present");

    assert_eq!(info.file_type, FsType::File);
    assert_eq!(info.size, 5);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stat_reports_absence_as_none_not_an_error() {
    // "Does this exist" has a normal answer; an error would force every caller to match a variant.
    let info = LocalFileSystem
        .stat(
            &FsTarget::from_path(std::path::Path::new("/nonexistent/x")),
            cancel(),
        )
        .await
        .unwrap();

    assert!(info.is_none());
}

#[tokio::test]
async fn stat_distinguishes_directories() {
    let dir = scratch("statdir");
    let info = LocalFileSystem
        .stat(&FsTarget::from_path(&dir), cancel())
        .await
        .unwrap()
        .expect("present");

    assert_eq!(info.file_type, FsType::Directory);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_version_changes_when_the_file_changes() {
    // The whole point of a freshness token.
    let dir = scratch("version");
    let path = dir.join("a.txt");
    let fs = LocalFileSystem;
    let target = FsTarget::from_path(&path);

    std::fs::write(&path, "one").unwrap();
    let first = fs.stat(&target, cancel()).await.unwrap().unwrap().version;

    // A different size guarantees a different version even if the clock has not ticked.
    std::fs::write(&path, "a longer content").unwrap();
    let second = fs.stat(&target, cancel()).await.unwrap().unwrap().version;

    assert_ne!(first, second);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- read ------------------------------------------------------------------------------------

#[tokio::test]
async fn read_text_returns_the_contents() {
    let dir = scratch("read");
    let path = dir.join("a.txt");
    std::fs::write(&path, "hello").unwrap();

    let text = LocalFileSystem
        .read_text(&FsTarget::from_path(&path), cancel())
        .await
        .unwrap();

    assert_eq!(text, "hello");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn reading_a_missing_file_is_missing() {
    let error = LocalFileSystem
        .read_text(
            &FsTarget::from_path(std::path::Path::new("/nonexistent/x")),
            cancel(),
        )
        .await
        .expect_err("missing");

    assert!(matches!(error, FsError::Missing { .. }));
}

#[tokio::test]
async fn reading_a_directory_is_a_wrong_type_error() {
    // Not an I/O error: the message would be platform-dependent and the caller could not branch on
    // it.
    let dir = scratch("readdir");
    let error = LocalFileSystem
        .read_text(&FsTarget::from_path(&dir), cancel())
        .await
        .expect_err("a directory is not text");

    assert!(matches!(
        error,
        FsError::WrongType {
            expected: FsType::File,
            ..
        }
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn reading_non_utf8_is_refused() {
    let dir = scratch("binary");
    let path = dir.join("b.bin");
    std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();

    let error = LocalFileSystem
        .read_text(&FsTarget::from_path(&path), cancel())
        .await
        .expect_err("not text");

    assert!(matches!(error, FsError::NotText { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn read_bytes_refuses_a_file_above_the_limit_without_reading_it() {
    let dir = scratch("tooLarge");
    let path = dir.join("big.txt");
    std::fs::write(&path, "0123456789").unwrap();

    let error = LocalFileSystem
        .read_bytes(&FsTarget::from_path(&path), 5, cancel())
        .await
        .expect_err("above the limit");

    let FsError::TooLarge { size, limit, .. } = error else {
        panic!("expected TooLarge, got {error:?}");
    };
    assert_eq!(size, 10);
    assert_eq!(limit, 5);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- list ------------------------------------------------------------------------------------

#[tokio::test]
async fn listing_is_sorted_so_two_runs_agree() {
    // A directory's own order is filesystem-dependent; an unstable order makes output untestable.
    let dir = scratch("list");
    for name in ["c.txt", "a.txt", "b.txt"] {
        std::fs::write(dir.join(name), "x").unwrap();
    }

    let entries = LocalFileSystem
        .list_dir(&FsTarget::from_path(&dir), cancel())
        .await
        .unwrap();

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["a.txt", "b.txt", "c.txt"]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- version-guarded writes -------------------------------------------------------------------

#[tokio::test]
async fn a_create_refuses_an_existing_file() {
    // `expected = None` means "create"; an existing target is a conflict, not an overwrite.
    let dir = scratch("create");
    let path = dir.join("a.txt");
    std::fs::write(&path, "exists").unwrap();

    let error = LocalFileSystem
        .write_text(&FsTarget::from_path(&path), "new", None, cancel())
        .await
        .expect_err("already exists");

    assert!(matches!(error, FsError::Exists { .. }));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "exists",
        "the file must be untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_create_succeeds_for_a_new_file() {
    let dir = scratch("create-new");
    let path = dir.join("a.txt");

    let version = LocalFileSystem
        .write_text(&FsTarget::from_path(&path), "content", None, cancel())
        .await
        .expect("created");

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "content");
    assert!(!version.0.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_write_with_the_current_version_succeeds() {
    let dir = scratch("write-ok");
    let path = dir.join("a.txt");
    let fs = LocalFileSystem;
    let target = FsTarget::from_path(&path);

    std::fs::write(&path, "one").unwrap();
    let version = fs.stat(&target, cancel()).await.unwrap().unwrap().version;

    fs.write_text(&target, "two", Some(&version), cancel())
        .await
        .expect("the version matches");

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_write_with_a_stale_version_is_refused() {
    // The property that stops a read-modify-write from discarding another writer's change.
    let dir = scratch("stale");
    let path = dir.join("a.txt");
    let fs = LocalFileSystem;
    let target = FsTarget::from_path(&path);

    std::fs::write(&path, "one").unwrap();
    let version = fs.stat(&target, cancel()).await.unwrap().unwrap().version;

    // Someone else changes the file after our read.
    std::fs::write(&path, "changed by someone else").unwrap();

    let error = fs
        .write_text(&target, "mine", Some(&version), cancel())
        .await
        .expect_err("stale");

    let FsError::Stale {
        expected, actual, ..
    } = error
    else {
        panic!("expected Stale, got {error:?}");
    };
    assert_eq!(expected, version);
    assert_ne!(actual, version);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "changed by someone else",
        "the other writer's change must survive"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_write_to_a_deleted_file_is_missing() {
    let dir = scratch("vanished");
    let path = dir.join("a.txt");
    let fs = LocalFileSystem;
    let target = FsTarget::from_path(&path);

    std::fs::write(&path, "one").unwrap();
    let version = fs.stat(&target, cancel()).await.unwrap().unwrap().version;
    std::fs::remove_file(&path).unwrap();

    let error = fs
        .write_text(&target, "two", Some(&version), cancel())
        .await
        .expect_err("gone");

    assert!(matches!(error, FsError::Missing { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_write_returns_the_new_version() {
    // So a caller can chain a second edit without re-reading.
    let dir = scratch("chain");
    let path = dir.join("a.txt");
    let fs = LocalFileSystem;
    let target = FsTarget::from_path(&path);

    let first = fs.write_text(&target, "one", None, cancel()).await.unwrap();
    let second = fs
        .write_text(&target, "two", Some(&first), cancel())
        .await
        .unwrap();

    assert_ne!(first, second);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_write_creates_missing_parents() {
    let dir = scratch("parents");
    let path = dir.join("deep").join("nested").join("a.txt");

    LocalFileSystem
        .write_text(&FsTarget::from_path(&path), "x", None, cancel())
        .await
        .expect("parents created");

    assert!(path.is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn concurrent_writes_to_one_file_do_not_both_succeed() {
    // Two writers that read the same version: exactly one may land.
    let dir = scratch("concurrent");
    let path = dir.join("a.txt");
    let fs = std::sync::Arc::new(LocalFileSystem);
    let target = FsTarget::from_path(&path);
    std::fs::write(&path, "original").unwrap();
    let version = fs.stat(&target, cancel()).await.unwrap().unwrap().version;

    let mut handles = Vec::new();
    for index in 0..4 {
        let fs = std::sync::Arc::clone(&fs);
        let target = target.clone();
        let version = version.clone();
        handles.push(tokio::spawn(async move {
            fs.write_text(
                &target,
                &format!("writer-{index}"),
                Some(&version),
                cancel(),
            )
            .await
            .is_ok()
        }));
    }
    let mut succeeded = 0;
    for handle in handles {
        if handle.await.unwrap() {
            succeeded += 1;
        }
    }

    assert_eq!(
        succeeded, 1,
        "exactly one writer may hold the version it read; the rest are stale"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- remove ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_remove_with_the_current_version_succeeds() {
    let dir = scratch("remove");
    let path = dir.join("a.txt");
    let fs = LocalFileSystem;
    let target = FsTarget::from_path(&path);
    std::fs::write(&path, "x").unwrap();
    let version = fs.stat(&target, cancel()).await.unwrap().unwrap().version;

    fs.remove(&target, &version, cancel())
        .await
        .expect("removed");

    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_remove_with_a_stale_version_is_refused() {
    let dir = scratch("remove-stale");
    let path = dir.join("a.txt");
    let fs = LocalFileSystem;
    let target = FsTarget::from_path(&path);
    std::fs::write(&path, "x").unwrap();
    let version = fs.stat(&target, cancel()).await.unwrap().unwrap().version;
    std::fs::write(&path, "changed").unwrap();

    let error = fs
        .remove(&target, &version, cancel())
        .await
        .expect_err("stale");

    assert!(matches!(error, FsError::Stale { .. }));
    assert!(path.exists(), "the file must survive a refused remove");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- resolve ---------------------------------------------------------------------------------

#[tokio::test]
async fn resolve_joins_a_relative_path_to_the_working_directory() {
    let target = LocalFileSystem.resolve("a.txt", Some(std::path::Path::new("/work")));
    assert_eq!(target.as_path(), std::path::Path::new("/work/a.txt"));
}

#[tokio::test]
async fn resolve_leaves_an_absolute_path_alone() {
    let target = LocalFileSystem.resolve("/etc/hosts", Some(std::path::Path::new("/work")));
    assert_eq!(target.as_path(), std::path::Path::new("/etc/hosts"));
}

// ---- the read helper -------------------------------------------------------------------------

#[tokio::test]
async fn the_read_helper_reports_a_directory_as_a_wrong_type() {
    // The ordering resolve -> stat -> read is the part that is easy to get wrong; the helper exists
    // so it is written once.
    let dir = scratch("helper");
    let error = read_file_via_fs(&LocalFileSystem, dir.to_str().unwrap(), None, cancel())
        .await
        .expect_err("a directory is not a file");

    assert!(matches!(error, FsError::WrongType { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_read_helper_returns_the_target_and_text() {
    let dir = scratch("helper-ok");
    let path = dir.join("a.txt");
    std::fs::write(&path, "content").unwrap();

    let (target, text) = read_file_via_fs(&LocalFileSystem, path.to_str().unwrap(), None, cancel())
        .await
        .expect("read");

    assert_eq!(text, "content");
    assert_eq!(target.as_path(), path);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- cancellation ----------------------------------------------------------------------------

#[tokio::test]
async fn a_pre_cancelled_call_does_no_io() {
    let token = CancellationToken::new();
    token.cancel();

    let error = LocalFileSystem
        .read_text(
            &FsTarget::from_path(std::path::Path::new("/etc/hostname")),
            token,
        )
        .await
        .expect_err("cancelled");

    assert!(matches!(error, FsError::Cancelled));
}
