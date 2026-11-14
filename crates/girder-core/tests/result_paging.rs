//! Tests for reading an oversized result back one page at a time (`result_paging`).
//!
//! Three properties carry the weight here, and only the first is about slicing.
//!
//! The *cursor* is the feature: a page that cannot be continued from is just a truncation with extra
//! steps. So the tests walk a whole result one page at a time and check they land on every line
//! exactly once, in order, rather than checking one page in isolation.
//!
//! The *end* is where a paging API silently goes wrong: `next: None` must mean "there is nothing
//! more", never "I could not tell". Both halves are checked — the last page reports `None`, and a
//! page whose window happens to end on the final line also reports `None`.
//!
//! The *range check* is the other silent failure. An empty page looks exactly like the end of the
//! data, so a cursor from a stale handle has to be an error; the tests pin that it is an error and
//! that the error carries the position and the total, since those are what a message needs.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the pager as written even before the crate root wires it in. That is also why nothing here
//! reaches for the rest of `girder_core`: the only dependency is the module itself.

#[path = "../src/result_paging.rs"]
mod result_paging;

use result_paging::{Cursor, CursorError, MAX_PAGE_LINES, Page, Request, page, page_from};

/// A result whose lines name their own index, so a page can be checked without counting.
fn lines(count: usize) -> Vec<String> {
    (0..count).map(|index| format!("line {index}")).collect()
}

/// Walks a whole result from the first page, collecting every line and every cursor.
///
/// Returns the lines in the order they were served, plus the cursors used to continue. Written as a
/// loop over `next` because that is the only client pattern this type exists to support, and a test
/// that used `page` directly would not exercise the cursor at all.
fn walk(lines: &[String], limit: usize) -> (Vec<String>, Vec<Cursor>) {
    let mut collected = Vec::new();
    let mut cursors = Vec::new();

    let mut current = page(lines, &Request { start: 0, limit }).expect("the first page");
    collected.extend(current.lines.iter().cloned());
    while let Some(cursor) = current.next.clone() {
        cursors.push(cursor.clone());
        current =
            page_from(lines, &cursor, limit).expect("a page from a cursor we were just given");
        collected.extend(current.lines.iter().cloned());
    }

    (collected, cursors)
}

// ---- the first page ------------------------------------------------------------------------------

#[test]
fn the_first_page_starts_at_the_first_line() {
    let all = lines(10);
    let first: Page =
        page(&all, &Request { start: 0, limit: 3 }).expect("a page inside the result");

    assert_eq!(first.lines, vec!["line 0", "line 1", "line 2"]);
    assert_eq!(first.total, 10);
    assert!(
        first.next.is_some(),
        "there are seven lines after this page"
    );
}

#[test]
fn a_request_in_the_middle_returns_that_window() {
    let all = lines(10);
    let middle = page(&all, &Request { start: 4, limit: 3 }).expect("a page inside the result");

    assert_eq!(middle.lines, vec!["line 4", "line 5", "line 6"]);
    // The total is the whole result even though this page is small: a caller that printed the page
    // without the total would describe three lines as if they were the result.
    assert_eq!(middle.total, 10);
}

// ---- paging with a cursor: the core -----------------------------------------------------------------

#[test]
fn every_line_is_served_exactly_once_while_paging() {
    let all = lines(25);
    let (collected, cursors) = walk(&all, 4);

    assert_eq!(
        collected, all,
        "the walk must reproduce the result in order"
    );
    // 25 lines at 4 per page is seven pages, so six cursors were needed to reach the seventh.
    assert_eq!(cursors.len(), 6);
}

#[test]
fn paging_from_a_cursor_continues_where_the_last_page_stopped() {
    let all = lines(10);
    let first = page(&all, &Request { start: 0, limit: 4 }).expect("the first page");
    let cursor = first.next.clone().expect("four lines of ten were not all");

    let second = page_from(&all, &cursor, 4).expect("a page from the cursor we were given");
    assert_eq!(second.lines, vec!["line 4", "line 5", "line 6", "line 7"]);
    assert_eq!(second.total, 10);
}

#[test]
fn a_cursor_is_stable_across_a_round_trip_through_its_string_form() {
    // A client stores the cursor, serializes it, sends it back. If the text form did not parse back
    // to the same position, paging would be correct only within one process's lifetime.
    let all = lines(20);
    let first = page(&all, &Request { start: 0, limit: 5 }).expect("the first page");
    let cursor = first
        .next
        .clone()
        .expect("five lines of twenty were not all");

    let reparsed = Cursor::parse(cursor.as_str()).expect("a cursor this module produced");
    assert_eq!(reparsed, cursor);

    let direct = page_from(&all, &cursor, 5).expect("a page from the original cursor");
    let round_tripped = page_from(&all, &reparsed, 5).expect("a page from the reparsed cursor");
    assert_eq!(round_tripped, direct);
    assert_eq!(
        round_tripped.lines,
        vec!["line 5", "line 6", "line 7", "line 8", "line 9"]
    );
}

