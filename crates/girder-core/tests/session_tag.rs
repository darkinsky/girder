//! Tests for the session tag vocabulary.
//!
//! The load-bearing property is that a name has exactly one spelling. Two spellings of the same tag
//! would split a filter's results, show the same chip twice, and let an inferred tag sit beside the
//! manual one it duplicates. Every test that adds two names and asserts one tag is testing that.
//!
//! The second property is that a refused operation is visible. `add` reports a duplicate instead of
//! ignoring it, and the set is unchanged when it does — a caller has to be able to tell a write that
//! happened from one that did not.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/session_tag.rs"]
mod session_tag;

use session_tag::{MAX_CONFIDENCE, MAX_NAME_CHARS, Source, Tag, TagError, TagSet, normalize};

/// A manual tag, spelled however the case wants.
fn manual(name: &str) -> Tag {
    Tag {
        name: name.to_owned(),
        source: Source::Manual,
    }
}

/// An inferred tag with a confidence.
fn inferred(name: &str, confidence: u8) -> Tag {
    Tag {
        name: name.to_owned(),
        source: Source::Inferred { confidence },
    }
}

// ---- normalization -----------------------------------------------------------------------------

#[test]
fn normalization_lowercases() {
    assert_eq!(normalize("Bug"), "bug");
    assert_eq!(normalize("BUG FIX"), "bug-fix");
}

#[test]
fn normalization_turns_spaces_into_hyphens() {
    assert_eq!(normalize("bug fix"), "bug-fix");
    assert_eq!(normalize("needs reproduction"), "needs-reproduction");
}

#[test]
fn normalization_strips_leading_and_trailing_whitespace() {
    assert_eq!(normalize("  bug fix  "), "bug-fix");
    assert_eq!(normalize("\t\nbug\t\n"), "bug");
}

#[test]
fn normalization_collapses_runs_of_separators() {
    // A run of separators is one separation, and the three characters people type for it — space,
    // hyphen, underscore — are the same spelling.
    assert_eq!(normalize("bug   fix"), "bug-fix");
    assert_eq!(normalize("bug__fix"), "bug-fix");
    assert_eq!(normalize("bug - _ fix"), "bug-fix");
    assert_eq!(normalize("bug\nfix"), "bug-fix");
}

#[test]
fn normalization_keeps_punctuation_that_carries_meaning() {
    // `#rust` is not `rust` to whoever typed the `#`, so the `#` is content rather than spelling.
    assert_eq!(normalize("#rust"), "#rust");
    assert_eq!(normalize("v1.2"), "v1.2");
}

#[test]
fn normalization_of_nothing_is_empty() {
    assert_eq!(normalize(""), "");
    assert_eq!(normalize("   "), "");
    assert_eq!(normalize(" - _ \t\n"), "");
}

#[test]
fn normalization_does_not_emit_a_trailing_hyphen() {
    // The hyphen in "bug  " used to be the classic way to produce `bug-`, which then fails to match
    // its own normalization on the next lookup.
    assert_eq!(normalize("bug  "), "bug");
    assert_eq!(normalize("bug-"), "bug");
    assert_eq!(normalize("  -bug"), "bug");
}

// ---- duplicate rejection: the core property ----------------------------------------------------

#[test]
fn two_spellings_of_one_name_are_one_tag() {
    let mut set = TagSet::new();

    assert_eq!(set.add(manual("Bug Fix")), Ok(()));
    assert_eq!(
        set.add(manual("bug-fix")),
        Err(TagError::Duplicate {
            name: "bug-fix".to_owned()
        })
    );
    assert_eq!(set.len(), 1);
}

#[test]
fn a_duplicate_is_reported_rather_than_ignored() {
    // A silent no-op is indistinguishable from a successful add at the call site; the error is what
    // lets a caller say "already tagged".
    let mut set = TagSet::new();
    set.add(manual("bug-fix")).expect("first add succeeds");

    let second = set.add(manual("BUG_FIX"));
    assert!(matches!(second, Err(TagError::Duplicate { .. })));
    // The message names the normalized spelling, which is the one `remove` and `promote` accept.
    assert_eq!(
        second.map_err(|error| error.to_string()),
        Err("a tag named `bug-fix` is already in the set".to_owned())
    );
}

