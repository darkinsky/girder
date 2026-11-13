//! Trimming a session log by moving its oldest events into a sibling archive file.
//!
//! A session's log is append-only and is the source of truth for every derived view, so it only
//! grows. Left alone, a long conversation becomes a file that is slow to decode on every step and
//! that nobody ever reads past the recent part. Something has to bound it.
//!
//! # Why this archives instead of deleting
//!
//! The obvious implementation — drop the first N events and rewrite the file — is data loss with a
//! receipt that cannot be produced later. When someone asks what the agent did three weeks ago, a
//! log whose prefix was silently removed cannot answer, and it cannot even say how much is missing:
//! a trimmed log and a log that was short to begin with are the same bytes. So the prefix is moved,
//! not dropped, into `{id}.archive-{timestamp}.jsonl.zst` beside the log, and the live log keeps the
//! tail. Both files are ordinary zstd JSONL that the existing reader already understands.
//!
//! # Why the archive is written first
//!
//! The two writes cannot be one atomic operation, so the order decides what a crash leaves behind:
//!
//! | order | state after a crash between the writes |
//! |---|---|
//! | archive → log | every event exists twice; the archive is a superset, so deleting it restores the old state |
//! | log → archive | the cut events exist **nowhere** |
//!
//! A duplicate is recoverable and a hole is not, so the archive is published before the log is
//! shortened. Each of the two writes is itself atomic (`write_atomic` stages a sibling and renames
//! it), which is what makes the inter-write window the only failure mode: at no point is either
//! file observed half-written, and a run that fails at any step leaves the original log intact.
//!
//! # What this is not
//!
//! It does not rewrite, merge, or summarise events. Every line is moved verbatim, because a
//! re-serialised event is not the event that happened: field order, integer width, and float
//! spelling can all change, and an audit that re-encodes its own evidence is worth less than one
//! that preserves it. Folding a span into a summary is compaction and belongs to a model.
//!
//! # Not safe against a concurrent writer
//!
//! Shortening the log publishes a new file over the old path, so a writer that has the old inode
//! open would append into an unlinked file. A caller must hold the session's write lock across the
//! call — the same lock [`SessionStore::append`](crate::SessionStore) takes. This is stated rather
//! than enforced because the lock lives with the caller's storage layer, and taking a second,
//! different lock here would serialise against nothing.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Error, ErrorKind};
use std::path::{Path, PathBuf};

use girder_core::{
    MS_PER_DAY, MS_PER_HOUR, MS_PER_MINUTE, MS_PER_SECOND, Mode, civil_from_days, write_atomic,
};

/// Suffix of a session's live log.
///
/// Repeated here rather than imported from the layout module because that module's constant is not
/// part of the crate's public surface, and this module must also build when it is included by path
/// from an integration test. The two spellings are checked against each other by the tests
/// here: every fixture is written through [`SessionStore`](crate::SessionStore), so a log this
/// module fails to recognise as a log would show up as a test failure rather than as a bad guess.
const LOG_SUFFIX: &str = ".jsonl.zst";

/// The marker that makes an archive recognisable, and therefore scannable, by name.
const ARCHIVE_MARKER: &str = ".archive-";

/// The id used when the log's name cannot yield one.
///
/// A file called exactly `.jsonl.zst` names no session; the archive still has to be placed
/// somewhere, and refusing to archive over a naming oddity would leave the log unbounded.
const FALLBACK_ID: &str = "session";

/// How many timestamp collisions one archive will step around before giving up.
///
/// A collision needs two archives of the same session in the same millisecond. Stepping around it
/// costs one `stat`, and overwriting instead would destroy the earlier archive — the exact data
/// loss this module exists to prevent.
const MAX_COLLISION_ATTEMPTS: usize = 1000;

/// The key under which a line whose `type` tag cannot be read is counted.
const UNKNOWN_TYPE: &str = "unknown";

/// A session log's statistics.
///
/// `events` and `by_type` are produced from the same scan, so a summary can never disagree with its
/// own breakdown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogStats {
    /// How many events the log holds.
    ///
    /// Counted as lines rather than as successfully decoded events: a line is the unit the log is
    /// cut on, and a summary whose total did not match what an archive would move would be
    /// misleading in exactly the situation it is consulted for.
    pub events: usize,
    /// The compressed size on disk, in bytes.
    pub bytes_on_disk: u64,
    /// How many events of each type, keyed by the log's own `type` tag.
    ///
    /// A `BTreeMap` so the order is stable: a report that reorders itself between two runs is hard
    /// to diff.
    pub by_type: BTreeMap<String, usize>,
}