#[test]
fn each_page_of_a_walk_agrees_on_the_total() {
    // The total is repeated on every page so a reader never has to remember an earlier response.
    let all = lines(9);
    let mut current = page(&all, &Request { start: 0, limit: 2 }).expect("the first page");
    loop {
        assert_eq!(current.total, 9);
        let Some(cursor) = current.next.clone() else {
            break;
        };
        current = page_from(&all, &cursor, 2).expect("a page from a cursor we were just given");
    }
}

// ---- the end of the result -----------------------------------------------------------------------

#[test]
fn the_last_page_reports_no_next_cursor() {
    let all = lines(5);
    // A limit that does not divide the result, so the last page is short rather than exactly full.
    let mut current = page(&all, &Request { start: 0, limit: 2 }).expect("the first page");
    let mut pages = 1;
    while let Some(cursor) = current.next.clone() {
        current = page_from(&all, &cursor, 2).expect("a page from a cursor we were just given");
        pages += 1;
    }

    assert_eq!(pages, 3);
    assert_eq!(current.lines, vec!["line 4"]);
    assert_eq!(current.next, None, "the result is exhausted");
}

#[test]
fn a_page_that_ends_exactly_on_the_last_line_reports_no_next_cursor() {
    // The boundary the off-by-one lives at: ten lines served in two pages of five ends *on* the last
    // line, and a `next` minted there would send the client to a page that cannot exist.
    let all = lines(10);
    let first = page(&all, &Request { start: 0, limit: 5 }).expect("the first page");
    let cursor = first.next.clone().expect("five lines of ten were not all");

    let second = page_from(&all, &cursor, 5).expect("the second page");
    assert_eq!(second.lines.len(), 5);
    assert_eq!(second.next, None);
}

#[test]
fn a_limit_larger_than_the_result_ends_in_one_page() {
    let all = lines(7);
    let only = page(
        &all,
        &Request {
            start: 0,
            limit: 500,
        },
    )
    .expect("a page covering everything");

    assert_eq!(only.lines, all);
    assert_eq!(only.total, 7);
    assert_eq!(only.next, None);
}

// ---- an out-of-range cursor: the other core case ----------------------------------------------------

#[test]
fn a_cursor_past_the_end_is_an_error_and_not_an_empty_page() {
    // A cursor from a larger version of the result. An empty page here would read as "you have seen
    // everything", which is the one wrong answer a reader cannot detect.
    let long = lines(400);
    let stale = page(
        &long,
        &Request {
            start: 0,
            limit: 300,
        },
    )
    .expect("the first page of the long result")
    .next
    .expect("more than 300 lines");

    let short = lines(12);
    let outcome = page_from(&short, &stale, 50);
    assert!(matches!(
        outcome,
        Err(CursorError::OutOfRange { at: 300, total: 12 })
    ));
}

#[test]
fn a_cursor_at_the_last_line_is_out_of_range() {
    // `total` is one past the last index, so a cursor naming it has no line to start from. The cursor
    // comes from a page of a *longer* result, which is how a stale handle is produced in practice.
    let source = lines(20);
    let cursor = page(
        &source,
        &Request {
            start: 0,
            limit: 10,
        },
    )
    .expect("the first page of the longer result")
    .next
    .expect("ten lines of twenty were not all");

    let all = lines(10);
    assert!(matches!(
        page_from(&all, &cursor, 5),
        Err(CursorError::OutOfRange { at: 10, total: 10 })
    ));
}

#[test]
fn an_out_of_range_request_reports_the_position_and_the_total() {
    let all = lines(3);
    let outcome = page(&all, &Request { start: 9, limit: 2 });

    let Err(CursorError::OutOfRange { at, total }) = outcome else {
        panic!("a start past the end must be out of range, got {outcome:?}");
    };
    assert_eq!(at, 9, "the message has to name where the caller asked");
    assert_eq!(total, 3, "and how much there actually is");
}

#[test]
fn a_start_one_past_the_last_line_is_the_empty_tail_not_an_error() {
    // The position after the last line is a legitimate place to be asked about: it is where a client
    // that has counted its way through the result ends up.
    let all = lines(3);
    let tail = page(&all, &Request { start: 3, limit: 2 }).expect("the exhausted position");

    assert!(tail.lines.is_empty());
    assert_eq!(tail.total, 3);
    assert_eq!(tail.next, None);
}

// ---- the limit ceiling --------------------------------------------------------------------------

#[test]
fn a_limit_above_the_ceiling_is_clamped() {
    // A limit of a million would make the response the thing paging exists to prevent.
    let all = lines(MAX_PAGE_LINES + 500);
    let huge = page(
        &all,
        &Request {
            start: 0,
            limit: 1_000_000,
        },
    )
    .expect("a page inside the result");

    assert_eq!(huge.lines.len(), MAX_PAGE_LINES);
    assert!(huge.next.is_some(), "the rest of the result is still there");
}

