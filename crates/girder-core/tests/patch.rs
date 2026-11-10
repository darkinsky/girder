//! Tests for patch parsing and application.
//!
//! The dangerous cases are the ones where a hunk could apply in the wrong place: an ambiguous context, a moved line
//! number, and a hunk that claims to be at the end of a file it is not.

use girder_core::{PatchFailureReason, PatchOperation, PatchParseError, apply_patch, parse_patch};

fn one_hunk(old: &[&str], new: &[&str]) -> Vec<girder_core::PatchHunk> {
    vec![girder_core::PatchHunk {
        line_number: 1,
        context: None,
        old_lines: old.iter().map(|line| (*line).to_owned()).collect(),
        new_lines: new.iter().map(|line| (*line).to_owned()).collect(),
        is_end_of_file: false,
    }]
}

// ---- parsing ----------------------------------------------------------------------------------

#[test]
fn an_add_operation_carries_its_content() {
    let patch = "*** Begin Patch\n*** Add File: a.txt\n+one\n+two\n*** End Patch\n";

    let operations = parse_patch(patch).expect("parsed");

    let PatchOperation::Add { path, content, .. } = &operations[0] else {
        panic!("expected Add");
    };
    assert_eq!(path, "a.txt");
    assert_eq!(content, "one\ntwo\n");
}

#[test]
fn a_delete_operation_names_one_path() {
    let patch = "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n";

    let operations = parse_patch(patch).expect("parsed");

    assert!(matches!(&operations[0], PatchOperation::Delete { path, .. } if path == "gone.txt"));
}

#[test]
fn an_update_operation_parses_its_hunk() {
    let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n one\n-two\n+three\n*** End Patch\n";

    let operations = parse_patch(patch).expect("parsed");

    let PatchOperation::Update { hunks, .. } = &operations[0] else {
        panic!("expected Update");
    };
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].old_lines, vec!["one", "two"]);
    assert_eq!(hunks[0].new_lines, vec!["one", "three"]);
}

#[test]
fn a_move_is_an_update_with_a_destination() {
    // Modelling it as two operations would let one succeed without the other.
    let patch =
        "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n-a\n+b\n*** End Patch\n";

    let operations = parse_patch(patch).expect("parsed");

    let PatchOperation::Update { move_to, .. } = &operations[0] else {
        panic!("expected Update");
    };
    assert_eq!(move_to.as_deref(), Some("b.txt"));
}

#[test]
fn several_operations_parse_in_order() {
    let patch = "*** Begin Patch\n*** Add File: a\n+x\n*** Delete File: b\n*** End Patch\n";

    let operations = parse_patch(patch).expect("parsed");

    assert_eq!(operations.len(), 2);
    assert!(matches!(&operations[0], PatchOperation::Add { .. }));
    assert!(matches!(&operations[1], PatchOperation::Delete { .. }));
}

#[test]
fn an_empty_patch_is_refused() {
    assert!(matches!(
        parse_patch("*** Begin Patch\n*** End Patch\n"),
        Err(PatchParseError::Empty)
    ));
}

#[test]
fn an_end_of_file_marker_is_recorded() {
    let patch =
        "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+two\n*** End of File\n*** End Patch\n";

    let operations = parse_patch(patch).expect("parsed");

    let PatchOperation::Update { hunks, .. } = &operations[0] else {
        panic!("expected Update");
    };
    assert!(hunks[0].is_end_of_file);
}

// ---- applying ---------------------------------------------------------------------------------

#[test]
fn a_hunk_replaces_the_lines_it_names() {
    let text = "one\ntwo\nthree\n";

    let applied = apply_patch(text, &one_hunk(&["two"], &["TWO"]));

    assert!(applied.is_complete());
    assert_eq!(applied.text, "one\nTWO\nthree\n");
}

#[test]
fn an_insertion_adds_a_line() {
    let applied = apply_patch(
        "one\nthree\n",
        &one_hunk(&["one", "three"], &["one", "two", "three"]),
    );

    assert_eq!(applied.text, "one\ntwo\nthree\n");
}

#[test]
fn a_deletion_removes_a_line() {
    let applied = apply_patch("one\ntwo\nthree\n", &one_hunk(&["one", "two"], &["one"]));

    assert_eq!(applied.text, "one\nthree\n");
}

#[test]
fn a_trailing_newline_survives() {
    // Dropping it would make every applied patch show as a whitespace change at the end of the file.
    let applied = apply_patch("one\ntwo\n", &one_hunk(&["two"], &["TWO"]));

    assert!(applied.text.ends_with('\n'));
}

#[test]
fn a_file_without_a_trailing_newline_keeps_none() {
    let applied = apply_patch("one\ntwo", &one_hunk(&["two"], &["TWO"]));

    assert!(!applied.text.ends_with('\n'));
}

#[test]
fn a_hunk_applies_where_the_context_is_even_when_the_line_number_is_stale() {
    // The line number is wrong whenever the file changed since the patch was made; the context is what survives.
    let text = "inserted\none\ntwo\n";
    let mut hunks = one_hunk(&["one", "two"], &["one", "TWO"]);
    hunks[0].line_number = 1;

    let applied = apply_patch(text, &hunks);

    assert!(applied.is_complete());
    assert_eq!(applied.text, "inserted\none\nTWO\n");
}

