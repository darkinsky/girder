//! Tests for output retention.
//!
//! Two properties carry this: a cut never splits a character, and the omission count is exact so a
//! caller can report "12 more matches" without implying the tool failed.

use girder_core::{ItemRetainer, ItemStrategy, Omitted, TextRetainer, TextStrategy};

// ---- items ------------------------------------------------------------------------------------

#[test]
fn a_head_retainer_keeps_the_first_units() {
    let mut retainer = ItemRetainer::new(ItemStrategy::Head { max_items: 3 });

    for index in 0..5 {
        retainer.push(index);
    }
    let result = retainer.finish();

    assert_eq!(result.items, vec![0, 1, 2]);
    assert!(result.truncated);
    assert_eq!(result.seen, 5);
    assert_eq!(result.kept, 3);
    assert_eq!(result.omitted, Omitted::Exact { count: 2 });
}

#[test]
fn a_retainer_under_its_budget_reports_no_omission() {
    let mut retainer = ItemRetainer::new(ItemStrategy::Head { max_items: 10 });

    for index in 0..3 {
        retainer.push(index);
    }
    let result = retainer.finish();

    assert_eq!(result.items.len(), 3);
    assert!(!result.truncated);
    assert_eq!(result.omitted, Omitted::None);
}

#[test]
fn push_reports_whether_the_unit_was_kept() {
    // So a caller streaming items can stop generating once the budget is full: the difference between
    // bounding output and bounding work.
    let mut retainer = ItemRetainer::new(ItemStrategy::Head { max_items: 2 });

    assert!(retainer.push("a"));
    assert!(retainer.push("b"));
    assert!(!retainer.push("c"));
}

#[test]
fn is_full_reports_the_budget() {
    let mut retainer = ItemRetainer::new(ItemStrategy::Head { max_items: 1 });
    assert!(!retainer.is_full());

    retainer.push(1);
    assert!(retainer.is_full());
}

#[test]
fn a_zero_budget_keeps_nothing() {
    let mut retainer = ItemRetainer::new(ItemStrategy::Head { max_items: 0 });
    retainer.push(1);

    let result = retainer.finish();

    assert!(result.items.is_empty());
    assert_eq!(result.omitted, Omitted::Exact { count: 1 });
}

#[test]
fn an_empty_result_is_not_truncated() {
    let retainer: ItemRetainer<u8> = ItemRetainer::new(ItemStrategy::Head { max_items: 5 });
    let result = retainer.finish();

    assert!(!result.truncated);
    assert_eq!(result.omitted, Omitted::None);
}

// ---- text -------------------------------------------------------------------------------------

#[test]
fn a_head_retainer_keeps_the_prefix() {
    let mut retainer = TextRetainer::new(TextStrategy::Head { max_bytes: 5 });
    retainer.push("hello world");

    let result = retainer.finish();

    assert_eq!(result.text, "hello");
    assert!(result.truncated);
    assert_eq!(result.omitted_bytes, Omitted::Exact { count: 6 });
}

#[test]
fn a_tail_retainer_keeps_the_suffix() {
    // A log's interesting part is usually the end.
    let mut retainer = TextRetainer::new(TextStrategy::Tail { max_bytes: 5 });
    retainer.push("hello world");

    let result = retainer.finish();

    assert_eq!(result.text, "world");
}

#[test]
fn a_tail_retainer_bounds_as_it_grows() {
    // A caller streaming a large command's output must not hold the whole thing first.
    let mut retainer = TextRetainer::new(TextStrategy::Tail { max_bytes: 4 });
    for _ in 0..1_000 {
        retainer.push("abcdefgh");
    }

    let result = retainer.finish();
    assert_eq!(result.text.len(), 4);
    assert_eq!(result.text, "efgh");
}

#[test]
fn a_head_tail_retainer_keeps_both_ends() {
    // What a shell tool wants: the command's echo at the start, its result at the end.
    let mut retainer = TextRetainer::new(TextStrategy::HeadTail {
        head_bytes: 4,
        tail_bytes: 4,
    });
    retainer.push("HEAD");
    retainer.push("middle");
    retainer.push("TAIL");

    let result = retainer.finish();

    assert!(result.text.starts_with("HEAD"), "{}", result.text);
    assert!(result.text.ends_with("TAIL"), "{}", result.text);
    assert!(!result.text.contains("middle"), "{}", result.text);
}

