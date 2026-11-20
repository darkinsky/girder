//! Tests for `terminal_controller_gap`: the screen model upstream gets from `@xterm/headless`.
//!
//! The properties pinned here are the ones upstream's `screen.ts`, `terminal.ts` and `agent.ts` rely
//! on, restated as behaviour rather than as recorded emulator output:
//!
//! - **Escape sequences are applied, not printed.** A reader sees what a person sees: `SGR` gone,
//!   carriage returns honoured, cursor motion applied.
//! - **Wrapped rows are one logical line.** A row that was continued joins the next, and trailing
//!   padding never reaches the reader.
//! - **A page is newest-relative and byte-bounded.** `offset` counts back from the newest line, and
//!   the *newest* bytes survive a byte cut.
//! - **A mark follows trimming.** A mark whose row was discarded reports `truncated` instead of
//!   rendering whatever now sits at that index.
//! - **Width is honest.** Wide characters occupy two columns, combining marks none, and a split
//!   UTF-8 sequence is completed by the next chunk rather than corrupted.
//!
//! The module is compiled by path so these tests compile against it before the crate root wires it
//! in. That is why the module reaches crate items through `girder_core::` rather than `crate::`:
//! under `#[path]` inclusion `crate::` would name *this test binary*.

#[path = "../src/terminal/controller.rs"]
pub mod terminal_controller_gap;

use terminal_controller_gap::{
    MAX_COLS, MAX_ROWS, MAX_SCROLLBACK, MarkedLines, Screen, ScreenError, ScreenPage,
    ScreenRequest, char_width, page_lines,
};

/// A screen whose size the test does not care about beyond having room.
fn screen(cols: usize, rows: usize) -> Screen {
    Screen::new(cols, rows, 100).expect("a usable screen")
}

/// A screen with an explicit scrollback bound.
fn scrolling(cols: usize, rows: usize, scrollback: usize) -> Screen {
    Screen::new(cols, rows, scrollback).expect("a usable screen")
}

fn read(screen: &Screen, offset: usize, count: usize) -> ScreenPage {
    screen.read_screen(&ScreenRequest::new(offset, count, 64 * 1024))
}

// ---- construction -----------------------------------------------------------------------------

#[test]
fn a_zero_dimension_is_refused_rather_than_clamped() {
    // A one-column-wide screen would render every write as an overwrite, which looks like corrupted
    // output instead of a bad request.
    assert_eq!(
        Screen::new(0, 24, 0).err(),
        Some(ScreenError::Size { cols: 0, rows: 24 })
    );
    assert_eq!(
        Screen::new(80, 0, 0).err(),
        Some(ScreenError::Size { cols: 80, rows: 0 })
    );
    assert_eq!(
        Screen::new(MAX_COLS + 1, 24, 0).err(),
        Some(ScreenError::Size {
            cols: MAX_COLS + 1,
            rows: 24
        })
    );
    assert_eq!(
        Screen::new(80, MAX_ROWS + 1, 0).err(),
        Some(ScreenError::Size {
            cols: 80,
            rows: MAX_ROWS + 1
        })
    );
    assert_eq!(
        Screen::new(80, 24, MAX_SCROLLBACK + 1).err(),
        Some(ScreenError::Scrollback {
            scrollback: MAX_SCROLLBACK + 1,
        })
    );
    assert!(Screen::new(MAX_COLS, MAX_ROWS, MAX_SCROLLBACK).is_ok());
}

#[test]
fn both_refusals_say_what_the_bounds_are() {
    let size = ScreenError::Size { cols: 0, rows: 24 }.to_string();
    assert!(size.contains('0'), "{size}");
    assert!(size.contains("at least 1"), "{size}");
    let scrollback = ScreenError::Scrollback { scrollback: 5 }.to_string();
    assert!(scrollback.contains('5'), "{scrollback}");
}

#[test]
fn a_fresh_screen_renders_one_empty_line_not_a_screenful_of_blanks() {
    // Upstream's `lastContentRow` never goes below the cursor row, so a screen that has printed
    // nothing renders as one empty line rather than as `rows` blank ones.
    let screen = screen(80, 24);
    assert_eq!(screen.render_all(), vec![String::new()]);
    assert_eq!(screen.total_lines(), 1);
    assert_eq!(screen.last_content_row(), 0);
    let page = read(&screen, 0, 24);
    assert_eq!(page.text, "");
    assert_eq!(page.total_lines, 1);
}

