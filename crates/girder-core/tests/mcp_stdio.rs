//! Tests for MCP stdio framing.
//!
//! Framing is the part of a stdio transport that breaks in production and cannot be reproduced on demand
//! against a live child: a message split across two reads, two messages coalesced into one, half a frame
//! that has arrived and half that has not. [`FrameBuffer`] performs no IO precisely so those sequences can
//! be replayed as values, which is what every test here does.
//!
//! # Why the module is compiled by path
//!
//! The module is not exported from the crate root yet; that wiring is a separate change to `lib.rs`.
//! Compiling the exact source file — rather than a copy — keeps these tests exercising the code a
//! consumer will reach through `girder_core::mcp_stdio`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/mcp_stdio.rs"]
mod mcp_stdio;

use mcp_stdio::{
    FrameBuffer, FrameError, MAX_FRAME_BYTES, StdioConfig, decode_frames,
    encode_content_length_frame, encode_frame,
};

/// A small JSON-RPC request, as a server would actually emit it.
const REQUEST: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

/// A second message, distinct from [`REQUEST`] so an order mix-up fails rather than passes.
const REPLY: &[u8] = br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;

/// Splits bytes into two halves, for the cross-packet cases.
///
/// Returns owned vectors because the caller builds on them; a borrowed pair would make every test fight
/// the borrow checker over a value it is about to feed twice.
fn halves(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let middle = bytes.len() / 2;
    (bytes[..middle].to_vec(), bytes[middle..].to_vec())
}

// -- Single frame ---------------------------------------------------------------------------------

#[test]
fn a_single_newline_frame_decodes() {
    let frames = decode_frames(&encode_frame(REQUEST));
    assert_eq!(frames, vec![REQUEST.to_vec()]);
}

#[test]
fn a_single_content_length_frame_decodes() {
    let frames = decode_frames(&encode_content_length_frame(REQUEST));
    assert_eq!(frames, vec![REQUEST.to_vec()]);
}

#[test]
fn a_bare_line_is_a_frame_even_without_a_trailing_newline() {
    // The last message of a stream may arrive before the peer exits, and a caller that got the bytes
    // should not have to wait for a newline that will never come to see a complete JSON object.
    let mut buffer = FrameBuffer::new();
    assert!(buffer.feed(REQUEST).is_empty());
    assert!(buffer.is_partial());
}

// -- Cross-packet frames (the core case) ----------------------------------------------------------

#[test]
fn a_newline_frame_split_across_two_reads_is_returned_once() {
    let mut buffer = FrameBuffer::new();
    let encoded = encode_frame(REQUEST);
    let (first, second) = halves(&encoded);

    // The first read ends mid-message: nothing is complete, and the buffer must say so rather than
    // hand a truncated payload to the JSON parser.
    assert!(buffer.feed(&first).is_empty());
    assert!(buffer.is_partial());
    assert_eq!(buffer.pending(), first.len());

    // The second read completes exactly one frame — not one and a fragment.
    let frames = buffer.feed(&second);
    assert_eq!(frames, vec![REQUEST.to_vec()]);
    assert!(!buffer.is_partial());
    assert_eq!(buffer.pending(), 0);
}

#[test]
fn a_content_length_frame_split_across_two_reads_is_returned_once() {
    let mut buffer = FrameBuffer::new();
    let encoded = encode_content_length_frame(REQUEST);
    let (first, second) = halves(&encoded);

    assert!(buffer.feed(&first).is_empty());
    assert!(buffer.is_partial());

    let frames = buffer.feed(&second);
    assert_eq!(frames, vec![REQUEST.to_vec()]);
    assert_eq!(buffer.pending(), 0);
}

#[test]
fn a_content_length_frame_split_inside_the_header_is_returned_once() {
    // The split lands in `Content-Length:` itself, so the buffer cannot yet even tell which framing it
    // is looking at. It must wait rather than guess and treat the partial header as a message.
    let encoded = encode_content_length_frame(REQUEST);
    let mut buffer = FrameBuffer::new();
    assert!(buffer.feed(&encoded[..8]).is_empty());
    assert!(buffer.is_partial());
    let frames = buffer.feed(&encoded[8..]);
    assert_eq!(frames, vec![REQUEST.to_vec()]);
}

