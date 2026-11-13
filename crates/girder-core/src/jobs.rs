//! Background jobs: long-running work with readable output.
//!
//! A tool call blocks; a job does not. The user starts `npm run dev`, gets an id, and reads its output
//! later while the conversation continues. That difference drives everything here.
//!
//! # The output ring
//!
//! A job's output can grow without bound — a dev server logs forever. [`OutputRing`] keeps a bounded
//! tail and discards the head, but **offsets never move**: a chunk keeps the offset it was assigned
//! when appended, so a reader's saved position stays meaningful after a trim.
//!
//! When a trim removes bytes a reader has not yet seen, the next chunk carries [`JobChunk::gap`]. That
//! is the difference between "the output started here" and "the output you missed was dropped": a
//! reader that sees a gap knows its position is no longer continuous, while one that does not would
//! silently read across a hole.
//!
//! # The lifecycle
//!
//! ```text
//! running ──kill──> stopping ──> killed
//!    │                            ▲
//!    ├──────────> completed       │
//!    └──────────> failed        (any terminal state is final)
//! ```
//!
//! `stopping` exists because a kill is not instantaneous. A process needs time to handle SIGTERM,
//! and a job that reported `killed` while its children were still alive would let a caller start a
//! second one against a port the first still holds.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::scope::BoxFut;

/// Identifies one job.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(String);

impl JobId {
    /// Builds an id.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty() {
            return None;
        }
        Some(Self(value))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a job is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Working.
    Running,
    /// Asked to stop, not yet stopped.
    Stopping,
    /// Finished on its own successfully.
    Completed,
    /// Stopped because it was asked to.
    Killed,
    /// Finished with an error.
    Failed,
}

impl Status {
    /// Whether the job has reached a terminal state.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Killed | Self::Failed)
    }

    /// Whether the job still occupies a slot.
    ///
    /// True for `stopping` as well as `running`: a caller that only checked for `running` would start
    /// a second job against resources the first has not released.
    #[must_use]
    pub fn is_live(self) -> bool {
        matches!(self, Self::Running | Self::Stopping)
    }

    /// The wire name, matching the upstream spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Killed => "killed",
            Self::Failed => "failed",
        }
    }
}

/// Which stream a chunk came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
    /// Producer narration — a workflow's phase lines — visible to observers but not to a model
    /// reading the job's output.
    Log,
}

/// One chunk of output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobChunk {
    /// The absolute offset of this chunk's first byte.
    ///
    /// Never changes, including after a trim: a reader's saved position stays meaningful.
    pub at: u64,
    /// The text exactly as appended.
    pub text: String,
    /// Which stream it came from.
    pub channel: Channel,
    /// Whether bytes just before this chunk were lost.
    pub gap: bool,
}

/// A bounded tail of a job's output.
#[derive(Clone, Debug)]
pub struct OutputRing {
    chunks: VecDeque<JobChunk>,
    /// Total bytes ever appended, which is also the offset of the next byte.
    total_bytes: u64,
    max_bytes: u64,
    /// Whether a trim has dropped data a reader has not been told about yet.
    ///
    /// Cleared when the flag is placed on a chunk, so one trim produces one gap rather than one per
    /// subsequent append.
    dropped: bool,
    /// The offset of the oldest retained byte.
    head_offset: u64,
}

impl OutputRing {
    /// Creates a ring capped at `max_bytes`.
    ///
    /// A zero cap is treated as one byte rather than as unbounded: a ring that grew without limit
    /// would be a memory leak wearing a configuration option.
    #[must_use]
    pub fn new(max_bytes: u64) -> Self {
        Self {
            chunks: VecDeque::new(),
            total_bytes: 0,
            max_bytes: max_bytes.max(1),
            dropped: false,
            head_offset: 0,
        }
    }

