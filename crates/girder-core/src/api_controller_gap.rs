//! The client-side logic of `packages/api/{job-controller,workspace-files,terminal-controller}`.
//!
//! # What upstream carries, measured
//!
//! | Subpackage | Lines | Sources | Shape |
//! | --- | --- | --- | --- |
//! | `job-controller` | 994 | 8 | `JobController` (Host, 3 Remote methods), `streamJobRows`, `observeJobOutput`, `OutputWaiter`, `ClientJobs`, `ClientJobsModel` |
//! | `workspace-files` | 1,624 | 9 | `WorkspaceFiles` (Host, ~9 Remote methods), `WorkspaceChangeFeed`, `searchWorkspace`, `ChangeFeed` + `Follower` |
//! | `terminal-controller` | 2,857 | 16 | `TerminalController` (Host, 9 Remote methods), `TerminalRosters`, `TerminalInventory`, `TerminalBindings`, `TerminalCloseRequests`, `TerminalWindowHold`, screen model |
//!
//! # What this crate already covers
//!
//! Judged by grepping every upstream export against `crates/`:
//!
//! | Upstream symbol | Local counterpart |
//! |---|---|
//! | `JobController` | `api_job` (descriptors), `job_follow_rpc` (`follow`), `jobs` (registry), `job_tools` |
//! | `streamJobRows`, `observeJobOutput` | *nothing* — `job_follow_rpc` polls by byte cursor instead |
//! | `ClientJobsModel`, `ClientJobs`, `OutputWaiter` | *nothing* |
//! | `WorkspaceFiles` | `workspace_files_rpc`, `workspace_files_extra` |
//! | `WorkspaceChangeFeed` (`changes`) | *nothing* |
//! | `ChangeFeed`, `Follower` | *nothing* |
//! | `searchWorkspace` | `workspace_files_extra` (name-substring walk) |
//! | `TerminalController` | `terminal_rpc`, `terminal_rpc_extra`, `terminal_attach`, `terminal_meta` |
//! | `TerminalRosters`, `TerminalInventory` | *nothing* |
//! | `TerminalBindings`, `TerminalCloseRequests` | *nothing* |
//! | screen model (`renderLines`, `pageLines`) | `terminal_controller_gap` |
//!
//! Measured counts: `JobController`, `streamJobRows`, `observeJobOutput`, `OutputWaiter`,
//! `ClientJobs`, `ClientJobsModel`, `WorkspaceChangeFeed`, `ChangeFeed`, `TerminalRosters`,
//! `TerminalInventory`, `TerminalBindings`, `TerminalCloseRequests` — **zero** occurrences each in
//! `crates/`. `WorkspaceFiles` and `searchWorkspace` have partial local counterparts, and the
//! terminal screen model is already ported.
//!
//! # Is the remaining part really non-streaming logic?
//!
//! The delegation brief said the job-controller core is an async stream and that this build
//! substitutes a byte-cursor poll. That is true of the *Host* half and the check holds up: both
//! generators are `AsyncIterable`s driven by an `AbortSignal`, and `job_follow_rpc` already answers
//! `job.follow` with `{frames, next, lost, done}` — a poll over the same absolute byte coordinate,
//! which is the same contract with the resumption token made explicit.
//!
//! What the poll does *not* replace is the part that is not I/O at all:
//!
//! 1. **Frame shaping.** `outputFrames` cuts one read into frames along a soft byte budget, and the
//!    cursor after a frame is `last.at + byteLength(last.text)`, not the read's `next`. Getting this
//!    wrong re-reads the last line on every poll.
//! 2. **The client model.** `ClientJobsModel` is 216 lines of pure state: whole-set roster
//!    replacement, an accumulated render tail bounded at 128K UTF-16 units without splitting a
//!    surrogate pair, a resume cursor, and the gap lattice (`lossy` frame or a `gapBefore` chunk or
//!    an anchor past `earliest`, all folding into one sticky flag).
//! 3. **Reference-counted stream sharing.** `ClientJobs.acquire`/`releaser` and `TerminalRosters`
//!    are pure bookkeeping over a `Map`, and the subtle rule — a release binds the *entry it was
//!    minted for*, never the map's current occupant — is a rule about a data structure, not about a
//!    socket.
//! 4. **The roster wake predicate.** `streamJobRows` subscribes with "any lifecycle event except
//!    `output`, for an unowned job or this session's". That predicate decides which events become
//!    frames, and it is a pure function of the event.
//! 5. **Terminal inventory reconciliation.** `TerminalInventory` is a load state machine
//!    (`unloaded`/`loading`/`ready`/`failed`/`disposed`) with in-flight mutation reconciliation:
//!    a `list` response that lands after an `update`/`remove` must not resurrect the stale row.
//! 6. **Change-feed follower filtering.** `Follower` queues invalidations until `stat` binds the
//!    Host path, then filters *queued and live* notices by that path with `\\` normalized to `/`.
//! 7. **Terminal roster sharing.** `TerminalRosters` shares one roster stream per Session, replays
//!    its latest roster to a late listener, and keeps a failed entry reopenable.
//!
//! Those seven are what this module ports. Each keeps upstream's *rule* and drops only the carrier:
//! a `Promise` becomes a return value, an `AbortSignal` becomes an explicit `end`, and the async
//! iterators become a queue the caller drains. Nothing here opens a socket, spawns a task, or reads
//! a clock, which is why every property below is a plain assertion rather than a timed test.
//!
//! # What is deliberately *not* ported here
//!
//! The Host-side generators themselves (`observeJobOutput`, `streamJobRows`), because their
//! contract is a live connection: `RemoteRegistry` has no stream mode, so a port would be a
//! different wire contract under the same name. `job_follow_rpc`'s poll is the substitute, and this
//! module supplies the two pure pieces it needs — [`plan_output_frames`] and [`wake_roster`].
//! `TerminalBindings` and `TerminalCloseRequests` are also out of scope: they are client store
//! plumbing whose only interesting content is the screen model, already in
//! [`terminal_controller_gap`](girder_core::terminal_controller_gap).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use girder_core::{JobChunk, JobStatus, MAX_SAFE_INTEGER};

