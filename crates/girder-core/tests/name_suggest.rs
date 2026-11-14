//! Tests for the nearest-name suggestion used when a call names a tool that does not exist.
//!
//! The module is wired into the crate by the integrator, so this test loads the source file
//! directly; that keeps the suite runnable against `src/name_suggest.rs` alone.
//!
//! Five properties carry the design, and the rest of the cases pin the arithmetic around them:
//! a name that is one character off is still recognised (the whole point), a distance past the cap
//! comes back as `cap + 1` and never as the true distance (so a long input cannot make the search
//! expensive), CJK names are counted in characters rather than bytes (or one wrong character looks
//! like three), the nearest name wins, and a name with nothing near it yields *no* suggestion
//! (because a fabricated hint sends the model somewhere the evidence does not point).

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the policy as written even before the crate root wires it in.
#[path = "../src/name_suggest.rs"]
mod name_suggest;

use name_suggest::{Candidate, closest, distance, suggest_tool};
use std::time::{Duration, Instant};

/// Names shaped like a real tool registry, used where the exact list does not matter.
fn tools() -> Vec<String> {
    [
        "read",
        "read_file",
        "write_file",
        "glob",
        "terminal_start",
        "terminal_send",
        "terminal_read",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// The names of every candidate, in the order they were returned.
fn names(candidates: &[Candidate]) -> Vec<&str> {
    candidates.iter().map(|entry| entry.name.as_str()).collect()
}

// ---- exact and one-character edits -------------------------------------------------------------

#[test]
fn an_identical_pair_is_distance_zero() {
    assert_eq!(distance("read_file", "read_file", 8), 0);
    assert_eq!(distance("", "", 8), 0);
}

#[test]
fn a_single_substitution_is_one_edit() {
    assert_eq!(distance("reed", "read", 8), 1);
    assert_eq!(distance("read", "reed", 8), 1);
}

#[test]
fn a_single_insertion_is_one_edit() {
    assert_eq!(distance("read_fil", "read_file", 8), 1);
    assert_eq!(distance("read_file", "read_fil", 8), 1);
}

#[test]
fn a_single_deletion_is_one_edit() {
    assert_eq!(distance("read_files", "read_file", 8), 1);
    assert_eq!(distance("terrminal_read", "terminal_read", 8), 1);
}

#[test]
fn the_classic_worked_example_is_three() {
    // The textbook case, kept because it exercises several edits inside one alignment rather than
    // one edit against a shared prefix.
    assert_eq!(distance("kitten", "sitting", 8), 3);
}

// ---- the cap -----------------------------------------------------------------------------------

#[test]
fn a_distance_past_the_cap_is_capped_plus_one() {
    // The true distance is 3; with a cap of 1 the answer must be 2, not 3.
    assert_eq!(distance("abc", "xyz", 1), 2);
    assert_eq!(distance("write_file", "read_file", 1), 2);
}

#[test]
fn a_cap_of_zero_admits_only_equality() {
    assert_eq!(distance("read", "read", 0), 0);
    assert_eq!(distance("read", "raed", 0), 1);
}

#[test]
fn a_length_gap_past_the_cap_is_rejected_without_a_table() {
    // No alignment can fix a length difference of five with a budget of two, so the answer is known
    // before any row is computed. The assertion is on the value; the value is what callers see.
    assert_eq!(distance("read", "read_file", 2), 3);
    assert_eq!(distance("", "read", 2), 3);
    assert_eq!(distance("read", "", 2), 3);
}

#[test]
fn the_sentinel_is_strictly_greater_than_the_cap() {
    // Callers compare against the cap to decide "close enough", so every rejected answer must be
    // above it -- a sentinel of `cap` would make a reject look like a match.
    for cap in 0..6 {
        let answer = distance("terminal_attach", "glob", cap);
        assert!(answer > cap, "cap {cap} produced {answer}");
        assert_eq!(answer, cap + 1, "cap {cap}");
    }
}

// ---- characters, not bytes ---------------------------------------------------------------------

#[test]
fn chinese_names_are_counted_in_characters() {
    // `写文件` and `读文件` are one character apart. Counted in bytes the difference would be three,
    // because each CJK character is three bytes of UTF-8, and a real typo would look like a rewrite.
    assert_eq!("写文件".len(), 9, "the byte count, which this must not use");
    assert_eq!(distance("写文件", "读文件", 8), 1);
    assert_eq!(distance("列出目录", "列出目录项", 8), 1);
}

#[test]
fn a_multi_byte_character_counts_once() {
    // An emoji is four bytes and one character; a byte-wise implementation would report four.
    assert_eq!("🦀".len(), 4);
    assert_eq!(distance("🦀x", "🦀y", 8), 1);
}

#[test]
fn a_chinese_typo_survives_the_tolerance() {
    let available = vec!["读文件".to_owned(), "列出目录".to_owned()];
    assert_eq!(
        suggest_tool("写文件", &available).as_deref(),
        Some("no tool named `写文件`; did you mean `读文件`?")
    );
}

// ---- closest -----------------------------------------------------------------------------------

#[test]
fn candidates_are_ordered_by_distance_then_name() {
    let available = vec![
        "read_fill".to_owned(),
        "glob".to_owned(),
        "read_file".to_owned(),
        "read_files".to_owned(),
    ];

    let found = closest("read_fil", &available, 8);

    // Two names are one edit away, so the tie is broken by name and the order is predictable: a
    // suggestion list that reshuffles between runs reads as randomness to a model retrying. `glob`
    // is dropped for being outside the tolerance of a seven-character target.
    assert_eq!(names(&found), ["read_file", "read_fill", "read_files"]);
    assert_eq!(found[0].distance, 1);
    assert_eq!(found[1].distance, 1);
    assert_eq!(found[2].distance, 2);
}

#[test]
fn the_limit_keeps_only_the_nearest() {
    let available = vec![
        "read_files".to_owned(),
        "read_file".to_owned(),
        "read_fill".to_owned(),
    ];

    let found = closest("read_fil", &available, 2);

    assert_eq!(names(&found), ["read_file", "read_fill"]);
    assert!(found.iter().all(|entry| entry.distance == 1));
}

#[test]
fn a_limit_of_zero_scores_nothing() {
    assert!(closest("read_file", &tools(), 0).is_empty());
}

#[test]
fn no_candidates_yields_no_matches() {
    assert!(closest("read_file", &[], 5).is_empty());
    assert_eq!(suggest_tool("read_file", &[]), None);
}

#[test]
fn candidates_beyond_the_tolerance_are_dropped() {
    // Every name here is at least four edits from `glob`, and the tolerance for a four-character
    // target is one, so none of them is offered.
    let available = vec!["read_file".to_owned(), "write_file".to_owned()];
    assert!(closest("glob", &available, 8).is_empty());
}

// ---- suggest_tool ------------------------------------------------------------------------------

#[test]
fn the_message_names_the_typo_and_the_fix() {
    let available = tools();
    assert_eq!(
        suggest_tool("read_fil", &available).as_deref(),
        Some("no tool named `read_fil`; did you mean `read_file`?")
    );
    assert_eq!(
        suggest_tool("reed", &available).as_deref(),
        Some("no tool named `reed`; did you mean `read`?")
    );
}

#[test]
fn the_motivating_typo_gets_a_suggestion() {
    // The case this module exists for: the model asked for `terminal_attach`, and the registry has
    // three terminal tools. `terminal_start` is the nearest, at four edits.
    let available = vec![
        "terminal_start".to_owned(),
        "terminal_send".to_owned(),
        "terminal_read".to_owned(),
    ];
    let message = suggest_tool("terminal_attach", &available).expect("a close name exists");
    assert!(message.starts_with("no tool named `terminal_attach`; did you mean `terminal_"));
    assert!(message.ends_with("`?"));
}

#[test]
fn nothing_close_means_no_suggestion() {
    // A fabricated candidate is worse than silence: `read` is four edits from `zzzz` and would send
    // the model somewhere the call does not justify.
    let available = tools();
    assert_eq!(suggest_tool("zzzz", &available), None);
    assert_eq!(suggest_tool("qqqq", &available), None);
}

#[test]
fn an_exact_name_is_not_suggested_back() {
    // An exact name that failed to resolve failed for another reason; telling the model to use the
    // name it just used is a loop, not a hint.
    assert_eq!(suggest_tool("read", &tools()), None);
}

#[test]
fn a_very_long_unknown_name_gets_no_fabricated_match() {
    // Model output can land a parameter value in the tool-name field. Whatever it was, it is not a
    // near miss of a five-letter tool name, and the answer is the original message.
    let available = tools();
    let unknown = "x".repeat(4_000);
    assert_eq!(suggest_tool(&unknown, &available), None);
}

// ---- bounded work ------------------------------------------------------------------------------

#[test]
fn a_long_input_does_not_explode() {
    // Two 50,000-character strings that share no prefix and no suffix. An unbounded table is
    // 2.5e9 cells and would not finish inside this budget; the banded one is a few thousand cells,
    // because a row whose best cell is already past the cap ends the computation.
    let left = "a".repeat(50_000);
    let right = "b".repeat(50_000);

    let started = Instant::now();
    let answer = distance(&left, &right, 8);
    let elapsed = started.elapsed();

    assert_eq!(answer, 9, "past the cap, so the sentinel");
    assert!(
        elapsed < Duration::from_secs(5),
        "50k x 50k took {elapsed:?}; the band is not bounding the work"
    );
}

#[test]
fn a_long_pair_differing_by_one_edit_is_still_exact() {
    // The bound must not cost correctness: a long pair that is genuinely one edit apart still
    // reports one, and the shared prefix and suffix keep it cheap.
    let left = format!("{}x{}", "a".repeat(20_000), "b".repeat(20_000));
    let right = format!("{}y{}", "a".repeat(20_000), "b".repeat(20_000));

    let started = Instant::now();
    assert_eq!(distance(&left, &right, 8), 1);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn the_whole_search_over_a_realistic_registry_is_cheap() {
    // Every name in the registry is scored against one unknown name; the price is per candidate,
    // so the list has to stay cheap too.
    let target = "terminal_attach";
    let started = Instant::now();
    let found = closest(target, &tools(), 3);
    assert!(!found.is_empty());
    assert!(started.elapsed() < Duration::from_millis(500));
    assert!(found[0].distance <= 5, "tolerance for a 15-character name");
}

#[test]
fn an_uncapped_distance_is_the_true_distance() {
    // `usize::MAX` is the natural way to say "no cap". The sentinel is `saturating_add(1)` rather
    // than `+ 1` for exactly this case, and the band widens to the whole table, so the answer is
    // the ordinary edit distance.
    assert_eq!(distance("kitten", "sitting", usize::MAX), 3);
    assert_eq!(distance("", "read", usize::MAX), 4);
}

#[test]
fn empty_inputs_are_handled_pairwise() {
    assert_eq!(distance("", "", 8), 0);
    assert_eq!(distance("", "abc", 8), 3);
    assert_eq!(distance("abc", "", 8), 3);
}