/// How much of a log to keep.
///
/// Both fields are part of one decision and are validated together: a policy whose `keep_last` is at
/// least `max_events` can never archive anything, because the trigger and the survivor count would
/// overlap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchivePolicy {
    /// Archive once the log holds more than this many events.
    pub max_events: usize,
    /// How many of the most recent events the live log keeps.
    pub keep_last: usize,
}

/// What one archive did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveOutcome {
    /// The file the older events were moved into.
    pub archive: PathBuf,
    /// How many events the live log still holds.
    pub kept: usize,
    /// How many events moved into the archive.
    pub archived: usize,
}

/// Summarises a log file.
///
/// A missing file is summarised as empty rather than refused, matching the session reader: a log
/// that was never written is not corrupt, and an archive pass over a directory must not fail because
/// a session disappeared between the listing and the scan.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read, is not a zstd frame, or does not decode
/// to UTF-8.
pub fn analyze(path: &Path) -> std::io::Result<LogStats> {
    let bytes_on_disk = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };

    let lines = read_log(path)?;
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    for line in &lines {
        *by_type.entry(type_of(line)).or_insert(0) += 1;
    }

    Ok(LogStats {
        events: lines.len(),
        bytes_on_disk,
        by_type,
    })
}

/// Moves the oldest events of the log at `path` into an archive beside it.
///
/// Returns `Ok(None)` when the log is at or below `policy.max_events`: there is nothing to do, and
/// treating "already small enough" as an error would make a periodic pass over every session noisy.
///
/// The live log keeps exactly `policy.keep_last` events, and the archive receives the rest, in
/// order. The archive's name is unique per call, so archiving twice never overwrites the first
/// archive.
///
/// # Errors
///
/// Returns [`ErrorKind::InvalidInput`] when the policy cannot be satisfied — `keep_last` is zero, or
/// `keep_last >= max_events`. Returns [`ErrorKind::InvalidData`] when the log is not a zstd JSONL
/// frame; nothing is written in either case.
///
/// A failure to write the *archive* leaves the live log untouched. A failure to rewrite the log
/// after the archive has been published leaves the archive in place, which is a superset of the log
/// — every event still exists, and the error names the archive path so a caller can decide whether
/// to delete it and retry or to finish the cut by hand. That is the deliberate asymmetry described
/// in the module docs: a duplicate is recoverable, a hole is not.
pub fn archive(path: &Path, policy: &ArchivePolicy) -> std::io::Result<Option<ArchiveOutcome>> {
    validate(policy)?;

    let lines = read_log(path)?;
    if lines.len() <= policy.max_events {
        return Ok(None);
    }

    // The split is computed from the fully decoded log, before anything is written, so a decode
    // failure cannot leave the log half moved.
    let split = lines.len() - policy.keep_last;
    let (archived, kept) = lines.split_at(split);

    let archive_bytes = encode_log(archived)?;
    let kept_bytes = encode_log(kept)?;

    let now = now_millis()?;
    let id = file_id(path);
    let archive_path = fresh_archive_path(path, &id, &stamp(now))?;

    // Order matters: publishing the archive first turns a crash between the two writes into a
    // duplicate, which is recoverable, instead of a hole, which is not. See the module docs.
    write_atomic(&archive_path, &archive_bytes, Mode::PRIVATE).map_err(|error| {
        Error::other(format!(
            "could not write the archive at `{}`: {error}",
            archive_path.display()
        ))
    })?;

    write_atomic(path, &kept_bytes, Mode::PRIVATE).map_err(|error| {
        Error::other(format!(
            "could not rewrite the log at `{}`; the archive at `{}` was kept, so no event was lost: {error}",
            path.display(),
            archive_path.display()
        ))
    })?;

    Ok(Some(ArchiveOutcome {
        archive: archive_path,
        kept: kept.len(),
        archived: archived.len(),
    }))
}

/// Rejects a policy that cannot express an archive decision.
///
/// `keep_last == 0` is refused on top of the overlap rule: a live log with no events reads as a
/// session that was never used — its history check answers "no user message", and a replay folds to
/// nothing — so the conversation would appear to have vanished even though its events survive in
/// the archive.
fn validate(policy: &ArchivePolicy) -> std::io::Result<()> {
    if policy.keep_last == 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "an archive policy must keep at least one event in the live log",
        ));
    }
    if policy.keep_last >= policy.max_events {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "an archive policy must keep fewer events than it triggers at, but it keeps {} and triggers at {}",
                policy.keep_last, policy.max_events
            ),
        ));
    }
    Ok(())
}