/// Upstream's default coalescing window, in milliseconds.
///
/// Carried as a constant rather than used as a timer: this module decides *what* one flush would
/// emit, and the carrier decides when.
pub const DEFAULT_OBSERVE_FLUSH_MS: u64 = 100;

/// Upstream's soft byte budget per `output` frame.
pub const DEFAULT_OBSERVE_MAX_FRAME_BYTES: usize = 64 * 1024;

/// Upstream's `RENDER_TAIL_LIMIT`: the bounded per-job render tail, in UTF-16 code units.
///
/// Code units, not bytes and not chars: the client's bound exists to protect a DOM string, and a
/// byte bound would let a multi-byte script overshoot it.
pub const RENDER_TAIL_LIMIT: usize = 128 * 1024;

// ---------------------------------------------------------------------------------------------
// Offsets and frame shaping: upstream `observe.ts`'s pure core.
// ---------------------------------------------------------------------------------------------

/// Why a resume offset was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OffsetError {
    /// The offset is beyond the range a JavaScript `number` represents exactly.
    ///
    /// Upstream rejects a non-safe integer. That test has two arms — not integral, and negative —
    /// and neither is expressible in a `u64`, so the one arm that *is* expressible is the one kept:
    /// a byte offset above [`MAX_SAFE_INTEGER`] would arrive as a rounded number and resume in the
    /// middle of a code point.
    #[error("invalid observe offset: expected a non-negative safe integer, got {from}")]
    Unsafe {
        /// The rejected offset.
        from: u64,
    },
}

/// Validates an optional resume offset.
///
/// `None` is legal and means "from the oldest retained byte", which is what a client that just
/// learned the id asks for.
///
/// # Errors
///
/// [`OffsetError::Unsafe`] above [`MAX_SAFE_INTEGER`].
pub fn validate_resume_offset(from: Option<u64>) -> Result<Option<u64>, OffsetError> {
    match from {
        Some(from) if from > MAX_SAFE_INTEGER => Err(OffsetError::Unsafe { from }),
        other => Ok(other),
    }
}

/// Whether a status ends an observation generation.
///
/// `stopping` is not terminal: the kill is on its way and output still arrives. A terminal `status`
/// frame sent at `stopping` would close the stream a beat before the last line.
#[must_use]
pub fn is_terminal(status: JobStatus) -> bool {
    !matches!(status, JobStatus::Running | JobStatus::Stopping)
}

/// One `output` frame: a run of chunks, the offset after them, and whether bytes before them went
/// missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFrame {
    /// The chunks, in ring order.
    pub chunks: Vec<JobChunk>,
    /// The offset to resume from after this frame.
    pub next: u64,
    /// Whether bytes between the requested offset and `chunks` were already evicted.
    pub lossy: bool,
}

/// The UTF-8 byte length of a chunk.
#[must_use]
pub fn chunk_bytes(chunk: &JobChunk) -> u64 {
    u64::try_from(chunk.text.len()).unwrap_or(u64::MAX)
}

/// Splits one read into frames along a soft per-frame byte budget.
///
/// The rules, each of which a naive implementation gets wrong:
///
/// - A frame's `next` is **the end of its last chunk**, not the read's `next`. Upstream:
///   `last.at + byteLength(last.text)`. Using the read's `next` makes a budgeted frame swallow
///   output it never carried, and the client's cursor then skips those bytes.
/// - The final frame of a read keeps the read's `next`, because there may be retained bytes the
///   read did not select; the ring's `next` is `total`, and only the read knows that.
/// - `lossy` is set on the **first** frame the read produces and no later one: the loss happened
///   before the first chunk, and repeating it on every frame would tell a client its position kept
///   breaking.
/// - A `lossy` read with no chunks still yields one empty frame. Otherwise the loss would be
///   dropped and the client would keep a position it can no longer trust.
/// - A chunk larger than the budget ships whole: retention's cap is the hard bound on a chunk, and
///   splitting inside a chunk would cut a UTF-8 sequence.
///
/// A `max_frame_bytes` of zero flushes every chunk, which is what the comparison
/// `batch_bytes >= budget` says when the budget is zero. Upstream's config floor is 1, so this arm
/// is unreachable through the service; it is defined rather than panicking because a zero budget is
/// a caller's mistake, not a corruption.
#[must_use]
pub fn plan_output_frames(
    chunks: &[JobChunk],
    next: u64,
    lossy: bool,
    max_frame_bytes: usize,
) -> Vec<OutputFrame> {
    let budget = u64::try_from(max_frame_bytes).unwrap_or(u64::MAX);
    let mut frames = Vec::new();
    let mut batch: Vec<JobChunk> = Vec::new();
    let mut batch_bytes: u64 = 0;
    let mut flagged = lossy;

    for chunk in chunks {
        batch.push(chunk.clone());
        batch_bytes = batch_bytes.saturating_add(chunk_bytes(chunk));
        if batch_bytes < budget {
            continue;
        }
        let end = batch
            .last()
            .map_or(next, |last| last.at.saturating_add(chunk_bytes(last)));
        frames.push(OutputFrame {
            chunks: std::mem::take(&mut batch),
            next: end,
            lossy: flagged,
        });
        flagged = false;
        batch_bytes = 0;
    }

    if !batch.is_empty() || flagged {
        frames.push(OutputFrame {
            chunks: batch,
            next,
            lossy: flagged,
        });
    }
    frames
}

