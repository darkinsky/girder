//! The two halves of the JSONL backend that the earlier ports left out: Windows durable namespace
//! publication, and the verifier concurrency gate.
//!
//! # What upstream has
//!
//! `packages/session/session-persistence-jsonl/src/` carries two files that this crate has so far
//! only *described* rather than ported:
//!
//! | upstream | lines | what it does |
//! |---|---|---|
//! | `win32.ts` | 208 | publishes a synced staging object with `MoveFileExW(…, MOVEFILE_WRITE_THROUGH)`, creates directories the same way, and takes the session write lock as a named kernel semaphore |
//! | `migration-verifier.ts` | 198 | runs one full-generation verification in a `Worker` thread, behind a process-wide gate of [`MAX_CONCURRENT_VERIFIERS`] |
//! | `worker.ts` | 56 | the worker entry: validate the request, verify, post one response, close the port |
//!
//! # Answering the obvious question first: what needs Windows, and what does not
//!
//! Every capability in those three files, classified by whether its *decision* is portable:
//!
//! | capability | upstream | needs the Windows API? |
//! |---|---|---|
//! | cross-volume refusal | flag word omits `MOVEFILE_COPY_ALLOWED` | **no** — it is a constant, and [`PUBLISH_FLAGS`] is it |
//! | write-through durability | `MOVEFILE_WRITE_THROUGH` | **no** for the flag; **yes** for the effect |
//! | "destination must not exist" | flag word omits `MOVEFILE_REPLACE_EXISTING` | **no** — [`PUBLISH_FLAGS`] |
//! | errno translation | `errnoCode` over 8 `Win32` codes | **no** — pure table, [`errno_code`] |
//! | failure classification | `isENOENT` / `isEEXIST` | **no** — [`PublishFailure`] |
//! | how often a failed publish is retried | *never*: one call, one throw | **no** — [`PUBLISH_ATTEMPTS`] |
//! | directory race resolution | delete staging, then accept the winner if it is a directory | **no** — [`commit_directory`] |
//! | path splitting for the durable walk | `parse`/`join`, split on `[\\/]+` | **no** — [`split_windows_path`] |
//! | long-path namespacing | `toNamespacedPath` on descendants only | **no** — [`needs_namespaced_path`] |
//! | staging-name construction | `mkdtemp(parent, '.dsh-mkdir-')` | **no** — [`MKDIR_STAGING_PREFIX`] |
//! | lock name derivation | `sha256(resolve(path).toLowerCase())` | **no** — [`lock_semaphore_name`] |
//! | lock acquisition verdict | `WaitForSingleObject` return code | **no** — [`classify_lock_wait`] |
//! | **the move itself** | `MoveFileExW` | **YES** |
//! | **the directory create** | `mkdtemp` + `MoveFileExW` | **YES** (the publish half) |
//! | **the lock itself** | `CreateSemaphoreW` / `WaitForSingleObject` / `ReleaseSemaphore` | **YES** |
//! | request validation | `parseRequest` in `worker.ts` | **no** — [`parse_request`] |
//! | response shape | `{ ok, result }` / `{ ok, message, stack }` | **no** — [`classify_response`] |
//! | exit-code adjudication | `exit` before/after a result | **no** — [`judge_exit`] |
//! | the concurrency gate | `VerificationScheduler` | **no** — [`VerifierGate`] |
//! | **thread isolation** | `new Worker(…)` | **NO**, but it is a runtime facility, not a decision — see below |
//!
//! So the split is clean: **the durable-namespace *publication* call and the named-semaphore *lease*
//! are genuinely `Win32`-only**, and this crate still has no `windows-sys` dependency and no
//! sanctioned `unsafe` exemption for them. Everything else in `win32.ts` is arithmetic over path
//! strings and error codes, and all of it is portable. That is what this file implements.
//!
//! # The gate is pure logic, and that is the interesting part
//!
//! `VerificationScheduler` is not a `Worker` facility: it is 45 lines of counter and queue in front
//! of one. Its contract, as upstream writes it:
//!
//! - **At most [`MAX_CONCURRENT_VERIFIERS`] verifications run at once, process-wide.** The bound is
//!   memory, not parallelism: each verifier decodes a whole generation, so N of them is N copies of
//!   that generation resident at once.
//! - **Waiters are served in arrival order.** A `shift()` from the head, so a burst of migrations
//!   cannot starve the oldest request.
//! - **A released slot is handed over, not given back.** When a waiter exists, `release` resolves
//!   that waiter directly and leaves `active` where it was; only an empty queue decrements. Read
//!   carelessly this looks like a leak — it is the opposite: decrement-then-wake would open a window
//!   in which a *third* caller sees a free slot and overtakes the waiter who was already promised it.
//! - **Cancelling a waiter removes it from the queue.** It must not consume a permit later.
//! - **Cancelling a *running* verification does not release early.** The permit is released in
//!   `finally`, after the operation has actually finished, because the worker is still holding the
//!   memory until then.
//!
//! [`VerifierGate`] implements exactly those five properties, and each one has a test. The permit is
//! RAII, so "released in `finally`" cannot be forgotten on an early return — which is the one place
//! Rust lets us make upstream's contract *stronger* rather than merely equal.
//!
//! The part that is **not** ported is worker-thread isolation itself. In Rust the equivalent is
//! `std::thread` or `tokio::task::spawn_blocking`; the crate verifies in the calling thread and
//! `crate::session::extra::verify_file` is the seam such a worker would call. Spawning a thread is not a
//! decision this file can make on a caller's behalf, so [`VerifierGate`] is deliberately usable both
//! in-thread and around a spawned task.
//!
//! # What is not ported, and why
//!
//! - **`MoveFileExW`.** Needs `windows-sys` and an `unsafe` exemption the crate does not have.
//!   [`publish_flags`] therefore returns the *flag word* and documents the call it belongs to; the
//!   caller on Windows supplies the call. Nothing here pretends to have published a file.
//! - **`CreateSemaphoreW`/`WaitForSingleObject`/`ReleaseSemaphore`.** Same reason. Note that the
//!   *name* the semaphore would be created under is [`lock_semaphore_name`] — pure, tested against
//!   `SHA-256` vectors, and bit-compatible with upstream so a future FFI binding cannot accidentally
//!   derive a different object name and stop excluding the processes it is supposed to exclude.
//! - **`worker.ts`'s `parentPort` transport.** There is no port to speak of; the request and response
//!   *shapes* are ported because they are the protocol, and the transport is not.
//!
//! # Testing note
//!
//! Like its siblings this file is compiled by `tests/persistence_win32_gap.rs` through `#[path]`, so
//! it names **no other part of the crate** — every import is `std`, `tokio`, or `serde_json`, all of
//! which resolve identically in that build.