// ---- printing and control sequences -----------------------------------------------------------

#[test]
fn plain_text_prints_and_newlines_make_rows() {
    let mut screen = screen(20, 4);
    screen.feed(b"hello\r\nworld");
    assert_eq!(read(&screen, 0, 10).text, "hello\nworld");
    assert_eq!(screen.cursor(), (1, 5));
}

#[test]
fn a_bare_line_feed_keeps_the_column() {
    // The tty layer turns `ONLCR` into `\r\n`; a program that writes `\n` alone moves down only.
    let mut screen = screen(20, 4);
    screen.feed(b"ab\ncd");
    assert_eq!(read(&screen, 0, 10).text, "ab\n  cd");
}

#[test]
fn sgr_never_reaches_the_reader() {
    let mut screen = screen(40, 4);
    screen.feed(b"\x1b[1;32m$ \x1b[0mls -la\x1b[m");
    assert_eq!(read(&screen, 0, 10).text, "$ ls -la");
}

#[test]
fn a_carriage_return_overwrites_in_place() {
    let mut screen = screen(10, 3);
    screen.feed(b"abcdef\rXY");
    assert_eq!(read(&screen, 0, 5).text, "XYcdef");
}

#[test]
fn a_progress_bar_redraws_one_line_instead_of_growing() {
    let mut screen = screen(10, 3);
    for step in 0..4 {
        screen.feed(b"\r");
        screen.feed(format!("{step}0%").as_bytes());
    }
    let page = read(&screen, 0, 5);
    assert_eq!(page.text, "30%");
    assert_eq!(page.total_lines, 1);
}

#[test]
fn cursor_positioning_paints_a_grid() {
    let mut screen = screen(10, 3);
    screen.feed(b"\x1b[2;3Hmiddle");
    assert_eq!(screen.cursor(), (1, 8));
    assert_eq!(read(&screen, 0, 5).text, "\n  middle");
}

#[test]
fn horizontal_and_vertical_absolute_positioning_use_one_based_parameters() {
    let mut screen = screen(10, 3);
    screen.feed(b"\x1b[3d\x1b[2Gx");
    assert_eq!(screen.cursor(), (2, 2));
    assert_eq!(read(&screen, 0, 5).text, "\n\n x");
}

#[test]
fn erase_line_tails_what_follows() {
    let mut screen = screen(10, 3);
    screen.feed(b"abcdef\x1b[3D\x1b[K");
    assert_eq!(read(&screen, 0, 5).text, "abc");
}

#[test]
fn erase_line_from_the_start_blanks_the_head_only() {
    let mut screen = screen(10, 3);
    screen.feed(b"abcdef\x1b[4D\x1b[1K");
    assert_eq!(read(&screen, 0, 5).text, "   def");
}

#[test]
fn erase_display_clears_below_the_cursor() {
    let mut screen = screen(10, 4);
    screen.feed(b"one\r\ntwo\r\nthree\x1b[2;1H\x1b[J");
    // "two" and "three" are erased from the cursor down; the cursor stays on the now-blank row.
    let page = read(&screen, 0, 5);
    assert_eq!(page.text, "one\n");
    assert_eq!(page.total_lines, 2);
}

#[test]
fn erase_display_clears_the_whole_grid() {
    let mut screen = screen(10, 4);
    screen.feed(b"one\r\ntwo\x1b[H\x1b[2J");
    assert_eq!(read(&screen, 0, 5).text, "");
    assert_eq!(screen.total_lines(), 1);
}

#[test]
fn insert_and_delete_characters_shift_the_row() {
    let mut screen = screen(10, 3);
    screen.feed(b"abc\x1b[1;1H\x1b[2@");
    assert_eq!(read(&screen, 0, 5).text, "  abc");
    screen.feed(b"\x1b[1;1H\x1b[2P");
    assert_eq!(read(&screen, 0, 5).text, "abc");
}

#[test]
fn insert_and_delete_lines_shift_the_grid() {
    let mut screen = screen(10, 4);
    screen.feed(b"one\r\ntwo\r\nthree");
    screen.feed(b"\x1b[2;1H\x1b[L");
    assert_eq!(
        read(&screen, 0, 5).text,
        "one\n\n>two\nthree".replace('>', "")
    );
    screen.feed(b"\x1b[2;1H\x1b[M");
    assert_eq!(read(&screen, 0, 5).text, "one\ntwo\nthree");
}