// ---------------------------------------------------------------------------------------------
// The roster's visible set and its wake predicate: upstream `rows.ts`.
// ---------------------------------------------------------------------------------------------

/// The output ring's coordinates, as `JobView.output` carries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobOutput {
    /// The offset the next chunk starts at.
    pub total: u64,
    /// The oldest retained byte; greater than zero exactly when retention dropped the head.
    pub earliest: u64,
}

/// The read-only projection of one job that a roster frame carries.
///
/// A narrower shape than upstream's `JobView` in the fields this module does not reason about
/// (`progress`, `detail`, `spillPaths`, timestamps), and *wider* than
/// [`api_job::JobView`](girder_core::api_job::JobView) in the two the client model needs:
/// `owner` decides visibility and `output.earliest` decides the opening gap mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobView {
    /// The registry id.
    pub id: String,
    /// The producer kind.
    pub kind: String,
    /// The one-line label.
    pub label: String,
    /// The owning session; `None` for an unowned job, which every caller sees.
    pub owner: Option<String>,
    /// The lifecycle state.
    pub status: JobStatus,
    /// The ring's coordinates.
    pub output: JobOutput,
}

/// One lifecycle commit, as a roster watcher sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobEventKind {
    /// A job was registered.
    Registered,
    /// Its progress line changed.
    Progress,
    /// It was asked to stop.
    Stopping,
    /// It settled.
    Settled,
    /// Its owner's teardown dropped the record.
    Removed,
    /// It wrote output.
    Output,
}

/// One commit from the job registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEvent {
    /// What happened.
    pub kind: JobEventKind,
    /// The job.
    pub job_id: String,
    /// Its owner, when it has one.
    pub owner: Option<String>,
}

/// Whether one commit refreshes a session's roster.
///
/// `output` never does. The docstring's reason is load-bearing: a settled projection already carries
/// the final byte count, so a roster frame per append would be a frame per line of a build log, and
/// the roster would be the busiest stream in the system for no new information.
///
/// Every other commit does, when the job is unowned (visible to everyone) or owned by this session.
#[must_use]
pub fn wake_roster(event: &JobEvent, session: &str) -> bool {
    if event.kind == JobEventKind::Output {
        return false;
    }
    match event.owner.as_deref() {
        None => true,
        Some(owner) => owner == session,
    }
}

/// The set one session can see: its own jobs plus every unowned one, in id order.
///
/// A free function over a slice rather than a method on the registry, because the roster is a
/// *projection*: two callers with the same registry and different sessions must get different sets
/// and neither may mutate anything to compute it.
#[must_use]
pub fn visible_roster(jobs: &[JobView], session: &str) -> Vec<JobView> {
    let mut rows: Vec<JobView> = jobs
        .iter()
        .filter(|job| match job.owner.as_deref() {
            None => true,
            Some(owner) => owner == session,
        })
        .cloned()
        .collect();
    rows.sort_by(|left, right| left.id.cmp(&right.id));
    rows
}

// ---------------------------------------------------------------------------------------------
// The bounded render tail: the one place a client string is cut.
// ---------------------------------------------------------------------------------------------

/// The UTF-16-safe tail of `text` no longer than `limit` code units.
///
/// A cut that landed on a low surrogate would take half a pair and render a replacement glyph, so
/// the cut advances by one when it does — the same correction upstream makes with
/// `text.charCodeAt(cut)`. A `limit` of zero yields an empty string rather than panicking:
/// [`RENDER_TAIL_LIMIT`] is never zero, but this helper is public and a caller may ask for nothing.
#[must_use]
pub fn utf16_tail(text: &str, limit: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= limit {
        return text.to_owned();
    }
    let mut cut = units.len() - limit;
    if cut >= units.len() {
        return String::new();
    }
    if (0xDC00..=0xDFFF).contains(&units[cut]) {
        cut += 1;
    }
    String::from_utf16_lossy(&units[cut..])
}

/// The UTF-16 length of a string, which is what the render bound counts.
#[must_use]
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

// ---------------------------------------------------------------------------------------------
// The client jobs model: upstream `client/model.ts`.
// ---------------------------------------------------------------------------------------------

/// One observed job's live view state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedJob {
    /// The job this view belongs to.
    pub job_id: String,
    /// The accumulated render tail, bounded to [`RENDER_TAIL_LIMIT`].
    pub text: String,
    /// Whether bytes before `text` were dropped: eviction, resume gap, or the render bound.
    ///
    /// Sticky. A flag that could clear would let a client that already lost its head render the
    /// remainder as if it were the whole log.
    pub gap_before: bool,
    /// Whether the observation stream is open and the job has not settled.
    pub streaming: bool,
    /// The terminal observation failure, when the stream ended abnormally.
    pub error: Option<String>,
}

/// One session's roster plus every observed job's view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobsSnapshot {
    /// The visible jobs per watched session. A session that sees no job has no key.
    pub rows: BTreeMap<String, Vec<JobView>>,
    /// The live observation state per job id.
    pub observed: BTreeMap<String, ObservedJob>,
}

/// One job's `opened` anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// The job projection at open time.
    pub job: JobView,
    /// The offset the first `output` frame continues from.
    pub from: u64,
}

