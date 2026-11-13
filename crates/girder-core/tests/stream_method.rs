//! Tests for the streaming-method table.
//!
//! Three properties carry the design, and each is tested directly rather than inferred from another
//! assertion passing by accident:
//!
//! - the ceiling is hard — the open that crosses it is refused, not merely counted differently;
//! - a closed stream's slot is reusable, because a table that leaks on close is a table whose
//!   ceiling silently lowers over the life of the process;
//! - a declaration and an open are different answers, so a probe that costs nothing cannot be
//!   confused with a reservation that costs a slot.
//!
//! # Why `close(self)` is not tested by calling it twice
//!
//! The property under test is a *compile-time* one: the second call has no receiver left, so the
//! program does not type-check. A runtime test cannot demonstrate that, because a program containing
//! the second call never runs. What can be tested is the half that is observable at run time — that
//! the handle is moved out of the caller's scope by `close`, so no alias survives it — and the
//! companion property that makes the move meaningful: [`StreamHandle`] is not `Clone`, so the caller
//! cannot reconstitute a second owner. Both are asserted below. The compile failure itself is
//! documented rather than executed, and the exact line that would not compile appears in a comment
//! in `close_consumes_the_handle_so_a_second_close_cannot_be_written`.
//!
//! # Why the module is compiled by path
//!
//! The table is not exported from the crate root yet; that wiring is a separate change to `lib.rs`.
//! Compiling the exact source file here — rather than a copy — keeps these tests exercising the same
//! code a consumer will reach through `girder_core::stream_method`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/stream_method.rs"]
mod stream_method;

use std::marker::PhantomData;

use stream_method::{StreamError, StreamHandle, StreamMethod, Streams};

/// A declaration with the two fields set to distinct, recognisable values.
///
/// The name is qualified the way the real surface spells it, so a test that passes it through
/// `is_addressable` is exercising the same shape a registered method has. The frame type is not a
/// real MIME type and is not meant to be one: the table never parses it.
fn method(name: &str) -> StreamMethod {
    StreamMethod {
        name: name.to_owned(),
        frame_type: "application/x-girder-frame".to_owned(),
    }
}

/// The qualified name used by most tests.
const STREAMING: &str = "session.stream";

// ---- a compile-time probe for `Clone` ------------------------------------------------------------

/// Asks whether `T: Clone`, where a negative bound cannot be written.
///
/// Method resolution prefers an inherent method over a trait method. The inherent `is_clone` exists
/// only when `T: Clone`; for any other `T` the call falls through to the trait method, which is
/// implemented unconditionally. That makes "this type is not `Clone`" a value the test can assert
/// instead of a claim in a comment.
struct Probe<T>(PhantomData<T>);

/// The answer for a type that is not `Clone`.
trait CloneFallback<T> {
    fn is_clone(&self) -> bool;
}

impl<T> CloneFallback<T> for Probe<T> {
    fn is_clone(&self) -> bool {
        false
    }
}

impl<T: Clone> Probe<T> {
    /// The answer for a type that is `Clone`. The payload is read so the impl applies to `T` rather than to the probe.
    #[allow(clippy::unused_self)]
    fn is_clone(&self) -> bool {
        let _ = core::marker::PhantomData::<T>;
        true
    }
}

// ---- open / close round trip --------------------------------------------------------------------

#[test]
fn round_trip_opens_then_closes_one_stream() {
    let streams = Streams::new(4);
    assert!(streams.declare(method(STREAMING)));
    assert!(streams.is_streaming(STREAMING));

    let handle = streams.open(&method(STREAMING)).expect("below the ceiling");
    assert_eq!(streams.open_count(), 1);
    assert!(streams.is_open(handle.id()));
    assert!(handle.id().starts_with(STREAMING));

    let id = handle.id().to_owned();
    streams.close(handle);

    assert_eq!(streams.open_count(), 0);
    assert!(!streams.is_open(&id));
}

// ---- the ceiling is hard -------------------------------------------------------------------------

#[test]
fn the_open_that_crosses_the_limit_is_refused() {
    let streams = Streams::new(2);
    assert!(streams.declare(method(STREAMING)));

    let first = streams.open(&method(STREAMING)).expect("slot one");
    let second = streams.open(&method(STREAMING)).expect("slot two");

    assert_eq!(
        streams.open(&method(STREAMING)).err(),
        Some(StreamError::TooMany { limit: 2 })
    );
    assert_eq!(streams.open_count(), 2);

    // The refused open must not have consumed anything: both holders can still close.
    streams.close(first);
    streams.close(second);
    assert_eq!(streams.open_count(), 0);
}