    /// Appends a chunk, trimming the head if needed.
    pub fn append(&mut self, text: impl Into<String>, channel: Channel) {
        let text = text.into();
        // A gap marks where this ring's readable history begins. It belongs on the first retained
        // chunk, not on every chunk appended after a trim: a flag per trim would report the same
        // discontinuity repeatedly and make a reader think its position kept breaking.
        //
        // The flag is therefore derived from the head offset at read time (see `read_from`) and
        // stored on a chunk only when that chunk is the head.
        let gap = false;

        let chunk = JobChunk {
            at: self.total_bytes,
            text,
            channel,
            gap,
        };
        self.total_bytes = self
            .total_bytes
            .saturating_add(u64::try_from(chunk.text.len()).unwrap_or(0));
        self.chunks.push_back(chunk);
        let head_before = self.head_offset;
        self.trim();
        // The chunk that is now the head carries the gap, and only that chunk: it is where the
        // readable history actually starts.
        if self.head_offset != head_before {
            if let Some(front) = self.chunks.front_mut() {
                front.gap = true;
            }
        }
    }

    /// Drops chunks from the head until the ring fits.
    fn trim(&mut self) {
        let mut used: u64 = self
            .chunks
            .iter()
            .map(|chunk| u64::try_from(chunk.text.len()).unwrap_or(0))
            .sum();

        while used > self.max_bytes && self.chunks.len() > 1 {
            let Some(removed) = self.chunks.pop_front() else {
                break;
            };
            let size = u64::try_from(removed.text.len()).unwrap_or(0);
            used = used.saturating_sub(size);
            self.head_offset = self.head_offset.saturating_add(size);
            // Only a trim that removed bytes a reader could still have wanted counts as a loss.
            self.dropped = true;
        }

        // A single chunk larger than the cap cannot be trimmed further without losing the newest
        // output, which is the only output a caller can act on. Keeping it whole is the lesser evil;
        // the loss is reported as a gap on the next append.
        if used > self.max_bytes && self.chunks.len() == 1 {
            self.dropped = true;
        }
    }

    /// The retained chunks.
    #[must_use]
    pub fn chunks(&self) -> &VecDeque<JobChunk> {
        &self.chunks
    }

    /// The offset of the oldest retained byte.
    #[must_use]
    pub fn head_offset(&self) -> u64 {
        self.head_offset
    }

    /// The offset one past the newest byte.
    #[must_use]
    pub fn end_offset(&self) -> u64 {
        self.total_bytes
    }

    /// Reads from `from`, returning the chunks and whether anything before them was lost.
    ///
    /// A `from` before the retained head means data was dropped; that is reported rather than
    /// silently starting at the head, because a caller resuming a log wants to know its position is
    /// no longer continuous.
    #[must_use]
    pub fn read_from(&self, from: u64) -> (Vec<JobChunk>, bool) {
        let lost = from < self.head_offset;
        let chunks = self
            .chunks
            .iter()
            .filter(|chunk| chunk.at >= from || chunk.at + chunk.text.len() as u64 > from)
            .cloned()
            .collect();
        (chunks, lost)
    }

    /// The retained text, joined.
    #[must_use]
    pub fn text(&self) -> String {
        self.chunks
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect()
    }
}

/// What a job produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Its terminal status.
    pub status: Status,
    /// Its exit code, when it had one.
    pub code: Option<i32>,
}

/// Why a transition was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JobError {
    /// The job is already in a terminal state.
    #[error("job `{id}` is already {status}")]
    AlreadySettled {
        /// The job.
        id: String,
        /// Its status.
        status: &'static str,
    },

    /// The job is not in the state a transition requires.
    #[error("job `{id}` is {status} and cannot {operation}")]
    WrongState {
        /// The job.
        id: String,
        /// What was asked.
        operation: &'static str,
        /// Its status.
        status: &'static str,
    },

    /// No job has that id.
    #[error("no job `{id}`")]
    NotFound {
        /// The id.
        id: String,
    },
}

/// A registered job.
#[derive(Clone, Debug)]
pub struct Job {
    /// Its identity.
    pub id: JobId,
    /// Its producer's kind name, opaque to this layer.
    pub kind: String,
    /// A human-readable label.
    pub label: String,
    /// Where it is.
    pub status: Status,
    /// Its output.
    pub output: OutputRing,
    /// Its exit code, once settled.
    pub code: Option<i32>,
    /// When it settled, in milliseconds since the epoch.
    pub settled_at: Option<u64>,
}

