//! Tests for atomic replacement and writer coordination.
//!
//! The properties are about what a *concurrent* observer sees, which is why several tests run writers
//! in parallel rather than asserting a sequence.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{AtomicError, DEFAULT_LOCK_WAIT, Mode, with_lock, write_text_atomic};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-atomic-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---- replacement ------------------------------------------------------------------------------

#[test]
fn a_write_creates_the_file() {
    let dir = scratch("create");
    let path = dir.join("a.txt");

    write_text_atomic(&path, "hello", Mode::PRIVATE).expect("written");

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_write_replaces_the_content() {
    let dir = scratch("replace");
    let path = dir.join("a.txt");
    write_text_atomic(&path, "old", Mode::PRIVATE).expect("written");

    write_text_atomic(&path, "new content", Mode::PRIVATE).expect("replaced");

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new content");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_write_sets_the_requested_mode() {
    // A file that silently became 0600 because a helper defaulted is a bug nobody notices until
    // another user cannot read it.
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("mode");
    let path = dir.join("a.txt");

    write_text_atomic(&path, "x", Mode::PUBLIC).expect("written");

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn replacing_keeps_the_mode_when_the_target_already_existed() {
    // The rename is what makes this true: the target ends up with the sibling's mode, not its own.
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("mode-replace");
    let path = dir.join("a.txt");
    write_text_atomic(&path, "old", Mode::PUBLIC).expect("written");

    write_text_atomic(&path, "new", Mode::PRIVATE).expect("replaced");

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the replacement's mode must win");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_write_creates_missing_parents() {
    let dir = scratch("parents");
    let path = dir.join("deep").join("nested").join("a.txt");

    write_text_atomic(&path, "x", Mode::PRIVATE).expect("written");

    assert!(path.is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_write_leaves_no_staging_file() {
    // A staging file that outlives its attempt is one nobody will ever clean up.
    let dir = scratch("debris");
    let path = dir.join("a.txt");

    // A directory in the target's place makes the rename fail.
    std::fs::create_dir(&path).unwrap();
    let _ = write_text_atomic(&path, "x", Mode::PRIVATE);

    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "staging files left behind: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_reader_never_sees_a_partial_write() {
    // The property the rename exists for: a concurrent reader sees one whole value or the other.
    let dir = scratch("concurrent-read");
    let path = dir.join("a.txt");
    let long = "a".repeat(64 * 1024);
    let short = "b".repeat(64 * 1024);
    write_text_atomic(&path, &long, Mode::PRIVATE).expect("written");

    let stop = Arc::new(AtomicU32::new(0));
    let reader = {
        let path = path.clone();
        let stop = Arc::clone(&stop);
        let long = long.clone();
        let short = short.clone();
        std::thread::spawn(move || {
            let mut bad = 0;
            while stop.load(Ordering::SeqCst) == 0 {
                if let Ok(text) = std::fs::read_to_string(&path)
                    && text != long
                    && text != short
                {
                    bad += 1;
                }
            }
            bad
        })
    };

    for index in 0..50 {
        let contents = if index % 2 == 0 { &short } else { &long };
        write_text_atomic(&path, contents, Mode::PRIVATE).expect("written");
    }
    stop.store(1, Ordering::SeqCst);

    assert_eq!(reader.join().unwrap(), 0, "a reader saw a torn value");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the lock ---------------------------------------------------------------------------------

async fn locked_increment(
    path: &std::path::Path,
    counter: &Arc<AtomicU32>,
) -> Result<(), AtomicError> {
    let owned = path.to_path_buf();
    let counter = Arc::clone(counter);
    with_lock(path, DEFAULT_LOCK_WAIT, move || {
        let path = owned.clone();
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            // A read-modify-write: without the lock two writers each read the old value.
            let current = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                .unwrap_or(0);
            counter.fetch_add(1, Ordering::SeqCst);
            write_text_atomic(&path, &(current + 1).to_string(), Mode::PRIVATE).map_err(
                |error| AtomicError::Write {
                    path: path.display().to_string(),
                    source: std::io::Error::other(error.to_string()),
                },
            )?;
            Ok::<(), AtomicError>(())
        })
    })
    .await
}

#[tokio::test]
async fn the_lock_serialises_a_read_modify_write() {
    // The property: N writers each incrementing a counter leave exactly N, not a lost update.
    let dir = scratch("lock");
    let path = dir.join("counter.txt");
    write_text_atomic(&path, "0", Mode::PRIVATE).expect("written");
    let counter = Arc::new(AtomicU32::new(0));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let path = path.clone();
        let counter = Arc::clone(&counter);
        handles.push(tokio::spawn(async move {
            locked_increment(&path, &counter).await
        }));
    }
    for handle in handles {
        handle.await.expect("joined").expect("incremented");
    }

    assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "8");
    assert_eq!(counter.load(Ordering::SeqCst), 8);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_lock_is_released_after_a_successful_operation() {
    let dir = scratch("release");
    let path = dir.join("a.txt");
    write_text_atomic(&path, "0", Mode::PRIVATE).expect("written");

    let counter = Arc::new(AtomicU32::new(0));
    locked_increment(&path, &counter).await.expect("first");
    locked_increment(&path, &counter).await.expect("second");

    assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_lock_is_released_after_a_failed_operation() {
    // A lock that survives a failed operation blocks every later writer.
    let dir = scratch("release-fail");
    let path = dir.join("a.txt");

    let failed = with_lock(&path, DEFAULT_LOCK_WAIT, || {
        Box::pin(async {
            Err::<(), AtomicError>(AtomicError::LockTimeout {
                path: "x".to_owned(),
            })
        })
    })
    .await;
    assert!(failed.is_err());

    let counter = Arc::new(AtomicU32::new(0));
    locked_increment(&path, &counter)
        .await
        .expect("the lock was released");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_lock_whose_holder_is_gone_is_taken_over() {
    // Without this every later writer waits out its timeout forever after one crash.
    let dir = scratch("stale");
    let path = dir.join("a.txt");
    let lock = dir.join(".a.txt.lock");
    // A pid that cannot exist.
    std::fs::write(&lock, "4294967294\n").unwrap();

    let counter = Arc::new(AtomicU32::new(0));
    locked_increment(&path, &counter)
        .await
        .expect("took over the stale lock");

    assert_eq!(counter.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_lock_with_no_readable_holder_is_taken_over() {
    // A file with no readable holder cannot be attributed to anyone, so it is stale by definition.
    let dir = scratch("stale-empty");
    let path = dir.join("a.txt");
    std::fs::write(dir.join(".a.txt.lock"), "not a pid").unwrap();

    let counter = Arc::new(AtomicU32::new(0));
    locked_increment(&path, &counter).await.expect("took over");

    assert_eq!(counter.load(Ordering::SeqCst), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