/// The client's rosters and per-job observation state.
///
/// Upstream's `ClientJobsModel` with the change notification replaced by a revision counter: a
/// subscriber there is re-run by a store, and a revision is the same information in a form a caller
/// can compare. The snapshot is rebuilt per call rather than cached, which trades upstream's
/// identity-stable object for the absence of interior mutability; every other rule is kept.
#[derive(Debug, Default)]
pub struct JobsModel {
    rows: BTreeMap<String, Vec<JobView>>,
    observed: BTreeMap<String, ObservedState>,
    revision: u64,
}

#[derive(Debug, Clone)]
struct ObservedState {
    view: ObservedJob,
    /// The resume offset for the next generation's `from`.
    cursor: Option<u64>,
}

impl JobsModel {
    /// An empty model: no rosters, no observations.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many state changes have been applied.
    ///
    /// A subscriber compares this instead of being called back, and a test asserts a no-op really
    /// was one by watching the number stay still.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The identity-stable shape of the current state.
    #[must_use]
    pub fn snapshot(&self) -> JobsSnapshot {
        JobsSnapshot {
            rows: self.rows.clone(),
            observed: self
                .observed
                .values()
                .map(|state| (state.view.job_id.clone(), state.view.clone()))
                .collect(),
        }
    }

    /// Replaces one session's roster with a frame's whole set.
    ///
    /// An empty set is stored as an absent key, so a consumer reads absence rather than a sentinel
    /// empty vector, and a repeated empty frame changes nothing (and does not bump the revision).
    pub fn rows_replaced(&mut self, session: &str, jobs: Vec<JobView>) -> bool {
        let changed = if jobs.is_empty() {
            self.rows.remove(session).is_some()
        } else {
            let same = self.rows.get(session) == Some(&jobs);
            if !same {
                self.rows.insert(session.to_owned(), jobs);
            }
            !same
        };
        if changed {
            self.revision = self.revision.saturating_add(1);
        }
        changed
    }

    /// Drops one session's roster after its last watcher stops or its stream fails.
    pub fn rows_dropped(&mut self, session: &str) -> bool {
        let changed = self.rows.remove(session).is_some();
        if changed {
            self.revision = self.revision.saturating_add(1);
        }
        changed
    }

    /// The resume offset for one job's next observation generation.
    #[must_use]
    pub fn cursor_of(&self, job_id: &str) -> Option<u64> {
        self.observed.get(job_id).and_then(|state| state.cursor)
    }

    /// Installs or resets observation state when an anchor arrives.
    ///
    /// Three ways an opening read can already owe a gap mark, and all three must fold in:
    ///
    /// - the anchor sits below the job's earliest retained byte, so the ring lost the head;
    /// - a *fresh* view anchored past offset zero, which means the head was evicted before anyone
    ///   looked (a resume that already accumulated text keeps its own recorded state instead);
    /// - a previous generation had already recorded a gap on this same view.
    pub fn observe_opened(&mut self, job_id: &str, opened: &Opened) {
        let existing = self.observed.get(job_id).cloned();
        let accumulated = existing
            .as_ref()
            .map(|state| state.view.text.clone())
            .unwrap_or_default();
        let fresh_past_head = accumulated.is_empty() && opened.from > 0;
        let recorded = existing.as_ref().is_some_and(|state| state.view.gap_before);
        let below_head = opened.from < opened.job.output.earliest;
        let view = ObservedJob {
            job_id: job_id.to_owned(),
            text: accumulated,
            gap_before: recorded || below_head || fresh_past_head,
            streaming: true,
            error: None,
        };
        self.observed.insert(
            job_id.to_owned(),
            ObservedState {
                view,
                cursor: Some(opened.from),
            },
        );
        self.revision = self.revision.saturating_add(1);
    }

    /// Appends one coalesced `output` frame to the bounded render tail.
    ///
    /// A frame for an untracked job is ignored rather than creating a view: upstream's `/* v8 ignore
    /// next */ if (state === undefined) return` records the same invariant — frames arrive only
    /// between `opened` and the terminal `status`.
    pub fn observe_output(&mut self, job_id: &str, frame: &OutputFrame) {
        let Some(state) = self.observed.get_mut(job_id) else {
            return;
        };
        let mut text = state.view.text.clone();
        let mut chunk_gap = false;
        for chunk in &frame.chunks {
            chunk_gap |= chunk.gap;
            text.push_str(&chunk.text);
        }
        let mut gap_before = state.view.gap_before || frame.lossy || chunk_gap;
        if utf16_len(&text) > RENDER_TAIL_LIMIT {
            text = utf16_tail(&text, RENDER_TAIL_LIMIT);
            gap_before = true;
        }
        state.view = ObservedJob {
            job_id: state.view.job_id.clone(),
            text,
            gap_before,
            streaming: state.view.streaming,
            error: state.view.error.clone(),
        };
        state.cursor = Some(frame.next);
        self.revision = self.revision.saturating_add(1);
    }

    /// Closes the live view once the terminal `status` frame arrived.
    ///
    /// The roster row carries the settled projection, so the view stops streaming but keeps its
    /// text: a panel that blanked on settlement would discard the only record the user was reading.
    pub fn observe_settled(&mut self, job_id: &str) {
        let Some(state) = self.observed.get_mut(job_id) else {
            return;
        };
        state.view.streaming = false;
        self.revision = self.revision.saturating_add(1);
    }