impl Job {
    /// Whether it has reached a terminal state.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.status.is_terminal()
    }

    /// The outcome, when settled.
    #[must_use]
    pub fn outcome(&self) -> Option<Outcome> {
        self.is_settled().then_some(Outcome {
            status: self.status,
            code: self.code,
        })
    }

    fn settle(&mut self, status: Status, code: Option<i32>, now: u64) {
        self.status = status;
        self.code = code;
        self.settled_at = Some(now);
    }
}

/// How many settled jobs a registry keeps when a new one starts.
///
/// A few dozen is what a client scrolls back through; the rest are records of work nobody is looking at, and each holds
/// an output ring up to the registry's ring size.
pub const SETTLED_KEPT: usize = 64;

/// A lifecycle driver for jobs.
///
/// Holding the jobs behind one lock rather than a registry with per-job locks: every operation here
/// is a small state change, and the interesting invariants span two jobs ("no second job while the
/// first is live"), which per-job locks cannot express.
#[derive(Debug, Default)]
pub struct JobRegistry {
    jobs: std::sync::Mutex<std::collections::BTreeMap<String, Job>>,
    ring_bytes: u64,
}

impl JobRegistry {
    /// Creates a registry whose jobs keep `ring_bytes` of output each.
    #[must_use]
    pub fn new(ring_bytes: u64) -> Self {
        Self {
            jobs: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            ring_bytes,
        }
    }

    /// Registers a running job.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::AlreadySettled`] when the id is taken: reusing an id would make a
    /// reader's saved position point into a different job's output.
    pub fn register(
        &self,
        id: &JobId,
        kind: impl Into<String>,
        label: impl Into<String>,
    ) -> Result<Job, JobError> {
        let mut jobs = self.lock();
        if let Some(existing) = jobs.get(id.as_str()) {
            return Err(JobError::AlreadySettled {
                id: id.to_string(),
                status: existing.status.as_str(),
            });
        }
        let job = Job {
            id: id.clone(),
            kind: kind.into(),
            label: label.into(),
            status: Status::Running,
            output: OutputRing::new(self.ring_bytes),
            code: None,
            settled_at: None,
        };
        jobs.insert(id.to_string(), job.clone());
        Ok(job)
    }

    /// Appends output to a job.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::NotFound`] or [`JobError::WrongState`] for a settled job: appending to a
    /// finished job would produce output nothing will ever settle against.
    pub fn append(
        &self,
        id: &str,
        text: impl Into<String>,
        channel: Channel,
    ) -> Result<(), JobError> {
        let mut jobs = self.lock();
        let job = jobs
            .get_mut(id)
            .ok_or_else(|| JobError::NotFound { id: id.to_owned() })?;
        if job.is_settled() {
            return Err(JobError::WrongState {
                id: id.to_owned(),
                operation: "append",
                status: job.status.as_str(),
            });
        }
        job.output.append(text, channel);
        Ok(())
    }

    /// Asks a running job to stop.
    ///
    /// Moves it to `stopping` rather than `killed`: a kill is not instantaneous, and a caller that
    /// saw `killed` while children were alive would start a second job against resources the first
    /// still holds.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::AlreadySettled`] or [`JobError::WrongState`].
    pub fn request_stop(&self, id: &str) -> Result<Job, JobError> {
        let mut jobs = self.lock();
        let job = jobs
            .get_mut(id)
            .ok_or_else(|| JobError::NotFound { id: id.to_owned() })?;
        match job.status {
            Status::Running => {
                job.status = Status::Stopping;
                Ok(job.clone())
            }
            Status::Stopping => Ok(job.clone()),
            status => Err(JobError::AlreadySettled {
                id: id.to_owned(),
                status: status.as_str(),
            }),
        }
    }

    /// Records that a job finished on its own.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::AlreadySettled`] for a job already settled.
    pub fn complete(&self, id: &str, code: i32, now: u64) -> Result<Job, JobError> {
        self.settle(
            id,
            if code == 0 {
                Status::Completed
            } else {
                Status::Failed
            },
            Some(code),
            now,
        )
    }

    /// Records that a job stopped because it was asked to.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::AlreadySettled`] for a job already settled.
    pub fn confirm_killed(&self, id: &str, now: u64) -> Result<Job, JobError> {
        self.settle(id, Status::Killed, None, now)
    }

