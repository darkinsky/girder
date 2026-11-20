//! Tests for `terminal.retain` as a callable handler.
//!
//! The properties pinned here are the ones a client of this method depends on, and they are
//! weighted toward the two ways a retention can look successful while being wrong:
//!
//! - **An omitted `lines` keeps everything.** This is the core of the contract. A default that
//!   quietly cut the transcript would leave a client holding a suffix it believes is a history —
//!   and a suffix that ends mid-error looks exactly like a terminal that printed that much. The
//!   test therefore compares the stored bytes with the *whole* input, not just with its tail.
//! - **A named `lines` keeps the tail.** A cut from the wrong end stores startup noise instead of
//!   the error the reader is looking for, so the assertion is on the last lines' content.
//! - **The reply is `bytes`/`lines`/`truncated` and nothing else.** `truncated` is the only place
//!   the caller can learn the transcript is a suffix, so its presence and its value are both
//!   checked — including that a cut which dropped nothing reports `false`.
//! - **Bad arguments are refused, not coerced.** A negative count, a fractional one, a string, and
//!   a request with no id must all be errors naming the field. Each of these, read by hand, would
//!   otherwise become a plausible default and retain something nobody asked for.
//! - **Empty output is refused and stores nothing.** A silently stored empty transcript is
//!   indistinguishable from a terminal that never printed.
//! - **The description publishes an `inputSchema`.** Without it a client cannot discover that the
//!   method takes an id at all, and `lines` must not be listed as required — its absence is what
//!   makes "keep everything" expressible.
//!
//! # Why the module is compiled by path
//!
//! `terminal_retain_rpc` is not yet declared in the crate root; mounting the method name in
//! `terminal_rpc` is a separate change. Compiling this exact source file — rather than a copy —
//! keeps these tests exercising the code a consumer will reach through the crate root, and makes
//! the suite fail if the module stops being self-contained.
//!
//! Each test writes into its own scratch directory under the temp directory, so no two tests share
//! a transcript and a failure points at one test's data.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "tests assert by panicking; a failed unwrap or index is a failed test"
)]

#[path = "../src/terminal/retain_rpc.rs"]
pub mod terminal_retain_rpc;

use std::path::PathBuf;

use serde_json::{Value, json};

use girder_core::TerminalPersistStore;
use terminal_retain_rpc::{Args, description, execute, parse_args};

// ---- fixtures ----------------------------------------------------------------------------------

/// A fresh scratch directory per test, under the temp directory.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-terminal-retain-rpc-{name}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created a scratch directory");
    dir
}

/// A store writing into a fresh scratch directory of its own.
fn store(name: &str) -> TerminalPersistStore {
    TerminalPersistStore::new(scratch(name).join("store"))
}

/// The arguments of a call, parsed the way a handler would.
fn args(value: &Value) -> Args {
    parse_args(value).expect("the arguments should be accepted")
}

/// A transcript of `count` numbered lines, each newline-terminated.
fn transcript(count: usize) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    for n in 1..=count {
        // The write cannot fail on a `String`, and a test that panicked inside a formatting helper
        // would report the panic rather than the property it was setting up.
        let _ = writeln!(text, "line-{n}");
    }
    text
}