#[test]
fn scrolling_pushes_the_top_row_into_scrollback() {
    let mut screen = scrolling(20, 2, 10);
    screen.feed(b"one\r\ntwo\r\nthree");
    // The grid is two rows, so "one" has scrolled off into history.
    let page = read(&screen, 0, 10);
    assert_eq!(page.text, "one\ntwo\nthree");
    assert_eq!(page.total_lines, 3);
    assert_eq!(screen.dropped_rows(), 0);
}

#[test]
fn scrollback_is_bounded_and_counts_what_it_dropped() {
    let mut screen = scrolling(20, 2, 2);
    for index in 0..6 {
        screen.feed(format!("line{index}\r\n").as_bytes());
    }
    // Ten feeds of two rows each scroll five rows off the top; the two-row scrollback keeps the last
    // two of those and counts the other three as dropped.
    assert_eq!(screen.dropped_rows(), 3);
    let page = read(&screen, 0, 100);
    assert_eq!(page.text, "line3\nline4\nline5\n");
    // Absolute indices still count the dropped rows, so a caller holding one is clamped, not lost.
    assert_eq!(screen.total_rows(), 3 + 2 + 2);
}

#[test]
fn a_zero_scrollback_screen_drops_instead_of_history() {
    let mut screen = scrolling(20, 2, 0);
    screen.feed(b"one\r\ntwo\r\nthree");
    assert_eq!(read(&screen, 0, 10).text, "two\nthree");
    assert_eq!(screen.dropped_rows(), 1);
}

#[test]
fn save_and_restore_cursor_round_trips() {
    let mut screen = screen(10, 3);
    screen.feed(b"\x1b[2;2H\x1b7\x1b[3;5H\x1b8x");
    assert_eq!(screen.cursor(), (1, 2));
    assert_eq!(read(&screen, 0, 5).text, "\n x");
}

#[test]
fn the_cursor_can_be_hidden_and_shown() {
    let mut screen = screen(10, 3);
    assert!(screen.cursor_visible());
    screen.feed(b"\x1b[?25l");
    assert!(!screen.cursor_visible());
    screen.feed(b"\x1b[?25h");
    assert!(screen.cursor_visible());
}

#[test]
fn margin_reverse_index_scrolls_down_inside_the_region() {
    let mut screen = screen(10, 3);
    screen.feed(b"one\r\ntwo\r\nthree\x1b[H\x1bM");
    // Reverse index at the top margin inserts a blank row there: the content moves down and the row
    // pushed off the bottom of the region is gone.
    assert_eq!(read(&screen, 0, 10).text, "\none\ntwo");
}

#[test]
fn a_scroll_region_confines_line_feeds() {
    let mut screen = screen(10, 5);
    screen.feed(b"top\r\nA\r\nB\r\nC\r\nbottom");
    // Restrict scrolling to rows 2..4 (0-based 1..3), then feed at the bottom of that region.
    screen.feed(b"\x1b[2;4r\x1b[4;1H\r\nshifted");
    let page = read(&screen, 0, 10);
    // Only rows 2..4 scroll, so the row above the region and the row below it stay put while the
    // region's content moves up by one.
    assert_eq!(page.text, "top\nB\nC\nshifted\nbottom");
}

#[test]
fn reset_clears_everything_the_screen_remembers() {
    let mut screen = screen(10, 4);
    screen.feed(b"one\r\ntwo\x1b[?25l");
    screen.feed(b"\x1bc");
    assert_eq!(read(&screen, 0, 5).text, "");
    assert!(screen.cursor_visible());
    assert_eq!(screen.dropped_rows(), 0);
}

#[test]
fn unknown_and_ignored_sequences_are_consumed_instead_of_printed() {
    let mut screen = screen(40, 4);
    // Bracketed paste, keypad mode, focus reporting, cursor style, window title, charset, and a
    // device-control payload all have to disappear without eating the text around them.
    screen.feed(b"\x1b[?2004h\x1b=\x1b[?1004h\x1b[2 q\x1b]0;title\x07\x1b(B\x1bP1$r\x1b\\ok");
    assert_eq!(read(&screen, 0, 5).text, "ok");
}

