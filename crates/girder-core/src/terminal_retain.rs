//! Keeping a terminal's output after the process that printed it is gone.
//!
//! A terminal lives only as long as the process that started it: the registry knows the handle, the
//! handle owns the output, and closing either discards both. `terminal.retain` is the request that
//! asks for output to *outlive* the process, and there is nowhere for it to go but disk. This module
//! is the bridge between the two — it takes the output a caller is holding in memory and hands the
//! part worth keeping to [`TerminalPersistStore`], which is what actually owns a file per terminal.
//!
//! It is deliberately not a store of its own. Storage has its own module with its own invariants —
//! atomic replacement, id validation, pruning — and a second implementation of those here would be
//! a second chance to get them wrong. What is *not* already solved is the decision this module
//! makes: which part of a transcript is worth keeping.
//!
//! # Why the tail rather than the head
//!
//! A retained transcript is read by a person trying to answer "what happened", and what happened is
//! at the end: the prompt that is waiting, the error that stopped the build, the last thing the
//! program printed before it died. The head is the banner, the shell startup, and whatever command
//! the user has long since forgotten. So a limit keeps the *last* lines, and the caller states it as
//! a count from the end. With no limit the whole transcript is kept, because a caller who did not
//! ask for a cut should not get one silently.
//!
//! # Why `truncated` is reported rather than inferred
//!
//! A retained transcript that stops mid-error looks exactly like a terminal that only ever printed
//! that much. The caller cannot tell "this is all there was" from "this was everything after the
//! cut", and those call for opposite answers: the first is a complete history, the second means the
//! part that explains the failure was thrown away. [`Retained::truncated`] is the only place that
//! distinction can come from, because it is the only place that saw the text before the cut.
//!
//! # Why empty output is an error
//!
//! Retaining nothing is never what the caller asked for, and it is the failure that hides itself: a
//! store that accepted an empty transcript would answer a later read with an empty string, and an
//! empty transcript is indistinguishable from a terminal that never printed. The caller would go on
//! believing the output was saved. Refusing it is the only outcome that tells the caller to go look
//! at why the terminal produced nothing.
//!
//! # Where a line ends
//!
//! A line is a run of bytes between newlines, and a trailing newline *terminates* the final line
//! instead of opening an empty one — otherwise `"a\nb\n"` would be three lines and a count of two
//! would keep a blank line the user never saw. This is the same rule `terminal_buffer` uses to
//! protect its own trailing lines, so a count that means two lines in one module means two lines in
//! the other.

use girder_core::TerminalPersistStore;

/// One request to retain terminal output.
///
/// Carries the terminal identity and the cut separately from the text itself: the text is what the
/// registry is holding at the moment of the call, and the request is what the client asked for, so a
/// caller can build one request and apply it to output it reads later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The terminal whose output is being retained.
    ///
    /// Also the name of the stored transcript, so the store validates it as a file name before
    /// anything is written.
    pub id: String,
    /// How many trailing lines to keep, or `None` for the whole transcript.
    ///
    /// `None` is the default meaning "everything": a caller that did not ask for a cut must not have
    /// one applied on its behalf.
    pub lines: Option<usize>,
}

/// The outcome of one retain.
///
/// Every field describes what is now on disk rather than what was requested, so a caller can report
/// the retention truthfully — and explain a transcript that stops earlier than expected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retained {
    /// How many bytes were written.
    pub bytes: u64,
    /// How many lines were written.
    ///
    /// Counted with the same rule the cut uses, so it is the number of lines a reader will see and
    /// not a number of newline bytes.
    pub lines: usize,
    /// Whether a line limit dropped output the terminal had printed.
    ///
    /// `true` means the stored transcript is a suffix of what the terminal produced, which is the
    /// fact a caller needs before treating it as the whole history.
    pub truncated: bool,
}

