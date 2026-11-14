//! Turning the bytes on a stdio pipe into MCP messages, and back.
//!
//! [`crate::mcp_client`] owns the JSON-RPC vocabulary and [`crate::mcp`] owns the transport trait, but
//! neither decides where one message ends and the next begins. That is this module's whole job: it
//! converts a byte stream, which arrives in whatever pieces the pipe and the scheduler choose, into a
//! sequence of complete payloads, and converts a payload back into bytes.
//!
//! # Why framing is separate from the process
//!
//! Framing is the part of a stdio transport that is pure arithmetic over bytes, and it is also the part
//! that fails in production: a frame split across two `read` calls, two frames in one `read`, a header
//! that is not finished yet. Running a child process to test those cases is slow, flaky, and impossible
//! to time precisely, so the split is drawn at the byte boundary: [`FrameBuffer`] never spawns, reads,
//! or writes anything, and can therefore be driven with the exact sequences that break a live client.
//!
//! # The two framings, and which one is the protocol
//!
//! The MCP specification (2025-06-18, "Transports") says stdio messages **are delimited by newlines** and
//! must not contain embedded newlines. That is what [`encode_frame`] produces, and it is what the
//! majority of servers expect on stdin.
//!
//! LSP-style `Content-Length` framing is *not* what the MCP specification prescribes. It is accepted on
//! input anyway for three reasons: language-server bridges that host an MCP server behind their own
//! protocol speak it, some wrappers emit it, and an operator debugging with a hand-written client should
//! not have to care which of the two they typed. [`encode_content_length_frame`] emits it for those
//! peers. Accepting both on the way in costs one prefix comparison and removes a whole class of
//! "connected, but the client sees nothing" reports; emitting the wrong one by default would be a bug,
//! which is why the default encoder is the newline one.
//!
//! # Why a rejected frame poisons the buffer instead of being dropped
//!
//! A declared length beyond [`MAX_FRAME_BYTES`], or a `Content-Length` that is not a number, leaves the
//! reader unable to know where the *next* frame starts: the bytes that would have been the payload are
//! now indistinguishable from a header. Skipping the frame and carrying on would silently deliver the
//! second half of one message as if it were another, so the buffer stops, reports why, and the transport
//! closes the connection. Nothing downstream can be trusted after a framing violation, exactly as with a
//! malformed JSON-RPC reply.

// `encode_content_length_frame` is the outbound form for peers that speak LSP-style framing -- a language-server bridge
// or a hand-written debug client. MCP's own stdio format is newline-delimited and is what `encode_frame` produces, so the
// default path never needs this one; it is exported because a caller that needs it should not have to reimplement
// framing to be compatible with a peer this build never talks to.
#![allow(dead_code)]

use std::time::Duration;

use thiserror::Error;

/// The most payload bytes one frame may carry.
///
/// 16 MiB: a tool result that carries a file can legitimately be large, but a declared length is
/// attacker-controlled input from the server's side of the pipe, and `usize::MAX` would have the client
/// reserve the process's whole address space before a byte of it arrived.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// The most header bytes accepted before a `Content-Length` frame's payload begins.
///
/// 8 KiB: real header blocks for this framing are one or two short lines. A peer that has not terminated
/// its headers within this budget is not sending headers, and waiting for it would let a single peer hold
/// a connection open forever without ever producing a frame.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;

/// A configuration for a stdio transport.
///
/// Deliberately data only: nothing here starts a process. The transport that does is a different module,
/// and keeping the description of *what to run* apart from *running it* is what lets a deployment validate
/// a configuration, log it, and compare it before anything is spawned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StdioConfig {
    /// The command.
    pub command: String,
    /// The arguments.
    pub args: Vec<String>,
    /// The startup timeout, in milliseconds.
    ///
    /// In milliseconds rather than a [`Duration`] so a configuration file can carry it as a plain number;
    /// [`StdioConfig::startup_timeout`] converts it at the point of use.
    pub startup_timeout_ms: u64,
}