/// Decodes a log into its event lines.
///
/// The lines are returned as written. Re-emitting the original text rather than a re-serialised
/// event keeps the archive byte-faithful to what was logged.
fn read_log(path: &Path) -> std::io::Result<Vec<String>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    // An empty file is a valid session that has no events; handing zero bytes to the decoder would
    // report a truncated frame instead.
    if bytes.is_empty() {
        return Ok(Vec::new());
    }

    let decoded = zstd::decode_all(bytes.as_slice()).map_err(|error| {
        Error::new(
            ErrorKind::InvalidData,
            format!("`{}` is not a zstd session log: {error}", path.display()),
        )
    })?;
    let text = String::from_utf8(decoded).map_err(|error| {
        Error::new(
            ErrorKind::InvalidData,
            format!("`{}` does not decode to UTF-8: {error}", path.display()),
        )
    })?;

    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

/// Compresses lines into one zstd frame.
///
/// One frame rather than a frame per line: a rewritten log is read as a whole, and a reader decodes
/// concatenated frames anyway, so the cheap-to-write per-line framing buys nothing here.
fn encode_log(lines: &[String]) -> std::io::Result<Vec<u8>> {
    let mut payload = String::new();
    for line in lines {
        payload.push_str(line);
        payload.push('\n');
    }
    zstd::encode_all(payload.as_bytes(), zstd::DEFAULT_COMPRESSION_LEVEL)
        .map_err(|error| Error::other(format!("a session log could not be compressed: {error}")))
}

/// The `type` tag of one event line.
///
/// A line that is not JSON, or is JSON without a string `type`, is counted as [`UNKNOWN_TYPE`]
/// rather than failing the scan: the log is the evidence, and refusing to describe an odd line would
/// make exactly the log an operator needs to inspect the one that cannot be summarised.
fn type_of(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| UNKNOWN_TYPE.to_owned())
}

/// The session id encoded in the log's file name, so the archive sits beside its log under the same
/// id and a directory scan can attribute it.
fn file_id(path: &Path) -> String {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return FALLBACK_ID.to_owned();
    };
    match name.strip_suffix(LOG_SUFFIX) {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => match path.file_stem().and_then(|stem| stem.to_str()) {
            Some(stem) if !stem.is_empty() => stem.to_owned(),
            _ => FALLBACK_ID.to_owned(),
        },
    }
}

/// A directory- and sort-friendly UTC stamp: `YYYYMMDDTHHMMSSmmmZ`.
///
/// Fixed-width and free of `:` so the name is legal on every filesystem and sorts chronologically
/// as plain text.
fn stamp(epoch_millis: i64) -> String {
    let days = epoch_millis.div_euclid(MS_PER_DAY);
    let within = epoch_millis.rem_euclid(MS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let hour = within / MS_PER_HOUR;
    let minute = (within % MS_PER_HOUR) / MS_PER_MINUTE;
    let second = (within % MS_PER_MINUTE) / MS_PER_SECOND;
    let millisecond = within % MS_PER_SECOND;
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}{millisecond:03}Z")
}

/// The wall clock in milliseconds since the Unix epoch.
///
/// An error rather than a silent zero when the clock is before the epoch: a stamp that sorted
/// before every other archive would be a quiet lie about when the evidence was set aside.
fn now_millis() -> std::io::Result<i64> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| {
            Error::other(format!(
                "the system clock is before the Unix epoch: {error}"
            ))
        })?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| Error::other("the system clock is too far in the future to name an archive"))
}

/// Picks an archive path that no earlier archive occupies.
///
/// The timestamp has millisecond resolution, and two archives of one session can legitimately land
/// in the same millisecond. The suffix steps around that instead of overwriting, because an
/// overwritten archive is unrecoverable data loss.
fn fresh_archive_path(path: &Path, id: &str, stamp: &str) -> std::io::Result<PathBuf> {
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let base = format!("{id}{ARCHIVE_MARKER}{stamp}");

    for attempt in 1..=MAX_COLLISION_ATTEMPTS {
        let name = if attempt == 1 {
            format!("{base}{LOG_SUFFIX}")
        } else {
            format!("{base}-{attempt}{LOG_SUFFIX}")
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(Error::new(
        ErrorKind::AlreadyExists,
        format!(
            "could not find a free archive name beside `{}`",
            path.display()
        ),
    ))
}