// The crate root is not wired to this module yet, so in a library build nothing here has a caller and
// `dead_code` would report the whole surface as waste. The test target exercises every item.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde_json::Value;
use tokio::sync::oneshot;

// =============================================================================================
// win32.ts, part 1: the publication call's flags and verdicts
// =============================================================================================

/// Replace an existing destination. **Not set by upstream, and not set here.**
///
/// This is the flag that turns a publish into an overwrite. Upstream's whole publication contract is
/// "the destination must not already exist", so two migrators that both stage cannot silently
/// replace each other — the loser learns it lost from `ERROR_FILE_EXISTS` instead.
pub const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;

/// Fall back to copy-and-delete when the move spans volumes. **Not set by upstream, and not set here.**
///
/// With this flag absent, a cross-volume publish fails with `ERROR_NOT_SAME_DEVICE` instead of being
/// emulated by a copy that is neither atomic nor durable: a crash mid-copy would leave a partial
/// destination with a name that claims to be a complete generation.
pub const MOVEFILE_COPY_ALLOWED: u32 = 0x0000_0002;

/// Do not return until the metadata change has reached the volume. This is the whole point.
///
/// POSIX publishes a new log by creating the directory entry and `fsync`ing the *parent directory*.
/// Windows does not expose that through Node, so the durable namespace primitive is the rename
/// itself: `MoveFileExW` with this flag is the documented way to make the entry update durable.
pub const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

/// The exact flag word upstream passes.
///
/// Exposed as a function-shaped constant pair ([`publish_flags`]) so the two *absent* bits are
/// assertable rather than merely absent by accident: a later edit that adds `MOVEFILE_COPY_ALLOWED`
/// "to make it work on another drive" changes the crash-consistency contract, and a test should
/// catch that, not a post-mortem.
pub const PUBLISH_FLAGS: u32 = MOVEFILE_WRITE_THROUGH;

/// How many times one publication attempt is made.
///
/// **One.** Upstream calls `MoveFileExW` exactly once and throws on failure; there is no retry loop
/// around the rename, not even for `ERROR_SHARING_VIOLATION`. This is worth stating because the
/// intuition "a rename can fail transiently, so retry it" is wrong here: the retry exists one layer
/// up, where an `EEXIST` means *somebody else won* and the winner is then verified. Retrying the
/// rename itself would replace "I lost" with "I am still trying", and a busy destination would
/// become an unbounded wait inside the publisher.
pub const PUBLISH_ATTEMPTS: u32 = 1;

/// `ERROR_FILE_NOT_FOUND`.
pub const ERROR_FILE_NOT_FOUND: u32 = 2;
/// `ERROR_PATH_NOT_FOUND`.
pub const ERROR_PATH_NOT_FOUND: u32 = 3;
/// `ERROR_ACCESS_DENIED`.
pub const ERROR_ACCESS_DENIED: u32 = 5;
/// `ERROR_NOT_SAME_DEVICE`.
pub const ERROR_NOT_SAME_DEVICE: u32 = 17;
/// `ERROR_SHARING_VIOLATION`.
pub const ERROR_SHARING_VIOLATION: u32 = 32;
/// `ERROR_FILE_EXISTS`.
pub const ERROR_FILE_EXISTS: u32 = 80;
/// `ERROR_INVALID_NAME`.
pub const ERROR_INVALID_NAME: u32 = 123;
/// `ERROR_ALREADY_EXISTS`.
pub const ERROR_ALREADY_EXISTS: u32 = 183;