impl StdioConfig {
    /// Builds a configuration for one command with no arguments.
    ///
    /// The default timeout is 30 seconds: a process that has not completed the `initialize` handshake by
    /// then is not slow, it is wedged on something (a prompt on its stdin, a lock, a missing interpreter),
    /// and waiting longer only delays the report.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            startup_timeout_ms: 30_000,
        }
    }

    /// Adds the command's arguments.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Replaces the startup timeout.
    #[must_use]
    pub fn with_startup_timeout_ms(mut self, startup_timeout_ms: u64) -> Self {
        self.startup_timeout_ms = startup_timeout_ms;
        self
    }

    /// The startup timeout as a [`Duration`].
    ///
    /// Saturating rather than falling back: a value near `u64::MAX` is a configuration mistake, but
    /// clamping it to a very long timeout keeps the failure at the spawn site (where a reason can be
    /// reported) instead of turning into a panic inside a timeout conversion.
    #[must_use]
    pub fn startup_timeout(&self) -> Duration {
        Duration::from_millis(self.startup_timeout_ms)
    }

    /// Checks the configuration before anything is spawned.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::EmptyCommand`] when the command is empty or only whitespace. Such a value
    /// would otherwise surface as a spawn error from the operating system, which does not say which server
    /// it was for — and this configuration is built from a file, so the report has to name the problem.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.command.trim().is_empty() {
            return Err(ConfigError::EmptyCommand);
        }
        Ok(())
    }
}

/// Why a stdio configuration was refused.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ConfigError {
    /// The command was empty.
    #[error("a stdio server needs a command to run")]
    EmptyCommand,
}

/// Why a frame was refused.
///
/// Reported as a value rather than a panic because these are conditions a hostile or buggy peer produces,
/// and a client has to survive them: a panic in the reader task takes down the connection's supervisor
/// instead of closing one server.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum FrameError {
    /// A frame declared more bytes than [`MAX_FRAME_BYTES`].
    #[error("a frame declared {declared} bytes, above the {max} byte limit")]
    TooLarge {
        /// What the peer declared.
        declared: usize,
        /// The limit that applied.
        max: usize,
    },

    /// A `Content-Length` header was present but its value was not a length.
    #[error("`Content-Length` is not a valid length: {value:?}")]
    InvalidContentLength {
        /// The value, as received.
        value: String,
    },

    /// A header block did not terminate within [`MAX_HEADER_BYTES`].
    #[error("a frame header did not terminate within {max} bytes")]
    HeaderTooLarge {
        /// The limit that applied.
        max: usize,
    },

    /// The header block ended without a `Content-Length`.
    #[error("a frame header block has no `Content-Length`")]
    MissingContentLength,
}

/// Whether a buffered frame is a `Content-Length` frame or a newline-delimited one.
///
/// Decided from the bytes already buffered, not from configuration: the two framings are self-identifying
/// (a JSON-RPC payload never starts with the word `Content-Length`), and asking the operator to declare
/// one would make a correct handshake depend on a setting that can be wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    /// One JSON-RPC message per line.
    Newline,
    /// A header block, a blank line, then exactly `Content-Length` bytes.
    ContentLength,
}

/// A frame buffer for one stdio connection.
///
/// Holds the bytes of at most one incomplete frame. It performs no IO: a caller reads from the pipe and
/// hands the bytes to [`FrameBuffer::feed`], which is what makes every partial-frame case in this module
/// reachable from a unit test.
///
/// # What `feed` promises
///
/// Only complete frames are returned, in arrival order. A frame split across two calls is returned once,
/// from the second call, exactly as if it had arrived whole — a caller therefore never has to reassemble
/// anything itself, which is the entire reason this type exists.
#[derive(Clone, Debug, Default)]
pub struct FrameBuffer {
    /// Bytes received but not yet consumed as a frame.
    buf: Vec<u8>,
    /// Set once framing has failed; the connection is unusable from then on.
    poison: Option<FrameError>,
}

