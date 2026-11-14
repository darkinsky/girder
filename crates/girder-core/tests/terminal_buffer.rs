//! Tests for the bounded terminal output buffer.
//!
//! The properties worth pinning are the ones a client depends on when it reconnects: an offset it saved
//! still means the same bytes, a lost offset is reported rather than disguised as silence, and the tail of
//! the output survives the bounds that keep the buffer small.
//!
//! The module is compiled in by path rather than reached through the crate root: the buffer is wired into
//! the crate's public surface elsewhere, and a test that waited for that wiring could not be run against the
//! file as it is written. Compiling the file directly also means this suite fails if the module stops being
//! self-contained.

#[path = "../src/terminal_buffer.rs"]
mod terminal_buffer;

use terminal_buffer::Buffer;

/// Reads a whole offset, asserting the next position is the end of the stream.
fn read_to_end(buffer: &Buffer, from: u64) -> String {
    let chunk = buffer.read(from);
    assert_eq!(chunk.next, buffer.total_bytes());
    chunk.text
}

// ---- reading -----------------------------------------------------------------------------------

#[test]
fn an_empty_buffer_reports_nothing_rather_than_a_loss() {
    // The first poll of a client that just connected lands here, so it must be an ordinary answer: no text,
    // no truncation, and an offset that is valid to start from.
    let buffer = Buffer::new(1024, 4);

    assert_eq!(buffer.total_bytes(), 0);
    assert_eq!(buffer.held_bytes(), 0);
    assert_eq!(buffer.first_readable(), 0);

    let chunk = buffer.read(0);
    assert_eq!(chunk.text, "");
    assert_eq!(chunk.next, 0);
    assert!(!chunk.truncated);
}

#[test]
fn reading_returns_what_was_appended() {
    let mut buffer = Buffer::new(1024, 4);
    buffer.append("hello ");

    assert_eq!(read_to_end(&buffer, 0), "hello ");
    assert_eq!(buffer.total_bytes(), 6);
    assert_eq!(buffer.held_bytes(), 6);
}

#[test]
fn a_read_resumes_where_the_previous_one_stopped() {
    // A client reads, then only asks for what came after: `next` is the position it saves between polls.
    let mut buffer = Buffer::new(1024, 4);
    buffer.append("first\n");

    let chunk = buffer.read(0);
    assert_eq!(chunk.text, "first\n");
    assert_eq!(chunk.next, 6);
    assert!(!chunk.truncated);

    buffer.append("second\n");
    let chunk = buffer.read(chunk.next);
    assert_eq!(chunk.text, "second\n");
    assert_eq!(chunk.next, 13);
    assert!(!chunk.truncated);
}

#[test]
fn polling_past_the_end_is_not_a_truncation() {
    // A poll often asks for the newest offset before anything new arrived. That is silence, not a loss.
    let mut buffer = Buffer::new(1024, 4);
    buffer.append("one");

    let chunk = buffer.read(3);
    assert_eq!(chunk.text, "");
    assert_eq!(chunk.next, 3);
    assert!(!chunk.truncated);

    let chunk = buffer.read(99);
    assert_eq!(chunk.text, "");
    assert!(!chunk.truncated);
}

#[test]
fn an_offset_inside_a_character_starts_at_the_next_one() {
    // Offsets come from clients and can be anything; a read that decoded half a character would hand back a
    // replacement character that was never printed.
    let mut buffer = Buffer::new(1024, 4);
    buffer.append("a\u{e9}\u{65e5}");

    assert_eq!(buffer.total_bytes(), 6);
    assert_eq!(read_to_end(&buffer, 2), "\u{65e5}");
    assert_eq!(read_to_end(&buffer, 1), "\u{e9}\u{65e5}");
}

// ---- truncation --------------------------------------------------------------------------------

#[test]
fn an_offset_that_was_dropped_reports_truncation_with_the_readable_start() {
    // The core property. An empty answer here would tell a client "nothing new" while its next bytes are
    // already gone, and it would wait forever for output that will never arrive.
    let mut buffer = Buffer::new(4, 0);
    buffer.append("abcdefgh");

    assert_eq!(buffer.total_bytes(), 8);
    assert_eq!(buffer.first_readable(), 4);
    assert_eq!(buffer.held_bytes(), 4);

    let chunk = buffer.read(0);
    assert!(chunk.truncated, "a dropped offset must be reported");
    assert_eq!(chunk.text, "efgh");
    assert_eq!(chunk.next, 8);

    // Once resynchronised, the same client reads without further truncation.
    let chunk = buffer.read(buffer.first_readable());
    assert!(!chunk.truncated);
    assert_eq!(chunk.text, "efgh");
}