#[test]
fn every_open_counts_toward_the_limit_across_methods() {
    // The ceiling is per table, not per method. A per-method ceiling would let a client multiply it
    // by the number of declared methods, which is not a ceiling at all.
    let streams = Streams::new(1);
    assert!(streams.declare(method("session.stream")));
    assert!(streams.declare(method("terminal.attach")));

    let handle = streams
        .open(&method("session.stream"))
        .expect("the only slot");
    assert_eq!(
        streams.open(&method("terminal.attach")).err(),
        Some(StreamError::TooMany { limit: 1 })
    );

    streams.close(handle);
    streams
        .open(&method("terminal.attach"))
        .expect("the slot is free again");
    assert_eq!(streams.open_count(), 1);
}

#[test]
fn a_zero_limit_refuses_every_open() {
    // A configured zero is honoured rather than clamped to one: an operator who set it meant
    // "no streams", and quietly allowing one would misreport the configuration back to them.
    let streams = Streams::new(0);
    assert!(streams.declare(method(STREAMING)));

    assert_eq!(
        streams.open(&method(STREAMING)).err(),
        Some(StreamError::TooMany { limit: 0 })
    );
    assert_eq!(streams.open_count(), 0);
}

// ---- a closed slot is reusable -------------------------------------------------------------------

#[test]
fn a_closed_stream_frees_its_slot_for_the_next_open() {
    let streams = Streams::new(1);
    assert!(streams.declare(method(STREAMING)));

    for _ in 0..8 {
        let handle = streams
            .open(&method(STREAMING))
            .expect("the previous handle was closed");
        streams.close(handle);
        assert_eq!(streams.open_count(), 0);
    }
}

// ---- declaration is static, opening is runtime ---------------------------------------------------

#[test]
fn declaring_a_method_does_not_open_a_stream() {
    let streams = Streams::new(4);
    assert!(streams.declare(method(STREAMING)));

    // The two questions have different answers, which is the whole point of keeping them apart:
    // a catalog probe is free, a handle costs a slot.
    assert!(streams.is_streaming(STREAMING));
    assert_eq!(streams.open_count(), 0);
}

#[test]
fn opening_an_undeclared_method_is_refused() {
    let streams = Streams::new(4);

    assert_eq!(
        streams
            .open(&method("session.stream"))
            .err()
            .map(|error| match error {
                StreamError::NotStreaming { method } => method,
                StreamError::TooMany { .. } => "unexpectedly refused for capacity".to_owned(),
            }),
        Some("session.stream".to_owned())
    );
    assert_eq!(streams.open_count(), 0);
}

#[test]
fn an_undeclared_method_is_refused_even_with_room_to_spare() {
    // Order matters: capacity must not mask a protocol error. A client told "too many" would retry
    // forever against a method that can never stream.
    let streams = Streams::new(64);
    assert!(streams.declare(method(STREAMING)));

    assert_eq!(
        streams.open(&method("terminal.attach")).err(),
        Some(StreamError::NotStreaming {
            method: "terminal.attach".to_owned(),
        })
    );
}

// ---- malformed declarations ----------------------------------------------------------------------

#[test]
fn an_empty_method_name_is_refused() {
    let streams = Streams::new(4);

    assert!(!streams.declare(method("")));
    assert!(!streams.is_streaming(""));
    assert_eq!(
        streams.open(&method("")).err(),
        Some(StreamError::NotStreaming {
            method: String::new(),
        })
    );
    assert_eq!(streams.open_count(), 0);
}

#[test]
fn names_that_cannot_address_a_method_are_refused() {
    let streams = Streams::new(4);

    // No namespace, whitespace, a trailing separator, surrounding spaces: none of these can be
    // called, so none of them may become a stream id.
    for name in [
        "stream",
        "session .stream",
        " session.stream",
        "session.stream ",
        " ",
    ] {
        assert!(!streams.declare(method(name)), "{name:?} was declared");
        assert!(!streams.is_streaming(name), "{name:?} is streaming");
        assert!(streams.open(&method(name)).is_err(), "{name:?} was opened");
    }
    assert_eq!(streams.open_count(), 0);
}

#[test]
fn an_empty_frame_type_is_refused() {
    // A frame nobody can name is a frame the client cannot decode. Refusing it at declaration keeps
    // the failure at start-up instead of at the first frame of the first stream.
    let streams = Streams::new(4);

    assert!(!streams.declare(StreamMethod {
        name: STREAMING.to_owned(),
        frame_type: String::new(),
    }));
    assert!(!streams.declare(StreamMethod {
        name: STREAMING.to_owned(),
        frame_type: "   ".to_owned(),
    }));
    assert!(!streams.is_streaming(STREAMING));
}