/// The `errno` spelling of a `Win32` error code.
///
/// Upstream maps `GetLastError()` onto the `code` of the thrown `Error` so that code written against
/// POSIX can classify a Windows failure with the same `isENOENT`/`isEEXIST` predicates. That mapping
/// is a fixed table and is reproduced exactly — including that `ERROR_FILE_EXISTS` and
/// `ERROR_ALREADY_EXISTS` both become `EEXIST`, because a publisher that treated them differently
/// would handle a directory race on one code and not the other.
///
/// Everything unmapped is `EIO`: guessing a more specific errno for an unknown code would let a
/// caller take a recovery path the failure does not justify.
pub const fn errno_code(win32_code: u32) -> &'static str {
    match win32_code {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => "ENOENT",
        ERROR_ACCESS_DENIED => "EACCES",
        ERROR_NOT_SAME_DEVICE => "EXDEV",
        ERROR_SHARING_VIOLATION => "EBUSY",
        ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS => "EEXIST",
        ERROR_INVALID_NAME => "EINVAL",
        _ => "EIO",
    }
}

/// What a failed `MoveFileExW` means to the publication state machine.
///
/// The variants are the *decisions*, not the codes: the publisher only ever needs to know "did a
/// competitor win the name", "did I cross a volume boundary", and "is this simply a failure".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishFailure {
    /// The destination already exists: another publisher won the name.
    AlreadyExists,
    /// The move spans volumes. Never emulated by a copy — see [`MOVEFILE_COPY_ALLOWED`].
    CrossDevice,
    /// A sharing violation. Not retried; see [`PUBLISH_ATTEMPTS`].
    Busy,
    /// The staging object or its parent directory vanished.
    NotFound,
    /// The directory or destination is not writable by this process.
    Denied,
    /// The name is not a legal path (a reserved name, an illegal character).
    InvalidName,
    /// Anything else: reported, never guessed at.
    Other,
}

impl PublishFailure {
    /// The `Win32` code this failure was classified from.
    pub const fn errno(self) -> &'static str {
        match self {
            Self::AlreadyExists => "EEXIST",
            Self::CrossDevice => "EXDEV",
            Self::Busy => "EBUSY",
            Self::NotFound => "ENOENT",
            Self::Denied => "EACCES",
            Self::InvalidName => "EINVAL",
            Self::Other => "EIO",
        }
    }

    /// Whether losing the name to a competitor is recoverable by verifying the winner.
    ///
    /// Only [`Self::AlreadyExists`]. `EXDEV` is emphatically *not* recoverable this way: there is a
    /// winner at the destination name, but it has nothing to do with the bytes we staged.
    pub const fn is_adoptable(self) -> bool {
        matches!(self, Self::AlreadyExists)
    }
}

/// Classifies a `GetLastError()` value into a publication verdict.
///
/// The `EEXIST`-shaped codes are deliberately collapsed: `ERROR_FILE_EXISTS` (80) and
/// `ERROR_ALREADY_EXISTS` (183) both mean "the name is taken" and both must reach
/// [`commit_directory`] as an adoptable race, because which one `MoveFileExW` reports depends on
/// whether the existing object is a file or a directory — a detail the caller must not branch on.
pub const fn classify_publish(win32_code: u32) -> PublishFailure {
    match win32_code {
        ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS => PublishFailure::AlreadyExists,
        ERROR_NOT_SAME_DEVICE => PublishFailure::CrossDevice,
        ERROR_SHARING_VIOLATION => PublishFailure::Busy,
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => PublishFailure::NotFound,
        ERROR_ACCESS_DENIED => PublishFailure::Denied,
        ERROR_INVALID_NAME => PublishFailure::InvalidName,
        _ => PublishFailure::Other,
    }
}

/// The flag word a Windows publisher must pass to `MoveFileExW`.
///
/// Returned as a value rather than documented in prose so the two absent bits are testable. The call
/// itself — `MoveFileExW(staging, destination, publish_flags())` — needs `windows-sys` and an
/// `unsafe` exemption this crate does not carry; see the module docs.
pub const fn publish_flags() -> u32 {
    PUBLISH_FLAGS
}

// =============================================================================================
// win32.ts, part 2: the durable-directory walk
// =============================================================================================

/// The prefix of the random staging directory a leaf create uses.
///
/// Upstream keeps the staging name independent of the target's basename on purpose: a legal 255-byte
/// final component plus a `.dsh-mkdir-` prefix and a six-character random suffix would exceed the
/// per-component limit, so the staging name is generated in the *parent* and shares nothing with the
/// target beyond its directory.
pub const MKDIR_STAGING_PREFIX: &str = ".dsh-mkdir-";

/// What a `stat` of a path in the durable walk found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryState {
    /// Nothing is at the path.
    Absent,
    /// A directory is at the path.
    Directory,
    /// Something that is not a directory is at the path (`ENOTDIR`).
    Other,
}

/// What to do with one segment of the durable-directory walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentStep {
    /// The directory is already there; move on.
    Descend,
    /// Create this leaf durably, then move on.
    Create,
    /// Something occupies the name and is not a directory: refuse.
    Refuse,
}

/// Decides what one segment of the walk does.
///
/// Upstream's loop is `if (!await assertDirectory(next)) await createLeafDirectoryWin32(...)`, where
/// `assertDirectory` returns `false` only for `ENOENT` and throws `ENOTDIR` otherwise. Restated as a
/// total function, that is this match — and the third arm is the one worth having: a *file* standing
/// where a session directory belongs must stop the walk, not be silently replaced, because replacing
/// it would destroy a file the caller never asked to touch.
pub const fn plan_segment(state: DirectoryState) -> SegmentStep {
    match state {
        DirectoryState::Directory => SegmentStep::Descend,
        DirectoryState::Absent => SegmentStep::Create,
        DirectoryState::Other => SegmentStep::Refuse,
    }
}

