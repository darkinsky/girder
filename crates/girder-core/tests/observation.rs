//! Tests for file observation and in-memory editing.
//!
//! The two failures these guard against are opposite: a write that clobbers a change the session never saw, and an
//! edit that changes code the caller never looked at.

use std::path::Path;

use girder_core::{
    EditError, FileVersion, Observation, ObservationConflict, ObservationSession,
    replace_exactly_once, replace_lines,
};

// ---- versions ---------------------------------------------------------------------------------

#[test]
fn two_versions_with_the_same_size_and_time_match() {
    let a = FileVersion::new(10, 1000);
    let b = FileVersion::new(10, 1000);

    assert!(a.matches(&b));
}

#[test]
fn a_size_difference_is_a_difference() {
    assert!(!FileVersion::new(10, 1000).matches(&FileVersion::new(11, 1000)));
}

#[test]
fn a_hash_decides_when_both_sides_have_one() {
    // A same-size edit within one timestamp tick is invisible to the cheaper fields, and that is precisely the
    // change a clobber check exists to catch.
    let observed = FileVersion::new(10, 1000).with_hash(1);
    let current = FileVersion::new(10, 1000).with_hash(2);

    assert!(!observed.matches(&current));
    assert!(observed.matches(&FileVersion::new(10, 1000).with_hash(1)));
}

#[test]
fn without_hashes_the_cheap_fields_decide() {
    let observed = FileVersion::new(10, 1000);
    let current = FileVersion::new(10, 1001);

    assert!(
        !observed.matches(&current),
        "a later modification time is a change"
    );
}

// ---- write checks -----------------------------------------------------------------------------

#[test]
fn an_unobserved_write_is_allowed() {
    // A session that never read a file has no expectation to violate.
    let session = ObservationSession::new();

    assert!(
        session
            .check_write(Path::new("/a"), Some(&FileVersion::new(1, 1)))
            .is_ok()
    );
}

#[test]
fn an_unchanged_file_may_be_written() {
    let session = ObservationSession::new();
    session.observe_present("/a", FileVersion::new(10, 1000));

    assert!(
        session
            .check_write(Path::new("/a"), Some(&FileVersion::new(10, 1000)))
            .is_ok()
    );
}

#[test]
fn a_changed_file_refuses_the_write() {
    let session = ObservationSession::new();
    session.observe_present("/a", FileVersion::new(10, 1000));

    let conflict = session
        .check_write(Path::new("/a"), Some(&FileVersion::new(20, 2000)))
        .expect_err("changed");

    let ObservationConflict::Changed { observed, current } = conflict else {
        panic!("expected Changed");
    };
    assert_eq!(observed.bytes, 10);
    assert_eq!(current.bytes, 20);
}

#[test]
fn a_deleted_file_refuses_the_write() {
    let session = ObservationSession::new();
    session.observe_present("/a", FileVersion::new(10, 1000));

    let conflict = session
        .check_write(Path::new("/a"), None)
        .expect_err("deleted");

    assert_eq!(conflict, ObservationConflict::Deleted);
}

#[test]
fn a_file_that_appeared_refuses_a_creating_write() {
    // A session that read a missing file has an expectation — that it is still missing.
    let session = ObservationSession::new();
    session.observe_absent("/a");

    let conflict = session
        .check_write(Path::new("/a"), Some(&FileVersion::new(1, 1)))
        .expect_err("created");

    assert_eq!(conflict, ObservationConflict::Created);
}

#[test]
fn a_still_absent_file_may_be_created() {
    let session = ObservationSession::new();
    session.observe_absent("/a");

    assert!(session.check_write(Path::new("/a"), None).is_ok());
}

#[test]
fn forgetting_a_path_removes_the_expectation() {
    // Called after a successful write: keeping the old expectation would make the session's next write fail against
    // a change it made itself.
    let session = ObservationSession::new();
    session.observe_present("/a", FileVersion::new(10, 1000));

    session.forget(Path::new("/a"));

    assert!(session.observed(Path::new("/a")).is_none());
    assert!(
        session
            .check_write(Path::new("/a"), Some(&FileVersion::new(99, 99)))
            .is_ok()
    );
}

#[test]
fn observations_are_per_path() {
    let session = ObservationSession::new();
    session.observe_present("/a", FileVersion::new(1, 1));
    session.observe_absent("/b");

    assert!(matches!(
        session.observed(Path::new("/a")),
        Some(Observation::Present(_))
    ));
    assert_eq!(session.observed(Path::new("/b")), Some(Observation::Absent));
    assert_eq!(session.paths().len(), 2);
}

#[test]
fn a_conflict_describes_itself_for_the_model() {
    let conflict = ObservationConflict::Changed {
        observed: FileVersion::new(10, 1000),
        current: FileVersion::new(20, 2000),
    };

    let text = conflict.describe();
    assert!(text.contains("10 bytes"), "{text}");
    assert!(text.contains("20 bytes"), "{text}");
}

// ---- exact replacement ------------------------------------------------------------------------

#[test]
fn one_occurrence_is_replaced() {
    let out = replace_exactly_once("one two three", "two", "TWO").expect("replaced");

    assert_eq!(out, "one TWO three");
}

#[test]
fn a_missing_text_is_refused() {
    assert_eq!(
        replace_exactly_once("one", "absent", "x"),
        Err(EditError::NotFound)
    );
}

#[test]
fn two_occurrences_are_refused_rather_than_replaced_wholesale() {
    // An editor that replaced every occurrence would change code the caller never looked at.
    let error = replace_exactly_once("a b a", "a", "X").expect_err("ambiguous");

    let EditError::Ambiguous { matches } = error else {
        panic!("expected Ambiguous");
    };
    assert_eq!(matches, 2);
}

#[test]
fn an_identical_replacement_is_refused() {
    assert_eq!(
        replace_exactly_once("same", "same", "same"),
        Err(EditError::NoChange)
    );
}

#[test]
fn an_empty_search_text_is_refused() {
    // Every position matches an empty string, so replacing one would be arbitrary.
    assert_eq!(
        replace_exactly_once("text", "", "x"),
        Err(EditError::NotFound)
    );
}

// ---- line replacement -------------------------------------------------------------------------

#[test]
fn a_line_range_is_replaced() {
    let out = replace_lines("a\nb\nc\n", 2, 2, "B").expect("replaced");

    assert_eq!(out, "a\nB\nc\n");
}

#[test]
fn a_multi_line_range_is_replaced() {
    let out = replace_lines("a\nb\nc\nd\n", 2, 3, "X\nY").expect("replaced");

    assert_eq!(out, "a\nX\nY\nd\n");
}

#[test]
fn a_range_beyond_the_text_is_refused() {
    assert_eq!(replace_lines("a\nb\n", 5, 6, "x"), Err(EditError::NotFound));
}

#[test]
fn a_zero_line_is_refused() {
    // The range is one-based, matching what a person sees.
    assert_eq!(replace_lines("a\n", 0, 1, "x"), Err(EditError::NotFound));
}

#[test]
fn a_reversed_range_is_refused() {
    assert_eq!(replace_lines("a\nb\n", 2, 1, "x"), Err(EditError::NotFound));
}

#[test]
fn a_trailing_newline_survives_a_line_edit() {
    // Otherwise every line edit shows as a whitespace change at the end of the file.
    let out = replace_lines("a\nb\n", 2, 2, "B").expect("replaced");

    assert!(out.ends_with('\n'));
}

#[test]
fn an_insertion_at_the_end_works() {
    let out = replace_lines("a\nb\n", 2, 2, "b\nc").expect("replaced");

    assert_eq!(out, "a\nb\nc\n");
}
