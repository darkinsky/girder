//! Diagnosing and repairing the tail of a session log that a crash left half-written.
//!
//! # What a crash actually leaves
//!
//! [`crate::SessionStore::append`] compresses one event into one zstd frame and appends that frame
//! with a single `write_all`. A process killed between the start and the end of that call leaves a
//! *prefix of a frame* at the end of the file, not a short line and not an empty file. The frames
//! before it are complete and intact, because each of them was a write that finished.
//!
//! That prefix is the whole subject of this module. It cannot be produced by anything except an
//! interrupted append, so it is the one damage a session log can be expected to carry, and it is
//! recoverable exactly by removing it.
//!
//! # Why reading the log is not already recovery
//!
//! Reading does not skip a damaged tail, it fails on it:
//!
//! - [`crate::read_session_file`] hands the whole file to `zstd::decode_all`, and a truncated frame
//!   makes that return `SessionError::Compression` — before a single line is parsed.
//! - [`crate::SessionStore::read_all`] decodes through a streaming [`std::io::Read`] and surfaces the
//!   same truncation as `SessionError::Io` from `BufRead::lines`.
//!
//! So one dead write makes an entire conversation unreadable, which is worse than losing the event
//! that was being written. The commentary in [`crate::session`] — "a truncated tail loses at most
//! the final event" — is true of the *format* but not of the readers as they stand: nothing in the
//! crate removes the incomplete frame, and [`crate::session_rewind`] states the reason in its own
//! docs ("`SessionStore` has no truncate ... a truncated frame is indistinguishable from an
//! interrupted write"). This module is the missing half of that sentence.
//!
//! # Why the cut point is computed, never guessed
//!
//! Cutting too late leaves a file that still will not decode. Cutting too early is silent data loss,
//! and it is the failure mode a user can never notice. So the cut is not "the last newline" or "the
//! file length minus a constant": it is the byte offset at which the first frame that cannot be
//! completed begins, found by walking frame boundaries with `ZSTD_findFrameCompressedSize` and
//! decoding each frame as it is passed. Nothing before that offset is touched.
//!
//! # Why a torn frame in the middle is refused instead of repaired
//!
//! This build has no repair step, so a store that crashed once simply appends onto the torn tail on
//! the next run: the file becomes *frames, a torn prefix, frames*. The frames after the tear are
//! perfectly readable, and truncating at the tear would delete them. A diagnosis therefore looks
//! past the tear for a frame that decodes, and when it finds one it answers
//! [`Recovery::Unusable`] rather than scheduling a truncation — refusing to act is the only choice
//! that cannot lose data, and the reason string says what was seen.
//!
//! # Why the damaged bytes are kept
//!
//! A truncated frame is the only record of what went wrong, and it is destroyed by the very
//! operation that fixes the log. Every [`repair`] therefore copies the file *as it was* to a
//! `{path}.torn-N` sibling first, and fsyncs it, before the live log is replaced. The suffix has no
//! `.jsonl.zst` extension, so session discovery does not mistake it for a log.

use std::fs::OpenOptions;
use std::io::{Error, ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use girder_core::{Mode, SessionEvent, write_atomic};

/// The four bytes every zstd frame begins with, little-endian `0xFD2FB528`.
///
/// Frame boundaries are found by the zstd API; this constant exists for the one question the API
/// cannot answer cheaply: whether a remainder too short to measure is the start of a frame (a kill
/// inside the magic number) or bytes that are not a log at all.
const FRAME_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// How many `{path}.torn-N` names are tried before giving up.
///
/// A bound rather than an open-ended loop: a directory that somehow fills with evidence files should
/// produce an error, not hang a startup path.
const MAX_EVIDENCE_ATTEMPTS: u32 = 1024;

/// What a diagnosis found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnosis {
    /// How many events are readable, in order, from the start of the log.
    ///
    /// An event counts only when its frame decoded and its line parsed, so this is the number of
    /// events a reader can actually see — counting bytes or lines would overstate a recovery.
    pub readable: usize,

    /// Whether the file *ends* inside a frame, i.e. the tail is a write that never completed.
    ///
    /// This describes the end of the file, not the middle: a log with a torn frame followed by good
    /// ones reports `false` here and names the tear in [`Diagnosis::torn_at`], because the file's
    /// last record is complete and the problem is that it cannot be cut away.
    pub torn_tail: bool,

    /// The byte offset at which the first unreadable record begins.
    ///
    /// `None` when everything read. `Some` for an incomplete frame and for damage that could not
    /// have come from this writer; [`Recovery`] says which of the two it is.
    pub torn_at: Option<u64>,

    /// The repair this diagnosis recommends.
    pub action: Recovery,
}