/// The keys of a JSON object, sorted, so a shape can be compared without depending on order.
fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("the reply should be a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

// ---- an omitted limit keeps everything ---------------------------------------------------------

#[test]
fn omitted_lines_keeps_the_whole_transcript() {
    // The core property. The comparison is against the whole input, because a suffix would also
    // "contain the last line" and pass a weaker assertion.
    let store = store("omitted");
    let output = transcript(4);
    let parsed = args(&json!({ "id": "terminal-1" }));

    let reply = execute(&store, &parsed, &output).expect("a retain of a real transcript");

    assert_eq!(
        store.load("terminal-1").unwrap().as_deref(),
        Some(output.as_str())
    );
    assert_eq!(reply["bytes"], json!(output.len() as u64));
    assert_eq!(reply["lines"], json!(4));
    // Nothing was dropped, so the transcript is not a suffix.
    assert_eq!(reply["truncated"], json!(false));
}

#[test]
fn null_lines_keeps_the_whole_transcript() {
    // A client that serialises every field sends `null` for what it is not using. That must mean
    // the same as absence rather than becoming zero or an error.
    let store = store("null");
    let output = transcript(3);
    let parsed = args(&json!({ "id": "terminal-1", "lines": null }));

    let reply = execute(&store, &parsed, &output).expect("a retain with a null limit");

    assert_eq!(parsed.lines, None);
    assert_eq!(
        store.load("terminal-1").unwrap().as_deref(),
        Some(output.as_str())
    );
    assert_eq!(reply["lines"], json!(3));
    assert_eq!(reply["truncated"], json!(false));
}

#[test]
fn omitted_lines_keeps_a_single_line_whole() {
    // The boundary where "cut to the tail" and "keep everything" coincide. Both must report the same
    // thing, and neither may report a cut.
    let output = "only one line";
    let store = store("single");

    let reply = execute(&store, &args(&json!({ "id": "t" })), output).expect("a one-line retain");

    assert_eq!(store.load("t").unwrap().as_deref(), Some(output));
    assert_eq!(reply["lines"], json!(1));
    assert_eq!(reply["truncated"], json!(false));
}

// ---- a named limit keeps the tail --------------------------------------------------------------

#[test]
fn named_lines_keeps_the_tail_of_the_transcript() {
    let store = store("tail");
    let output = "alpha\nbeta\ngamma\ndelta\n";
    let parsed = args(&json!({ "id": "terminal-1", "lines": 2 }));

    let reply = execute(&store, &parsed, output).expect("a tail retain");

    // The last two lines, not the first two: what a reader is looking for is at the end.
    assert_eq!(
        store.load("terminal-1").unwrap().as_deref(),
        Some("gamma\ndelta\n")
    );
    assert_eq!(reply["bytes"], json!("gamma\ndelta\n".len() as u64));
    assert_eq!(reply["lines"], json!(2));
    assert_eq!(reply["truncated"], json!(true));
}

#[test]
fn named_lines_longer_than_the_transcript_drops_nothing() {
    // Asking for more lines than exist must not be an error and must not report a cut: a caller
    // reading `truncated: true` here would go looking for output that was never printed.
    let store = store("oversize");
    let output = "alpha\nbeta\n";

    let reply = execute(&store, &args(&json!({ "id": "t", "lines": 99 })), output)
        .expect("a limit larger than the transcript");

    assert_eq!(store.load("t").unwrap().as_deref(), Some(output));
    assert_eq!(reply["lines"], json!(2));
    assert_eq!(reply["truncated"], json!(false));
}

#[test]
fn named_lines_cuts_on_a_character_boundary() {
    // The cut counts newlines and slices bytes. A multi-byte character next to the boundary must
    // survive the slice intact, or the stored transcript would be refused by the store as non-UTF-8.
    let store = store("unicode");
    let output = "héllo\nwörld\n";

    let reply = execute(&store, &args(&json!({ "id": "t", "lines": 1 })), output)
        .expect("a cut through a multi-byte transcript");

    assert_eq!(reply["lines"], json!(1));
    assert_eq!(reply["truncated"], json!(true));
    // The unit of a line is a line, not a fixed number of bytes.
    assert_eq!(store.load("t").unwrap().as_deref(), Some("wörld\n"));
}

// ---- the reply shape ---------------------------------------------------------------------------

#[test]
fn the_reply_has_exactly_the_three_reported_fields() {
    // Pinning the exact key set catches both directions of drift: a field removed (a client loses a
    // fact it cannot recompute, `truncated` especially) and a field added (a second source of truth
    // for the stored bytes, which `terminal.read` already serves).
    let store = store("shape");
    let reply = execute(&store, &args(&json!({ "id": "t" })), "a\nb\n").expect("a retain");

    assert_eq!(keys(&reply), ["bytes", "lines", "truncated"]);
    assert!(reply["bytes"].is_u64());
    assert!(reply["lines"].is_u64());
    assert!(reply["truncated"].is_boolean());
}

#[test]
fn the_reply_counts_bytes_and_lines_separately() {
    // Lines are not bytes: a reply that swapped the two would pass a loose check on a one-line
    // transcript. Two lines of four characters each plus two newlines is ten bytes and two lines.
    let store = store("units");
    let reply = execute(&store, &args(&json!({ "id": "t" })), "abcd\nefgh\n").expect("a retain");

    assert_eq!(reply["bytes"], json!(10));
    assert_eq!(reply["lines"], json!(2));
}

#[test]
fn a_second_retain_replaces_the_first() {
    // The store replaces rather than appends, and a client that retains twice must be answered with
    // the second transcript alone: appending would duplicate every byte of the first.
    let store = store("replace");

    execute(&store, &args(&json!({ "id": "t" })), "first\nsecond\n").expect("the first retain");
    let reply = execute(
        &store,
        &args(&json!({ "id": "t", "lines": 1 })),
        "third\nfourth\n",
    )
    .expect("the second retain");

    assert_eq!(store.load("t").unwrap().as_deref(), Some("fourth\n"));
    assert_eq!(reply["lines"], json!(1));
}

// ---- argument parsing --------------------------------------------------------------------------

#[test]
fn unknown_fields_are_ignored() {
    // Forward compatibility: a client built against a later version sends fields this build has
    // never heard of, and refusing its request would be a failure it cannot see or correct.
    let parsed = args(&json!({ "id": "t", "lines": 2, "future": { "nested": true } }));

    assert_eq!(parsed.id, "t");
    assert_eq!(parsed.lines, Some(2));
}

#[test]
fn a_missing_id_is_refused() {
    let error = parse_args(&json!({ "lines": 2 })).unwrap_err();

    // The message names the field, so a caller knows what to add rather than that "something" failed.
    assert!(error.contains("id"), "unexpected message: {error}");
}

#[test]
fn an_empty_id_is_refused() {
    let error = parse_args(&json!({ "id": "" })).unwrap_err();

    assert!(error.contains("`id`"), "unexpected message: {error}");
    assert!(error.contains("empty"), "unexpected message: {error}");
}

#[test]
fn a_zero_line_limit_is_refused() {
    // Reachable only from an explicit `0`; absence is `None` and keeps everything. A zero-line
    // retain would keep nothing, which is the failure mode empty output already refuses.
    let error = parse_args(&json!({ "id": "t", "lines": 0 })).unwrap_err();

    assert!(error.contains("`lines`"), "unexpected message: {error}");
}

#[test]
fn a_negative_line_limit_is_refused() {
    // Read by hand, a negative number becomes a default and retains everything. Typed as `usize`,
    // it is refused — and the message names the field.
    let error = parse_args(&json!({ "id": "t", "lines": -1 })).unwrap_err();

    assert!(error.contains("lines"), "unexpected message: {error}");
}

#[test]
fn a_fractional_line_limit_is_refused() {
    let error = parse_args(&json!({ "id": "t", "lines": 1.5 })).unwrap_err();

    assert!(error.contains("lines"), "unexpected message: {error}");
}

#[test]
fn a_string_line_limit_is_refused() {
    // A number sent as a string is the mistake a hand-written client makes most often, and coercing
    // it would hide the client bug behind a silently applied cut.
    let error = parse_args(&json!({ "id": "t", "lines": "2" })).unwrap_err();

    assert!(error.contains("lines"), "unexpected message: {error}");
}

#[test]
fn a_non_string_id_is_refused() {
    let error = parse_args(&json!({ "id": 7 })).unwrap_err();

    assert!(error.contains("id"), "unexpected message: {error}");
}

#[test]
fn a_non_object_argument_list_is_refused() {
    // Each top-level type is checked, including `null` and an array, because a client that sent the
    // wrong shape must be told so rather than answered with a default id.
    for value in [json!(null), json!([]), json!("t"), json!(7), json!(true)] {
        let error = parse_args(&value).unwrap_err();
        assert!(
            error.contains("JSON object"),
            "unexpected message for {value}: {error}"
        );
    }
}

// ---- empty output ------------------------------------------------------------------------------

#[test]
fn empty_output_is_refused_and_stores_nothing() {
    // The failure that hides itself: a stored empty transcript reads back as an empty string, which
    // is indistinguishable from a terminal that never printed.
    let store = store("empty");

    let error = execute(&store, &args(&json!({ "id": "t" })), "")
        .expect_err("empty output must not be retained");

    assert!(
        error.contains("no terminal output"),
        "unexpected message: {error}"
    );
    // Nothing was written, so a later read answers "no transcript" rather than an empty one.
    assert_eq!(store.load("t").unwrap(), None);
}

#[test]
fn empty_output_is_refused_even_with_a_line_limit() {
    let store = store("empty-limited");

    let error = execute(&store, &args(&json!({ "id": "t", "lines": 3 })), "")
        .expect_err("empty output must not be retained");

    assert!(
        error.contains("no terminal output"),
        "unexpected message: {error}"
    );
    assert_eq!(store.load("t").unwrap(), None);
}

#[test]
fn an_unusable_id_is_refused_with_the_stores_own_message() {
    // The store owns the id rules (separators, dots, length). Its message is forwarded rather than
    // rewritten, because it is the only text that says the *id* was the problem.
    let store = store("bad-id");

    let error = execute(&store, &args(&json!({ "id": "../escape" })), "output\n")
        .expect_err("a traversing id must not be retained");

    assert!(
        error.contains("not a usable terminal id"),
        "unexpected message: {error}"
    );
}

// ---- the description ---------------------------------------------------------------------------

#[test]
fn the_description_publishes_an_input_schema() {
    // A method a client cannot discover the arguments of is one it cannot call.
    let described = description();
    let schema = &described["inputSchema"];

    assert_eq!(described["name"], json!("terminal.retain"));
    assert!(
        described["description"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "the description must be readable prose"
    );
    assert!(schema.is_object(), "`inputSchema` must be a JSON object");
    assert_eq!(schema["type"], json!("object"));
    assert!(schema["properties"]["id"].is_object());
    assert!(schema["properties"]["lines"].is_object());
}

#[test]
fn the_description_requires_the_id_but_not_the_lines() {
    // `lines` absent from `required` is how a client learns it may omit the field — which is what
    // makes "keep the whole transcript" expressible at all.
    let described = description();
    let required = described["inputSchema"]["required"]
        .as_array()
        .expect("`required` should be an array");

    assert_eq!(required, &vec![json!("id")]);
}

#[test]
fn the_description_agrees_with_what_the_parser_accepts() {
    // A schema stricter than the parser makes a validating client refuse a request the server would
    // answer; one that is looser makes it send something that is then rejected. The bounds are
    // therefore stated once and compared here.
    let described = description();

    assert_eq!(
        described["inputSchema"]["properties"]["lines"]["minimum"],
        json!(1)
    );
    assert_eq!(
        described["inputSchema"]["properties"]["id"]["minLength"],
        json!(1)
    );
    // Unknown fields are ignored by the parser, so a client must not be told otherwise.
    assert!(
        described["inputSchema"]["additionalProperties"].is_null(),
        "the schema must not forbid fields the parser ignores"
    );
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod terminal {
    pub use super::terminal_retain_rpc as retain_rpc;
}