/// The outcome of publishing one leaf directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryCommit {
    /// The staging directory became the target.
    Published,
    /// The name was already taken by a *directory*: a competitor won, and the walk continues.
    Adopted,
    /// The name was already taken by something that is not a directory.
    NotADirectory,
    /// The publish failed for any other reason.
    Failed(PublishFailure),
}

impl DirectoryCommit {
    /// Whether the durability contract is satisfied and the walk may continue.
    pub const fn is_durable(self) -> bool {
        matches!(self, Self::Published | Self::Adopted)
    }
}

/// Resolves a leaf-directory publication against a competitor.
///
/// This is upstream's `catch` block, verbatim in effect:
///
/// 1. the staging directory is removed **whatever the failure was**, so a failed create never leaves
///    a `.dsh-mkdir-XXXXXX` behind;
/// 2. an `EEXIST` whose winner *is* a directory is success — the directory this call was asked to
///    ensure exists now exists, and somebody else's `MOVEFILE_WRITE_THROUGH` is what made it durable;
/// 3. an `EEXIST` whose winner is not a directory is a refusal, not a retry;
/// 4. every other failure propagates unchanged.
///
/// The `winner_is_directory` argument is only consulted for [`PublishFailure::AlreadyExists`] — for
/// any other failure upstream never even stats the target, so passing `true` there must not turn a
/// cross-volume failure into success.
pub const fn commit_directory(
    result: Result<(), PublishFailure>,
    winner_is_directory: bool,
) -> DirectoryCommit {
    match result {
        Ok(()) => DirectoryCommit::Published,
        Err(PublishFailure::AlreadyExists) if winner_is_directory => DirectoryCommit::Adopted,
        Err(PublishFailure::AlreadyExists) => DirectoryCommit::NotADirectory,
        Err(failure) => DirectoryCommit::Failed(failure),
    }
}

/// A Windows path split into the root prefix and the components beneath it.
///
/// Borrowed from the input, because the walk only ever needs the pieces and never owns a new string
/// per segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsPath<'a> {
    /// The root as Windows spells it: `C:\`, `\\server\share\`, `\`, or empty for a relative path.
    pub root: &'a str,
    /// The components beneath the root, with empty components dropped.
    pub segments: Vec<&'a str>,
}

/// Splits an absolute Windows path into its root and the segments the durable walk visits.
///
/// This is the pure half of `ensureDurableDirectoryWin32`: `resolve(target)` makes the path absolute,
/// `parse(absolute).root` takes the root, and the remainder is split on `[\\/]+` with empty parts
/// dropped. Reproducing it lets the walk be tested on a machine that has no `C:` drive at all.
///
/// The recognizers, in the order Windows applies them:
///
/// - an extended-length or device prefix (`\\?\`, `\\.\`) — the rest keeps its own root rules;
/// - a UNC share (`\\server\share\…`), whose root is the server and share segments *including* the
///   trailing separator, because a UNC path with no share is not a directory at all;
/// - a drive letter (`C:`), whose root includes the separator when one follows;
/// - a drive-relative absolute path (`\…`), whose root is the separator alone.
///
/// A relative path yields an empty root. Upstream cannot produce one — `resolve` has already run —
/// but returning a total answer beats panicking on an input the type system does not exclude.
#[allow(
    clippy::indexing_slicing,
    reason = "every index is bounded by a length check on the same byte slice two lines above"
)]
pub fn split_windows_path(path: &str) -> WindowsPath<'_> {
    let bytes = path.as_bytes();
    let is_sep = |b: u8| b == b'\\' || b == b'/';

    // An extended-length or device prefix keeps its own syntax and is recognized first, because
    // `\\?\` would otherwise be read as a UNC server named `?`. `\\?\UNC\server\share` is a UNC
    // path in disguise, so the prefix is dropped and the UNC share becomes the root.
    let mut offset = 0usize;
    let mut unc_from = None;
    if bytes.len() >= 4
        && is_sep(bytes[0])
        && is_sep(bytes[1])
        && (bytes[2] == b'?' || bytes[2] == b'.')
        && is_sep(bytes[3])
    {
        offset = 4;
        // Compared as bytes so a multi-byte character just past the prefix cannot make a string
        // slice land mid-character.
        let marker = bytes.get(offset..offset.saturating_add(4));
        if marker
            .is_some_and(|marker| marker[..3].eq_ignore_ascii_case(b"UNC") && is_sep(marker[3]))
        {
            offset += 4;
            unc_from = Some(offset);
        }
    }

    if unc_from.is_none()
        && bytes.len() >= offset + 2
        && is_sep(bytes[offset])
        && is_sep(bytes[offset + 1])
    {
        unc_from = Some(offset);
    }

    let boundary = match unc_from {
        Some(from) => Boundary::at(unc_root_end(path, from)),
        None => split_from_drive_or_root(path, offset),
    };
    WindowsPath {
        root: &path[..boundary.root_end],
        segments: boundary.segments(path),
    }
}

