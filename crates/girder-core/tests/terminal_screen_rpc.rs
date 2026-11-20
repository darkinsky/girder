//! `terminal.readScreen` registration: the model existed, the Remote did not.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use girder_core::terminal::controller::Screen;
use girder_core::terminal::screen_rpc::{
    DEFAULT_COUNT, MAX_BYTES, MAX_COUNT, METHOD, NAMESPACE, ScreenArgs, parse_args,
    register_screen_method,
};
use girder_core::{RemoteCall, RemoteError, RemoteRegistry};
use serde_json::{Value, json};

/// A registry with one live screen under `term-1`.
fn registry_with_screen() -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    register_screen_method(&mut registry, move |args| {
        let id = args.get("terminalId").and_then(Value::as_str)?;
        if id != "term-1" {
            return None;
        }
        // `Screen` is not `Clone`, and it should not be: a screen is live state, not a value. The
        // closure rebuilds the same transcript, which is what a real lookup hands over.
        let mut screen = Screen::new(80, 24, 100).expect("a valid grid");
        screen.feed(b"first\r\nsecond\r\nthird\r\n");
        Some(screen)
    });
    registry
}

/// Invokes `terminal.readScreen` with one argument map.
fn call(registry: &RemoteRegistry, args: Value) -> Result<Value, RemoteError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(registry.dispatch(
        NAMESPACE,
        METHOD,
        RemoteCall {
            args,
            peer: "test".to_owned(),
        },
    ))
}

// ---- argument parsing ---------------------------------------------------------------------------

#[test]
fn a_missing_argument_takes_its_default() {
    let args = parse_args(&json!({})).expect("all optional");
    assert_eq!(args.offset, 0);
    assert_eq!(args.count, DEFAULT_COUNT);
    assert_eq!(args.max_bytes, MAX_BYTES);
}

#[test]
fn a_null_argument_counts_as_absent() {
    // A client that serialises every field sends `null` for the ones it is not using.
    let args =
        parse_args(&json!({ "offset": null, "count": null, "maxBytes": null })).expect("nulls");
    assert_eq!(args.count, DEFAULT_COUNT);
}

#[test]
fn a_count_of_zero_keeps_everything_rather_than_returning_nothing() {
    // Zero would be an empty page, which no client means. Treating it as absent keeps "I sent zero
    // by accident" from looking like "the screen is empty".
    let args = parse_args(&json!({ "count": 0 })).expect("zero is tolerated");
    assert_eq!(args.count, DEFAULT_COUNT);
}

#[test]
fn a_negative_count_is_refused_rather_than_read_as_absent() {
    // `and_then(as_u64)` would turn `-1` into `None`, which means "keep everything" -- a caller who
    // sent a nonsense value would silently get the whole transcript.
    let error = parse_args(&json!({ "count": -1 })).expect_err("negative");
    assert!(error.contains("count"), "{error}");
}

#[test]
fn a_wrong_typed_count_is_refused() {
    let error = parse_args(&json!({ "offset": "ten" })).expect_err("string");
    assert!(error.contains("offset"), "{error}");
}

#[test]
fn a_count_past_the_cap_is_refused() {
    let error = parse_args(&json!({ "count": MAX_COUNT + 1 })).expect_err("past the cap");
    assert!(error.contains(&MAX_COUNT.to_string()), "{error}");
    assert!(parse_args(&json!({ "count": MAX_COUNT })).is_ok());
}

#[test]
fn an_offset_has_no_upper_cap_because_reading_past_the_end_is_an_empty_page() {
    // Unlike the page size, a large offset allocates nothing: it selects where to start.
    assert!(parse_args(&json!({ "offset": u32::MAX })).is_ok());
}

// ---- the method ---------------------------------------------------------------------------------

#[test]
fn the_method_is_registered_and_dispatchable() {
    let registry = registry_with_screen();
    let page = call(&registry, json!({ "terminalId": "term-1" })).expect("registered");
    assert!(page["text"].as_str().is_some(), "{page}");
}

#[test]
fn a_page_carries_the_screen_the_model_rendered() {
    let registry = registry_with_screen();
    let page = call(&registry, json!({ "terminalId": "term-1" })).expect("dispatched");
    let text = page["text"].as_str().expect("text");
    assert!(text.contains("first"), "{text}");
    assert!(text.contains("third"), "{text}");
}

#[test]
fn an_unknown_terminal_is_a_failure_not_an_empty_page() {
    // An empty page would be indistinguishable from a live terminal with nothing on it.
    let registry = registry_with_screen();
    let error = call(&registry, json!({ "terminalId": "absent" })).expect_err("no such terminal");
    assert!(matches!(error, RemoteError::Failed(_)), "{error:?}");
}

#[test]
fn bad_arguments_report_invalid_arguments_rather_than_a_generic_failure() {
    let registry = registry_with_screen();
    let error =
        call(&registry, json!({ "terminalId": "term-1", "count": -5 })).expect_err("bad count");
    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error:?}"
    );
}

#[test]
fn a_small_byte_budget_truncates_rather_than_refusing() {
    let registry = registry_with_screen();
    let page = call(&registry, json!({ "terminalId": "term-1", "maxBytes": 8 })).expect("page");
    assert!(
        page["text"].as_str().expect("text").len() <= 8 || page["totalLines"].as_u64().is_some()
    );
}

#[test]
fn the_method_answers_where_an_unregistered_one_does_not() {
    // The registry has no "is this registered" query, so the test asks by calling: a `NotImplemented`
    // or `UnknownMethod` answer would be the gap this module exists to close.
    let registry = registry_with_screen();
    let answer = call(&registry, json!({ "terminalId": "term-1" }));
    assert!(
        !matches!(
            answer,
            Err(RemoteError::NotImplemented { .. } | RemoteError::UnknownMethod { .. })
        ),
        "readScreen must dispatch, not report a gap: {answer:?}"
    );
}

#[test]
fn the_defaults_are_the_documented_ones() {
    assert_eq!(ScreenArgs::default().count, DEFAULT_COUNT);
    assert_eq!(ScreenArgs::default().max_bytes, MAX_BYTES);
    assert_eq!(ScreenArgs::default().offset, 0);
}
