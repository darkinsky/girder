//! Tests for the undo log.
//!
//! Three properties carry this module, and none of them is visible from a signature:
//!
//! - the stack is bounded, so the *oldest* change is the one that disappears when capacity is
//!   reached — keeping the newest is the whole point of an undo;
//! - `before: None` means "the file was created", so undo must report a delete rather than an
//!   empty write, and a test that only compared strings would miss it;
//! - `undo` returns data and writes nothing, which these tests rely on by never creating a file.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against it as written even before the crate root wires it in.

#[path = "../src/fs/undo_log.rs"]
pub mod undo_log;

use undo_log::{Change, DEFAULT_CAPACITY, Log};

/// A change to an existing file, with the previous content given.
fn edit(path: &str, before: &str) -> Change {
    Change {
        path: path.to_owned(),
        before: Some(before.to_owned()),
        at_ms: 1_700_000_000_000,
        tool: "write".to_owned(),
    }
}

/// A change that created a file, so there is nothing to restore.
fn create(path: &str) -> Change {
    Change {
        path: path.to_owned(),
        before: None,
        at_ms: 1_700_000_000_000,
        tool: "write".to_owned(),
    }
}

#[test]
fn recording_one_change_and_undoing_it_returns_it() {
    let mut log = Log::new();
    log.record(edit("src/main.rs", "fn main() {}\n"));

    let undone = log.undo();

    let undone = undone.expect("the recorded change must be undoable");
    assert_eq!(undone.path, "src/main.rs");
    assert_eq!(undone.before.as_deref(), Some("fn main() {}\n"));
    assert_eq!(undone.tool, "write");
    assert!(log.is_empty());
}

#[test]
fn undo_is_last_in_first_out() {
    let mut log = Log::new();
    log.record(edit("a.txt", "a0"));
    log.record(edit("b.txt", "b0"));
    log.record(edit("c.txt", "c0"));

    let first = log.undo().expect("three changes were recorded");
    let second = log.undo().expect("two changes remain");
    let third = log.undo().expect("one change remains");

    assert_eq!(first.path, "c.txt");
    assert_eq!(second.path, "b.txt");
    assert_eq!(third.path, "a.txt");
    assert!(log.is_empty());
    assert_eq!(log.undo(), None, "an exhausted log has nothing to undo");
}

#[test]
fn the_oldest_change_is_dropped_at_capacity() {
    let mut log = Log::new();
    for index in 0..DEFAULT_CAPACITY {
        log.record(edit(&format!("file-{index}.txt"), "before"));
    }
    assert_eq!(log.len(), DEFAULT_CAPACITY);

    // One past capacity: the first entry must be the one that disappears.
    log.record(edit("overflow.txt", "before"));

    assert_eq!(log.len(), DEFAULT_CAPACITY, "the log must stay bounded");
    assert_eq!(log.capacity(), DEFAULT_CAPACITY);

    let mut paths = Vec::new();
    while let Some(change) = log.undo() {
        paths.push(change.path);
    }
    assert_eq!(paths.len(), DEFAULT_CAPACITY);
    // Popped newest first, so `overflow.txt` leads and the survivor of the oldest pair trails.
    assert_eq!(paths.first().map(String::as_str), Some("overflow.txt"));
    assert_eq!(paths.last().map(String::as_str), Some("file-1.txt"));
    assert!(
        !paths.iter().any(|path| path == "file-0.txt"),
        "the oldest change was dropped, so it must not be undoable"
    );
}

#[test]
fn the_bound_holds_over_many_more_records_than_capacity() {
    let mut log = Log::new();
    for index in 0..(DEFAULT_CAPACITY * 5) {
        log.record(edit(&format!("file-{index}.txt"), "before"));
    }

    assert_eq!(log.len(), DEFAULT_CAPACITY);

    // Only the tail survives, still in order.
    let newest = log.undo().expect("the log is not empty");
    assert_eq!(
        newest.path,
        format!("file-{}.txt", DEFAULT_CAPACITY * 5 - 1)
    );
    assert_eq!(log.len(), DEFAULT_CAPACITY - 1);
}

#[test]
fn undoing_a_created_file_reports_deletion() {
    let mut log = Log::new();
    log.record(create("new_file.rs"));

    let undone = log.undo().expect("the creation was recorded");

    assert_eq!(
        undone.before, None,
        "a created file must not come back as an empty write"
    );
    assert_eq!(undone.path, "new_file.rs");
}

#[test]
fn an_empty_file_is_not_a_created_file() {
    let mut log = Log::new();
    log.record(edit("emptied.txt", ""));

    let undone = log.undo().expect("the change was recorded");

    assert_eq!(
        undone.before.as_deref(),
        Some(""),
        "an existing empty file must be restored as empty, not deleted"
    );
}

#[test]
fn clearing_forgets_every_change() {
    let mut log = Log::new();
    log.record(edit("a.txt", "a0"));
    log.record(edit("b.txt", "b0"));

    log.clear();

    assert!(log.is_empty());
    assert_eq!(log.len(), 0);
    assert_eq!(log.undo(), None);

    // The log stays usable after a clear.
    log.record(edit("c.txt", "c0"));
    assert_eq!(log.len(), 1);
    assert_eq!(
        log.undo().map(|change| change.path),
        Some("c.txt".to_owned())
    );
}

#[test]
fn undoing_an_empty_log_returns_none() {
    let mut log = Log::new();

    assert_eq!(log.undo(), None);
    assert!(log.is_empty());
}

#[test]
fn many_changes_can_be_undone_in_reverse_order() {
    let mut log = Log::new();
    let paths = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
    for path in paths {
        log.record(edit(path, &format!("{path}.orig")));
    }
    assert_eq!(log.len(), paths.len());

    let mut restored = Vec::new();
    while let Some(change) = log.undo() {
        restored.push(change.path.clone());
        assert_eq!(
            change.before.as_deref(),
            Some(format!("{}.orig", change.path).as_str())
        );
    }

    assert_eq!(restored, vec!["e.txt", "d.txt", "c.txt", "b.txt", "a.txt"]);
}

#[test]
fn a_mixed_stack_undoes_creations_and_edits_together() {
    let mut log = Log::new();
    log.record(edit("existing.txt", "old"));
    log.record(create("brand_new.txt"));

    let creation = log.undo().expect("the creation was recorded last");
    assert_eq!(creation.path, "brand_new.txt");
    assert_eq!(creation.before, None);

    let edit_back = log.undo().expect("the edit was recorded first");
    assert_eq!(edit_back.path, "existing.txt");
    assert_eq!(edit_back.before.as_deref(), Some("old"));
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod fs {
    pub use super::undo_log as undo_log;
}