/// The repair a [`Diagnosis`] recommends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// The log is whole; use it as it is.
    Healthy,

    /// Drop everything from `bytes` to the end of the file.
    ///
    /// The event in the incomplete frame is already lost — its bytes were never all written — so
    /// this removes no readable event, and it is the only edit that makes the log decodable again.
    TruncateTo {
        /// The length the file must be cut down to.
        bytes: u64,
    },

    /// The file cannot be recovered by cutting a tail, and guessing would lose data.
    Unusable {
        /// What was found, in the form an operator needs to decide what to do next.
        reason: String,
    },
}

/// Where frame walking stopped.
struct Stop {
    /// The byte offset of the record that could not be read.
    offset: usize,
    /// Why it could not be read.
    kind: StopKind,
}

/// The two ways a log stops being readable, which call for opposite responses.
enum StopKind {
    /// Everything from the offset to EOF is a prefix of a frame: an append that was interrupted.
    Torn,

    /// The bytes at the offset are not the start of a frame this writer could have produced.
    Corrupt(String),
}

/// The result of walking the file frame by frame.
struct Scan {
    /// Events in frames that decoded and parsed.
    readable: usize,
    /// Where the walk stopped, when it stopped early.
    stop: Option<Stop>,
}

/// Diagnoses the session log at `path`.
///
/// Reads the file once and decides whether it is whole, ends in an interrupted append, or is damaged
/// in a way a truncation cannot fix. Nothing is written; [`repair`] applies the answer.
///
/// # Why a missing file is an error
///
/// A path with no log is not a healthy log: an empty *log* is a session that has no events yet, while
/// a missing *file* means the caller is looking in the wrong place. Answering `Healthy` would send a
/// caller on to read a file that is not there, and answering `TruncateTo { bytes: 0 }` would create
/// one.
///
/// # Errors
///
/// Returns [`ErrorKind::NotFound`] when there is no file, and any other I/O error from reading it.
pub fn diagnose(path: &Path) -> std::io::Result<Diagnosis> {
    let bytes = std::fs::read(path)?;
    diagnose_bytes(&bytes)
}

/// Applies `diagnosis` to the log at `path`.
///
/// [`Recovery::Healthy`] is a no-op and does not open the file. [`Recovery::TruncateTo`] keeps the
/// original at a `{path}.torn-N` sibling and then replaces the log with its first `bytes` bytes,
/// atomically. [`Recovery::Unusable`] is an error: there is no edit that is known to be safe.
///
/// # Why the diagnosis is re-derived before it is applied
///
/// A diagnosis is a snapshot, and the log can be appended to between the snapshot and the edit — by
/// this process, or by another one that resumed the session. Applying a stale byte offset would then
/// cut *readable* events off the end. So the file is walked again from the bytes about to be
/// rewritten, and a diagnosis that no longer matches is rejected instead of trusted.
///
/// # Errors
///
/// Returns [`ErrorKind::InvalidData`] when the diagnosis says the log is unusable or no longer
/// matches the file, [`ErrorKind::NotFound`] when the file is gone, and any I/O error from copying
/// the evidence or replacing the log.
pub fn repair(path: &Path, diagnosis: &Diagnosis) -> std::io::Result<()> {
    match &diagnosis.action {
        // Nothing to do, and deliberately nothing to check: a caller that repaired a log and calls
        // again must not fail because the file was archived or rotated in between.
        Recovery::Healthy => Ok(()),
        Recovery::Unusable { reason } => Err(Error::new(
            ErrorKind::InvalidData,
            format!("`{}` cannot be repaired: {reason}", path.display()),
        )),
        Recovery::TruncateTo { bytes } => truncate(path, *bytes),
    }
}