/// The offset just past a UNC root's server and share, including the separator after the share.
///
/// A UNC path with no share names nothing — there is no directory to walk — so the share belongs to
/// the root rather than becoming a component the walk would try to create.
fn unc_root_end(path: &str, from: usize) -> usize {
    let bytes = path.as_bytes();
    let is_sep = |b: u8| b == b'\\' || b == b'/';
    let mut cursor = from + 2;
    let mut parts = 0;
    while cursor < bytes.len() && parts < 2 {
        if is_sep(bytes[cursor]) {
            if cursor > from + 2 && !is_sep(bytes[cursor - 1]) {
                parts += 1;
            }
            cursor += 1;
        } else {
            cursor += 1;
        }
    }
    // A run of separators after the share belongs to the root, not to an empty component.
    while cursor < bytes.len() && is_sep(bytes[cursor]) {
        cursor += 1;
    }
    cursor
}

/// The root boundary a path resolves to, plus where its components begin.
struct Boundary {
    root_end: usize,
    segments_start: usize,
}

impl Boundary {
    /// A boundary whose root ends exactly where its segments begin.
    const fn at(offset: usize) -> Self {
        Self {
            root_end: offset,
            segments_start: offset,
        }
    }

    fn segments<'a>(&self, path: &'a str) -> Vec<&'a str> {
        path[self.segments_start..]
            .split(['\\', '/'])
            .filter(|part| !part.is_empty())
            .collect()
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "every index is bounded by a length check on the same byte slice"
)]
fn split_from_drive_or_root(path: &str, start: usize) -> Boundary {
    let bytes = path.as_bytes();
    let is_sep = |b: u8| b == b'\\' || b == b'/';

    // Drive letter: two bytes, and a separator if one follows.
    if bytes.len() >= start + 2 && bytes[start + 1] == b':' {
        let after = start + 2;
        let end = if after < bytes.len() && is_sep(bytes[after]) {
            after + 1
        } else {
            after
        };
        return Boundary {
            root_end: end,
            segments_start: end,
        };
    }

    // Rooted without a drive: `\…` or `/…`.
    if bytes.len() > start && is_sep(bytes[start]) {
        return Boundary {
            root_end: start + 1,
            segments_start: start + 1,
        };
    }

    Boundary {
        root_end: start,
        segments_start: start,
    }
}

/// Whether the long-path namespace prefix must be applied before probing this path.
///
/// Upstream namespaces descendants but **not** a bare drive root: `toNamespacedPath('C:\\')` is
/// `\\?\C:\`, which Node rejects with `EISDIR` from `stat`, so the one path that most needs a `stat`
/// would fail for the one reason unrelated to the caller's problem.
///
/// "Is a bare root" is therefore "has no segments", which is exactly what [`split_windows_path`]
/// already computes.
pub fn needs_namespaced_path(path: &str) -> bool {
    !split_windows_path(path).segments.is_empty()
}

// =============================================================================================
// win32.ts, part 3: the named-semaphore lease — the name, and the verdicts
// =============================================================================================

/// The prefix of the kernel object name, scoped to the session (`Local\`) and never global.
///
/// `Local\` is what makes the object per-session rather than per-machine: two users on one host each
/// get their own namespace, so one user's wedged writer cannot block another's.
pub const LOCK_NAME_PREFIX: &str = "Local\\dsh-session-lock-";

/// `WaitForSingleObject` returned ownership immediately.
pub const WAIT_OBJECT_0: u32 = 0;
/// `WaitForSingleObject` timed out.
pub const WAIT_TIMEOUT: u32 = 0x0000_0102;
/// The timeout upstream waits for. Zero, so a contended lock is reported, never waited on.
pub const LOCK_WAIT_MILLISECONDS: u32 = 0;
/// The semaphore's initial and maximum count. One, because it is a lock, not a pool.
pub const LOCK_SEMAPHORE_COUNT: i32 = 1;

/// What a zero-timeout wait on the lock semaphore means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockWait {
    /// The handle owns the lock.
    Acquired,
    /// Another holder has it. Reported as `EBUSY`, which is the same verdict a POSIX `EWOULDBLOCK`
    /// produces here — one contention story for both platforms.
    Contended,
    /// The wait itself failed; `GetLastError()` decides what.
    Failed(u32),
}

/// Classifies a `WaitForSingleObject` return code.
///
/// Three outcomes, and the middle one is the whole contract: a zero-timeout wait that times out is
/// contention, not an error — and contention must never become a *wait*, because a blocking acquire
/// against a live but wedged holder is an unbounded hang with no diagnostic.
pub const fn classify_lock_wait(code: u32) -> LockWait {
    match code {
        WAIT_OBJECT_0 => LockWait::Acquired,
        WAIT_TIMEOUT => LockWait::Contended,
        other => LockWait::Failed(other),
    }
}