#[test]
fn a_duplicate_leaves_the_original_untouched() {
    let mut set = TagSet::new();
    set.add(inferred("flaky", 40)).expect("first add succeeds");

    let _ = set.add(inferred("Flaky", 90));

    let tags = set.all();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].name, "flaky");
    // The rejected write must not have raised the confidence of the tag that stayed.
    assert_eq!(tags[0].source, Source::Inferred { confidence: 40 });
}

// ---- rejections --------------------------------------------------------------------------------

#[test]
fn an_empty_name_is_rejected() {
    let mut set = TagSet::new();

    assert_eq!(set.add(manual("")), Err(TagError::Empty));
    assert!(set.is_empty());
}

#[test]
fn a_name_of_only_separators_is_rejected() {
    let mut set = TagSet::new();

    assert_eq!(set.add(manual("   ")), Err(TagError::Empty));
    assert_eq!(set.add(manual(" - _ ")), Err(TagError::Empty));
    assert!(set.is_empty());
}

#[test]
fn an_over_long_name_is_rejected() {
    let mut set = TagSet::new();
    let long = "a".repeat(MAX_NAME_CHARS + 1);

    assert_eq!(
        set.add(manual(&long)),
        Err(TagError::TooLong {
            length: MAX_NAME_CHARS + 1,
            max: MAX_NAME_CHARS,
        })
    );
    assert!(set.is_empty());
}

#[test]
fn a_name_exactly_at_the_limit_is_accepted() {
    // The bound is inclusive; an off-by-one here would make the documented limit wrong.
    let mut set = TagSet::new();
    let at_limit = "a".repeat(MAX_NAME_CHARS);

    assert_eq!(set.add(manual(&at_limit)), Ok(()));
    assert!(set.has(&at_limit));
}

#[test]
fn the_length_limit_counts_characters_not_bytes() {
    // A byte limit would give this name an eighth of the budget of a Latin one.
    let mut set = TagSet::new();
    let cjk = "标".repeat(MAX_NAME_CHARS);

    assert_eq!(set.add(manual(&cjk)), Ok(()));
}

#[test]
fn length_is_measured_after_normalization() {
    // Padding is spelling, not content: a name that is short once trimmed is short.
    let mut set = TagSet::new();
    let padded = format!("  {}  ", "a".repeat(MAX_NAME_CHARS));

    assert_eq!(set.add(manual(&padded)), Ok(()));
}

// ---- removal ------------------------------------------------------------------------------------

#[test]
fn remove_takes_the_tag_out() {
    let mut set = TagSet::new();
    set.add(manual("bug-fix")).expect("add succeeds");

    assert!(set.remove("bug-fix"));
    assert!(!set.has("bug-fix"));
    assert!(set.is_empty());
}

#[test]
fn remove_normalizes_its_argument() {
    let mut set = TagSet::new();
    set.add(manual("bug-fix")).expect("add succeeds");

    assert!(set.remove("  Bug Fix  "));
}

#[test]
fn removing_something_absent_reports_no_change() {
    let mut set = TagSet::new();
    set.add(manual("bug-fix")).expect("add succeeds");

    assert!(!set.remove("feature"));
    assert_eq!(set.len(), 1);
}

#[test]
fn a_removed_tag_can_be_added_again() {
    // `remove` is a real delete rather than a tombstone, so re-tagging is an ordinary add.
    let mut set = TagSet::new();
    set.add(manual("bug-fix")).expect("add succeeds");
    assert!(set.remove("bug-fix"));

    assert_eq!(set.add(inferred("bug fix", 70)), Ok(()));
    assert_eq!(set.len(), 1);
}

// ---- ordering ----------------------------------------------------------------------------------

#[test]
fn all_is_sorted_by_name() {
    let mut set = TagSet::new();
    for name in ["zeta", "Alpha", "mid fix", "beta"] {
        set.add(manual(name)).expect("add succeeds");
    }

    let names: Vec<&str> = set.all().iter().map(|tag| tag.name.as_str()).collect();
    assert_eq!(names, ["alpha", "beta", "mid-fix", "zeta"]);
}

#[test]
fn two_sets_built_in_different_orders_are_equal() {
    let mut first = TagSet::new();
    let mut second = TagSet::new();
    for name in ["one", "two", "three"] {
        first.add(manual(name)).expect("add succeeds");
    }
    for name in ["three", "one", "two"] {
        second.add(manual(name)).expect("add succeeds");
    }

    assert_eq!(first.all(), second.all());
    assert_eq!(first, second);
}

// ---- manual versus inferred: the other core property -------------------------------------------