#[test]
fn an_osc_payload_terminated_by_st_is_consumed() {
    let mut screen = screen(40, 4);
    screen.feed(b"\x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\");
    assert_eq!(read(&screen, 0, 5).text, "link");
}

#[test]
fn c1_control_forms_are_accepted_like_their_escape_forms() {
    let mut screen = screen(10, 3);
    // C1 CSI (0x9b) then a C1 string introducer (0x90) ended by C1 ST (0x9c).
    screen.feed_str("\u{9b}2;1Hone\u{90}x\u{9c}two");
    // The string payload up to and including its ST terminator is consumed whole, so "two" follows
    // "one" with no separator rather than starting a new column for the terminator.
    assert_eq!(read(&screen, 0, 5).text, "\nonetwo");
}

#[test]
fn a_sequence_split_across_two_feeds_is_still_applied() {
    let mut screen = screen(10, 3);
    screen.feed(b"\x1b[");
    screen.feed(b"2;1H");
    screen.feed(b"late");
    assert_eq!(read(&screen, 0, 5).text, "\nlate");
}

#[test]
fn the_device_status_report_gives_the_cursor_position_one_based() {
    let mut screen = screen(10, 5);
    screen.feed(b"\x1b[3;4H\x1b[6n");
    assert_eq!(screen.take_responses(), vec!["\u{1b}[3;4R".to_string()]);
    // Taking the replies empties them, so a caller that polls never sends a reply twice.
    assert!(screen.take_responses().is_empty());
}

#[test]
fn a_device_attributes_query_is_answered() {
    let mut screen = screen(10, 5);
    screen.feed(b"\x1b[c");
    assert_eq!(screen.take_responses(), vec!["\u{1b}[?1;2c".to_string()]);
}

#[test]
fn a_private_device_status_query_gets_the_private_reply_form() {
    let mut screen = screen(10, 5);
    screen.feed(b"\x1b[5n\x1b[?6n");
    assert_eq!(
        screen.take_responses(),
        vec!["\u{1b}[0n".to_string(), "\u{1b}[?1;1R".to_string()]
    );
}

#[test]
fn repeat_uses_the_last_printed_character() {
    let mut screen = screen(10, 3);
    screen.feed(b"a\x1b[4b");
    assert_eq!(read(&screen, 0, 5).text, "aaaaa");
}

#[test]
fn tabs_advance_to_the_next_stop_and_back_tabs_return() {
    let mut screen = screen(30, 3);
    screen.feed(b"a\tb");
    assert_eq!(read(&screen, 0, 5).text, format!("a{}b", " ".repeat(7)));
    screen.feed(b"\x1b[Z");
    assert_eq!(screen.cursor(), (0, 8));
}

#[test]
fn a_backspace_moves_left_without_erasing() {
    let mut screen = screen(10, 3);
    screen.feed(b"abc\x08\x08X");
    assert_eq!(read(&screen, 0, 5).text, "aXc");
}

// ---- wrapping ---------------------------------------------------------------------------------

#[test]
fn a_row_that_fills_the_width_wraps_and_joins_into_one_logical_line() {
    let mut screen = screen(5, 4);
    screen.feed(b"abcdefgh");
    // Eight characters in five columns occupy two rows, and a reader sees one line.
    let page = read(&screen, 0, 10);
    assert_eq!(page.text, "abcdefgh");
    assert_eq!(page.total_lines, 1);
}

#[test]
fn a_wrapped_line_and_a_hard_newline_read_differently() {
    let mut wrapped = screen(5, 4);
    wrapped.feed(b"abcdefgh");
    let mut broken = screen(5, 4);
    broken.feed(b"abcde\r\nfgh");
    assert_eq!(read(&wrapped, 0, 10).text, "abcdefgh");
    assert_eq!(read(&broken, 0, 10).text, "abcde\nfgh");
}

#[test]
fn wrapping_moves_the_cursor_to_the_next_row() {
    let mut screen = screen(5, 4);
    screen.feed(b"abcdeX");
    assert_eq!(screen.cursor(), (1, 1));
}

#[test]
fn auto_wrap_can_be_disabled_for_the_last_column() {
    let mut screen = screen(5, 4);
    screen.feed(b"\x1b[?7l");
    screen.feed(b"abcdefgh");
    // With wrapping off every character past the last column lands on that column.
    assert_eq!(read(&screen, 0, 10).text, "abcdh");
    assert_eq!(screen.total_lines(), 1);
}

#[test]
fn a_carriage_return_at_the_right_edge_stops_the_pending_wrap() {
    let mut screen = screen(5, 4);
    screen.feed(b"abcde\rX");
    assert_eq!(read(&screen, 0, 10).text, "Xbcde");
}

// ---- width ------------------------------------------------------------------------------------

#[test]
fn a_wide_character_occupies_two_columns() {
    let mut screen = screen(10, 3);
    screen.feed("你好".as_bytes());
    assert_eq!(screen.cursor(), (0, 4));
    assert_eq!(read(&screen, 0, 5).text, "你好");
}

#[test]
fn a_wide_character_does_not_straddle_the_last_column() {
    let mut screen = screen(3, 3);
    screen.feed("ab你".as_bytes());
    // Two columns are taken, so the wide character goes to the next row instead of being cut.
    assert_eq!(read(&screen, 0, 5).text, "ab\n你");
}

#[test]
fn a_combining_mark_attaches_to_the_character_before_it() {
    let mut screen = screen(10, 3);
    screen.feed("e\u{301}".as_bytes());
    assert_eq!(screen.cursor(), (0, 1));
    assert_eq!(read(&screen, 0, 5).text, "e\u{301}");
}

#[test]
fn a_combining_mark_with_nothing_before_it_writes_nothing() {
    let mut screen = screen(10, 3);
    screen.feed("\u{301}ab".as_bytes());
    assert_eq!(read(&screen, 0, 5).text, "ab");
}

#[test]
fn character_widths_cover_the_cases_alignment_depends_on() {
    assert_eq!(char_width('a'), 1);
    assert_eq!(char_width('你'), 2);
    assert_eq!(char_width('\u{301}'), 0);
    assert_eq!(char_width('\u{fe0f}'), 0);
    assert_eq!(char_width('ア'), 2);
}

#[test]
fn overwriting_half_of_a_wide_character_clears_the_other_half() {
    let mut screen = screen(10, 3);
    screen.feed("你x".as_bytes());
    // The cursor is after `x`; return to the wide character's first column and overwrite it.
    screen.feed(b"\rZ");
    assert_eq!(read(&screen, 0, 5).text, "Z x");
}

#[test]
fn a_split_utf8_sequence_is_completed_by_the_next_feed() {
    let mut screen = screen(10, 3);
    let bytes = "héllo".as_bytes();
    for chunk in bytes.chunks(2) {
        screen.feed(chunk);
    }
    assert_eq!(read(&screen, 0, 5).text, "héllo");
}

#[test]
fn invalid_utf8_becomes_the_replacement_character() {
    let mut screen = screen(10, 3);
    screen.feed(b"a\xffb");
    assert_eq!(read(&screen, 0, 5).text, "a\u{fffd}b");
}

// ---- paging ----------------------------------------------------------------------------------

#[test]
fn a_page_is_newest_relative() {
    let lines: Vec<String> = (0..10).map(|index| format!("line{index}")).collect();
    let all = page_lines(&lines, &ScreenRequest::new(0, 100, 4096));
    assert_eq!(all.total_lines, 10);
    assert_eq!(all.text.split('\n').count(), 10);
    let newest_two = page_lines(&lines, &ScreenRequest::new(0, 2, 4096));
    assert_eq!(newest_two.text, "line8\nline9");
    assert_eq!(newest_two.line_begin, 0);
    assert_eq!(newest_two.line_end, 2);
    let back_two = page_lines(&lines, &ScreenRequest::new(4, 2, 4096));
    assert_eq!(back_two.text, "line4\nline5");
    assert_eq!(back_two.line_begin, 4);
}

#[test]
fn an_offset_past_the_oldest_line_returns_nothing() {
    let lines: Vec<String> = (0..3).map(|index| format!("line{index}")).collect();
    let page = page_lines(&lines, &ScreenRequest::new(10, 5, 4096));
    assert_eq!(page.text, "");
    assert_eq!(page.total_lines, 3);
    assert!(!page.truncated);
}

#[test]
fn a_byte_bound_drops_the_oldest_lines_and_keeps_the_newest() {
    let lines = vec!["aaaa".to_string(), "bbbb".to_string(), "cccc".to_string()];
    // Nine bytes: two four-byte lines plus the separator, so the oldest line cannot fit.
    let page = page_lines(&lines, &ScreenRequest::new(0, 10, 9));
    assert_eq!(page.text, "bbbb\ncccc");
    assert!(page.truncated);
}

#[test]
fn a_byte_bound_that_fits_everything_is_not_truncated() {
    let lines = vec!["aa".to_string(), "bb".to_string()];
    let page = page_lines(&lines, &ScreenRequest::new(0, 10, 5));
    assert_eq!(page.text, "aa\nbb");
    assert!(!page.truncated);
}

#[test]
fn a_bound_too_small_for_one_line_cuts_the_newest_line_tail() {
    let lines = vec!["abcdefgh".to_string()];
    let page = page_lines(&lines, &ScreenRequest::new(0, 10, 3));
    assert_eq!(page.text, "fgh");
    assert!(page.truncated);
}

#[test]
fn a_cut_line_tail_never_splits_a_character() {
    let lines = vec!["你好世界".to_string()];
    // Five bytes fit two whole three-byte characters; a naive cut would leave half of one.
    let page = page_lines(&lines, &ScreenRequest::new(0, 10, 7));
    assert_eq!(page.text, "世界");
    assert!(page.truncated);
}

#[test]
fn a_page_of_a_live_screen_counts_logical_lines() {
    let mut screen = screen(5, 4);
    screen.feed(b"abcdefgh\r\nsecond");
    let page = read(&screen, 0, 10);
    assert_eq!(page.text, "abcdefgh\nsecond");
    assert_eq!(page.total_lines, 2);
    let newest = read(&screen, 0, 1);
    assert_eq!(newest.text, "second");
}

// ---- marks -----------------------------------------------------------------------------------

#[test]
fn a_mark_renders_output_written_after_it() {
    let mut screen = screen(20, 5);
    screen.feed(b"first\r\n");
    let mark = screen.mark();
    screen.feed(b"second\r\nthird");
    let lines = mark.lines(&screen);
    assert!(!lines.truncated);
    assert_eq!(lines.lines, vec!["second", "third"]);
}

#[test]
fn a_mark_taken_mid_line_starts_at_that_logical_line() {
    let mut screen = screen(20, 5);
    screen.feed(b"keep this\r\nsecond");
    // The cursor sits at the end of "second", so the mark is that whole line.
    let mark = screen.mark();
    screen.feed(b" more");
    assert_eq!(mark.lines(&screen).lines, vec!["second more"]);
}

#[test]
fn a_mark_reports_that_trimming_discarded_its_row() {
    let mut screen = scrolling(20, 2, 1);
    screen.feed(b"one\r\n");
    let mark = screen.mark();
    for index in 0..6 {
        screen.feed(format!("line{index}\r\n").as_bytes());
    }
    let lines = mark.lines(&screen);
    assert!(lines.truncated, "the marked row was trimmed");
    // Rendering falls back to the oldest retained row instead of the wrong row at that index.
    assert_eq!(lines.lines.first().map(String::as_str), Some("line4"));
}

#[test]
fn a_mark_survives_a_switch_to_the_alternate_screen_by_starting_over() {
    let mut screen = screen(20, 5);
    screen.feed(b"main text\r\n");
    let mark = screen.mark();
    screen.feed(b"\x1b[?1049h");
    screen.feed(b"full screen");
    let lines = mark.lines(&screen);
    // The mark names a row in a buffer that is no longer active; upstream renders from the oldest
    // row of the active buffer rather than reading the wrong row.
    assert!(!lines.truncated);
    assert_eq!(lines.lines, vec!["full screen"]);
}

#[test]
fn a_mark_exposes_the_row_it_was_taken_at() {
    let mut screen = screen(20, 5);
    screen.feed(b"one\r\ntwo");
    assert_eq!(screen.mark().row(), 1);
}

#[test]
fn the_cursor_line_start_walks_back_through_wrapped_rows() {
    let mut screen = screen(5, 6);
    screen.feed(b"abcdefgh");
    // The cursor wrapped onto the second row of one logical line.
    assert_eq!(screen.cursor_line_start(), 0);
}

#[test]
fn last_content_row_ignores_blank_rows_below_the_cursor() {
    let mut screen = screen(20, 5);
    screen.feed(b"one");
    assert_eq!(screen.last_content_row(), 0);
    screen.feed(b"\r\ntwo");
    assert_eq!(screen.last_content_row(), 1);
}

// ---- alternate screen and resize --------------------------------------------------------------

#[test]
fn the_alternate_screen_hides_the_main_buffer_and_restores_it() {
    let mut screen = screen(20, 5);
    screen.feed(b"main\r\n");
    screen.feed(b"\x1b[?1049h");
    assert!(screen.alternate_screen());
    screen.feed(b"full");
    assert_eq!(read(&screen, 0, 10).text, "full");
    screen.feed(b"\x1b[?1049l");
    assert!(!screen.alternate_screen());
    // The main buffer comes back with its cursor row, so the blank row it was on is still there.
    assert_eq!(read(&screen, 0, 10).text, "main\n");
}

#[test]
fn the_alternate_screen_keeps_no_scrollback() {
    let mut screen = scrolling(10, 2, 10);
    screen.feed(b"main\r\n");
    screen.feed(b"\x1b[?1049h");
    for index in 0..5 {
        screen.feed(format!("row{index}\r\n").as_bytes());
    }
    // A full-screen program's repaint is not history: leaving the buffer discards it entirely.
    screen.feed(b"\x1b[?1049l");
    assert_eq!(read(&screen, 0, 50).text, "main\n");
    assert!(!read(&screen, 0, 50).text.contains("row"));
}

#[test]
fn growing_the_grid_adds_blank_rows_without_losing_text() {
    let mut screen = screen(10, 2);
    screen.feed(b"one\r\ntwo");
    screen.resize(10, 4).expect("a usable size");
    assert_eq!(screen.rows(), 4);
    assert_eq!(read(&screen, 0, 10).text, "one\ntwo");
}

#[test]
fn shrinking_the_grid_moves_cut_rows_into_scrollback() {
    let mut screen = screen(10, 4);
    screen.feed(b"one\r\ntwo\r\nthree");
    screen.resize(10, 2).expect("a usable size");
    // The two rows cut off the top become history, and the cursor follows the grid's new bottom row,
    // which is why the render ends with a blank line.
    assert_eq!(screen.dropped_rows(), 0);
    assert_eq!(read(&screen, 0, 10).text, "one\ntwo\nthree\n");
    assert_eq!(screen.rows(), 2);
}

#[test]
fn growing_the_width_keeps_the_text_and_the_cursor_inside_the_grid() {
    let mut screen = screen(4, 3);
    screen.feed(b"abcd");
    screen.resize(8, 3).expect("a usable size");
    assert_eq!(screen.cols(), 8);
    let (row, col) = screen.cursor();
    assert!(col < 8);
    assert_eq!(read(&screen, 0, 5).text, "abcd");
    screen.feed(b"ef");
    assert_eq!(read(&screen, 0, 5).text, "abcdef");
    let (_, after) = screen.cursor();
    assert_eq!((row, after), (0, 6));
}

#[test]
fn shrinking_the_width_clamps_the_cursor_into_the_grid() {
    let mut screen = screen(10, 3);
    screen.feed(b"abcdefgh");
    screen.resize(3, 3).expect("a usable size");
    let (_, col) = screen.cursor();
    assert!(col < 3);
    assert_eq!(read(&screen, 0, 5).text, "abc");
}

#[test]
fn a_resize_to_an_unusable_size_is_refused_and_changes_nothing() {
    let mut screen = screen(10, 3);
    screen.feed(b"keep");
    assert_eq!(
        screen.resize(0, 3).err(),
        Some(ScreenError::Size { cols: 0, rows: 3 })
    );
    assert_eq!(screen.cols(), 10);
    assert_eq!(read(&screen, 0, 5).text, "keep");
}

#[test]
fn an_empty_mark_renders_the_one_empty_line_the_screen_has() {
    let screen = screen(10, 3);
    let mark = screen.mark();
    // The same rule as `lastContentRow`: an untouched screen has one blank line, not none.
    assert_eq!(
        mark.lines(&screen),
        MarkedLines {
            lines: vec![String::new()],
            truncated: false,
        }
    );
}

#[test]
fn a_huge_count_parameter_cannot_allocate_or_hang() {
    let mut screen = screen(10, 3);
    screen.feed(b"x");
    screen.feed(b"\x1b[999999999C");
    screen.feed(b"\x1b[999999999B");
    screen.feed(b"\x1b[999999999L");
    screen.feed(b"\x1b[999999999P");
    // The count is clamped, so the cursor stays inside the grid and the screen still renders.
    assert_eq!(screen.cursor(), (2, 9));
    assert!(read(&screen, 0, 5).total_lines <= 3);
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod terminal {
    pub use super::terminal_controller_gap as controller;
}