/// The kernel object name the session lock is created under.
///
/// Upstream: `Local\\dsh-session-lock-` followed by the lowercase hex `SHA-256` of the resolved lock
/// path, lowercased first because Windows paths are case-insensitive — `C:\Users\Ann\x.lock` and
/// `c:\users\ann\X.LOCK` name one file and therefore must name one lock.
///
/// The argument is the **already-resolved absolute** path. Upstream calls `resolve()`, which is a
/// filesystem-dependent normalization (it consults the current directory and collapses `..`); taking
/// it as a parameter keeps this function pure and lets its output — the part that must be bit-exact
/// for a future FFI binding — be tested anywhere.
///
/// The digest is deliberately length-independent: a 32,000-character path and a 12-character one
/// produce the same 64-character suffix, so the 32,767-character kernel object-name limit can never
/// be reached by a legal path.
pub fn lock_semaphore_name(absolute: &str) -> String {
    let folded = absolute.to_lowercase();
    let digest = sha256_hex(folded.as_bytes());
    let mut name = String::with_capacity(LOCK_NAME_PREFIX.len() + digest.len());
    name.push_str(LOCK_NAME_PREFIX);
    name.push_str(&digest);
    name
}

// =============================================================================================
// migration-verifier.ts: the request and response protocol (worker.ts)
// =============================================================================================

/// The physical encoding of a log file. Upstream's `JsonlCompression`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// Plain `JSONL`.
    None,
    /// `zstd` frames.
    Zstd,
}

impl Compression {
    /// Parses the wire spelling, which is the only spelling accepted.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "zstd" => Some(Self::Zstd),
            _ => None,
        }
    }

    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Zstd => "zstd",
        }
    }
}

/// The largest integer JavaScript can represent exactly (`2^53 - 1`).
///
/// Upstream validates with `Number.isSafeInteger`, which is *not* the same as "fits in a double": a
/// count of `2^53` is a double but not an exact integer sequence, so a length comparison against it
/// is meaningless. Rust's `u64` would happily accept it, hence the explicit bound.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// A physical byte prefix already proven to be a valid complete generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedPrefix {
    /// How many bytes of the file the prefix covers.
    pub bytes: u64,
    /// The lowercase hex `SHA-256` of those bytes.
    pub digest: String,
}

/// One verification request, as it crosses into the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationRequest {
    /// The staged or competing current-generation path.
    pub path: String,
    /// The configured physical encoding.
    pub compression: Compression,
    /// The session id expected in the decoded header.
    pub expected_id: String,
    /// The exact logical event count expected after decoding.
    pub expected_event_count: u64,
    /// A verified physical prefix; an append tail beyond it is allowed and not validated.
    pub expected_prefix: Option<ExpectedPrefix>,
}

/// Why a worker request was rejected.
///
/// Two variants because upstream has two distinct messages, and the distinction is operational: "not
/// an object" is a transport bug, "malformed" is a caller bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// The payload was not a JSON object.
    NotAnObject,
    /// The payload was an object but a field was missing or out of range.
    Malformed,
}

impl RequestError {
    /// The message upstream throws for this failure.
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotAnObject => "migration verifier request must be an object",
            Self::Malformed => "migration verifier request is malformed",
        }
    }
}

/// Whether `digest` is a lowercase hex `SHA-256`.
///
/// Lowercase only: `/^[0-9a-f]{64}$/` in upstream, and it matters because the digest is compared
/// byte-for-byte against one this crate produced, which is always lowercase. Accepting uppercase
/// would turn a formatting difference into a verification failure three layers deeper.
pub fn is_sha256_hex(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Reads a JSON number as a non-negative safe integer.
fn safe_integer(value: Option<&Value>) -> Option<u64> {
    let number = value?.as_u64()?;
    (number <= MAX_SAFE_INTEGER).then_some(number)
}

/// Validates and parses a worker request payload.
///
/// This is `parseRequest` from `worker.ts`, field for field:
///
/// | field | rule |
/// |---|---|
/// | the payload | a JSON object |
/// | `path` | a string (empty is allowed upstream, and here) |
/// | `compression` | exactly `"none"` or `"zstd"` |
/// | `expectedId` | a string |
/// | `expectedEventCount` | a safe integer, at least zero |
/// | `expectedPrefix` | absent, or `{ bytes: safe integer ≥ 0, digest: /^[0-9a-f]{64}$/ }` |
///
/// One divergence, and it is the boundary rather than the rule: upstream receives a live JavaScript
/// object and this receives parsed JSON, so `1.0` — a float literal that `Number.isSafeInteger`
/// accepts — is rejected here. Every value a real caller produces is an integer literal.
pub fn parse_request(value: &Value) -> Result<VerificationRequest, RequestError> {
    let object = value.as_object().ok_or(RequestError::NotAnObject)?;

    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or(RequestError::Malformed)?;
    let compression = object
        .get("compression")
        .and_then(Value::as_str)
        .and_then(Compression::parse)
        .ok_or(RequestError::Malformed)?;
    let expected_id = object
        .get("expectedId")
        .and_then(Value::as_str)
        .ok_or(RequestError::Malformed)?;
    let expected_event_count =
        safe_integer(object.get("expectedEventCount")).ok_or(RequestError::Malformed)?;

    let expected_prefix = match object.get("expectedPrefix") {
        None => None,
        Some(prefix) => {
            let prefix = prefix.as_object().ok_or(RequestError::Malformed)?;
            let bytes = safe_integer(prefix.get("bytes")).ok_or(RequestError::Malformed)?;
            let digest = prefix
                .get("digest")
                .and_then(Value::as_str)
                .ok_or(RequestError::Malformed)?;
            if !is_sha256_hex(digest) {
                return Err(RequestError::Malformed);
            }
            Some(ExpectedPrefix {
                bytes,
                digest: digest.to_owned(),
            })
        }
    };

    Ok(VerificationRequest {
        path: path.to_owned(),
        compression,
        expected_id: expected_id.to_owned(),
        expected_event_count,
        expected_prefix,
    })
}

/// What the worker's single message said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseShape {
    /// `{ ok: true, result: … }` — a result was reported.
    Success,
    /// `{ ok: false, message: … }` — a verification failure was reported.
    Failure,
    /// Not an object, or no boolean `ok`: the worker violated its own protocol.
    Invalid,
}

