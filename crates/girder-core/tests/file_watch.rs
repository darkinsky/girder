//! Tests for polling-based workspace change detection.
//!
//! Every test drives the watcher by hand — there is no background thread and no sleep — because the
//! interesting property is the *comparison* between two snapshots, not the timing of the tick.
//!
//! The module is compiled into this test binary through `#[path]` because exporting it from
//! `lib.rs` is a separate change. When it is exported, this can become
//! `use girder_core::file_watch::{ChangeKind, PollingWatcher, snapshot};`.

#[path = "../src/file_watch.rs"]
mod file_watch;

use std::path::{Path, PathBuf};
use std::time::Duration;

use file_watch::{ChangeKind, PollingWatcher, snapshot};

/// A fresh directory per test, named after the test so a leftover from a failed run is identifiable.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-watch-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// A watcher over `dir` with an interval the tests never actually wait on.
fn watcher(dir: &Path) -> PollingWatcher {
    PollingWatcher::new(dir.to_path_buf(), Duration::from_millis(200))
}

/// Writes a file, creating parent directories.
fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent directory");
    }
    std::fs::write(path, content).expect("file written");
}

/// The `(path, kind)` pairs of a change list, for readable assertions.
fn pairs(changes: &[file_watch::Change]) -> Vec<(String, ChangeKind)> {
    changes
        .iter()
        .map(|change| (change.path.clone(), change.kind))
        .collect()
}

// ---- baseline ---------------------------------------------------------------------------------