#[test]
fn the_byte_ceiling_holds_when_no_lines_are_protected() {
    let mut buffer = Buffer::new(8, 0);
    for index in 0..100 {
        buffer.append(&format!("{index}\n"));
    }

    assert!(buffer.held_bytes() <= 8, "held {}", buffer.held_bytes());
    assert!(buffer.first_readable() > 0);
    // Nothing readable was invented: the retained text is a suffix of what was appended.
    let retained = read_to_end(&buffer, buffer.first_readable());
    assert!(retained.ends_with("99\n"));
}

#[test]
fn total_bytes_keeps_growing_after_content_is_dropped() {
    // Truncation must not rewind the clock: total_bytes is how a client decides whether to ask again.
    let mut buffer = Buffer::new(4, 0);
    let mut appended = 0_u64;

    for index in 0..50 {
        let text = format!("line-{index}\n");
        appended += u64::try_from(text.len()).expect("small");
        buffer.append(&text);
        assert_eq!(buffer.total_bytes(), appended);
    }

    assert!(buffer.first_readable() > 0, "the head must have been dropped");
    assert_eq!(buffer.total_bytes(), appended);
    assert!(buffer.held_bytes() < usize::try_from(appended).expect("small"));
}

// ---- the line floor ----------------------------------------------------------------------------

#[test]
fn the_last_lines_survive_a_byte_ceiling_that_would_cut_them() {
    // Without the floor the ceiling would keep an arbitrary number of bytes and start mid-line; with it the
    // three most recent lines stay whole even though they exceed the ceiling.
    let mut buffer = Buffer::new(8, 3);
    buffer.append("aaa\nbbb\nccc\nddd\n");

    assert_eq!(buffer.total_bytes(), 16);
    assert_eq!(buffer.first_readable(), 4);
    assert!(buffer.held_bytes() > 8, "the floor outranks the ceiling");

    let chunk = buffer.read(0);
    assert!(chunk.truncated);
    assert_eq!(chunk.text, "bbb\nccc\nddd\n");
}

#[test]
fn a_single_long_line_cannot_pin_the_history() {
    // A progress bar redrawn on one line can be megabytes long. Protecting it whole would let one writer
    // decide how much memory the terminal holds, so it is protected only for its own tail.
    let mut buffer = Buffer::new(16, 2);
    buffer.append(&"x".repeat(100));

    assert_eq!(buffer.total_bytes(), 100);
    assert!(buffer.held_bytes() <= 16, "held {}", buffer.held_bytes());
    assert_eq!(buffer.first_readable(), 84);

    let chunk = buffer.read(0);
    assert!(chunk.truncated);
    assert_eq!(chunk.text, "x".repeat(16));
}

#[test]
fn a_zero_capacity_still_retains_the_newest_byte() {
    // A buffer holding nothing could not answer any read truthfully, so zero is treated as one byte.
    let mut buffer = Buffer::new(0, 0);
    buffer.append("abcdef");

    assert!(buffer.held_bytes() <= 1, "held {}", buffer.held_bytes());
    assert_eq!(buffer.first_readable(), 5);
    assert_eq!(read_to_end(&buffer, buffer.first_readable()), "f");
}

// ---- character integrity -----------------------------------------------------------------------

#[test]
fn truncation_never_cuts_a_multi_byte_character() {
    // The ceiling falls in the middle of `é`; the cut moves forward instead of splitting it.
    let mut buffer = Buffer::new(4, 0);
    buffer.append("a\u{e9}\u{65e5}");

    assert_eq!(buffer.total_bytes(), 6);
    assert!(buffer.first_readable() >= 3, "the cut landed inside a character");

    let chunk = buffer.read(0);
    assert!(chunk.truncated);
    assert_eq!(chunk.text, "\u{65e5}");
    assert!(!chunk.text.contains('\u{fffd}'), "a lossy decode was returned");
}

#[test]
fn truncation_keeps_whole_characters_across_many_appends() {
    // Each append is its own entry, so a cut can land inside the first entry and later ones must still line
    // up: the retained text is every character still inside the window, in order.
    let mut buffer = Buffer::new(7, 0);
    for character in ["\u{4f60}", "\u{597d}", "\u{4e16}\u{754c}", "\n", "\u{1f600}"] {
        buffer.append(character);
    }

    let everything = "\u{4f60}\u{597d}\u{4e16}\u{754c}\n\u{1f600}";
    let retained = read_to_end(&buffer, buffer.first_readable());
    assert!(!retained.contains('\u{fffd}'));
    assert!(
        everything.ends_with(retained.as_str()),
        "retained {retained:?} is not a suffix of what was appended"
    );

    let lost = buffer.read(0);
    assert!(lost.truncated);
    assert_eq!(lost.text, retained);
}