/// Classifies the worker's message.
///
/// The `ok` field must be a *boolean*; upstream rejects anything else rather than coercing, because a
/// protocol violation reported as a verification failure would send a caller looking for corruption
/// that is not there.
pub fn classify_response(value: &Value) -> ResponseShape {
    let Some(object) = value.as_object() else {
        return ResponseShape::Invalid;
    };
    match object.get("ok").and_then(Value::as_bool) {
        Some(true) => ResponseShape::Success,
        Some(false) => ResponseShape::Failure,
        None => ResponseShape::Invalid,
    }
}

/// What the natural exit of a verifier worker means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitVerdict {
    /// A result was reported and the worker exited zero: hand the result over.
    Resolve,
    /// The worker exited without ever reporting: there is no result to hand over.
    BeforeResult(i32),
    /// The worker reported and then exited non-zero: a result exists but its producer failed.
    AfterResult(i32),
}

impl ExitVerdict {
    /// The message upstream rejects with, or `None` when there is nothing to report.
    pub fn message(self) -> Option<String> {
        match self {
            Self::Resolve => None,
            Self::BeforeResult(code) => Some(format!(
                "migration verifier exited before reporting a result (code {code})"
            )),
            Self::AfterResult(code) => Some(format!(
                "migration verifier exited after reporting a result (code {code})"
            )),
        }
    }
}

/// Adjudicates a worker's exit code against whether it had reported.
///
/// The two failure branches are distinct on purpose. A worker that exits before reporting may have
/// died mid-parse, so there is no result at all; one that reports and *then* exits non-zero produced
/// a result whose producer then failed, and upstream refuses both rather than preferring the result.
/// The asymmetry is the interesting part: success requires **both** a reported result and a zero
/// exit, so nothing downstream has to know which of the two was the last to arrive.
pub const fn judge_exit(reported_result: bool, code: i32) -> ExitVerdict {
    match (reported_result, code) {
        (true, 0) => ExitVerdict::Resolve,
        (true, _) => ExitVerdict::AfterResult(code),
        (false, code) => ExitVerdict::BeforeResult(code),
    }
}

// =============================================================================================
// migration-verifier.ts: the concurrency gate
// =============================================================================================

/// Process-wide bound on concurrent full-generation verifiers.
///
/// Two, and the reason is resident memory rather than parallelism: each verifier decodes a whole
/// generation, so the bound is "how many generations may be in memory at once". A higher number is
/// a throughput decision that trades against peak RSS on a machine that is already running the
/// session that owns those generations.
pub const MAX_CONCURRENT_VERIFIERS: usize = 2;

/// One in-flight verification's permission to run.
///
/// Dropping it releases the slot — which is how "released in a `finally`" becomes un-forgettable in
/// Rust. Releasing early on cancellation would be wrong for the reason the module docs give: the
/// worker still holds its generation in memory until it actually stops.
#[derive(Debug)]
pub struct Permit<'a> {
    gate: Option<&'a VerifierGate>,
}

impl<'a> Permit<'a> {
    fn granted(gate: &'a VerifierGate) -> Self {
        Self { gate: Some(gate) }
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let Some(gate) = self.gate.take() {
            gate.release();
        }
    }
}

/// A queued waiter: its identity, and the channel a release uses to hand it the slot.
#[derive(Debug)]
struct Waiter {
    id: u64,
    sender: oneshot::Sender<()>,
}

/// The gate's mutable state, behind one lock.
#[derive(Debug, Default)]
struct GateInner {
    /// How many verifications hold a slot. Invariant: `active <= capacity`.
    active: usize,
    /// Waiters in arrival order; `release` serves from the front.
    waiters: VecDeque<Waiter>,
}

/// The process-wide verifier gate.
///
/// See the module docs for the five properties this reproduces. Held as a `std::sync::Mutex`
/// because no field of it is ever held across an `await`: the waiter is enqueued under the lock, the
/// lock is dropped, and only then does the acquire future wait.
#[derive(Debug)]
pub struct VerifierGate {
    inner: Mutex<GateInner>,
    capacity: usize,
}

impl Default for VerifierGate {
    fn default() -> Self {
        Self::new()
    }
}

impl VerifierGate {
    /// A gate bounded by [`MAX_CONCURRENT_VERIFIERS`] — the production gate.
    pub fn new() -> Self {
        Self::with_capacity(MAX_CONCURRENT_VERIFIERS)
    }

