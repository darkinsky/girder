//! Reading back through a result that was too large to return in one piece.
//!
//! Two layers already bound a tool result. [`crate::core::tool_result_pruning`] keeps the head and the tail
//! of an oversized result and replaces the middle with a notice; [`crate::spill`] saves a result that
//! is still too large and hands the model a path plus a hint. Both answer "this does not fit". Neither
//! answers "give me the part you left out" *in the same turn*: pruning has thrown the middle away and
//! only reports how many lines it removed, and a spill costs a second tool call to a file whose
//! location the model has to carry forward.
//!
//! This module is the third option: the result is kept whole, and the reader asks for it one bounded
//! page at a time. The page is the unit because a tool result is line-structured — a build log, a
//! diff, a listing, a trace — and a line is what a model quotes, edits against and counts.
//!
//! # Why this is not the `read` tool's `offset`/`limit`
//!
//! The `read` tool has an `offset` and a `limit` and they look like the same idea. They are not the
//! same capability:
//!
//! - `read` resolves a **path in the workspace** and is one-based; paging addresses an **in-memory
//!   result** that may correspond to no file at all (a shell command's output, a search, an HTTP
//!   body) and is zero-based.
//! - A spilled file can indeed be re-read with `read`'s range, which is why spill and paging are
//!   alternatives rather than a pipeline. A result that was only pruned has no file to read, so its
//!   dropped middle is unreachable without this module.
//! - A cursor survives the result changing shape in ways a `path` does not describe: it names a
//!   position *and* detects that it no longer applies (see [`CursorError::OutOfRange`]).
//!
//! # Why the cursor is opaque
//!
//! [`Cursor`] renders as an opaque string and the only supported way to get one is from a [`Page`] or
//! [`Cursor::parse`]; clients pass it back untouched. This is the difference between an encoding that
//! can be replaced and one that is a wire format by accident. The current encoding is a version tag,
//! a base-ten offset and a checksum, all internal — a future version can carry a page id, a snapshot
//! hash or a signed position without any client changing, because no client is entitled to read it.
//!
//! The checksum is not security. It exists so that a cursor which was truncated, copied with a typo or
//! produced by another version fails as [`CursorError::Malformed`] instead of silently naming some
//! other line — a reader that silently jumps is worse than one that refuses.
//!
//! # Why the limit has a ceiling
//!
//! A page size is the caller's answer to "how much can I afford to carry", and nothing in the type
//! system stops that answer from being "1000000". This layer is the one that has to answer for the
//! response size, so it caps the request at [`MAX_PAGE_LINES`]: the point of paging is that a reader
//! opts into a bounded slice, and a request that defeats the bound has opted out of the feature.
//! The cap is a clamp rather than an error because the caller asked for *fewer* lines than it will
//! get, never for something wrong; an error here would force every caller to re-derive the constant.
//!
//! # Why `next` is `None` and not "unknown"
//!
//! [`Page::next`] is `Some` exactly when at least one more line exists, and `None` exactly when the
//! page ends the result. There is no third state, because a client loop of the form "page until
//! `next` is `None`" is the only thing this type is for, and a `next` that means "probably nothing
//! more" turns that loop into a guess.
//!
//! # Why an out-of-range cursor is an error
//!
//! The empty page is the dangerous answer: it looks exactly like the end of the data. A cursor that
//! names line 400 of a result that now has 12 lines comes from a stale handle or a different result,
//! and a caller that receives `lines: []` and `next: None` will report "there is nothing more" about
//! material it never saw. [`CursorError::OutOfRange`] carries the position and the total so the
//! caller can say what went wrong instead of guessing.

use std::fmt;

/// The marker at the start of every cursor this version produces.
///
/// A version tag as well as a prefix: a cursor minted by a later encoding is rejected as
/// [`CursorError::Malformed`] rather than misread as a position.
const CURSOR_PREFIX: &str = "rp1";

/// The field separator inside the cursor text.
const CURSOR_SEPARATOR: char = '.';

/// How many hex digits the checksum occupies.
const CHECKSUM_DIGITS: usize = 8;

/// The largest number of lines one page may carry.
///
/// 1000: a page of a diagnostic is a screen's worth of lines several times over, and a result larger
/// than this is a result to page *through*, not to pull in one response. The number is deliberately
/// generous for a human reader and deliberately small for a context window.
pub const MAX_PAGE_LINES: usize = 1000;