    /// Records a terminal observation failure.
    ///
    /// A failure before the anchor still creates a view: a job that vanished between the click and
    /// the open owes the panel a notice, and a later successful anchor replaces this view and
    /// resumes from no cursor.
    pub fn observe_failed(&mut self, job_id: &str, error: impl Into<String>) {
        let error = error.into();
        match self.observed.get_mut(job_id) {
            None => {
                self.observed.insert(
                    job_id.to_owned(),
                    ObservedState {
                        view: ObservedJob {
                            job_id: job_id.to_owned(),
                            text: String::new(),
                            gap_before: false,
                            streaming: false,
                            error: Some(error),
                        },
                        cursor: None,
                    },
                );
            }
            Some(state) => {
                state.view.streaming = false;
                state.view.error = Some(error);
            }
        }
        self.revision = self.revision.saturating_add(1);
    }

    /// Drops observation state after the last observer stops.
    pub fn observe_stopped(&mut self, job_id: &str) -> bool {
        let changed = self.observed.remove(job_id).is_some();
        if changed {
            self.revision = self.revision.saturating_add(1);
        }
        changed
    }

    /// One job's view, for assertions and for a panel that already knows the id.
    #[must_use]
    pub fn observed(&self, job_id: &str) -> Option<&ObservedJob> {
        self.observed.get(job_id).map(|state| &state.view)
    }
}

// ---------------------------------------------------------------------------------------------
// Reference-counted streams: upstream `client/service.ts`'s acquire/release core.
// ---------------------------------------------------------------------------------------------

/// A handle to one acquired stream.
///
/// Carries the entry's identity, not the key: that is the whole point. A release must decrement the
/// entry it was minted for, because a later acquire on the same key may have replaced a stopped
/// entry, and releasing through the key alone would tear down the *new* stream's references.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    entry: u64,
}

impl Lease {
    /// The entry this lease refers to, for diagnostics and equality in tests.
    #[must_use]
    pub fn entry(self) -> u64 {
        self.entry
    }
}

/// What an `acquire` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Acquired {
    /// The lease to release later.
    pub lease: Lease,
    /// Whether this acquire *started* the stream (rather than joining a live one).
    pub started: bool,
    /// The reference count after the acquire.
    pub refs: usize,
}

/// What a `release` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Released {
    /// Another watcher still holds it.
    RefCounted {
        /// The count after the release.
        remaining: usize,
    },
    /// The last watcher left and the stream must now be disposed.
    Disposed {
        /// Whether the model's state for the key should be cleared.
        ///
        /// False when a successor already acquired the key during the dispose round-trip, which is
        /// why the caller must ask rather than assume.
        cleared_state: bool,
    },
    /// The lease named an entry that is no longer tracked; nothing was decremented.
    Stale,
}

/// One shared stream's bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamEntry {
    id: u64,
    refs: usize,
    stopped: bool,
}

/// Reference-counted streams keyed by session or job id.
#[derive(Debug, Default)]
pub struct StreamBook {
    entries: BTreeMap<String, StreamEntry>,
    key_of_entry: BTreeMap<u64, String>,
    next_entry: u64,
}

impl StreamBook {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Joins the live entry under `key` or starts one.
    ///
    /// A *stopped* entry is not joined: its stream is being torn down, and sharing it would hand a
    /// new watcher a subscription that is already closed.
    pub fn acquire(&mut self, key: &str) -> Acquired {
        if let Some(entry) = self.entries.get_mut(key) {
            if !entry.stopped {
                entry.refs = entry.refs.saturating_add(1);
                return Acquired {
                    lease: Lease { entry: entry.id },
                    started: false,
                    refs: entry.refs,
                };
            }
        }
        let id = self.next_entry;
        self.next_entry = self.next_entry.saturating_add(1);
        self.entries.insert(
            key.to_owned(),
            StreamEntry {
                id,
                refs: 1,
                stopped: false,
            },
        );
        self.key_of_entry.insert(id, key.to_owned());
        Acquired {
            lease: Lease { entry: id },
            started: true,
            refs: 1,
        }
    }

    /// Releases one watcher's reference.
    pub fn release(&mut self, lease: Lease) -> Released {
        let Some(key) = self.key_of_entry.get(&lease.entry).cloned() else {
            return Released::Stale;
        };
        let Some(entry) = self.entries.get_mut(&key) else {
            return Released::Stale;
        };
        if entry.refs > 1 {
            entry.refs -= 1;
            return Released::RefCounted {
                remaining: entry.refs,
            };
        }
        entry.refs = 0;
        entry.stopped = true;
        self.entries.remove(&key);
        self.key_of_entry.remove(&lease.entry);
        // A successor that acquired the key while this dispose was in flight holds a *different*
        // entry, so the state must survive. Asking the map rather than assuming is the difference
        // between a stale clear and a correct one.
        Released::Disposed {
            cleared_state: !self.entries.contains_key(&key),
        }
    }

    /// Stops every stream, as a plugin's teardown does, and returns the keys that were open.
    pub fn dispose_all(&mut self) -> Vec<String> {
        let keys: Vec<String> = self.entries.keys().cloned().collect();
        for entry in self.entries.values_mut() {
            entry.stopped = true;
            entry.refs = 0;
        }
        self.entries.clear();
        self.key_of_entry.clear();
        keys
    }

    /// Whether a live stream is open under `key`.
    #[must_use]
    pub fn is_open(&self, key: &str) -> bool {
        self.entries.get(key).is_some_and(|entry| !entry.stopped)
    }

    /// The reference count under `key`, or zero.
    #[must_use]
    pub fn refs_of(&self, key: &str) -> usize {
        self.entries.get(key).map_or(0, |entry| entry.refs)
    }

    /// Every key with a live stream, in order.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }
}

// ---------------------------------------------------------------------------------------------
// Terminal inventory: upstream `terminal-controller/client/inventory.ts`.
// ---------------------------------------------------------------------------------------------