#[test]
fn a_frame_split_into_many_single_byte_reads_is_returned_once() {
    // A pipe is allowed to deliver one byte at a time, and a client that only works on convenient
    // boundaries is a client that hangs on a loaded machine.
    for encoded in [encode_frame(REQUEST), encode_content_length_frame(REQUEST)] {
        let mut buffer = FrameBuffer::new();
        let mut frames = Vec::new();
        for byte in &encoded {
            frames.extend(buffer.feed(&[*byte]));
        }
        assert_eq!(frames, vec![REQUEST.to_vec()]);
        assert_eq!(buffer.pending(), 0);
    }
}

// -- Multiple frames in one read ------------------------------------------------------------------

#[test]
fn several_newline_frames_arriving_together_are_all_returned_in_order() {
    let mut encoded = encode_frame(REQUEST);
    encoded.extend_from_slice(&encode_frame(REPLY));

    let frames = decode_frames(&encoded);
    assert_eq!(frames, vec![REQUEST.to_vec(), REPLY.to_vec()]);
}

#[test]
fn several_content_length_frames_arriving_together_are_all_returned_in_order() {
    let mut encoded = encode_content_length_frame(REQUEST);
    encoded.extend_from_slice(&encode_content_length_frame(REPLY));

    let frames = decode_frames(&encoded);
    assert_eq!(frames, vec![REQUEST.to_vec(), REPLY.to_vec()]);
}

#[test]
fn a_complete_frame_and_half_of_the_next_returns_only_the_complete_one() {
    // The realistic case: a read boundary falls between two messages. One is delivered, the fragment is
    // held, and the caller is told the difference. The split is placed inside the second message rather
    // than at the midpoint of the concatenation, whose position depends on both payload lengths.
    let mut buffer = FrameBuffer::new();
    let mut first = encode_frame(REQUEST);
    let second = encode_frame(REPLY);
    first.extend_from_slice(&second[..second.len() / 2]);

    let frames = buffer.feed(&first);
    assert_eq!(frames, vec![REQUEST.to_vec()]);
    assert!(buffer.is_partial());

    let frames = buffer.feed(&second[second.len() / 2..]);
    assert_eq!(frames, vec![REPLY.to_vec()]);
    assert!(!buffer.is_partial());
}

#[test]
fn a_read_that_ends_exactly_on_a_frame_boundary_leaves_nothing_pending() {
    // The complement of the test above: when the split lands between messages, the completed frame is
    // delivered and the buffer is clean. Treating this as "a frame had begun" would report a partial frame
    // forever and make a healthy connection look wedged.
    let mut encoded = encode_frame(REQUEST);
    let second = encode_frame(REPLY);
    encoded.extend_from_slice(&second);

    let mut buffer = FrameBuffer::new();
    let frames = buffer.feed(&encoded[..encode_frame(REQUEST).len()]);
    assert_eq!(frames, vec![REQUEST.to_vec()]);
    assert!(!buffer.is_partial());
    assert_eq!(buffer.pending(), 0);

    let frames = buffer.feed(&encoded[encode_frame(REQUEST).len()..]);
    assert_eq!(frames, vec![REPLY.to_vec()]);
    assert!(!buffer.is_partial());
}

// -- Half a frame is not a frame (the core case) --------------------------------------------------

#[test]
fn half_a_newline_frame_is_never_reported_as_complete() {
    let encoded = encode_frame(REQUEST);
    let mut buffer = FrameBuffer::new();
    let frames = buffer.feed(&encoded[..encoded.len() - 2]);
    assert!(frames.is_empty(), "a fragment must not be a message");
    assert!(buffer.is_partial());
    assert!(buffer.pending() > 0);
}

#[test]
fn a_content_length_header_without_its_body_is_partial_not_complete() {
    // The headers are entirely present, so a naive reader would consider the frame started and might
    // report an empty payload. It is a partial frame.
    let encoded = encode_content_length_frame(REQUEST);
    let header_len = encoded.len() - REQUEST.len();
    let mut buffer = FrameBuffer::new();
    let frames = buffer.feed(&encoded[..header_len]);
    assert!(frames.is_empty());
    assert!(buffer.is_partial());
    assert_eq!(buffer.pending(), header_len);
}

