//! Tests for deliverables.
//!
//! A deliverable is an announcement, not a directory scan, so the properties are about what a list shows: announcement
//! order, no duplicates, and an honest answer when a file is gone.

use girder_core::{Deliverable, DeliverableKind, DeliverableSet};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-deliv-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

// ---- kinds ------------------------------------------------------------------------------------

#[test]
fn a_declared_kind_has_a_stable_name() {
    assert_eq!(DeliverableKind::Document.as_str(), "document");
    assert_eq!(DeliverableKind::Spreadsheet.as_str(), "spreadsheet");
    assert_eq!(DeliverableKind::Deck.as_str(), "deck");
    assert_eq!(DeliverableKind::Artefact.as_str(), "artefact");
}

#[test]
fn a_path_implies_a_likely_kind() {
    assert_eq!(
        DeliverableKind::from_path(std::path::Path::new("report.md")),
        DeliverableKind::Document
    );
    assert_eq!(
        DeliverableKind::from_path(std::path::Path::new("data.csv")),
        DeliverableKind::Spreadsheet
    );
    assert_eq!(
        DeliverableKind::from_path(std::path::Path::new("slides.pptx")),
        DeliverableKind::Deck
    );
    assert_eq!(
        DeliverableKind::from_path(std::path::Path::new("chart.PNG")),
        DeliverableKind::Image
    );
}

#[test]
fn an_unknown_or_missing_extension_is_other() {
    assert_eq!(
        DeliverableKind::from_path(std::path::Path::new("data.json")),
        DeliverableKind::Other,
        "a .json is not a document by default"
    );
    assert_eq!(
        DeliverableKind::from_path(std::path::Path::new("Makefile")),
        DeliverableKind::Other
    );
}

// ---- one deliverable --------------------------------------------------------------------------

#[test]
fn a_deliverable_needs_a_title() {
    // An entry with no title cannot be chosen from a list.
    assert!(Deliverable::new("/a/b.md", "", DeliverableKind::Document).is_none());
    assert!(Deliverable::new("/a/b.md", "  ", DeliverableKind::Document).is_none());
}

#[test]
fn a_deliverable_carries_a_description_and_a_size() {
    let entry = Deliverable::new("/a/b.md", "The report", DeliverableKind::Document)
        .expect("valid")
        .described("What the build produced")
        .with_size(1234);

    assert_eq!(
        entry.description.as_deref(),
        Some("What the build produced")
    );
    assert_eq!(entry.bytes, Some(1234));
}

#[test]
fn an_announced_file_that_exists_is_present() {
    let dir = scratch("exists");
    let path = dir.join("out.md");
    std::fs::write(&path, "hi").expect("written");

    let entry = Deliverable::new(&path, "Out", DeliverableKind::Document).expect("valid");

    assert!(entry.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_deleted_file_is_not_present() {
    // The file can be deleted between the announcement and the click, and a click on a missing file should be a
    // state rather than a filesystem error.
    let entry = Deliverable::new(
        "/nonexistent/girder-out.md",
        "Out",
        DeliverableKind::Document,
    )
    .expect("valid");

    assert!(!entry.exists());
}

// ---- the set ----------------------------------------------------------------------------------

#[test]
fn a_set_preserves_announcement_order() {
    // The agent announced them in the order it judged useful, and a sort would put the report after the raw data.
    let mut set = DeliverableSet::new();
    for title in ["report", "raw", "appendix"] {
        set.add(
            Deliverable::new(format!("/out/{title}.md"), title, DeliverableKind::Document)
                .expect("valid"),
        );
    }

    let titles: Vec<&str> = set
        .entries()
        .iter()
        .map(|entry| entry.title.as_str())
        .collect();
    assert_eq!(titles, vec!["report", "raw", "appendix"]);
}

#[test]
fn announcing_the_same_path_twice_adds_one_entry() {
    // A tool that wrote the same file twice announced it once, and a list that showed it twice would look like two
    // outputs.
    let mut set = DeliverableSet::new();
    set.add(Deliverable::new("/out/a.md", "first", DeliverableKind::Document).expect("valid"));
    set.add(Deliverable::new("/out/a.md", "second", DeliverableKind::Document).expect("valid"));

    assert_eq!(set.len(), 1);
    assert_eq!(
        set.entries()[0].title,
        "first",
        "the first announcement stands"
    );
}

#[test]
fn a_set_separates_present_from_missing() {
    let dir = scratch("split");
    let present = dir.join("here.md");
    std::fs::write(&present, "hi").expect("written");

    let mut set = DeliverableSet::new();
    set.add(Deliverable::new(&present, "here", DeliverableKind::Document).expect("valid"));
    set.add(
        Deliverable::new(dir.join("gone.md"), "gone", DeliverableKind::Document).expect("valid"),
    );

    assert_eq!(set.present().len(), 1);
    assert_eq!(set.missing().len(), 1);
    assert_eq!(set.present()[0].title, "here");
    assert_eq!(set.missing()[0].title, "gone");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_set_reports_itself_empty() {
    let set = DeliverableSet::new();

    assert!(set.is_empty());
    assert_eq!(set.len(), 0);
    assert!(set.entries().is_empty());
}

#[test]
fn a_set_can_hold_different_kinds() {
    let mut set = DeliverableSet::new();
    set.add(Deliverable::new("/a.md", "doc", DeliverableKind::Document).expect("valid"));
    set.add(Deliverable::new("/b.csv", "sheet", DeliverableKind::Spreadsheet).expect("valid"));

    assert_eq!(set.len(), 2);
    assert_eq!(set.entries()[1].kind, DeliverableKind::Spreadsheet);
}