/// One retained terminal's metadata, as a roster reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalInfo {
    /// The terminal id.
    pub id: String,
    /// Its title.
    pub title: String,
    /// Whether a window is currently attached.
    pub attached: bool,
}

impl TerminalInfo {
    /// Builds metadata.
    #[must_use]
    pub fn new(id: impl Into<String>, title: impl Into<String>, attached: bool) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            attached,
        }
    }
}

/// Identifies one in-flight `list` request.
///
/// Upstream compares the `Loading` object's identity; a number does the same job without keeping a
/// promise alive, and it survives into a test's assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestId(u64);

impl RequestId {
    /// The request's number, for assertions.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// What a caller should do when it asks for the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    /// The cached list, which the caller may read through [`Inventory::items`].
    Cached,
    /// A request is already in flight; wait for its completion.
    Pending {
        /// The request to wait on.
        request: RequestId,
    },
    /// Start this request and report back through [`Inventory::complete`] or [`Inventory::fail`].
    Start {
        /// The request to run.
        request: RequestId,
    },
    /// A retained failure, which stays cached until a refresh or a reset.
    Failed,
    /// The inventory was disposed; nothing may be read or started.
    Disposed,
}

/// What applying a result did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The result became the cached list.
    Applied,
    /// The result belonged to a superseded request and was dropped.
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Unloaded,
    Loading(RequestId),
    Ready,
    Failed,
    Disposed,
}

/// One Session's cached terminal list, its retained failure, and its in-flight reconciliation.
///
/// The rule worth stating: a `list` response that lands *after* an `update` or a `remove` must not
/// resurrect the row the mutation already changed or deleted. Upstream records the mutations in a
/// side table and replays them over the response; this does the same, because the alternative —
/// applying the response verbatim — re-adds a terminal the user just closed and leaves it on screen
/// until the next roster frame.
#[derive(Debug)]
pub struct Inventory {
    phase: Phase,
    items: BTreeMap<String, TerminalInfo>,
    updates: BTreeMap<String, Option<TerminalInfo>>,
    roster: Option<BTreeSet<String>>,
    next_request: u64,
    /// The retained failure message, kept while the phase is `failed`.
    message: Option<String>,
}

impl Default for Inventory {
    fn default() -> Self {
        Self::new()
    }
}

impl Inventory {
    /// An unloaded inventory.
    #[must_use]
    pub fn new() -> Self {
        Self {
            phase: Phase::Unloaded,
            items: BTreeMap::new(),
            updates: BTreeMap::new(),
            roster: None,
            next_request: 0,
            message: None,
        }
    }

    /// Reads the cached list, joins the in-flight request, or starts one.
    ///
    /// `refresh` is the explicit "replace a settled result or a settled failure" arm; without it a
    /// cached failure is returned again rather than retried, which is what keeps a dead Host from
    /// being hammered by every pane that mounts.
    pub fn read(&mut self, refresh: bool) -> Read {
        match self.phase {
            Phase::Disposed => Read::Disposed,
            Phase::Loading(request) => Read::Pending { request },
            Phase::Ready if !refresh => Read::Cached,
            Phase::Failed if !refresh => Read::Failed,
            Phase::Unloaded | Phase::Ready | Phase::Failed => Read::Start {
                request: self.start(),
            },
        }
    }

    fn start(&mut self) -> RequestId {
        let request = RequestId(self.next_request);
        self.next_request = self.next_request.saturating_add(1);
        self.phase = Phase::Loading(request);
        self.updates.clear();
        self.roster = None;
        request
    }

    /// Applies one `list` response.
    ///
    /// Three filters, in this order, and each is a bug if dropped:
    ///
    /// 1. a stale response (a refresh overtook it) is ignored;
    /// 2. when a whole roster arrived while loading, terminals the roster does not list are dropped
    ///    — unless a mutation already touched them, which step 3 owns;
    /// 3. the recorded mutations win over the response.
    pub fn complete(&mut self, request: RequestId, listed: Vec<TerminalInfo>) -> Applied {
        if self.phase != Phase::Loading(request) {
            return Applied::Ignored;
        }
        let mut items: BTreeMap<String, TerminalInfo> = listed
            .into_iter()
            .map(|info| (info.id.clone(), info))
            .collect();
        if let Some(roster) = self.roster.take() {
            items.retain(|id, _| roster.contains(id) || self.updates.contains_key(id));
        }
        for (id, info) in std::mem::take(&mut self.updates) {
            match info {
                None => {
                    items.remove(&id);
                }
                Some(info) => {
                    items.insert(id, info);
                }
            }
        }
        self.items = items;
        self.phase = Phase::Ready;
        Applied::Applied
    }

    /// Records that one request failed.
    pub fn fail(&mut self, request: RequestId, message: impl Into<String>) -> Applied {
        if self.phase != Phase::Loading(request) {
            return Applied::Ignored;
        }
        self.phase = Phase::Failed;
        self.message = Some(message.into());
        self.updates.clear();
        self.roster = None;
        Applied::Applied
    }

    /// Preserves acknowledged mutations and attachment metadata across a pending response.
    pub fn update(&mut self, info: TerminalInfo) {
        if self.phase == Phase::Disposed {
            return;
        }
        if let Phase::Loading(_) = self.phase {
            self.updates.insert(info.id.clone(), Some(info.clone()));
        }
        self.items.insert(info.id.clone(), info);
    }