/// Why output could not be retained.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RetainError {
    /// Nothing would have been stored, so nothing was.
    ///
    /// Covers empty output and a request to keep zero lines: both leave an empty transcript, and an
    /// empty transcript is the thing a caller must never mistake for a successful save.
    #[error("there is no terminal output to retain")]
    Empty,

    /// The store refused the id or failed to write the transcript.
    ///
    /// An id the store rejects lands here rather than in a variant of its own: from this module's
    /// side nothing was retained, and the caller's response — surface the message — is the same
    /// either way. The store's own message is carried through, so the reason is not lost.
    #[error("terminal output could not be retained: {detail}")]
    Io {
        /// What the store reported.
        detail: String,
    },
}

/// Retains a terminal's output, keeping its last `request.lines` lines or all of it.
///
/// The text is cut first and stored whole: [`TerminalPersistStore`] replaces a transcript rather
/// than appending to it, and this module always has the transcript as a whole, so every retain is
/// one atomic replacement.
///
/// # Errors
///
/// Returns [`RetainError::Empty`] when the output is empty or the request keeps zero lines, before
/// anything is written. Returns [`RetainError::Io`] when the store refuses the id or cannot write
/// the file.
pub fn retain(
    store: &TerminalPersistStore,
    request: &Request,
    output: &str,
) -> Result<Retained, RetainError> {
    if output.is_empty() {
        return Err(RetainError::Empty);
    }

    let start = match request.lines {
        // No limit: the cut is a scan of the whole transcript and would find nothing to remove.
        None => 0,
        Some(lines) => tail_start(output, lines),
    };
    let retained = &output[start..];
    // `Some(0)` reaches here with an empty slice. Saving it would create a transcript that says
    // nothing and cannot be told apart from a terminal that never printed.
    if retained.is_empty() {
        return Err(RetainError::Empty);
    }

    let bytes = store
        .save(&request.id, retained)
        .map_err(|error| RetainError::Io {
            detail: error.to_string(),
        })?;

    Ok(Retained {
        bytes,
        lines: lines_in(retained),
        truncated: start > 0,
    })
}

/// Reads back the transcript retained for `id`.
///
/// Returns `None` when there is no transcript to hand back: the terminal never printed, was never
/// retained, or was pruned. An unreadable file answers `None` too, because this signature has no
/// way to say which happened and every one of those cases leaves the caller with nothing to show. A
/// caller that needs to tell damage from absence reads the store's own `load`.
#[must_use]
pub fn read(store: &TerminalPersistStore, id: &str) -> Option<String> {
    store.load(id).ok().flatten()
}

/// The byte offset where the last `lines` lines of `text` begin.
///
/// Scanned backwards and stopped as soon as enough newlines are counted, so the cost is the size of
/// the tail asked for rather than the size of the transcript — a terminal's output can be megabytes
/// and the caller usually wants the last handful of lines.
///
/// The returned offset is always a character boundary: it is either `0`, the end of the text, or one
/// byte past a newline, and a newline is a complete character. Only bytes are examined to find it,
/// which is safe for multi-byte characters because a continuation byte never equals `b'\n'`.
fn tail_start(text: &str, lines: usize) -> usize {
    if lines == 0 {
        return text.len();
    }

    let bytes = text.as_bytes();
    let mut end = bytes.len();
    // The newline that ends the text closes the final line rather than starting an empty one, so it
    // must not spend one of the caller's lines. Without this, `"a\nb\n"` cut to one line would keep
    // the empty string after the last newline.
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
    }

    // Walked backwards, the `lines`-th newline met is the one that opens the `lines`-th line from
    // the end, which is where the kept suffix starts.
    let mut remaining = lines;
    let mut index = end;
    while index > 0 {
        let position = index - 1;
        if bytes[position] == b'\n' {
            remaining -= 1;
            if remaining == 0 {
                return position + 1;
            }
        }
        index = position;
    }

    // Fewer lines than the cut asks for: the whole text is the answer that drops nothing the caller
    // said to keep.
    0
}

/// How many lines `text` holds, using the rule [`tail_start`] cuts with.
///
/// A trailing newline closes the final line, so it is not counted twice. Kept next to the cut
/// because the two have to agree: a count that disagreed would report a line total the retained
/// text does not contain.
fn lines_in(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.bytes().filter(|byte| *byte == b'\n').count();
    if text.ends_with('\n') {
        newlines
    } else {
        newlines.saturating_add(1)
    }
}