    /// A gate with an explicit bound.
    ///
    /// The seam the tests use to make ordering observable with a single slot; upstream has no such
    /// knob, and production callers should use [`Self::new`] or [`verifier_gate`].
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(GateInner::default()),
            capacity: capacity.max(1),
        }
    }

    /// The gate's bound.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many verifications hold a slot right now.
    pub fn active(&self) -> usize {
        self.lock().active
    }

    /// How many verifications are queued right now.
    pub fn waiting(&self) -> usize {
        self.lock().waiters.len()
    }

    fn lock(&self) -> MutexGuard<'_, GateInner> {
        // A panicking verifier must not turn the gate into a permanently unusable one: every field is
        // a plain counter or queue, so the state left behind by a panic is still consistent.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes a slot if one is free, without waiting.
    ///
    /// Returns `None` rather than queueing, which is how the acquire loop below decides whether it
    /// has to enqueue at all. A permit taken here counts immediately, so two racing callers cannot
    /// both see the last slot.
    fn try_take(&self) -> Option<Permit<'_>> {
        let mut inner = self.lock();
        if inner.active < self.capacity {
            inner.active += 1;
            return Some(Permit::granted(self));
        }
        None
    }

    /// Waits for a slot, or for the future to be dropped.
    ///
    /// Dropping the returned future removes the waiter from the queue *immediately*, so a cancelled
    /// verification neither delays the next waiter (as an abandoned entry would until a release
    /// happened to pop it) nor consumes the slot a release hands it.
    pub async fn acquire(&self) -> Permit<'_> {
        loop {
            if let Some(permit) = self.try_take() {
                return permit;
            }
            let (sender, receiver) = oneshot::channel();
            let id = next_waiter_id();
            self.lock().waiters.push_back(Waiter { id, sender });
            let mut pending = PendingWaiter {
                gate: self,
                id,
                armed: true,
            };
            let granted = receiver.await.is_ok();
            pending.disarm();
            if granted {
                // The slot was transferred, not counted: `active` is already correct.
                return Permit::granted(self);
            }
            // The sender was dropped without a grant (the gate rejected a stale waiter). Try again
            // rather than proceeding without a permit.
        }
    }

    /// Returns a slot, handing it to the oldest waiter when there is one.
    ///
    /// The hand-over is the delicate half: with a waiter present, `active` is left alone and the
    /// waiter is granted directly. Decrementing and *then* waking would let a third caller take the
    /// freed slot between the two steps, so the waiter that was already promised a slot would queue
    /// again behind a newcomer — the starvation the FIFO queue exists to prevent.
    fn release(&self) {
        let mut inner = self.lock();
        while let Some(waiter) = inner.waiters.pop_front() {
            if waiter.sender.send(()).is_ok() {
                return;
            }
            // The receiver is gone: that caller's future was dropped without us noticing. Its entry
            // costs nothing to skip, and the slot is still ours to give away.
        }
        inner.active = inner.active.saturating_sub(1);
    }

    /// Removes a waiter that gave up. A no-op once the waiter has been granted.
    fn abandon(&self, id: u64) {
        self.lock().waiters.retain(|waiter| waiter.id != id);
    }
}

/// Removes a dropped acquire future's queue entry.
struct PendingWaiter<'a> {
    gate: &'a VerifierGate,
    id: u64,
    armed: bool,
}

impl PendingWaiter<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingWaiter<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.gate.abandon(self.id);
        }
    }
}

/// Waiter identity, so [`VerifierGate::abandon`] can find exactly one entry.
fn next_waiter_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The process-wide gate, created once.
///
/// Upstream has a module-level `const verificationScheduler = new VerificationScheduler()`. Two
/// separately constructed gates would each allow [`MAX_CONCURRENT_VERIFIERS`], which is double the
/// memory bound the constant exists to enforce — so callers share this one. `VerifierGate::new()`
/// remains public for tests that need an exclusive gate.
pub fn verifier_gate() -> &'static VerifierGate {
    static GATE: OnceLock<VerifierGate> = OnceLock::new();
    GATE.get_or_init(VerifierGate::new)
}

// =============================================================================================
// SHA-256
// =============================================================================================

/// The lowercase hex `SHA-256` of `bytes`.
///
/// The crate has no hash dependency, and the modules that need one keep their own copy
/// (`crate::session::extra::digest_hex` is the same decision for the same reason). This file is compiled
/// through `#[path]` by its test target and therefore cannot import that copy, so it carries its own
/// — verified against the `FIPS 180-4` vectors in that test rather than trusted.
#[allow(
    clippy::many_single_char_names,
    reason = "the round variable names are the ones FIPS 180-4 uses; renaming them would hide a transcription error"
)]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let bit_length = u64::try_from(bytes.len())
        .unwrap_or(u64::MAX)
        .wrapping_mul(8);
    let mut padded = Vec::with_capacity(bytes.len() + 72);
    padded.extend_from_slice(bytes);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0_u32; 64];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut hex = String::with_capacity(64);
    for word in state {
        for byte in word.to_be_bytes() {
            hex.push(nibble(byte >> 4));
            hex.push(nibble(byte & 0x0f));
        }
    }
    hex
}

/// The low nibble of `value` as a lowercase hex digit.
fn nibble(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        _ => char::from(b'a' + (value - 10)),
    }
}

/// The `SHA-256` round constants: the first 32 bits of the fractional parts of the cube roots of the
/// first 64 primes.
const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];