#[test]
fn redeclaring_a_method_replaces_its_frame_type() {
    let streams = Streams::new(4);
    assert!(streams.declare(method(STREAMING)));
    assert!(streams.declare(StreamMethod {
        name: STREAMING.to_owned(),
        frame_type: "text/event-stream".to_owned(),
    }));

    let handle = streams.open(&method(STREAMING)).expect("declared twice");
    assert_eq!(handle.frame_type(), "text/event-stream");
}

#[test]
fn the_handle_carries_the_declared_frame_type_not_the_caller_s() {
    // A second copy of the frame type at the call site is a copy that can disagree with the catalog
    // the client decoded from. The declaration wins.
    let streams = Streams::new(4);
    assert!(streams.declare(method(STREAMING)));

    let handle = streams
        .open(&StreamMethod {
            name: STREAMING.to_owned(),
            frame_type: "something/else".to_owned(),
        })
        .expect("the name is what selects a declaration");
    assert_eq!(handle.frame_type(), "application/x-girder-frame");
}

// ---- ids are unique ------------------------------------------------------------------------------

#[test]
fn concurrently_open_streams_have_distinct_ids() {
    let streams = Streams::new(8);
    assert!(streams.declare(method(STREAMING)));

    let handles: Vec<StreamHandle> = (0..8)
        .map(|_| streams.open(&method(STREAMING)).expect("eight slots"))
        .collect();

    let mut ids: Vec<&str> = handles.iter().map(StreamHandle::id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 8, "an id was reused while its stream was open");
    assert_eq!(streams.open_count(), 8);
}

#[test]
fn a_handle_from_another_table_closes_nothing() {
    // Ids are process-wide, so this cannot take the slot of a different stream that happens to be
    // open. The mismatched close is ignored rather than panicking, because it runs on teardown.
    let elsewhere = Streams::new(4);
    let here = Streams::new(4);
    assert!(elsewhere.declare(method(STREAMING)));
    assert!(here.declare(method(STREAMING)));

    let foreign = elsewhere.open(&method(STREAMING)).expect("its own table");
    let local = here.open(&method(STREAMING)).expect("its own table");
    assert_ne!(foreign.id(), local.id());

    here.close(foreign);

    assert_eq!(here.open_count(), 1);
    assert!(here.is_open(local.id()));
}

// ---- the property `close(self)` exists to buy ----------------------------------------------------

#[test]
fn close_consumes_the_handle_so_a_second_close_cannot_be_written() {
    let streams = Streams::new(4);
    assert!(streams.declare(method(STREAMING)));

    let handle = streams.open(&method(STREAMING)).expect("below the ceiling");
    let id = handle.id().to_owned();

    streams.close(handle); // `handle` is moved here.
    // A second `streams.close(handle);` on this line would not compile:
    //   error[E0382]: use of moved value: `handle`
    // That is the point of taking the handle by value. A `close(&self)` would compile and silently
    // decrement the same stream twice, and the count would drift below the truth under load.
    assert!(!streams.is_open(&id));
    assert_eq!(streams.open_count(), 0);
}

#[test]
fn the_handle_is_not_cloneable_so_the_move_cannot_be_undone() {
    // `close(self)` alone would not prevent a second close if the handle could be cloned first.
    // This is the half of the property that is checkable at run time.
    assert!(
        !Probe::<StreamHandle>(PhantomData).is_clone(),
        "StreamHandle must not be Clone: a duplicated handle permits a second close"
    );

    // The probe is only meaningful if it answers `true` for a type that really is `Clone`;
    // otherwise this would assert a property of the probe rather than of the handle.
    assert!(Probe::<String>(PhantomData).is_clone());
}

// ---- the table is shared, so the lock must not be exclusive in practice ---------------------------

#[test]
fn a_shared_table_serves_two_threads() {
    let streams = std::sync::Arc::new(Streams::new(64));
    assert!(streams.declare(method(STREAMING)));

    let threads: Vec<_> = (0..4)
        .map(|_| {
            let streams = std::sync::Arc::clone(&streams);
            std::thread::spawn(move || {
                for _ in 0..16 {
                    let handle = streams.open(&method(STREAMING)).expect("room for 64");
                    assert!(streams.is_open(handle.id()));
                    streams.close(handle);
                }
            })
        })
        .collect();

    for thread in threads {
        thread.join().expect("no thread panicked");
    }

    // Every open was matched by a close, so nothing is left behind.
    assert_eq!(streams.open_count(), 0);
}