/// Cuts `path` down to `cut` bytes, keeping the original beside it.
fn truncate(path: &Path, cut: u64) -> std::io::Result<()> {
    let original = std::fs::read(path)?;

    // A cut that does not match what this file needs is refused: the log moved under the diagnosis.
    match diagnose_bytes(&original)?.action {
        Recovery::TruncateTo { bytes } if bytes == cut => {}
        other => {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "`{}` changed since it was diagnosed (it now needs {other:?}); diagnose it again",
                    path.display()
                ),
            ));
        }
    }

    let cut = usize::try_from(cut).map_err(|_| {
        Error::other("the repair offset does not fit in this platform's address space")
    })?;
    match cut.cmp(&original.len()) {
        // Already cut by someone else: still a success, so a retried repair converges.
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "the repair offset {cut} is past the end of `{}` ({} bytes)",
                path.display(),
                original.len()
            ),
        )),
        std::cmp::Ordering::Less => {
            // Evidence before edit: a crash between the two leaves a duplicate, which is harmless,
            // while the reverse order would destroy the only record of the damage.
            keep_evidence(path, &original)?;
            // The replacement is atomic, so a reader sees either the torn log or the cut one and
            // never a half-written file; the removed bytes survive in the evidence copy.
            let mode = existing_mode(path)?;
            write_atomic(path, &original[..cut], mode).map_err(|error| {
                Error::other(format!("could not rewrite `{}`: {error}", path.display()))
            })
        }
    }
}

/// Copies `contents` to the first free `{path}.torn-N` sibling.
///
/// The copy is fsynced before it is reported as written, because it is about to justify destroying
/// the original: a copy still in the page cache when the machine loses power would be no evidence at
/// all.
fn keep_evidence(path: &Path, contents: &[u8]) -> std::io::Result<PathBuf> {
    for attempt in 0..MAX_EVIDENCE_ATTEMPTS {
        let candidate = evidence_path(path, attempt);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => file,
            // An existing `.torn-N` is another repair's evidence. It is never overwritten: the two
            // runs may describe different tears, and the older one is the record of the worse state.
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = file.write_all(contents).and_then(|()| file.sync_all()) {
            // A partial evidence file is worse than none: it looks like a complete record of the
            // damage and would be trusted. It is debris this call created, so removing it is safe.
            drop(file);
            let _ = std::fs::remove_file(&candidate);
            return Err(error);
        }
        return Ok(candidate);
    }
    Err(Error::other(format!(
        "no free `.torn-N` name beside `{}` after {MAX_EVIDENCE_ATTEMPTS} attempts",
        path.display()
    )))
}

/// The evidence filename for attempt `attempt`.
fn evidence_path(path: &Path, attempt: u32) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "session-log".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!("{name}.torn-{attempt}"))
}

/// The mode the log already has, so a repair does not change who can read a session.
///
/// A truncation is not an occasion to widen permissions: a log that was `0600` must stay `0600`, and
/// one that was `0644` must not silently become private when it is repaired.
fn existing_mode(path: &Path) -> std::io::Result<Mode> {
    let metadata = std::fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(Mode(metadata.permissions().mode() & 0o777))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(Mode::PRIVATE)
    }
}

/// Diagnoses an in-memory log, for callers that already hold the bytes and for `repair`.
fn diagnose_bytes(bytes: &[u8]) -> std::io::Result<Diagnosis> {
    let scan = scan(bytes);
    let Some(stop) = scan.stop else {
        return Ok(Diagnosis {
            readable: scan.readable,
            torn_tail: false,
            torn_at: None,
            action: Recovery::Healthy,
        });
    };
    let torn_at = Some(byte_offset(stop.offset)?);

    // Readable events after the break would be deleted by cutting at it, so the only safe answer is
    // to refuse and say what was found. This is checked before the kind is, because a torn frame
    // whose header is intact still *measures* as a whole frame — the following frame's bytes satisfy
    // the declared size — so the walk reports it as undecodable rather than as short.
    if let Some(later) = decodable_frame_after(bytes, stop.offset) {
        let break_kind = match stop.kind {
            StopKind::Torn => "an interrupted frame",
            StopKind::Corrupt(_) => "an unreadable frame",
        };
        return Ok(Diagnosis {
            readable: scan.readable,
            torn_tail: false,
            torn_at,
            action: Recovery::Unusable {
                reason: format!(
                    "{break_kind} starts at byte {} and a readable frame starts at byte {later}, so \
                     the log was appended to after the break; cutting at the break would discard \
                     readable events",
                    stop.offset
                ),
            },
        });
    }

    match stop.kind {
        StopKind::Torn => Ok(Diagnosis {
            readable: scan.readable,
            torn_tail: true,
            torn_at,
            action: Recovery::TruncateTo {
                bytes: byte_offset(stop.offset)?,
            },
        }),
        StopKind::Corrupt(reason) => Ok(Diagnosis {
            readable: scan.readable,
            torn_tail: false,
            torn_at,
            action: Recovery::Unusable { reason },
        }),
    }
}

/// Converts a byte offset into the `u64` the public API reports.
fn byte_offset(offset: usize) -> std::io::Result<u64> {
    u64::try_from(offset).map_err(|_| Error::other("a byte offset does not fit in 64 bits"))
}