#[test]
fn a_body_one_byte_short_is_partial() {
    let encoded = encode_content_length_frame(REQUEST);
    let mut buffer = FrameBuffer::new();
    assert!(buffer.feed(&encoded[..encoded.len() - 1]).is_empty());
    assert!(buffer.is_partial());
    // The final byte is enough: a short body is not an error, it is not finished.
    let frames = buffer.feed(&encoded[encoded.len() - 1..]);
    assert_eq!(frames, vec![REQUEST.to_vec()]);
}

#[test]
fn an_empty_buffer_is_neither_pending_nor_partial() {
    // The distinction `pending`/`is_partial` exists for: a quiet server and a half-sent one look the same
    // to a caller that only counts frames.
    let buffer = FrameBuffer::new();
    assert_eq!(buffer.pending(), 0);
    assert!(!buffer.is_partial());
    assert!(!buffer.is_poisoned());
}

// -- Round trip ----------------------------------------------------------------------------------

#[test]
fn encode_then_decode_round_trips_both_framings() {
    let payload = br#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#;
    assert_eq!(
        decode_frames(&encode_frame(payload)),
        vec![payload.to_vec()]
    );
    assert_eq!(
        decode_frames(&encode_content_length_frame(payload)),
        vec![payload.to_vec()]
    );
}

#[test]
fn encoding_a_payload_that_already_has_a_newline_adds_no_second_one() {
    // A second newline would produce an empty line, which peers may reject as a stray separator, and
    // would make the round trip depend on the payload's own terminator.
    let mut payload = REQUEST.to_vec();
    payload.push(b'\n');
    let encoded = encode_frame(&payload);
    assert_eq!(encoded, encode_frame(REQUEST));
    assert_eq!(decode_frames(&encoded), vec![REQUEST.to_vec()]);
}

#[test]
fn a_content_length_frame_declares_the_exact_payload_size() {
    let encoded = encode_content_length_frame(REQUEST);
    let text = String::from_utf8_lossy(&encoded[..encoded.len() - REQUEST.len()]).to_string();
    assert_eq!(text, format!("Content-Length: {}\r\n\r\n", REQUEST.len()));
}

// -- Empty input ---------------------------------------------------------------------------------

#[test]
fn an_empty_read_produces_no_frames() {
    assert!(decode_frames(b"").is_empty());

    let mut buffer = FrameBuffer::new();
    assert!(buffer.feed(b"").is_empty());
    assert_eq!(buffer.pending(), 0);
    assert!(!buffer.is_partial());
}

#[test]
fn separator_bytes_are_not_messages() {
    // A peer that terminates with CRLF, or leaves a blank line between messages, must not produce
    // zero-length frames the JSON layer would have to reject.
    assert!(decode_frames(b"\r\n\n\r\n").is_empty());
    let frames = decode_frames(b"\r\n{\"a\":1}\r\n\r\n{\"b\":2}\r\n");
    assert_eq!(frames, vec![b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()]);
}

// -- Oversized frames ----------------------------------------------------------------------------

#[test]
fn a_content_length_above_the_limit_is_refused() {
    let declared = MAX_FRAME_BYTES + 1;
    let header = format!("Content-Length: {declared}\r\n\r\n");
    let mut buffer = FrameBuffer::new();

    // The refusal happens on the header alone: the client must not wait for 16 MiB of body, and must
    // not reserve them, before deciding the frame is unacceptable.
    assert!(buffer.feed(header.as_bytes()).is_empty());
    assert!(buffer.is_poisoned());
    assert_eq!(
        buffer.take_error(),
        Some(FrameError::TooLarge {
            declared,
            max: MAX_FRAME_BYTES,
        })
    );
    assert!(!buffer.is_partial(), "a refused frame is not a pending one");
}

#[test]
fn a_frame_at_exactly_the_limit_is_accepted() {
    // The boundary is inclusive; an off-by-one here would reject a legitimate maximum-size result.
    let payload = vec![b'x'; MAX_FRAME_BYTES];
    let frames = decode_frames(&encode_content_length_frame(&payload));
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].len(), MAX_FRAME_BYTES);
}

#[test]
fn a_poisoned_buffer_ignores_further_bytes() {
    let mut buffer = FrameBuffer::new();
    let header = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
    buffer.feed(header.as_bytes());

    // Whatever arrives next cannot be attributed to a frame boundary, so it must not be buffered: doing
    // so would let a peer that keeps talking grow memory without bound.
    assert!(buffer.feed(b"ignore me").is_empty());
    assert_eq!(buffer.pending(), 0);
}

