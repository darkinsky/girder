//! Tests for the on-disk layout of a session.
//!
//! Three properties carry the weight. An id is a *file name segment*, so an id carrying a separator
//! or `..` must be refused before any path is built — that check is the only thing between a client
//! argument and a delete outside the session directory. An id is a session only when its log exists,
//! because a leftover `.title` names a conversation nobody can open. And deletion must be complete
//! and idempotent, because a retried delete is the ordinary case and a half-deleted session is the
//! state a retry has to finish.
//!
//! The module is included by path rather than through the crate root, so these tests compile against
//! it as written even before the crate root wires it in.

#[path = "../src/session_layout.rs"]
mod session_layout;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use session_layout::{ARCHIVED_SUFFIX, LOG_SUFFIX, Layout, LayoutError, TITLE_SUFFIX};

/// A scratch directory unique to one test.
///
/// The counter keeps two tests in the same process apart; the pid keeps two concurrent `cargo test`
/// runs apart. Each test removes the directory it created, so a rerun starts clean either way.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-layout-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory is created");
    dir
}

/// Writes a file with `contents`.
fn touch(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("fixture file is written");
}

/// Writes the log of session `id` into `dir`.
fn seed_log(dir: &Path, id: &str) {
    touch(
        &dir.join(format!("{id}{LOG_SUFFIX}")),
        "{\"type\":\"turn/start\"}\n",
    );
}

// ---- path assembly -----------------------------------------------------------------------------

#[test]
fn a_normal_id_assembles_the_three_paths() {
    let dir = scratch("paths");
    let layout = Layout::new(&dir);

    let files = layout.files("session-1794285520378").expect("a valid id");

    assert_eq!(files.log, dir.join("session-1794285520378.jsonl.zst"));
    assert_eq!(files.title, dir.join("session-1794285520378.title"));
    assert_eq!(files.archived, dir.join("session-1794285520378.archived"));
}