/// Walks the log frame by frame, counting the events that decode and finding the first one that does
/// not.
///
/// The walk is what makes "cut only the torn part" true: every byte before the reported offset
/// belongs to a frame that decoded and parsed, so cutting there cannot remove a readable event.
fn scan(bytes: &[u8]) -> Scan {
    let mut offset = 0;
    let mut readable = 0;

    while offset < bytes.len() {
        let remaining = &bytes[offset..];

        // A kill can land inside the four magic bytes, so a short prefix of them is an interrupted
        // frame rather than a file that is not a log.
        if remaining.len() < FRAME_MAGIC.len() {
            let kind = if FRAME_MAGIC.starts_with(remaining) {
                StopKind::Torn
            } else {
                StopKind::Corrupt(format!(
                    "the trailing {} bytes are not a frame prefix",
                    remaining.len()
                ))
            };
            return Scan {
                readable,
                stop: Some(Stop { offset, kind }),
            };
        }

        if !remaining.starts_with(&FRAME_MAGIC) {
            return Scan {
                readable,
                stop: Some(Stop {
                    offset,
                    kind: StopKind::Corrupt(format!("byte {offset} does not begin a zstd frame")),
                }),
            };
        }

        // A frame the appender began but did not finish cannot be measured; one that measures larger
        // than what is left is the same case seen from the other side.
        let size = match zstd::zstd_safe::find_frame_compressed_size(remaining) {
            Ok(size) if size > 0 && size <= remaining.len() => size,
            Ok(_) => {
                return Scan {
                    readable,
                    stop: Some(Stop {
                        offset,
                        kind: StopKind::Corrupt(format!(
                            "the frame at byte {offset} is not measurable"
                        )),
                    }),
                };
            }
            Err(_) => {
                return Scan {
                    readable,
                    stop: Some(Stop {
                        offset,
                        kind: StopKind::Torn,
                    }),
                };
            }
        };

        match zstd::decode_all(&remaining[..size]) {
            Ok(payload) => match count_events(&payload, offset) {
                Ok(events) => readable += events,
                Err(reason) => {
                    return Scan {
                        readable,
                        stop: Some(Stop {
                            offset,
                            kind: StopKind::Corrupt(reason),
                        }),
                    };
                }
            },
            // A complete frame that will not decode is disk corruption, not an interrupted write:
            // no prefix of a frame can measure as a whole one.
            Err(error) => {
                return Scan {
                    readable,
                    stop: Some(Stop {
                        offset,
                        kind: StopKind::Corrupt(format!(
                            "the frame at byte {offset} does not decode: {error}"
                        )),
                    }),
                };
            }
        }

        offset += size;
    }

    Scan {
        readable,
        stop: None,
    }
}

/// Counts the events in one decoded frame, or explains why they are not events.
///
/// The line is parsed rather than merely counted: a line that will not deserialise is not readable,
/// so counting it would report a recovery a caller cannot actually read. Unknown event types land
/// here too, which is deliberate — this build genuinely cannot read such a log.
fn count_events(payload: &[u8], frame_offset: usize) -> Result<usize, String> {
    let text = std::str::from_utf8(payload)
        .map_err(|error| format!("the frame at byte {frame_offset} is not UTF-8: {error}"))?;
    let mut events = 0;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        serde_json::from_str::<SessionEvent>(line).map_err(|error| {
            format!(
                "line {} of the frame at byte {frame_offset} is not a session event: {error}",
                index + 1
            )
        })?;
        events += 1;
    }
    Ok(events)
}

/// The offset of the first frame that starts after `from` and decodes, if any.
///
/// This answers one question: can the tear at `from` be cut away without discarding readable events?
/// A byte scan for the magic finds candidates; only a candidate that measures and decodes counts,
/// because that is evidence the log was appended to after the interrupted write rather than
/// compressed data that happens to contain the magic.
fn decodable_frame_after(bytes: &[u8], from: usize) -> Option<usize> {
    let start = from.saturating_add(1);
    let last = bytes.len().checked_sub(FRAME_MAGIC.len())?;
    for index in start..=last {
        if !bytes[index..].starts_with(&FRAME_MAGIC) {
            continue;
        }
        let remaining = &bytes[index..];
        let Ok(size) = zstd::zstd_safe::find_frame_compressed_size(remaining) else {
            continue;
        };
        if size == 0 || size > remaining.len() {
            continue;
        }
        if zstd::decode_all(&remaining[..size]).is_ok() {
            return Some(index);
        }
    }
    None
}