    /// Records that a job failed.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::AlreadySettled`] for a job already settled.
    pub fn fail(&self, id: &str, code: Option<i32>, now: u64) -> Result<Job, JobError> {
        self.settle(id, Status::Failed, code, now)
    }

    fn settle(
        &self,
        id: &str,
        status: Status,
        code: Option<i32>,
        now: u64,
    ) -> Result<Job, JobError> {
        let mut jobs = self.lock();
        let job = jobs
            .get_mut(id)
            .ok_or_else(|| JobError::NotFound { id: id.to_owned() })?;
        if job.is_settled() {
            // A terminal state is final: a second settle would rewrite history a reader already saw.
            return Err(JobError::AlreadySettled {
                id: id.to_owned(),
                status: job.status.as_str(),
            });
        }
        job.settle(status, code, now);
        Ok(job.clone())
    }

    /// Reads a job.
    ///
    /// # Errors
    ///
    /// Returns [`JobError::NotFound`].
    pub fn get(&self, id: &str) -> Result<Job, JobError> {
        self.lock()
            .get(id)
            .cloned()
            .ok_or_else(|| JobError::NotFound { id: id.to_owned() })
    }

    /// The ids of every registered job, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// The ids of jobs that still occupy a slot.
    #[must_use]
    pub fn live(&self) -> Vec<String> {
        self.lock()
            .values()
            .filter(|job| job.status.is_live())
            .map(|job| job.id.to_string())
            .collect()
    }

    /// The live jobs of one kind.
    ///
    /// The check a caller runs before starting a second job that needs the same resource — a port, a
    /// lock file, a build directory.
    #[must_use]
    /// Forgets settled jobs older than `older_than_ms`, returning how many were removed.
    ///
    /// Nothing removed them before: a long-lived server accumulated a record for every job it ever ran, each holding its
    /// output ring. The registry is a view of what is running, and keeping a settled job forever makes it a log that
    /// happens to be unbounded.
    ///
    /// A running job is never removed whatever its age, because removing it would make a job a client is watching
    /// disappear. `now` is passed rather than read so a caller can decide what "old" means and a test can state it.
    pub fn retain_recent(&self, now: u64, older_than_ms: u64) -> usize {
        let mut jobs = self.lock();
        let before = jobs.len();
        jobs.retain(|_, job| {
            // `is_settled` is the test, not the status name: a job that is stopping has not stopped, and dropping it
            // while it is still winding down would hide the outcome a client is waiting for.
            !job.is_settled()
                || job
                    .settled_at
                    .is_none_or(|settled| now.saturating_sub(settled) <= older_than_ms)
        });
        before - jobs.len()
    }

    /// Forgets at most `keep` settled jobs, oldest first, returning how many were removed.
    ///
    /// A count rather than an age for a caller that cannot reason about clocks, and a bound on this pass so one call
    /// does not walk a registry that is already huge.
    pub fn forget_settled(&self, keep: usize) -> usize {
        let mut jobs = self.lock();
        let mut settled: Vec<(u64, String)> = jobs
            .iter()
            .filter(|(_, job)| job.is_settled())
            // A settled job with no recorded time sorts first: it is the least recently accounted for, so it is the
            // first that may go.
            .map(|(id, job)| (job.settled_at.unwrap_or(0), id.clone()))
            .collect();
        if settled.len() <= keep {
            return 0;
        }
        settled.sort();
        let remove = settled.len() - keep;
        for (_, id) in settled.into_iter().take(remove) {
            jobs.remove(&id);
        }
        remove
    }

