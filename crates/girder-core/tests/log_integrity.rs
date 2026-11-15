//! Tests for telling an empty session log apart from a damaged one.
//!
//! The two properties this file exists for are the ones the surrounding code gets wrong today, when
//! `read_session_file(path).unwrap_or_default()` turns a torn log into `events: []`, `blank: true`:
//!
//! - **A zero-byte log is `Empty` and `is_empty` is true** — it is the one legal way to have no
//!   events, and a session that was created and never used must not be reported as damaged.
//! - **A torn log is not empty** — it has bytes, so content existed, and answering "nothing was ever
//!   written" about it is a wrong answer stated with full confidence.
//!
//! Those two are asserted against each other as well as individually, because a predicate that is
//! true for both would pass either test alone and still destroy the distinction the module is for.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/log_integrity.rs"]
mod log_integrity;

use log_integrity::{Integrity, inspect};

/// Three complete records, the way a healthy log ends: on a terminator, not inside a record.
const WHOLE: &[u8] = b"alpha\nbravo\ncharlie\n";

/// The same log inside an append that was cut off partway through a fourth record.
const TORN: &[u8] = b"alpha\nbravo\nchar\t";

#[test]
fn zero_bytes_is_empty_and_nothing_else() {
    let verdict = inspect(b"", Ok(0));

    assert_eq!(verdict, Integrity::Empty);
    assert!(verdict.is_empty(), "zero bytes is the only empty log");
    assert!(!verdict.is_readable(), "there is no content to read");
    assert!(!verdict.needs_repair(), "there is nothing to repair");
}

#[test]
fn a_failing_reader_cannot_turn_absent_content_into_damage() {
    // Emptiness is decided from the bytes, before the outcome is consulted: a reader that fails on
    // a zero-byte file has lost nothing, and letting it decide would invent damage.
    let verdict = inspect(b"", Err("compression: unexpected end of input"));

    assert_eq!(verdict, Integrity::Empty);
    assert!(verdict.is_empty());
    assert!(!verdict.needs_repair());
}

#[test]
fn a_torn_log_is_never_empty() {
    let verdict = inspect(TORN, Err("compression: unexpected end of input"));

    assert_eq!(
        verdict,
        Integrity::TornTail {
            readable: 2,
            at_byte: 12,
        }
    );
    assert!(
        !verdict.is_empty(),
        "a torn log has bytes, so it is not empty"
    );
    assert!(verdict.is_readable(), "everything before the tear is whole");
    assert!(verdict.needs_repair(), "truncating the tear loses nothing");
}

#[test]
fn empty_and_torn_disagree_on_every_question_that_matters() {
    let empty = inspect(b"", Ok(0));
    let torn = inspect(TORN, Err("unexpected end of input"));

    // The core distinction, stated as one assertion pair so a change that collapses it fails here
    // rather than in a report shown to a user.
    assert!(empty.is_empty());
    assert!(!torn.is_empty());
    assert_ne!(empty, torn);

    // Emptiness and readability are independent questions, not opposites: a torn log is both
    // non-empty and readable, which no single boolean can express.
    assert!(torn.is_readable());
    assert!(!empty.is_readable());
}

#[test]
fn damage_inside_the_log_is_broken_and_must_not_be_truncated() {
    // The stream ends on a record boundary and the reader still refused it, so this is not a tear:
    // something corrupted records that were written in full, and the last one cannot be trusted.
    let verdict = inspect(WHOLE, Err("frame: checksum mismatch at record 3"));

    assert_eq!(
        verdict,
        Integrity::Broken {
            readable: 2,
            at_byte: 12,
            detail: "frame: checksum mismatch at record 3".to_owned(),
        }
    );
    assert!(
        !verdict.is_empty(),
        "bytes exist, so nothing was empty here"
    );
    assert!(
        !verdict.needs_repair(),
        "truncation would delete the records that follow the damage"
    );
    assert!(
        !verdict.is_readable(),
        "an event exists that a reader cannot reach"
    );
}

#[test]
fn a_readable_log_reports_its_event_count() {
    let verdict = inspect(WHOLE, Ok(3));

    assert_eq!(verdict, Integrity::Readable { events: 3 });
    assert!(verdict.is_readable());
    assert!(!verdict.is_empty());
    assert!(!verdict.needs_repair());
}

#[test]
fn describe_states_the_numbers_a_user_needs() {
    let torn = inspect(TORN, Err("unexpected end of input"));
    let text = torn.describe();
    assert!(text.contains("12"), "the cut point is named: {text}");
    assert!(text.contains('2'), "the readable count is named: {text}");

    let broken = inspect(WHOLE, Err("checksum mismatch"));
    let text = broken.describe();
    assert!(text.contains("12"), "the damage offset is named: {text}");
    assert!(
        text.contains("checksum mismatch"),
        "the reader's account is kept: {text}"
    );
    assert!(
        text.contains("truncating would delete"),
        "the refusal says why it refuses: {text}"
    );

    let readable = inspect(WHOLE, Ok(3));
    assert!(readable.describe().contains('3'));

    // Every description is a sentence a user can read, not a debug rendering of the enum.
    for verdict in [Integrity::Empty, readable, torn, broken] {
        let text = verdict.describe();
        assert!(
            text.ends_with(|c: char| c.is_alphanumeric() || c == ')' || c == '`'),
            "{text}"
        );
        assert!(!text.contains("Integrity"), "{text}");
    }
}

#[test]
fn a_single_unterminated_record_is_a_torn_tail_with_nothing_readable() {
    // The smallest log an interrupted append can leave: bytes, no completed record, still not empty.
    let verdict = inspect(b"alp", Err("unexpected end of input"));

    assert_eq!(
        verdict,
        Integrity::TornTail {
            readable: 0,
            at_byte: 0
        }
    );
    assert!(!verdict.is_empty(), "three bytes is not zero bytes");
    assert!(verdict.needs_repair());
}

#[test]
fn a_newline_alone_is_a_complete_record_not_an_empty_log() {
    // One terminator is one record of zero length: the file has content, so it is not empty, and it
    // ends on a boundary, so a failure is damage rather than a tear.
    let verdict = inspect(b"\n", Err("empty record"));

    assert_eq!(
        verdict,
        Integrity::Broken {
            readable: 0,
            at_byte: 0,
            detail: "empty record".to_owned(),
        }
    );
    assert!(!verdict.is_empty());
}