/// One page of a result.
///
/// `lines` is empty only when the result itself is empty or the request was refused — never as a
/// silent stand-in for "past the end", which is [`CursorError::OutOfRange`]'s job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    /// The lines this page carries.
    pub lines: Vec<String>,
    /// The cursor for the following page, or `None` when this page ends the result.
    pub next: Option<Cursor>,
    /// How many lines the whole result has.
    ///
    /// Repeated on every page rather than held once: the reader is usually told "lines 1-50 of 5000",
    /// and a total it has to remember from an earlier response is a total that goes missing.
    pub total: usize,
}

/// An opaque position inside a result.
///
/// Produced by [`Page::next`] and consumed by [`page_from`]. Treat the string form as a token: hold
/// it, pass it back, do not read it. The encoding is private so it can change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor(String);

impl Cursor {
    /// Parses a cursor from its string form.
    ///
    /// Accepts only text this version could have produced: a wrong prefix, a missing field, a
    /// non-numeric offset and a failed checksum are all [`CursorError::Malformed`]. The returned
    /// cursor's validity against a particular result is decided by [`page_from`], which is the only
    /// place that knows how many lines there are.
    ///
    /// # Errors
    ///
    /// Returns [`CursorError::Malformed`] when the text is not a cursor this version produced.
    pub fn parse(text: &str) -> Result<Self, CursorError> {
        let malformed = |detail: &str| CursorError::Malformed {
            detail: detail.to_owned(),
        };

        let mut fields = text.split(CURSOR_SEPARATOR);
        let Some(prefix) = fields.next() else {
            return Err(malformed("a cursor must not be empty"));
        };
        if prefix != CURSOR_PREFIX {
            return Err(malformed(&format!(
                "unsupported cursor version `{prefix}`; expected `{CURSOR_PREFIX}`"
            )));
        }
        let Some(offset) = fields.next() else {
            return Err(malformed("a cursor must carry a position and a checksum"));
        };
        let Some(stated) = fields.next() else {
            return Err(malformed("a cursor must carry a checksum"));
        };
        if fields.next().is_some() {
            return Err(malformed("a cursor must not carry extra fields"));
        }

        if offset.is_empty() || !offset.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(malformed(
                "the position in a cursor must be a non-empty run of decimal digits",
            ));
        }
        let Ok(start) = offset.parse::<usize>() else {
            return Err(malformed(
                "the position in a cursor is too large for this platform to address",
            ));
        };

        let expected = checksum(offset);
        if stated != format!("{expected:0CHECKSUM_DIGITS$x}") {
            return Err(malformed(
                "the cursor's checksum does not match its position; it was altered or truncated",
            ));
        }

        Ok(Self(format!(
            "{CURSOR_PREFIX}{CURSOR_SEPARATOR}{start}{CURSOR_SEPARATOR}{expected:0CHECKSUM_DIGITS$x}"
        )))
    }

    /// The string form to hand back to a client.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The position this cursor names.
    ///
    /// Private: the offset is an implementation detail, and a caller that read it would start
    /// depending on the encoding the opacity is there to protect.
    fn start(&self) -> usize {
        // The cursor was built by `parse`, which validates the shape, so the second field is a
        // decimal offset. `unwrap_or` keeps this total rather than panicking on a value only a
        // corrupted in-memory cursor could produce.
        self.0
            .split(CURSOR_SEPARATOR)
            .nth(1)
            .and_then(|offset| offset.parse::<usize>().ok())
            .unwrap_or(0)
    }

    /// Builds the cursor naming `start`.
    ///
    /// Failure-free by construction: the position and its checksum are formatted here, so no input
    /// can produce a cursor this module would reject.
    fn at(start: usize) -> Self {
        let offset = start.to_string();
        let expected = checksum(&offset);
        Self(format!(
            "{CURSOR_PREFIX}{CURSOR_SEPARATOR}{offset}{CURSOR_SEPARATOR}{expected:0CHECKSUM_DIGITS$x}"
        ))
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The FNV-1a hash of a cursor's position, rendered as fixed-width hex.
///
/// FNV-1a rather than a cryptographic digest: this detects a damaged cursor, and nothing here is
/// defending against a caller who forges one. A cursor is a position in output the caller already
/// has, so forgery buys nothing the caller could not have asked for directly.
fn checksum(offset: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in offset.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Why a cursor was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorError {
    /// The text is not a cursor this version produced.
    Malformed {
        /// What is wrong with it, for a message the caller can show.
        detail: String,
    },
    /// The cursor names a position this result does not have.
    OutOfRange {
        /// The position the cursor asked for.
        at: usize,
        /// How many lines the result actually has.
        total: usize,
    },
}

impl fmt::Display for CursorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { detail } => {
                write!(formatter, "not a valid result cursor: {detail}")
            }
            Self::OutOfRange { at, total } => write!(
                formatter,
                "the cursor names line {at}, but the result has only {total} line{}; \
                 the cursor is stale or belongs to a different result",
                if *total == 1 { "" } else { "s" }
            ),
        }
    }
}