    pub fn live_of_kind(&self, kind: &str) -> Vec<String> {
        self.lock()
            .values()
            .filter(|job| job.kind == kind && job.status.is_live())
            .map(|job| job.id.to_string())
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<String, Job>> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Shared registry handle.
pub type JobRegistryHandle = Arc<JobRegistry>;

/// The seam a producer implements to run a job.
///
/// The registry owns bookkeeping; this owns the work. Separating them is what lets the same lifecycle
/// drive a shell command, a workflow, and an in-process task without the registry knowing which.
pub trait JobProducer: Send + Sync {
    /// Starts the work and returns how to stop it.
    fn spawn(
        &self,
        id: JobId,
        spec: crate::subprocess::SpawnOptions,
    ) -> BoxFut<Result<(), JobError>>;
}

/// Spawns a command as a background job.
///
/// The one function that connects the job lifecycle to a real process. Everything else in this module
/// is bookkeeping; this is where a job becomes something a caller can watch.
///
/// # How the pieces fit
///
/// A task owns the process and drives it: read a chunk, append it to the ring, repeat until the
/// streams close, then settle. The registry is shared, so a reader sees output as it is produced
/// rather than after the command exits — which is the entire point of a job over a tool call.
///
/// # Errors
///
/// Returns [`JobError::AlreadySettled`] when the id is taken, and [`JobError::NotFound`] if the
/// registration vanished, which cannot happen while the caller holds the registry.
pub fn spawn_job(
    registry: Arc<JobRegistry>,
    id: &JobId,
    kind: impl Into<String>,
    label: impl Into<String>,
    options: crate::subprocess::SpawnOptions,
) -> Result<tokio::task::JoinHandle<()>, JobError> {
    let id_for_task = id.clone();
    registry.register(id, kind, label)?;
    // Swept here because this is the only moment the map can have grown: a registry nobody starts jobs on cannot
    // accumulate settled ones. A background timer would be a second thing to shut down and would fire on a registry
    // that has nothing to do.
    registry.forget_settled(SETTLED_KEPT);

    let handle = tokio::spawn(async move {
        let id = id_for_task;
        let mut process = match crate::subprocess::start(&options) {
            Ok(process) => process,
            Err(_error) => {
                // A spawn failure is a failed job, not a silent absence: a caller watching the id
                // must see it settle rather than hang forever on a job that never existed.
                let _ = registry.fail(&id.to_string(), None, now_millis());
                return;
            }
        };

        // Drain until both streams close OR a stop is requested.
        //
        // The stop check has to race the read rather than follow it: `next_chunk` blocks while the
        // process is silent, so a `sleep 30` with no output would never reach a check placed after
        // the loop. That is a real hang, not a slow test.
        loop {
            tokio::select! {
                chunk = process.next_chunk() => match chunk {
                    Some((channel, text)) => {
                        if registry.append(&id.to_string(), text, channel).is_err() {
                            // Settled from outside; stop reading.
                            process.terminate();
                            break;
                        }
                    }
                    None => break,
                },
                () = wait_for_stop(&registry, &id) => {
                    process.terminate();
                    let _ = registry.confirm_killed(&id.to_string(), now_millis());
                    // Wait for the group to go so the task does not outlive its process.
                    let _ = process.wait().await;
                    return;
                }
            }
        }

        // A job asked to stop settles as killed; one that exited on its own settles by its code.
        let requested_stop = registry
            .get(&id.to_string())
            .is_ok_and(|job| job.status == Status::Stopping);
        if requested_stop {
            process.terminate();
            let _ = registry.confirm_killed(&id.to_string(), now_millis());
        } else {
            match process.wait().await {
                Ok(Some(0)) => {
                    let _ = registry.complete(&id.to_string(), 0, now_millis());
                }
                Ok(Some(code)) => {
                    let _ = registry.fail(&id.to_string(), Some(code), now_millis());
                }
                Ok(None) => {
                    // Signalled: not a clean exit, and not a stop we asked for.
                    let _ = registry.fail(&id.to_string(), None, now_millis());
                }
                Err(_) => {
                    let _ = registry.fail(&id.to_string(), None, now_millis());
                }
            }
        }
    });

    Ok(handle)
}

/// Resolves once a job has been asked to stop.
///
/// Polling rather than a channel: the registry is the shared state and adding a notification channel
/// would give the stop path a second source of truth. The interval is short enough that a kill feels
/// immediate and long enough not to spin.
async fn wait_for_stop(registry: &JobRegistry, id: &JobId) {
    loop {
        if registry
            .get(&id.to_string())
            .is_ok_and(|job| job.status == Status::Stopping)
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The current time in milliseconds since the epoch.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}