    /// Removes a terminal after a confirmed closure or unavailability.
    pub fn remove(&mut self, id: &str) {
        if self.phase == Phase::Disposed {
            return;
        }
        if let Phase::Loading(_) = self.phase {
            self.updates.insert(id.to_owned(), None);
        }
        self.items.remove(id);
    }

    /// Applies a whole Host roster: listed terminals update, the others leave.
    pub fn reconcile(&mut self, terminals: Vec<TerminalInfo>) {
        if self.phase == Phase::Disposed {
            return;
        }
        let listed: BTreeSet<String> = terminals.iter().map(|info| info.id.clone()).collect();
        let stale: Vec<String> = self
            .items
            .keys()
            .filter(|id| !listed.contains(*id))
            .cloned()
            .collect();
        for id in stale {
            self.remove(&id);
        }
        for info in terminals {
            self.update(info);
        }
        if let Phase::Loading(_) = self.phase {
            self.roster = Some(listed);
        }
    }

    /// Replaces discovery after a connection change.
    ///
    /// `None` for an unused or disposed inventory: a Session that never listed anything must not
    /// start listing because the transport changed under it.
    pub fn reset(&mut self) -> Option<RequestId> {
        if matches!(self.phase, Phase::Unloaded | Phase::Disposed) {
            return None;
        }
        self.phase = Phase::Unloaded;
        self.message = None;
        Some(self.start())
    }

    /// Discards metadata and prevents late responses from publishing.
    pub fn dispose(&mut self) {
        self.phase = Phase::Disposed;
        self.items.clear();
        self.updates.clear();
        self.roster = None;
        self.message = None;
    }

    /// The cached list.
    #[must_use]
    pub fn items(&self) -> &BTreeMap<String, TerminalInfo> {
        &self.items
    }

    /// The retained failure message, when the phase is `failed`.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Whether the inventory has been disposed.
    #[must_use]
    pub fn is_disposed(&self) -> bool {
        self.phase == Phase::Disposed
    }

    /// Whether a request is in flight.
    #[must_use]
    pub fn is_loading(&self) -> bool {
        matches!(self.phase, Phase::Loading(_))
    }
}

// ---------------------------------------------------------------------------------------------
// Change-feed followers: upstream `workspace-files/client/change-feed.ts`.
// ---------------------------------------------------------------------------------------------

/// What a follower reports to its consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Re-read metadata: the target's identity was resolved and everything queued is suspect.
    Refresh,
    /// The target changed, at this Host version.
    Changed {
        /// The Host's version token.
        version: u64,
    },
    /// The target is gone.
    Absent,
}

/// The follower key of one absolute path, with `\\` normalized to `/`.
///
/// Normalization is not cosmetic: a Windows Host and a POSIX-looking configured path disagree on the
/// separator, and a path that does not compare equal silently filters out every change.
#[must_use]
pub fn path_key(path: &str) -> String {
    path.replace('\\', "/")
}

/// The stream key of one Session and requested path.
///
/// Upstream's `JSON.stringify([sessionId, path])`. Two components, so a Session id that embeds the
/// separator cannot collide with a different Session's path.
#[must_use]
pub fn feed_key(session: &str, path: &str) -> String {
    format!(
        "[{},{}]",
        serde_json::Value::String(session.to_owned()),
        serde_json::Value::String(path.to_owned())
    )
}

/// One follower's queue: invalidations wait here until its consumer pulls them.
///
/// Filtering happens at *pull* time, not at push time, and that is the whole design: the follower is
/// registered before `stat` supplies the Host path, so a change that arrives during the wait must be
/// judged against the path the follower binds afterwards. A push-time filter would either drop the
/// change or deliver it as if the target it referred to were known.
#[derive(Debug, Default)]
pub struct Follower {
    pending: VecDeque<(String, Notice)>,
    host_key: Option<String>,
    started: bool,
    ended: bool,
}

impl Follower {
    /// A follower whose Host path is not yet known.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Selects the Host path for queued and future changes.
    pub fn bind(&mut self, absolute_path: &str) {
        self.host_key = Some(path_key(absolute_path));
    }

    /// The Host acknowledged an active subscription and resolved the workspace root.
    pub fn start(&mut self) {
        self.started = true;
    }

    /// Queues one notice for the consumer.
    pub fn push(&mut self, notice: Notice, key: &str) {
        self.pending.push_back((key.to_owned(), notice));
    }

    /// Delivers what is queued, then lets the iterator finish.
    pub fn end(&mut self) {
        self.ended = true;
    }