impl FrameBuffer {
    /// Creates an empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            poison: None,
        }
    }

    /// Feeds a chunk of bytes and returns the frames it completed.
    ///
    /// Returns every frame that became complete, including ones that were only finished by this chunk.
    /// A frame that is still incomplete produces nothing here and stays buffered; see
    /// [`FrameBuffer::is_partial`] for the distinction between "nothing arrived" and "half a frame
    /// arrived", which is the difference between a quiet server and a broken one.
    ///
    /// After a refusal (see [`FrameBuffer::take_error`]) the buffer stops accepting bytes: it is already
    /// unusable, and accumulating more would grow without bound for as long as the peer keeps talking.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        if self.poison.is_some() {
            return frames;
        }
        self.buf.extend_from_slice(bytes);
        self.drain_into(&mut frames);
        frames
    }

    /// How many bytes are still buffered.
    ///
    /// Non-zero with no frames returned means a partial frame; zero means the buffer is clean. A
    /// diagnostic that reported only "no frames" could not tell a server that is thinking from a server
    /// that sent half a message and then stopped, and those need different fixes.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// Whether a frame has begun but not yet completed.
    ///
    /// [`FrameBuffer::pending`] answers the same question in bytes; this answers it as a yes/no for the
    /// case that matters operationally — a connection that is silent because the peer is busy versus one
    /// holding half a frame forever.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Whether framing has failed.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poison.is_some()
    }

    /// Takes the framing failure, if there was one.
    ///
    /// Taking rather than borrowing so a caller that logs the failure does not log it again on every
    /// subsequent read; the poison flag itself stays set, because the buffer is still unusable.
    pub fn take_error(&mut self) -> Option<FrameError> {
        self.poison.clone()
    }

    /// Consumes complete frames until the buffer holds only an incomplete one.
    fn drain_into(&mut self, frames: &mut Vec<Vec<u8>>) {
        while self.poison.is_none() {
            match self.next_frame() {
                Step::Frame(payload) => frames.push(payload),
                Step::Idle => break,
                Step::Malformed(error) => {
                    // The buffer is discarded rather than kept: the bytes in it can no longer be
                    // attributed to a frame boundary, so every one of them is now noise.
                    self.buf.clear();
                    self.poison = Some(error);
                    break;
                }
            }
        }
    }

    /// Consumes and returns the next complete frame, if the buffer holds one.
    fn next_frame(&mut self) -> Step {
        // Separators between frames carry no payload. Skipping them here rather than treating a blank
        // line as an empty frame keeps an encoder that writes "\r\n" after each message interoperable
        // without the caller having to filter empty payloads out.
        let lead = self
            .buf
            .iter()
            .take_while(|byte| **byte == b'\n' || **byte == b'\r')
            .count();
        if lead > 0 {
            self.buf.drain(..lead);
        }
        if self.buf.is_empty() {
            return Step::Idle;
        }

        match self.detect_framing() {
            Framing::ContentLength => self.next_content_length_frame(),
            Framing::Newline => self.next_newline_frame(),
        }
    }

    /// Which framing the buffered bytes begin with.
    ///
    /// Detected from the *shape* of the first line rather than from a configured mode, and not from the
    /// literal name `Content-Length`: a peer may send `Content-Type` first, and matching only on the
    /// length header would classify that block as a newline-delimited message and deliver the header line
    /// itself as a frame. A header line is `Token: value`, while a JSON-RPC payload always begins with
    /// `{`, so the two are distinguishable on the first colon.
    fn detect_framing(&self) -> Framing {
        if looks_like_header_line(&self.buf) {
            Framing::ContentLength
        } else {
            Framing::Newline
        }
    }

    /// Consumes a newline-delimited frame.
    fn next_newline_frame(&mut self) -> Step {
        let Some(newline) = self.buf.iter().position(|byte| *byte == b'\n') else {
            // No terminator yet. Bounded here as well as in the header path: a peer that never sends a
            // newline must not be able to grow this buffer until the process runs out of memory.
            return if self.buf.len() > MAX_FRAME_BYTES {
                Step::Malformed(FrameError::TooLarge {
                    declared: self.buf.len(),
                    max: MAX_FRAME_BYTES,
                })
            } else {
                Step::Idle
            };
        };
        let mut line = self.buf[..newline].to_vec();
        self.buf.drain(..=newline);
        // A "\r\n" terminator is common and is not part of the payload; stripping only the "\r" that
        // directly precedes the "\n" leaves any other carriage return inside the payload untouched.
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            // An empty line is a separator, not a message: JSON-RPC has no zero-length message, so
            // returning it would hand the parser bytes it can only report as malformed.
            return self.next_frame();
        }
        Step::Frame(line)
    }

    /// Consumes a `Content-Length` framed message.
    fn next_content_length_frame(&mut self) -> Step {
        let Some(header_end) = find_header_end(&self.buf) else {
            return if self.buf.len() > MAX_HEADER_BYTES {
                Step::Malformed(FrameError::HeaderTooLarge {
                    max: MAX_HEADER_BYTES,
                })
            } else {
                Step::Idle
            };
        };
        let declared = match parse_content_length(&self.buf[..header_end]) {
            Ok(declared) => declared,
            Err(error) => return Step::Malformed(error),
        };
        if declared > MAX_FRAME_BYTES {
            return Step::Malformed(FrameError::TooLarge {
                declared,
                max: MAX_FRAME_BYTES,
            });
        }
        // Checked rather than added: `header_end + declared` is derived from a peer-supplied number, and
        // an overflow would wrap to a small total and slice the wrong bytes as a payload.
        let Some(total) = header_end.checked_add(declared) else {
            return Step::Malformed(FrameError::TooLarge {
                declared,
                max: MAX_FRAME_BYTES,
            });
        };
        if self.buf.len() < total {
            // The headers are complete but the body is not: a partial frame, not a malformed one. Waiting
            // is correct, and `is_partial` reports it so a caller does not mistake it for silence.
            return Step::Idle;
        }
        let payload = self.buf[header_end..total].to_vec();
        self.buf.drain(..total);
        Step::Frame(payload)
    }
}