#[test]
fn an_unterminated_content_length_is_not_confused_with_a_newline_frame() {
    // No `Content-Length` value yet and no blank line: the buffer is mid-header, not mid-message.
    let mut buffer = FrameBuffer::new();
    assert!(buffer.feed(b"Content-Length:").is_empty());
    assert!(buffer.is_partial());
    assert!(!buffer.is_poisoned());
}

// -- Malformed headers ---------------------------------------------------------------------------

#[test]
fn a_non_numeric_content_length_is_refused() {
    let mut buffer = FrameBuffer::new();
    assert!(buffer.feed(b"Content-Length: banana\r\n\r\n{}").is_empty());
    assert_eq!(
        buffer.take_error(),
        Some(FrameError::InvalidContentLength {
            value: "banana".to_owned(),
        })
    );
}

#[test]
fn a_negative_content_length_is_refused() {
    // Parsed as an unsigned number, so `-1` cannot become a huge length through a cast.
    let mut buffer = FrameBuffer::new();
    buffer.feed(b"Content-Length: -1\r\n\r\n");
    assert!(matches!(
        buffer.take_error(),
        Some(FrameError::InvalidContentLength { .. })
    ));
}

#[test]
fn a_header_block_without_content_length_is_refused() {
    let mut buffer = FrameBuffer::new();
    buffer.feed(b"Content-Type: application/json\r\n\r\n{}");
    assert_eq!(buffer.take_error(), Some(FrameError::MissingContentLength));
}

#[test]
fn a_header_block_that_never_terminates_is_refused() {
    let mut buffer = FrameBuffer::new();
    // One long header line with no blank line, bounded by the header budget rather than by the peer's
    // willingness to keep sending.
    let mut header = b"Content-Type: application/json\r\n".to_vec();
    header.extend(std::iter::repeat_n(b'x', MAX_FRAME_BYTES.min(32 * 1024)));
    assert!(buffer.feed(&header).is_empty());
    assert!(matches!(
        buffer.take_error(),
        Some(FrameError::HeaderTooLarge { .. })
    ));
}

#[test]
fn header_names_are_case_insensitive() {
    // HTTP-style header names are case-insensitive, and a peer that sends `content-length` is not
    // malformed; refusing it would fail a connection over capitalisation.
    let payload = b"{\"ok\":true}";
    let raw = format!("content-length: {}\r\n\r\n", payload.len());
    let mut bytes = raw.into_bytes();
    bytes.extend_from_slice(payload);
    assert_eq!(decode_frames(&bytes), vec![payload.to_vec()]);
}

#[test]
fn extra_headers_are_ignored() {
    // `Content-Type` is routinely sent alongside this framing; an unknown header must not break it.
    let payload = b"{\"ok\":true}";
    let raw = format!(
        "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
        payload.len()
    );
    let mut bytes = raw.into_bytes();
    bytes.extend_from_slice(payload);
    assert_eq!(decode_frames(&bytes), vec![payload.to_vec()]);
}

#[test]
fn a_bare_lf_header_block_is_accepted() {
    // A proxy or a hand-written client may normalise CRLF to LF; the payload never sees those bytes, so
    // rejecting them would be a framing failure over line endings.
    let raw = b"Content-Length: 11\n\n{\"ok\":true}";
    assert_eq!(decode_frames(raw), vec![b"{\"ok\":true}".to_vec()]);
}

// -- Config --------------------------------------------------------------------------------------

#[test]
fn a_config_round_trips_its_timeout() {
    let config = StdioConfig::new("uvx")
        .with_args(["mcp-server"])
        .with_startup_timeout_ms(2_500);
    assert_eq!(config.startup_timeout().as_millis(), 2_500);
}

#[test]
fn a_huge_timeout_saturates_rather_than_panicking() {
    // `Duration::from_millis(u64::MAX)` is fine, but the millisecond-to-duration conversion is the kind
    // of place a future rounding change could panic inside a startup path.
    let config = StdioConfig::new("uvx").with_startup_timeout_ms(u64::MAX);
    assert!(config.startup_timeout().as_secs() > 0);
}

#[test]
fn an_empty_command_is_refused_before_anything_is_spawned() {
    assert!(StdioConfig::new("").validate().is_err());
    assert!(StdioConfig::new("  \t ").validate().is_err());
    assert!(StdioConfig::new("uvx").validate().is_ok());
}