#[test]
fn an_empty_directory_reports_nothing() {
    let dir = scratch("empty");
    let mut watch = watcher(&dir);

    // The first scan establishes the baseline; the second has nothing to compare it against.
    assert!(watch.scan().is_empty(), "first scan is the baseline");
    assert!(watch.scan().is_empty(), "nothing changed");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn existing_files_are_not_announced_as_created() {
    let dir = scratch("preexisting");
    write_file(&dir.join("already.txt"), "here");
    let mut watch = watcher(&dir);

    // A watcher that reported the tree on its first tick would announce the whole workspace as new
    // every time a session started.
    assert!(watch.scan().is_empty(), "startup must not report the tree");
    assert!(watch.scan().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the three transitions ---------------------------------------------------------------------

#[test]
fn a_new_file_is_reported_as_created() {
    let dir = scratch("create");
    let mut watch = watcher(&dir);
    assert!(watch.scan().is_empty());

    write_file(&dir.join("new.txt"), "hello");

    assert_eq!(
        pairs(&watch.scan()),
        vec![("new.txt".to_string(), ChangeKind::Created)]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_nested_new_file_is_reported_by_its_relative_path() {
    let dir = scratch("nested-create");
    let mut watch = watcher(&dir);
    assert!(watch.scan().is_empty());

    write_file(&dir.join("src").join("deep").join("file.txt"), "x");

    // Both the new directory and the file inside it are reported, each by a workspace-relative path.
    let changes = watch.scan();
    let paths: Vec<&str> = changes.iter().map(|change| change.path.as_str()).collect();
    assert!(
        paths.contains(&"src"),
        "the new directory is reported: {paths:?}"
    );
    assert!(
        paths.contains(&"src/deep/file.txt"),
        "the file is reported with / separators: {paths:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_modified_file_is_reported_as_modified() {
    let dir = scratch("modify");
    let file = dir.join("edited.txt");
    write_file(&file, "short");
    let mut watch = watcher(&dir);
    assert!(watch.scan().is_empty());

    write_file(&file, "a much longer body than before");

    // The size differs, which is what makes this detectable even when both writes land in the same
    // filesystem timestamp tick.
    assert_eq!(
        pairs(&watch.scan()),
        vec![("edited.txt".to_string(), ChangeKind::Modified)]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_removed_file_is_reported_as_removed() {
    let dir = scratch("remove");
    let file = dir.join("gone.txt");
    write_file(&file, "temporary");
    let mut watch = watcher(&dir);
    assert!(watch.scan().is_empty());

    std::fs::remove_file(&file).expect("removed");

    assert_eq!(
        pairs(&watch.scan()),
        vec![("gone.txt".to_string(), ChangeKind::Removed)]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn all_three_transitions_are_reported_in_one_scan() {
    let dir = scratch("mixed");
    let kept = dir.join("kept.txt");
    let edited = dir.join("edited.txt");
    let gone = dir.join("gone.txt");
    write_file(&kept, "unchanged");
    write_file(&edited, "one");
    write_file(&gone, "bye");
    let mut watch = watcher(&dir);
    assert!(watch.scan().is_empty());

    write_file(&dir.join("added.txt"), "new");
    write_file(&edited, "one and then some more");
    std::fs::remove_file(&gone).expect("removed");

    // Sorted by path, so an unchanged file is simply absent and the rest are in a stable order.
    assert_eq!(
        pairs(&watch.scan()),
        vec![
            ("added.txt".to_string(), ChangeKind::Created),
            ("edited.txt".to_string(), ChangeKind::Modified),
            ("gone.txt".to_string(), ChangeKind::Removed),
        ]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- skipped directories ----------------------------------------------------------------------

#[test]
fn changes_inside_skipped_directories_are_not_reported() {
    let dir = scratch("skipped");
    let mut watch = watcher(&dir);
    assert!(watch.scan().is_empty());

    // Each of these is enormous and machine-generated in a real workspace; walking them would
    // dominate the scan, so a change inside one is deliberately invisible.
    for skipped in [".git", "node_modules", "target"] {
        write_file(&dir.join(skipped).join("package.json"), "{}");
        write_file(
            &dir.join(skipped).join("nested").join("artifact.bin"),
            "data",
        );
    }

    assert!(
        watch.scan().is_empty(),
        "nothing inside a skipped directory may surface"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_skipped_directory_does_not_hide_a_sibling_change() {
    let dir = scratch("skipped-sibling");
    write_file(&dir.join("node_modules").join("dep.js"), "x");
    let mut watch = watcher(&dir);
    assert!(watch.scan().is_empty());

    write_file(&dir.join("src").join("main.rs"), "fn main() {}");
    write_file(&dir.join("node_modules").join("dep.js"), "changed");

    // Pruning one subtree must not blind the watcher to the rest of the tree.
    let paths: Vec<String> = watch.scan().into_iter().map(|change| change.path).collect();
    assert!(paths.iter().any(|path| path == "src/main.rs"), "{paths:?}");
    assert!(
        !paths.iter().any(|path| path.contains("node_modules")),
        "{paths:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- snapshots --------------------------------------------------------------------------------

#[test]
fn snapshot_keys_are_sorted_and_relative() {
    let dir = scratch("snapshot-order");
    write_file(&dir.join("zeta.txt"), "z");
    write_file(&dir.join("alpha.txt"), "a");
    write_file(&dir.join("mid").join("beta.txt"), "b");

    let first = snapshot(&dir);
    let keys: Vec<&str> = first.keys().map(String::as_str).collect();

    // A BTreeMap makes the order part of the contract, so two snapshots of one tree compare and
    // print identically.
    assert_eq!(keys, vec!["alpha.txt", "mid", "mid/beta.txt", "zeta.txt"]);

    let second = snapshot(&dir);
    assert_eq!(first, second, "an unchanged tree snapshots identically");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_snapshot_records_the_size_of_a_file_and_zero_for_a_directory() {
    let dir = scratch("snapshot-size");
    write_file(&dir.join("four.txt"), "1234");
    std::fs::create_dir_all(dir.join("sub")).expect("directory");

    let entries = snapshot(&dir);
    assert_eq!(entries.get("four.txt").map(|(_, size)| *size), Some(4));
    assert_eq!(entries.get("sub").map(|(_, size)| *size), Some(0));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_root_snapshots_empty() {
    // A workspace that has not been created yet is a state, not a crash.
    let dir = scratch("missing-root").join("not-there");
    assert!(snapshot(&dir).is_empty());

    let _ = std::fs::remove_dir_all(dir.parent().expect("parent"));
}

// ---- accessors --------------------------------------------------------------------------------

#[test]
fn a_watcher_exposes_its_configuration_and_baseline() {
    let dir = scratch("accessors");
    let mut watch = PollingWatcher::new(dir.clone(), Duration::from_secs(2));

    assert_eq!(watch.workspace(), dir.as_path());
    assert_eq!(watch.interval(), Duration::from_secs(2));
    assert!(
        watch.baseline().is_none(),
        "no baseline before the first scan"
    );

    assert!(watch.scan().is_empty());
    assert!(
        watch.baseline().is_some(),
        "the first scan stores the baseline"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