    /// Whether the Host ever acknowledged the subscription.
    #[must_use]
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Whether the follower has been ended.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// What is deliverable right now, drained in order.
    ///
    /// The synchronous substitute for upstream's `for await`: it returns everything queued that
    /// passes the filter and unblocks immediately when nothing is, where upstream awaits a wake.
    /// A caller that sees an empty vector and [`Self::is_ended`] is done; one that sees an empty
    /// vector and is not ended has nothing to render yet.
    pub fn drain(&mut self) -> Vec<Notice> {
        let mut out = Vec::new();
        while let Some((key, notice)) = self.pending.pop_front() {
            let visible = matches!(notice, Notice::Refresh)
                || self.host_key.as_deref().is_none_or(|host| host == key);
            if visible {
                out.push(notice);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Terminal rosters: upstream `terminal-controller/client/roster.ts`.
// ---------------------------------------------------------------------------------------------

/// A handle to one roster watcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watch {
    listener: u64,
    session: u64,
}

impl Watch {
    /// The listener number, for assertions.
    #[must_use]
    pub fn listener(self) -> u64 {
        self.listener
    }

    /// The entry number of the Session stream this watcher joined, for assertions.
    #[must_use]
    pub fn session(self) -> u64 {
        self.session
    }
}

/// What `watch` produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watching {
    /// The lease to release later.
    pub watch: Watch,
    /// Whether this watcher had to open the stream.
    pub opened: bool,
    /// The roster to deliver immediately, when one is already known.
    ///
    /// A listener added to a live stream first receives its latest roster — the same replay rule as
    /// [`StreamBook`], except the thing replayed is data rather than a subscription.
    pub replay: Option<Vec<TerminalInfo>>,
}

/// What releasing a watcher did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watched {
    /// Other listeners remain on this Session.
    Shared,
    /// The last listener left and the stream must be disposed.
    Disposed,
    /// The handle named a stream that is no longer tracked.
    Stale,
}

#[derive(Debug, Clone)]
struct RosterEntry {
    id: u64,
    listeners: BTreeSet<u64>,
    latest: Option<Vec<TerminalInfo>>,
    stopped: bool,
    /// Whether a live stream is attached; false after a failure, which keeps the entry reopenable.
    live: bool,
}

/// Per-Session terminal roster sharing.
///
/// Upstream's `TerminalRosters`, minus the carrier. The rules that survive the port:
///
/// - one stream per Session, shared by its watchers;
/// - a watcher added to a live stream is handed the latest roster at once;
/// - the last watcher leaving stops the stream and forgets the Session's entry;
/// - a stream that *failed* leaves its entry in place with no live stream, so
///   [`Self::resume`] or the next [`Self::watch`] reopens it for the remaining watchers instead of
///   leaving them with nothing.
#[derive(Debug, Default)]
pub struct RosterBook {
    entries: BTreeMap<String, RosterEntry>,
    session_of_entry: BTreeMap<u64, String>,
    next_entry: u64,
    next_listener: u64,
}

impl RosterBook {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Watches one Session's terminals.
    pub fn watch(&mut self, session: &str) -> Watching {
        let entry = self.entries.entry(session.to_owned()).or_insert_with(|| {
            let id = self.next_entry;
            self.next_entry = self.next_entry.saturating_add(1);
            RosterEntry {
                id,
                listeners: BTreeSet::new(),
                latest: None,
                stopped: false,
                live: false,
            }
        });
        if entry.stopped {
            entry.stopped = false;
            entry.latest = None;
            entry.live = false;
        }
        let opened = !entry.live;
        if opened {
            entry.live = true;
        }
        let id = entry.id;
        let listener = self.next_listener;
        self.next_listener = self.next_listener.saturating_add(1);
        entry.listeners.insert(listener);
        let replay = entry.latest.clone();
        self.session_of_entry.insert(id, session.to_owned());
        Watching {
            watch: Watch {
                listener,
                session: id,
            },
            opened,
            replay,
        }
    }

    /// Releases one watcher.
    pub fn release(&mut self, watch: Watch) -> Watched {
        let Some(session) = self.session_of_entry.get(&watch.session).cloned() else {
            return Watched::Stale;
        };
        let Some(entry) = self.entries.get_mut(&session) else {
            return Watched::Stale;
        };
        if !entry.listeners.remove(&watch.listener) {
            return Watched::Stale;
        }
        if !entry.listeners.is_empty() {
            return Watched::Shared;
        }
        entry.stopped = true;
        entry.live = false;
        let id = entry.id;
        self.entries.remove(&session);
        self.session_of_entry.remove(&id);
        Watched::Disposed
    }

    /// Records one whole roster and returns which Sessions must deliver it.
    pub fn received(&mut self, session: &str, terminals: Vec<TerminalInfo>) -> bool {
        let Some(entry) = self.entries.get_mut(session) else {
            return false;
        };
        entry.latest = Some(terminals);
        entry.live = true;
        true
    }

    /// Records that one Session's stream failed while watched.
    ///
    /// The entry survives with no live stream: its listeners still want rosters, and
    /// [`Self::resume`] reopens it. Dropping the entry would silently stop delivering to a watcher
    /// that never released.
    pub fn stream_failed(&mut self, session: &str) -> bool {
        let Some(entry) = self.entries.get_mut(session) else {
            return false;
        };
        entry.live = false;
        true
    }

    /// The Sessions whose streams must reopen, in order.
    ///
    /// An entry is reopenable exactly when it still has listeners and no live stream, which is the
    /// state a failure leaves and a `resume` after a connection change consumes.
    #[must_use]
    pub fn resume(&mut self) -> Vec<String> {
        let mut reopen = Vec::new();
        for (session, entry) in &mut self.entries {
            if !entry.live && !entry.listeners.is_empty() {
                entry.live = true;
                reopen.push(session.clone());
            }
        }
        reopen
    }

    /// Stops everything, as a plugin teardown does, and returns the Sessions that were watched.
    pub fn dispose(&mut self) -> Vec<String> {
        let sessions: Vec<String> = self.entries.keys().cloned().collect();
        for entry in self.entries.values_mut() {
            entry.stopped = true;
            entry.live = false;
            entry.listeners.clear();
        }
        self.entries.clear();
        self.session_of_entry.clear();
        sessions
    }

    /// How many watchers one Session has.
    #[must_use]
    pub fn watchers_of(&self, session: &str) -> usize {
        self.entries
            .get(session)
            .map_or(0, |entry| entry.listeners.len())
    }

    /// The latest roster known for one Session.
    #[must_use]
    pub fn latest_of(&self, session: &str) -> Option<&[TerminalInfo]> {
        self.entries
            .get(session)
            .and_then(|entry| entry.latest.as_deref())
    }

    /// Every watched Session, in order.
    #[must_use]
    pub fn sessions(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }
}