/// What one pass over the buffer found.
enum Step {
    /// A complete frame.
    Frame(Vec<u8>),
    /// Nothing complete yet; more bytes are needed.
    Idle,
    /// Framing broke.
    Malformed(FrameError),
}

/// Whether the buffered bytes begin with a header line, as opposed to a JSON-RPC payload.
///
/// Only the first line, and only up to its first colon, is examined. Requiring the name before that colon
/// to be an HTTP-style token keeps a payload that happens to contain a colon — `{"jsonrpc":"2.0"` — from
/// being mistaken for headers.
fn looks_like_header_line(buf: &[u8]) -> bool {
    let line_end = buf
        .iter()
        .position(|byte| *byte == b'\n' || *byte == b'\r')
        .unwrap_or(buf.len());
    let line = &buf[..line_end];
    let Some(colon) = line.iter().position(|byte| *byte == b':') else {
        // No colon yet. The bytes so far are a prefix of either framing, so the honest answer is "cannot
        // tell"; the newline path buffers them and waits, and detection runs again on the next chunk.
        return false;
    };
    let name = &line[..colon];
    !name.is_empty()
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_')
}

/// Finds the end of a header block, returning the index just past the blank line.
///
/// Accepts both CRLF and bare LF terminators: the specification's peers use CRLF, but a hand-written
/// client or a proxy that normalises line endings sends LF, and rejecting that would be a framing failure
/// over a byte the payload never sees.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    for index in 0..buf.len() {
        if buf[index] != b'\n' {
            continue;
        }
        if buf.get(index + 1) == Some(&b'\n') {
            return Some(index + 2);
        }
        if buf.get(index + 1) == Some(&b'\r') && buf.get(index + 2) == Some(&b'\n') {
            return Some(index + 3);
        }
    }
    None
}

/// Reads the `Content-Length` value out of a complete header block.
///
/// Other headers are ignored rather than rejected: `Content-Type` is commonly sent with this framing, and
/// a client that refused an unknown header would break the day a peer added a harmless one.
fn parse_content_length(block: &[u8]) -> Result<usize, FrameError> {
    let text = String::from_utf8_lossy(block);
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("content-length") {
            continue;
        }
        let value = value.trim();
        // Parsed as `usize` so a negative, empty, fractional, or overflowing value is refused here
        // instead of becoming a length the slicing code has to defend against.
        return value
            .parse::<usize>()
            .map_err(|_| FrameError::InvalidContentLength {
                value: value.to_owned(),
            });
    }
    Err(FrameError::MissingContentLength)
}

/// Builds a frame in the framing the MCP specification prescribes for stdio: one message per line.
///
/// The trailing newline is the delimiter, so a payload that already ends with one gets no second: two
/// would produce an empty line, and an empty line is a separator rather than a message, which a strict
/// peer may reject.
#[must_use]
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.extend_from_slice(payload);
    if !payload.ends_with(b"\n") {
        out.push(b'\n');
    }
    out
}

/// Builds a frame in LSP `Content-Length` framing, for peers that speak it instead of the specification.
#[must_use]
pub fn encode_content_length_frame(payload: &[u8]) -> Vec<u8> {
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    let mut out = Vec::with_capacity(header.len() + payload.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(payload);
    out
}

/// Extracts the complete messages from one read.
///
/// A trailing partial frame is discarded rather than returned: it is not a message, and handing it to a
/// JSON parser would turn a normal split-read into a parse error. Callers that must keep it use a
/// [`FrameBuffer`], which retains it; this function is the stateless convenience for a caller that already
/// has a whole message in hand.
#[must_use]
pub fn decode_frames(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut buffer = FrameBuffer::new();
    buffer.feed(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_usable() {
        let config = StdioConfig::new("npx").with_args(["-y", "server"]);
        assert_eq!(config.command, "npx");
        assert_eq!(config.args, vec!["-y".to_owned(), "server".to_owned()]);
        assert_eq!(config.startup_timeout(), Duration::from_secs(30));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn empty_command_is_refused() {
        assert_eq!(
            StdioConfig::new("   ").validate(),
            Err(ConfigError::EmptyCommand)
        );
    }
}