#[test]
fn a_limit_of_zero_is_raised_to_one_line_rather_than_repeating_a_cursor() {
    // A zero-line page would hand back the cursor it was given; a client looping on `next` would spin
    // without ever producing an error. One line is the smallest page that makes progress.
    let all = lines(3);
    let page = page(&all, &Request { start: 0, limit: 0 }).expect("a page inside the result");

    assert_eq!(page.lines, vec!["line 0"]);
}

#[test]
fn the_ceiling_itself_is_allowed() {
    // The bound is inclusive: a caller that asked for exactly the maximum gets exactly the maximum.
    let all = lines(MAX_PAGE_LINES + 1);
    let page = page(
        &all,
        &Request {
            start: 0,
            limit: MAX_PAGE_LINES,
        },
    )
    .expect("a page inside the result");

    assert_eq!(page.lines.len(), MAX_PAGE_LINES);
}

// ---- empty input, and a result that fits in one page -------------------------------------------------

#[test]
fn an_empty_result_yields_an_empty_first_page_with_no_cursor() {
    let empty: Vec<String> = Vec::new();
    let page = page(
        &empty,
        &Request {
            start: 0,
            limit: 10,
        },
    )
    .expect("the only page of nothing");

    assert!(page.lines.is_empty());
    assert_eq!(page.total, 0);
    assert_eq!(page.next, None);
}

#[test]
fn any_cursor_naming_a_line_of_an_empty_result_is_out_of_range() {
    // There is no line 0 in an empty result, so a cursor pointing at one is stale — even though the
    // page it would produce is indistinguishable from the correct empty page.
    let source = lines(5);
    let cursor = page(&source, &Request { start: 0, limit: 2 })
        .expect("the first page")
        .next
        .expect("two lines of five were not all");

    assert!(matches!(
        page_from(&Vec::new(), &cursor, 10),
        Err(CursorError::OutOfRange { at: 2, total: 0 })
    ));
}

#[test]
fn a_single_line_result_fits_in_one_page() {
    let one = lines(1);
    let page = page(
        &one,
        &Request {
            start: 0,
            limit: 10,
        },
    )
    .expect("the only page");

    assert_eq!(page.lines, vec!["line 0"]);
    assert_eq!(page.total, 1);
    assert_eq!(page.next, None);
}

#[test]
fn a_result_that_fits_in_one_page_never_offers_a_cursor() {
    // The whole point of `next: None`: the client's loop terminates on the first page and does not
    // spend a round trip asking for a page it has already received.
    let all = lines(20);
    let only = page(
        &all,
        &Request {
            start: 0,
            limit: 20,
        },
    )
    .expect("a page covering the result");

    assert_eq!(only.lines.len(), 20);
    assert_eq!(only.next, None);
}

// ---- malformed cursors --------------------------------------------------------------------------

#[test]
fn text_that_is_not_a_cursor_is_malformed() {
    for text in [
        "",
        "nonsense",
        "rp1",
        "rp1.",
        "rp1.10",
        "rp1.10.00",
        "wrong.10.00000000",
    ] {
        let outcome = Cursor::parse(text);
        assert!(
            matches!(outcome, Err(CursorError::Malformed { .. })),
            "`{text}` should not parse, got {outcome:?}"
        );
    }
}

#[test]
fn a_cursor_with_a_bad_checksum_is_malformed() {
    // The checksum is what turns a truncated or mistyped cursor into a refusal rather than a jump to
    // a line the reader never asked for.
    let source = lines(10);
    let cursor = page(&source, &Request { start: 0, limit: 7 })
        .expect("the first page")
        .next
        .expect("seven lines of ten were not all");
    let mut damaged: Vec<char> = cursor.as_str().chars().collect();
    let last = damaged.len() - 1;
    damaged[last] = if damaged[last] == '0' { '1' } else { '0' };
    let text: String = damaged.into_iter().collect();

    assert!(matches!(
        Cursor::parse(&text),
        Err(CursorError::Malformed { .. })
    ));
}

#[test]
fn a_cursor_carries_no_readable_position_for_a_client_to_depend_on() {
    // Opacity is a boundary, not a promise that the text is scrambled: the test pins that a client
    // must go through `parse`, by showing that the string form is not accepted as a bare offset.
    let all = lines(10);
    let cursor = page(&all, &Request { start: 0, limit: 3 })
        .expect("the first page")
        .next
        .expect("three lines of ten were not all");

    assert_ne!(cursor.as_str(), "3");
    assert!(matches!(
        Cursor::parse("3"),
        Err(CursorError::Malformed { .. })
    ));
    assert_eq!(
        Cursor::parse(cursor.as_str()).expect("its own form"),
        cursor
    );
}