#[test]
fn a_missing_context_fails() {
    let applied = apply_patch("one\ntwo\n", &one_hunk(&["absent"], &["x"]));

    assert!(!applied.is_complete());
    assert_eq!(
        applied.failures[0].reason,
        PatchFailureReason::ContextNotFound
    );
}

#[test]
fn a_partial_application_leaves_the_text_unchanged() {
    // A file that is neither the old one nor the new one is worse than not applying at all.
    let text = "one\ntwo\n";
    let hunks = vec![
        girder_core::PatchHunk {
            line_number: 1,
            context: None,
            old_lines: vec!["one".to_owned()],
            new_lines: vec!["ONE".to_owned()],
            is_end_of_file: false,
        },
        girder_core::PatchHunk {
            line_number: 2,
            context: None,
            old_lines: vec!["absent".to_owned()],
            new_lines: vec!["x".to_owned()],
            is_end_of_file: false,
        },
    ];

    let applied = apply_patch(text, &hunks);

    assert!(!applied.is_complete());
    assert_eq!(applied.text, text, "the input is returned untouched");
    assert_eq!(applied.applied, 0);
}

#[test]
fn an_ambiguous_context_is_refused_rather_than_guessed() {
    // Guessing would edit the wrong function.
    let text = "a\n}\nb\n}\nc\n";
    let applied = apply_patch(text, &one_hunk(&["}", "b"], &["}", "B"]));

    // `}` + `b` occurs once, so this one applies; the ambiguous case is a repeated block.
    assert!(applied.is_complete());

    // The stated line lands nowhere near either occurrence, which is what a patch made against a different version
    // of the file looks like. Picking the nearest would edit a function the patch was never about.
    let mut hunks = one_hunk(&["x", "}"], &["X", "}"]);
    hunks[0].line_number = 2;
    let ambiguous = apply_patch("x\n}\ny\nz\nx\n}\ny\n", &hunks);
    assert!(
        matches!(
            ambiguous.failures.first().map(|failure| &failure.reason),
            Some(PatchFailureReason::Ambiguous { .. })
        ),
        "{:?}",
        ambiguous.failures
    );
}

#[test]
fn an_ambiguous_context_is_broken_when_the_stated_line_points_at_one_match() {
    let text = "x\n}\ny\nx\n}\ny\n";
    let mut hunks = one_hunk(&["x", "}"], &["X", "}"]);
    // The second occurrence begins at zero-based index 3, which is one-based line 4.
    hunks[0].line_number = 4;

    let applied = apply_patch(text, &hunks);

    assert!(applied.is_complete(), "{:?}", applied.failures);
    assert_eq!(
        applied.text, "x\n}\ny\nX\n}\ny\n",
        "the later occurrence changed"
    );
}

#[test]
fn a_hunk_marked_at_the_end_must_be_at_the_end() {
    let text = "one\ntwo\nthree\n";
    let mut hunks = one_hunk(&["one"], &["ONE"]);
    hunks[0].is_end_of_file = true;

    let applied = apply_patch(text, &hunks);

    assert_eq!(
        applied
            .failures
            .first()
            .map(|failure| failure.reason.clone()),
        Some(PatchFailureReason::NotAtEnd)
    );
}

#[test]
fn a_hunk_marked_at_the_end_applies_at_the_end() {
    let text = "one\ntwo\n";
    let mut hunks = one_hunk(&["two"], &["TWO"]);
    hunks[0].is_end_of_file = true;

    let applied = apply_patch(text, &hunks);

    assert!(applied.is_complete());
    assert_eq!(applied.text, "one\nTWO\n");
}

#[test]
fn several_hunks_apply_in_sequence() {
    let text = "a\nb\nc\nd\n";
    let hunks = vec![
        girder_core::PatchHunk {
            line_number: 1,
            context: None,
            old_lines: vec!["a".to_owned()],
            new_lines: vec!["A".to_owned()],
            is_end_of_file: false,
        },
        girder_core::PatchHunk {
            line_number: 4,
            context: None,
            old_lines: vec!["d".to_owned()],
            new_lines: vec!["D".to_owned()],
            is_end_of_file: false,
        },
    ];

    let applied = apply_patch(text, &hunks);

    assert!(applied.is_complete());
    assert_eq!(applied.applied, 2);
    assert_eq!(applied.text, "A\nb\nc\nD\n");
}

#[test]
fn a_whitespace_difference_is_a_difference() {
    // Whitespace is content; a matcher that normalised it would apply to a line the patch was not made against.
    let applied = apply_patch("one  \n", &one_hunk(&["one"], &["ONE"]));

    assert!(!applied.is_complete());
}

#[test]
fn every_failure_is_reported_not_just_the_first() {
    // Three round trips where one would do.
    let hunks = vec![
        girder_core::PatchHunk {
            line_number: 1,
            context: None,
            old_lines: vec!["absent-a".to_owned()],
            new_lines: vec!["x".to_owned()],
            is_end_of_file: false,
        },
        girder_core::PatchHunk {
            line_number: 2,
            context: None,
            old_lines: vec!["absent-b".to_owned()],
            new_lines: vec!["y".to_owned()],
            is_end_of_file: false,
        },
    ];

    let applied = apply_patch("one\n", &hunks);

    assert_eq!(applied.failures.len(), 2);
    assert_eq!(applied.failures[0].hunk, 0);
    assert_eq!(applied.failures[1].hunk, 1);
}