impl std::error::Error for CursorError {}

/// A request for one page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// The first line wanted, zero-based.
    ///
    /// Zero-based while the `read` tool is one-based: this indexes a list in memory, and a caller
    /// that computed a position from an index should not have to adjust it.
    pub start: usize,
    /// How many lines wanted.
    ///
    /// Clamped to `1..=`[`MAX_PAGE_LINES`]. A zero is raised to one so that a page always advances:
    /// a zero-line page would return the same cursor it was given, and a client looping on `next`
    /// would spin forever without an error to show for it.
    pub limit: usize,
}

/// Returns the page a request asks for.
///
/// `start` may equal `total` — that is the position one past the last line, and it yields an empty
/// page with `next: None`, which is the honest answer for "the caller asked where the data ends".
/// Beyond that it is [`CursorError::OutOfRange`].
///
/// # Errors
///
/// Returns [`CursorError::OutOfRange`] when `start` is greater than the number of lines.
pub fn page(lines: &[String], request: &Request) -> Result<Page, CursorError> {
    let total = lines.len();
    if request.start > total {
        return Err(CursorError::OutOfRange {
            at: request.start,
            total,
        });
    }

    let limit = request.limit.clamp(1, MAX_PAGE_LINES);
    let end = request.start.saturating_add(limit).min(total);
    let next = if end < total {
        Some(Cursor::at(end))
    } else {
        None
    };

    Ok(Page {
        // The slice is copied rather than borrowed because a page outlives the call that produced it
        // and is handed to a client; borrowing would tie the response to the result's storage.
        lines: lines[request.start..end].to_vec(),
        next,
        total,
    })
}

/// Returns the page that follows a cursor.
///
/// Stricter than [`page`] on purpose: a cursor is only ever minted for a line that exists, so a cursor
/// that lands at or past the end cannot have come from this result as it stands now. Reporting that as
/// an empty page would let a stale handle present itself as the end of the data.
///
/// `limit` is clamped exactly as in [`page`].
///
/// # Errors
///
/// Returns [`CursorError::OutOfRange`] when the cursor does not name a line of `lines`.
pub fn page_from(lines: &[String], cursor: &Cursor, limit: usize) -> Result<Page, CursorError> {
    let total = lines.len();
    let start = cursor.start();
    if start >= total {
        return Err(CursorError::OutOfRange { at: start, total });
    }
    page(lines, &Request { start, limit })
}

#[cfg(test)]
mod tests {
    //! Unit tests for the parts an integration test cannot reach.
    //!
    //! [`Cursor::at`] is private on purpose: minting a cursor is the pager's job, and a public
    //! constructor would invite a client to invent positions. The round trip and the checksum are
    //! therefore tested here, against a cursor this module built, while the paging behaviour itself is
    //! tested through the public surface in `tests/result_paging.rs`.

    use super::*;

    #[test]
    fn a_cursor_round_trips_through_its_string_form() {
        let cursor = Cursor::at(42);
        let parsed = Cursor::parse(cursor.as_str()).expect("a cursor it produced");
        assert_eq!(parsed, cursor);
        assert_eq!(parsed.as_str(), cursor.as_str());
        assert_eq!(parsed.start(), 42);
    }

    #[test]
    fn a_tampered_cursor_is_malformed() {
        let cursor = Cursor::at(42);
        let mut tampered: Vec<char> = cursor.as_str().chars().collect();
        // Change the position and leave the checksum: the shape is right, the claim is not.
        let digit = tampered
            .iter()
            .position(char::is_ascii_digit)
            .expect("a cursor carries digits");
        tampered[digit] = '9';
        let text: String = tampered.into_iter().collect();
        assert!(matches!(
            Cursor::parse(&text),
            Err(CursorError::Malformed { .. })
        ));
    }
}