#[test]
fn a_text_retainer_that_keeps_everything_is_not_truncated() {
    let mut retainer = TextRetainer::new(TextStrategy::Head { max_bytes: 100 });
    retainer.push("short");

    let result = retainer.finish();

    assert_eq!(result.text, "short");
    assert!(!result.truncated);
    assert_eq!(result.omitted_bytes, Omitted::None);
}

// ---- utf-8 boundaries -------------------------------------------------------------------------

#[test]
fn a_head_cut_never_splits_a_character() {
    // Three-byte characters, so a budget of 4 lands inside the second one.
    let mut retainer = TextRetainer::new(TextStrategy::Head { max_bytes: 4 });
    retainer.push("字字字");

    let result = retainer.finish();

    assert_eq!(
        result.text, "字",
        "one whole character, not one and a third"
    );
    assert!(std::str::from_utf8(result.text.as_bytes()).is_ok());
}

#[test]
fn a_tail_cut_never_splits_a_character() {
    // The ring drops bytes from the front, so the first retained byte may be a continuation.
    let mut retainer = TextRetainer::new(TextStrategy::Tail { max_bytes: 4 });
    retainer.push("字字字");

    let result = retainer.finish();

    assert_eq!(result.text, "字");
    assert!(std::str::from_utf8(result.text.as_bytes()).is_ok());
}

#[test]
fn a_head_tail_cut_never_splits_a_character() {
    let mut retainer = TextRetainer::new(TextStrategy::HeadTail {
        head_bytes: 4,
        tail_bytes: 4,
    });
    retainer.push("字字字字字");

    let result = retainer.finish();

    // Two whole characters, one from each end.
    assert_eq!(result.text, "字字");
    assert!(std::str::from_utf8(result.text.as_bytes()).is_ok());
}

#[test]
fn an_ascii_cut_keeps_the_full_budget() {
    // A boundary walk that always backed up one byte would silently return less than asked for.
    let mut retainer = TextRetainer::new(TextStrategy::Head { max_bytes: 10 });
    retainer.push("0123456789abcdef");

    assert_eq!(retainer.finish().text, "0123456789");
}

#[test]
fn a_mixed_script_cut_stays_valid() {
    let mut retainer = TextRetainer::new(TextStrategy::Head { max_bytes: 8 });
    // 1 byte + 2 bytes + 3 bytes + 3 bytes = 9; a budget of 8 lands inside the last character.
    retainer.push("aé字字");

    let result = retainer.finish();

    assert_eq!(result.text, "aé字");
    assert!(std::str::from_utf8(result.text.as_bytes()).is_ok());
}

// ---- the omission record ----------------------------------------------------------------------

#[test]
fn an_omission_count_is_exact() {
    // A caller reports "12 more matches" from this; an approximate count would be a lie in a sentence
    // addressed to a model.
    let mut retainer = ItemRetainer::new(ItemStrategy::Head { max_items: 2 });
    for index in 0..14 {
        retainer.push(index);
    }

    let result = retainer.finish();

    assert_eq!(result.omitted, Omitted::Exact { count: 12 });
    assert_eq!(result.omitted.count(), Some(12));
}

#[test]
fn omitted_none_counts_as_zero() {
    assert_eq!(Omitted::None.count(), Some(0));
    assert!(!Omitted::None.is_some());
}

#[test]
fn omitted_unknown_has_no_count() {
    assert_eq!(Omitted::Unknown.count(), None);
    assert!(Omitted::Unknown.is_some());
}

#[test]
fn truncation_is_not_reported_for_content_that_was_never_offered() {
    // `truncated` means the retainer dropped something, not that the source was incomplete.
    let mut retainer = ItemRetainer::new(ItemStrategy::Head { max_items: 100 });
    retainer.push("only one");

    assert!(!retainer.finish().truncated);
}