#[test]
fn an_inferred_tag_does_not_replace_a_manual_one() {
    // A rule runs over every session; if it could overwrite the tag a person typed, the person's
    // assertion would last only until the next rule pass.
    let mut set = TagSet::new();
    set.add(manual("bug-fix")).expect("add succeeds");

    let result = set.add(inferred("bug fix", 95));

    assert_eq!(
        result,
        Err(TagError::Duplicate {
            name: "bug-fix".to_owned()
        })
    );
    assert_eq!(set.all()[0].source, Source::Manual);
}

#[test]
fn a_manual_tag_does_not_replace_an_inferred_one_either() {
    // Also a duplicate, because `add` must not rewrite an existing tag's source behind a caller's
    // back. The sanctioned upgrade is `promote`.
    let mut set = TagSet::new();
    set.add(inferred("bug-fix", 60)).expect("add succeeds");

    let result = set.add(manual("Bug Fix"));

    assert!(matches!(result, Err(TagError::Duplicate { .. })));
    assert_eq!(set.all()[0].source, Source::Inferred { confidence: 60 });
}

#[test]
fn promote_turns_an_inferred_tag_into_a_manual_one() {
    let mut set = TagSet::new();
    set.add(inferred("bug-fix", 60)).expect("add succeeds");

    assert!(set.promote("Bug Fix"));

    assert_eq!(set.all()[0].source, Source::Manual);
    assert_eq!(set.all()[0].name, "bug-fix");
    assert_eq!(set.len(), 1);
}

#[test]
fn promoting_a_manual_tag_changes_nothing() {
    let mut set = TagSet::new();
    set.add(manual("bug-fix")).expect("add succeeds");

    assert!(set.promote("bug-fix"));
    assert_eq!(set.all()[0].source, Source::Manual);
}

#[test]
fn promoting_an_absent_tag_reports_no_change() {
    let mut set = TagSet::new();

    assert!(!set.promote("bug-fix"));
    assert!(set.is_empty());
}

#[test]
fn a_manual_tag_and_an_inferred_tag_differ_in_source() {
    // Two sets with the same name but a different source are different sets, so a caller comparing
    // them sees the promotion rather than "no change".
    let mut manual_set = TagSet::new();
    manual_set.add(manual("bug-fix")).expect("add succeeds");
    let mut inferred_set = TagSet::new();
    inferred_set.add(inferred("bug-fix", 60)).expect("add succeeds");

    assert_ne!(manual_set, inferred_set);
}

// ---- confidence --------------------------------------------------------------------------------

#[test]
fn confidence_is_clamped_to_the_maximum() {
    let mut set = TagSet::new();
    set.add(inferred("bug-fix", 250)).expect("add succeeds");

    assert_eq!(
        set.all()[0].source,
        Source::Inferred {
            confidence: MAX_CONFIDENCE
        }
    );
}

#[test]
fn a_zero_confidence_tag_is_still_a_tag() {
    // Zero means "a wild guess", not "do not store": the caller asked for it, and dropping it would
    // make the set's contents depend on a number rather than on an instruction.
    let mut set = TagSet::new();
    set.add(inferred("speculative", 0)).expect("add succeeds");

    assert!(set.has("speculative"));
    assert_eq!(set.all()[0].source, Source::Inferred { confidence: 0 });
}

// ---- membership --------------------------------------------------------------------------------

#[test]
fn has_normalizes_its_argument() {
    let mut set = TagSet::new();
    set.add(manual("needs reproduction")).expect("add succeeds");

    assert!(set.has("needs-reproduction"));
    assert!(set.has("  Needs   Reproduction "));
    assert!(!set.has("needs-repro"));
}

#[test]
fn an_empty_set_contains_nothing() {
    let set = TagSet::new();

    assert!(set.is_empty());
    assert_eq!(set.len(), 0);
    assert!(!set.has(""));
    assert!(!set.has("bug-fix"));
    assert!(set.all().is_empty());
}

#[test]
fn the_stored_name_is_the_normalized_one() {
    let mut set = TagSet::new();
    set.add(manual("  Bug   Fix ")).expect("add succeeds");

    assert_eq!(set.all()[0].name, "bug-fix");
}

#[test]
fn all_returns_deeply_ordered_tags_not_map_keys() {
    let mut set = TagSet::new();
    set.add(manual("second-name")).expect("add succeeds");

    let tag: &Tag = set.all()[0];
    assert_eq!(tag.name, "second-name");
    assert_eq!(tag.source, Source::Manual);
}