#[test]
fn the_paths_match_the_suffixes_the_rest_of_the_harness_writes() {
    // `DirectoryCatalog` names these two files itself; if the constants drifted, a session renamed by
    // one and deleted by the other would leave an orphan.
    let dir = scratch("suffixes");
    let layout = Layout::new(&dir);
    let files = layout.files("s1").expect("a valid id");

    assert!(files.log.ends_with(format!("s1{LOG_SUFFIX}")));
    assert!(files.title.ends_with(format!("s1{TITLE_SUFFIX}")));
    assert!(files.archived.ends_with(format!("s1{ARCHIVED_SUFFIX}")));
    assert_eq!(LOG_SUFFIX, ".jsonl.zst");
    assert_eq!(TITLE_SUFFIX, ".title");
    assert_eq!(ARCHIVED_SUFFIX, ".archived");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_root_is_returned_as_given() {
    let layout = Layout::new("/tmp/girder-sessions");
    assert_eq!(layout.root(), Path::new("/tmp/girder-sessions"));
}

// ---- the id check ------------------------------------------------------------------------------

#[test]
fn an_id_with_a_forward_slash_is_refused() {
    // The classic traversal: without this check the second path component would be built in a
    // directory the caller never named.
    let layout = Layout::new("/tmp/girder-sessions");

    let error = layout
        .files("../escape")
        .expect_err("a traversal is refused");

    assert!(matches!(error, LayoutError::Separator { .. }), "{error}");
}

#[test]
fn an_id_with_an_embedded_separator_is_refused() {
    let layout = Layout::new("/tmp/girder-sessions");

    let error = layout
        .files("a/../../etc/passwd")
        .expect_err("a slash is refused");

    assert!(matches!(error, LayoutError::Separator { .. }), "{error}");
}

#[test]
fn an_id_with_a_backslash_is_refused() {
    // A backslash is a separator on Windows, and on Unix it is an ordinary name character. Refusing
    // it everywhere means one rule for both platforms; the alternative is a session that a Unix
    // build writes and a Windows build cannot delete.
    let layout = Layout::new("/tmp/girder-sessions");

    let error = layout
        .files(r"..\escape")
        .expect_err("a backslash is refused");

    assert!(matches!(error, LayoutError::Separator { .. }), "{error}");
}

#[test]
fn the_directory_names_are_refused() {
    let layout = Layout::new("/tmp/girder-sessions");

    for id in [".", ".."] {
        let error = layout.files(id).expect_err("a directory name is refused");
        assert!(
            matches!(error, LayoutError::DirectoryName { .. }),
            "{error}"
        );
    }
}

#[test]
fn an_empty_id_is_refused() {
    let layout = Layout::new("/tmp/girder-sessions");

    let error = layout.files("").expect_err("an empty id is refused");

    assert!(matches!(error, LayoutError::EmptyId), "{error}");
}

#[test]
fn an_id_with_a_control_character_is_refused() {
    let layout = Layout::new("/tmp/girder-sessions");

    for id in ["a\nb", "a\0b", "a\tb"] {
        let error = layout
            .files(id)
            .expect_err("a control character is refused");
        assert!(
            matches!(error, LayoutError::ControlCharacter { .. }),
            "{error}"
        );
    }
}

#[test]
fn an_id_longer_than_a_file_name_is_refused() {
    let layout = Layout::new("/tmp/girder-sessions");
    let id = "x".repeat(400);

    let error = layout.files(&id).expect_err("an over-long id is refused");

    assert!(matches!(error, LayoutError::TooLong { .. }), "{error}");
}

#[test]
fn an_id_at_the_length_limit_is_accepted() {
    // The boundary is inclusive, and a session id derived from a timestamp is nowhere near it: this
    // case exists so the limit is off-by-one-free rather than merely "very long ids fail".
    let dir = scratch("max-id");
    let layout = Layout::new(&dir);
    let id = "x".repeat(255 - LOG_SUFFIX.len());

    let files = layout.files(&id).expect("the longest id is accepted");

    assert!(files.log.file_name().is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_path_stays_directly_inside_the_root() {
    // The invariant the id check exists to protect, stated as a test rather than as a comment.
    let dir = scratch("inside");
    let layout = Layout::new(&dir);

    let files = layout.files("s1").expect("a valid id");

    for path in [&files.log, &files.title, &files.archived] {
        assert_eq!(path.parent(), Some(dir.as_path()));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- listing -----------------------------------------------------------------------------------

#[test]
fn ids_lists_only_the_sessions_that_have_a_log() {
    let dir = scratch("ids");
    seed_log(&dir, "s1");
    seed_log(&dir, "s2");
    // A title with no log is an orphan: an interrupted delete or a file from an older build. Listing
    // it would put a row in the UI that cannot be opened.
    touch(&dir.join("orphan.title"), "Left behind");
    let layout = Layout::new(&dir);

    let ids = layout.ids().expect("the directory is readable");

    assert_eq!(ids, vec!["s1".to_owned(), "s2".to_owned()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn companion_files_do_not_produce_extra_ids() {
    let dir = scratch("companions");
    seed_log(&dir, "s1");
    touch(&dir.join("s1.title"), "Refactor the parser");
    touch(&dir.join("s1.archived"), "");
    let layout = Layout::new(&dir);

    let ids = layout.ids().expect("the directory is readable");

    assert_eq!(ids, vec!["s1".to_owned()], "three files, one session");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ids_are_ordered() {
    // A caller that paginates or diffs two listings must not depend on the order the filesystem
    // returned entries in.
    let dir = scratch("order");
    for id in ["c", "a", "b"] {
        seed_log(&dir, id);
    }
    let layout = Layout::new(&dir);

    let ids = layout.ids().expect("the directory is readable");

    assert_eq!(ids, vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_directory_lists_no_sessions() {
    // A fresh install has no sessions directory, and that is an answer rather than a failure: making
    // an empty list look like a broken install would be worse than an empty list.
    let dir = scratch("missing").join("nothing-here");
    let layout = Layout::new(&dir);

    let ids = layout.ids().expect("an absent directory is not an error");

    assert!(ids.is_empty());
    let _ = std::fs::remove_dir_all(dir.parent().expect("a parent"));
}

#[test]
fn a_file_without_an_id_names_no_session() {
    // `.jsonl.zst` alone would give an empty id, and an empty id is not a session.
    let dir = scratch("empty-id");
    touch(&dir.join(LOG_SUFFIX), "{}");
    seed_log(&dir, "s1");
    let layout = Layout::new(&dir);

    let ids = layout.ids().expect("the directory is readable");

    assert_eq!(ids, vec!["s1".to_owned()]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unrelated_files_are_ignored() {
    let dir = scratch("unrelated");
    seed_log(&dir, "s1");
    touch(&dir.join("README.md"), "not a session");
    touch(&dir.join("notes.txt"), "not a session either");
    let layout = Layout::new(&dir);

    let ids = layout.ids().expect("the directory is readable");

    assert_eq!(ids, vec!["s1".to_owned()]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- counting ----------------------------------------------------------------------------------

#[test]
fn count_matches_the_logs_present() {
    let dir = scratch("count");
    seed_log(&dir, "s1");
    seed_log(&dir, "s2");
    seed_log(&dir, "s3");
    // Sidecars do not add to the count, for the same reason they do not add to the listing.
    touch(&dir.join("orphan.title"), "Left behind");
    let layout = Layout::new(&dir);

    assert_eq!(layout.count().expect("the directory is readable"), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_directory_holds_no_sessions() {
    let dir = scratch("count-empty");
    let layout = Layout::new(&dir);

    assert_eq!(layout.count().expect("the directory is readable"), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- deletion ----------------------------------------------------------------------------------

#[test]
fn remove_deletes_the_log_and_every_companion() {
    let dir = scratch("remove");
    seed_log(&dir, "s1");
    touch(&dir.join("s1.title"), "Named");
    touch(&dir.join("s1.archived"), "");
    let layout = Layout::new(&dir);

    layout.remove("s1").expect("the session is removed");

    assert!(!dir.join("s1.jsonl.zst").exists(), "the log is gone");
    assert!(!dir.join("s1.title").exists(), "the title is gone");
    assert!(
        !dir.join("s1.archived").exists(),
        "the archive mark is gone"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remove_leaves_another_session_alone() {
    let dir = scratch("remove-neighbour");
    seed_log(&dir, "s1");
    seed_log(&dir, "s2");
    touch(&dir.join("s2.title"), "Kept");
    let layout = Layout::new(&dir);

    layout.remove("s1").expect("the session is removed");

    assert!(dir.join("s2.jsonl.zst").is_file());
    assert!(dir.join("s2.title").is_file());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn removing_a_session_with_only_a_log_succeeds() {
    // The ordinary case: most sessions were never renamed or archived, so two of the three files are
    // absent and that is not a failure.
    let dir = scratch("remove-log-only");
    seed_log(&dir, "s1");
    let layout = Layout::new(&dir);

    layout
        .remove("s1")
        .expect("the missing sidecars are not an error");

    assert!(!dir.join("s1.jsonl.zst").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn removing_twice_is_not_an_error() {
    // A retried delete is the ordinary case, and the second attempt has nothing left to remove.
    let dir = scratch("remove-twice");
    seed_log(&dir, "s1");
    let layout = Layout::new(&dir);

    layout.remove("s1").expect("the first delete succeeds");
    layout.remove("s1").expect("the second delete is a no-op");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn removing_a_session_that_never_existed_is_not_an_error() {
    // Idempotency is the point: the caller asked for the session to be gone and it is gone.
    let dir = scratch("remove-absent");
    let layout = Layout::new(&dir);

    layout
        .remove("never-existed")
        .expect("an absent session is a no-op");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn removing_with_an_unsafe_id_is_refused_and_deletes_nothing() {
    // The sharpest version of the security boundary: a traversal in a *delete* is unrecoverable, so
    // the rejection has to happen before the first file is touched.
    let dir = scratch("remove-unsafe");
    let outside = dir.parent().expect("a parent").join(format!(
        "girder-layout-outside-{}-{}",
        std::process::id(),
        "secret"
    ));
    touch(&outside, "must survive");
    seed_log(&dir, "s1");
    let layout = Layout::new(&dir);

    let error = layout
        .remove("../girder-layout-outside-secret")
        .expect_err("a traversal is refused");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
    assert!(outside.is_file(), "the file outside the root survives");
    assert!(dir.join("s1.jsonl.zst").is_file(), "nothing was deleted");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&outside);
}

#[test]
fn a_removed_session_disappears_from_the_listing_and_the_count() {
    let dir = scratch("remove-list");
    seed_log(&dir, "s1");
    seed_log(&dir, "s2");
    let layout = Layout::new(&dir);

    layout.remove("s1").expect("the session is removed");

    assert_eq!(layout.ids().expect("readable"), vec!["s2".to_owned()]);
    assert_eq!(layout.count().expect("readable"), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
