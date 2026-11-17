//! Where a workspace file change comes from, and which ones a watch must report.
//!
//! `packages/api/workspace-files/src/changes.ts` is 162 lines and splits into two halves that look
//! alike and are not:
//!
//! | Half | Upstream | Needs a platform notifier? |
//! | --- | --- | --- |
//! | Opening a generation: resolve root, resolve target, refuse a directory outside the workspace, `await fs.watch(...)`, yield `ready` | `WorkspaceChangeFeed.follow` | **yes** — `watch` is the notifier |
//! | Deciding what to do with what the notifier said: path relevance, coalescing, version-dedupe, `absent` vs `present` shaping | same generator's loop + `ChangeFollower` | no |
//!
//! The delegation brief for this module said the `changes` stream is the largest remaining gap
//! because "there is no platform notifier". That is true of the first half only. The second half is
//! a pure function of `(raw notifier event, fresh stat)` → `frame`, and this module is that half:
//! [`plan_watch`] is the gate that decides whether a generation may open at all, and
//! [`Generation`] is the queue whose whole job is to turn many notifications into the few frames a
//! client can act on.
//!
//! # What a notifier would have to provide, stated honestly
//!
//! [`crate::fs::FileSystem`] has no `watch`. Upstream's base `FileSystem.watch` *rejects* by
//! default ("Filesystem watching is not supported by this provider") and a backend overrides it —
//! so "no notifier" is not a local omission, it is the upstream default, and the error a client sees
//! is `workspace-file/watch-unsupported`. [`plan_watch`] and [`WatchError::code`] reproduce that
//! contract, which means the day a notifier lands the port is a trait method and one `await`, not a
//! redesign: nothing below this line needs to know which OS facility fired.
//!
//! What a notifier must give the port, in the shape of [`Generation::observe`]:
//!
//! - **which path changed**, or nothing when the facility reports only "your target changed" (the
//!   shape `fs.watch`'s callback actually has, and the only shape upstream's loop consumes);
//! - **an error**, when the watcher itself failed, which upstream turns into `follower.fail`.
//!
//! It must *not* give a version. Upstream re-`stat`s the target after every dequeue on purpose: a
//! notifier's payload is an event, not an observation, and a client that echoed the event's payload
//! would show a version that was already stale when it arrived.
//!
//! # The rules this module owns
//!
//! 1. **A directory outside the workspace is refused; a file outside it is not.** Upstream's `stat`
//!    closure throws `workspace-file/outside-workspace` only for `info.type === 'directory'`. The
//!    asymmetry is deliberate and easy to get wrong: following a single absolute file is a
//!    supported deployment, following a tree that escapes the workspace is not.
//! 2. **An absent target is still watchable.** `stat` returning `undefined` before `watch` is not an
//!    error — the whole point of watching a path that does not exist is to learn when it appears.
//! 3. **A child event invalidates the *target*, not the child.** A directory's own version does not
//!    change when a child's metadata changes (`types.ts` says so explicitly: "directory consumers
//!    relist on every frame because a child's metadata can change without changing the directory
//!    version"). So a directory generation reports the directory and lets the consumer relist.
//! 4. **A file's repeat version is not a change; a directory's is.** The same fact read the other
//!    way: echoing an unchanged file version would re-render a file that did not change, while
//!    suppressing an unchanged directory version would hide a child's edit.
//! 5. **Many notifications, one frame.** A save is several events (write, chmod, rename) and a
//!    build is thousands. Coalescing is per reported path and per drain, so a burst of 500 events
//!    on one file costs one `stat` and one frame.
//! 6. **Overflow announces itself.** A queue with no bound turns a stalled consumer into a memory
//!    leak; a queue that silently drops turns it into a liar. Past [`DEFAULT_DIRTY_LIMIT`] the queue
//!    is replaced by a resync flag, which is a truthful "you cannot trust what you were told".
//!
//! # What is deliberately not here
//!
//! The follower queue itself (a `Deque`, a wake, an `async *read()`), because
//! [`api_controller_gap`](girder_core::api_controller_gap) already ports the client-side
//! `Follower`/`StreamBook` pair, and the two together are the whole carrier: this module produces
//! frames, that one filters and refcounts them. Duplicating either would give the two halves two
//! opinions about the same queue.
//!
//! One incompatibility the wiring owner must settle: `api_controller_gap::Notice::Changed` carries
//! `version: u64`, while the wire (and [`FsVersion`]) is an opaque `String`. It is lossless in
//! neither direction, so a bridge between [`Change`] and `Notice` cannot be written correctly until
//! that field is widened to [`FsVersion`].

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use girder_core::api_controller_gap::path_key;
use girder_core::{
    FsInfo, FsType, FsVersion, normalize_path, relative_to, resolve_in_workspace,
    workspace_contains,
};

/// Upstream's error code for a provider that cannot watch.
pub const WATCH_UNSUPPORTED: &str = "workspace-file/watch-unsupported";

/// Upstream's error code for a directory target that escapes the workspace root.
pub const OUTSIDE_WORKSPACE: &str = "workspace-file/outside-workspace";

/// How many distinct dirty paths a generation retains before it gives up and resyncs.
///
/// 4,096: far above any burst a person or a build produces on one target, and small enough that a
/// process whose consumer stopped draining fails at a bounded cost. A generation that hits it does
/// not lose data quietly — see [`Changes::observe`].
pub const DEFAULT_DIRTY_LIMIT: usize = 4096;

/// How a notifier's events relate to the watched target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The notifier reports only the target itself.
    ///
    /// This is what `fs.watch(target, cb)` gives upstream, and it is the default because it is the
    /// only shape every platform can deliver.
    Target,
    /// The notifier reports descendants too, and each is worth its own frame.
    ///
    /// A recursive facility (a tree watcher, a container's overlay events) can say *which* file
    /// changed. Reporting that child instead of collapsing it into its parent is strictly more
    /// useful to a consumer, so it is offered — but never assumed.
    Recursive,
}

/// What one raw event turned out to be.
///
/// Returned rather than kept internal so a caller can log or count what its notifier is actually
/// delivering: a backend that reports everything as [`Relevance::Unrelated`] is misconfigured, and
/// that is invisible if the answer is only ever `()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relevance {
    /// The event named the target itself.
    SelfTarget,
    /// The event named something inside the watched directory.
    Child,
    /// The event named a path unrelated to the target and was ignored.
    Unrelated,
}

/// Why a generation may not open.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WatchError {
    /// The provider has no notifier for this target.
    ///
    /// Upstream's base `FileSystem.watch` rejects with exactly this, so a build without a notifier
    /// is a *conforming* provider, not a broken one; the client's answer is to poll `stat`.
    #[error("watching `{path}` is not supported: {message}")]
    NotSupported {
        /// The requested path, as the caller spelled it.
        path: String,
        /// The provider's own message.
        message: String,
    },

    /// A directory target is outside the workspace root.
    #[error("`{path}` is a directory outside the workspace")]
    OutsideWorkspace {
        /// The requested path, as the caller spelled it.
        path: String,
    },
}

impl WatchError {
    /// The wire code a client matches on.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotSupported { .. } => WATCH_UNSUPPORTED,
            Self::OutsideWorkspace { .. } => OUTSIDE_WORKSPACE,
        }
    }
}

/// One target a generation is allowed to watch.
///
/// Produced only by [`plan_watch`], so holding one is proof the gate ran: an unplanned watch is not
/// representable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchPlan {
    /// The absolute target path, `\` normalized to `/`.
    ///
    /// Normalized because it is the key the relevance filter compares against, and a Windows host
    /// reporting `C:\a\b` against a configured `C:/a/b` would otherwise filter out every change.
    pub absolute_path: String,
    /// What the host says the target is right now.
    ///
    /// [`FsType::Other`] when the target was absent at planning time: a path that does not exist has
    /// no type, and refusing to watch it would defeat watching for creation.
    pub kind: FsType,
    /// Whether the target existed when the generation was planned.
    pub existed: bool,
    /// The path in the host's execution world, in the host's own spelling.
    ///
    /// Deliberately *not* normalized the way [`Self::absolute_path`] is. The two answer different
    /// questions: the key exists to be compared and must fold `\\` into `/`, while this one exists to
    /// be opened by the platform and must keep whatever spelling the platform produced. Upstream
    /// draws the same line between `targetKey` and `processPath(target)`.
    ///
    /// Carried on the plan rather than recomputed because it is what a frame reports, and a client
    /// that received only a target key would have nothing to open.
    pub process_path: String,
}

impl WatchPlan {
    /// The key changes are matched against.
    #[must_use]
    pub fn key(path: &str) -> String {
        path_key(path)
    }
}

/// Decides whether a generation may watch `path`, and in what shape.
///
/// `root` is the session's workspace root, resolved by the caller first (upstream returns early when
/// the root does not resolve, which is a *stream that ends*, not an error, and is the caller's
/// decision rather than this function's).
///
/// `info` is the target's `stat` at planning time: `None` for a path that does not exist yet, which
/// is legal and is exactly how watching for creation works.
///
/// # Errors
///
/// [`WatchError::OutsideWorkspace`] when the target is a *directory* that is not inside `root`.
/// A file is never refused for being outside: following one absolute file is a supported mode, and
/// the type — not the position — is what makes a tree walk dangerous.
pub fn plan_watch(
    root: Option<&Path>,
    path: &str,
    info: Option<&FsInfo>,
) -> Result<WatchPlan, WatchError> {
    let resolved = resolve_in_workspace(root, path);
    let absolute_path = WatchPlan::key(&resolved.display().to_string());
    let kind = info.map_or(FsType::Other, |info| info.file_type);
    if kind == FsType::Directory {
        let inside = root.is_some_and(|root| {
            workspace_contains(
                &normalize_path(root),
                &normalize_path(Path::new(&absolute_path)),
            )
        });
        if !inside {
            return Err(WatchError::OutsideWorkspace {
                path: path.to_owned(),
            });
        }
    }
    Ok(WatchPlan {
        absolute_path,
        kind,
        existed: info.is_some(),
        process_path: resolved.display().to_string(),
    })
}

/// The state of one reported path, as a frame carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetState {
    /// The target is there, at this version.
    Present {
        /// The freshness token from the fresh `stat`.
        version: FsVersion,
        /// What the target is now.
        kind: FsType,
    },
    /// The target is gone.
    Absent,
}

impl TargetState {
    /// The version, when present.
    #[must_use]
    pub fn version(&self) -> Option<&FsVersion> {
        match self {
            Self::Present { version, .. } => Some(version),
            Self::Absent => None,
        }
    }

    /// Whether this state is a directory.
    #[must_use]
    pub fn is_directory(&self) -> bool {
        matches!(
            self,
            Self::Present {
                kind: FsType::Directory,
                ..
            }
        )
    }
}

/// One reported change: `{absolutePath, version}` or `{absolutePath, absent: true}`.
///
/// Serialized by hand rather than derived, because the two wire shapes are an externally-tagged
/// union in TypeScript (`version` present XOR `absent: true`) and no serde enum layout produces
/// exactly that from a Rust enum without adding a tag the client does not expect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Absolute path in the same form as `WorkspaceFileStat.absolutePath`.
    pub absolute_path: String,
    state: TargetState,
}

/// The wire shape of [`Change`]: upstream's two variants flattened into one record.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RawChange {
    absolute_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    absent: Option<bool>,
}

impl Change {
    /// A change naming a present target.
    #[must_use]
    pub fn present(absolute_path: impl Into<String>, version: FsVersion, kind: FsType) -> Self {
        Self {
            absolute_path: absolute_path.into(),
            state: TargetState::Present { version, kind },
        }
    }

    /// A change naming a target that is gone.
    #[must_use]
    pub fn absent(absolute_path: impl Into<String>) -> Self {
        Self {
            absolute_path: absolute_path.into(),
            state: TargetState::Absent,
        }
    }

    /// The state this change reports.
    #[must_use]
    pub fn state(&self) -> &TargetState {
        &self.state
    }

    /// Whether this change reports absence.
    #[must_use]
    pub fn is_absent(&self) -> bool {
        matches!(self.state, TargetState::Absent)
    }
}

impl Serialize for Change {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (version, absent) = match &self.state {
            TargetState::Present { version, .. } => (Some(version.0.clone()), None),
            TargetState::Absent => (None, Some(true)),
        };
        RawChange {
            absolute_path: self.absolute_path.clone(),
            version,
            absent,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Change {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawChange::deserialize(deserializer)?;
        match (raw.version, raw.absent) {
            (Some(version), None) => Ok(Self {
                absolute_path: raw.absolute_path,
                // A frame's version is a freshness token, never parsed, so the kind is unknown here.
                // `Other` is the honest answer: it says "no claim about what this is", which is what
                // a client that only has the wire actually knows.
                state: TargetState::Present {
                    version: FsVersion(version),
                    kind: FsType::Other,
                },
            }),
            (None, Some(true)) => Ok(Self {
                absolute_path: raw.absolute_path,
                state: TargetState::Absent,
            }),
            (Some(_), Some(_)) => Err(D::Error::custom(
                "a workspace file change carries `version` or `absent: true`, not both",
            )),
            (None, _) => Err(D::Error::custom(
                "a workspace file change carries `version` or `absent: true`",
            )),
        }
    }
}

/// One frame of a watch generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Frame {
    /// The host initialized the watcher; queued and live invalidations follow.
    Ready,
    /// The target changed, as of the `stat` that produced this frame.
    Change {
        /// The change.
        change: Change,
    },
}

impl Frame {
    /// The change this frame carries, when it carries one.
    #[must_use]
    pub fn change(&self) -> Option<&Change> {
        match self {
            Self::Ready => None,
            Self::Change { change } => Some(change),
        }
    }
}

/// What one drain produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flushed {
    /// Whether the queue overflowed and the consumer must treat everything it knows as stale.
    pub resync: bool,
    /// The frames to send, in the order their paths first went dirty.
    pub changes: Vec<Change>,
}

impl Flushed {
    /// Frames for this drain, `ready` excluded (that is [`Generation::ready`]'s job).
    #[must_use]
    pub fn frames(&self) -> Vec<Frame> {
        let mut frames: Vec<Frame> = Vec::with_capacity(self.changes.len());
        for change in &self.changes {
            frames.push(Frame::Change {
                change: change.clone(),
            });
        }
        frames
    }

    /// Whether there is nothing to send.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.resync && self.changes.is_empty()
    }
}

/// The dirty set and the dedupe memory of one generation.
///
/// Separated from [`Generation`] because the two answer different questions and only the second one
/// is about the wire: this type is "which paths are dirty and did the fresh stat actually say
/// anything new", and that is testable with a closure and no frames at all.
#[derive(Debug)]
pub struct Changes {
    /// The reported path of the watched target.
    target: String,
    /// The target's kind, which decides whether an unchanged version is still worth a frame.
    kind: FsType,
    /// Whether child events are reported individually.
    reach: Reach,
    /// Dirty paths, in first-dirty order.
    order: Vec<String>,
    /// The dirty set, for O(log n) membership.
    dirty: BTreeSet<String>,
    /// The last state reported per path, for version dedupe.
    delivered: BTreeMap<String, TargetState>,
    /// Distinct dirty paths retained before the queue gives up.
    limit: usize,
    /// Set when the limit was hit.
    overflowed: bool,
}

impl Changes {
    /// A queue for one planned target.
    #[must_use]
    pub fn new(plan: &WatchPlan, reach: Reach) -> Self {
        Self::with_limit(plan, reach, DEFAULT_DIRTY_LIMIT)
    }

    /// Same, with an explicit limit.
    #[must_use]
    pub fn with_limit(plan: &WatchPlan, reach: Reach, limit: usize) -> Self {
        Self {
            target: plan.absolute_path.clone(),
            kind: plan.kind,
            reach,
            order: Vec::new(),
            dirty: BTreeSet::new(),
            delivered: BTreeMap::new(),
            limit: limit.max(1),
            overflowed: false,
        }
    }

    /// Which path this queue reports the target as.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// How many paths are dirty.
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether nothing is dirty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Whether the queue overflowed since the last drain.
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Classifies one raw event against the watched target.
    ///
    /// `changed` is the path the notifier named, or `None` when it only said "your target changed".
    #[must_use]
    pub fn classify(&self, changed: Option<&str>) -> Relevance {
        let Some(changed) = changed else {
            return Relevance::SelfTarget;
        };
        let changed = normalize_path(Path::new(&path_key(changed)));
        let target = normalize_path(Path::new(&self.target));
        if changed == target {
            return Relevance::SelfTarget;
        }
        let Some(relative) = relative_to(&target, &changed) else {
            return Relevance::Unrelated;
        };
        let depth = relative.components().count();
        let inside = matches!(self.kind, FsType::Directory | FsType::Other);
        // A file target has no children to report. `Other` (absent at planning time) is treated as a
        // directory so that watching a path that later becomes a directory still reports its
        // entries; the alternative is a generation that silently never fires.
        if !inside || depth != 1 {
            return Relevance::Unrelated;
        }
        Relevance::Child
    }

    /// Records one raw event, merging it into the dirty set.
    ///
    /// Returns what the event was, so a caller can count what its notifier produces.
    pub fn observe(&mut self, changed: Option<&str>) -> Relevance {
        let relevance = self.classify(changed);
        let Some(path) = self.dirty_path(relevance, changed) else {
            return relevance;
        };
        if self.dirty.contains(&path) {
            return relevance;
        }
        if self.order.len() >= self.limit {
            // Truthful overflow: keep the flag, drop the detail, and force the consumer to re-read.
            self.order.clear();
            self.dirty.clear();
            self.delivered.clear();
            self.overflowed = true;
            return relevance;
        }
        self.dirty.insert(path.clone());
        self.order.push(path);
        relevance
    }

    /// The path one relevant event dirties.
    ///
    /// A child event dirties the *target* unless the notifier can name descendants, because a
    /// directory's own version does not change when a child's metadata does — reporting the child
    /// would leave a consumer that keys on the target waiting for a version that never moves.
    fn dirty_path(&self, relevance: Relevance, changed: Option<&str>) -> Option<String> {
        match (relevance, self.reach) {
            (Relevance::Unrelated, _) => None,
            // A self event names the target; a child event under a target-only notifier collapses
            // into the target, because the directory's own version is what a consumer relists on.
            (Relevance::SelfTarget | Relevance::Child, Reach::Target)
            | (Relevance::SelfTarget, Reach::Recursive) => Some(self.target.clone()),
            (Relevance::Child, Reach::Recursive) => changed.map(path_key),
        }
    }

    /// Stats every dirty path and produces the frames that survive dedupe.
    ///
    /// `stat` is injected rather than taken from a trait object because the interesting behaviour
    /// here is *which* paths it is asked about — once per drain per path, never once per event — and
    /// a test can only assert that if it supplies the function.
    ///
    /// ```text
    /// stat(path) -> Option<FsInfo>   // None means absent, exactly as `FileSystem::stat` reports it
    /// ```
    pub fn drain<F>(&mut self, mut stat: F) -> Flushed
    where
        F: FnMut(&str) -> Option<FsInfo>,
    {
        let resync = std::mem::replace(&mut self.overflowed, false);
        let order = std::mem::take(&mut self.order);
        self.dirty.clear();
        let mut changes = Vec::with_capacity(order.len());
        for path in order {
            let state = match stat(&path) {
                Some(info) => TargetState::Present {
                    version: info.version,
                    kind: info.file_type,
                },
                None => TargetState::Absent,
            };
            if self.is_duplicate(&path, &state) {
                continue;
            }
            self.remember(&path, &state);
            changes.push(Change {
                absolute_path: path,
                state,
            });
        }
        Flushed { resync, changes }
    }

    /// Whether a fresh state says the same thing the last frame already said.
    ///
    /// Only for regular files. A directory's version is not a change detector for its entries, so
    /// the same version is still worth a frame; and an *absent* target's absence is worth repeating
    /// only for a file target, because for a directory the consumer's relist is the point.
    fn is_duplicate(&self, path: &str, state: &TargetState) -> bool {
        let statable = match state {
            TargetState::Present { kind, .. } => *kind == FsType::File,
            TargetState::Absent => self.kind == FsType::File,
        };
        statable && self.delivered.get(path) == Some(state)
    }

    /// Records what was reported, bounding the memory it costs.
    fn remember(&mut self, path: &str, state: &TargetState) {
        if self.delivered.len() >= self.limit && !self.delivered.contains_key(path) {
            // Evict the first remembered path: a later re-observation then produces one redundant
            // frame, which is the harmless direction to be wrong in.
            if let Some(oldest) = self.delivered.keys().next().cloned() {
                self.delivered.remove(&oldest);
            }
        }
        self.delivered.insert(path.to_owned(), state.clone());
    }
}

/// One watch generation: the ready handshake plus a [`Changes`] queue.
///
/// The carrier upstream splits across a generator and a `ChangeFollower`; here both are explicit
/// calls, because nothing in this crate may spawn a task or block on a wake.
#[derive(Debug)]
pub struct Generation {
    plan: WatchPlan,
    changes: Changes,
    started: bool,
    ended: bool,
}

impl Generation {
    /// Opens a generation for a planned target.
    #[must_use]
    pub fn open(plan: WatchPlan, reach: Reach) -> Self {
        let changes = Changes::new(&plan, reach);
        Self {
            plan,
            changes,
            started: false,
            ended: false,
        }
    }

    /// Acknowledge that the notifier is active, yielding `ready` exactly once.
    ///
    /// Idempotent on purpose: a reconnect that reports `ready` twice must not produce two `ready`
    /// frames, because a client that saw the second one would discard its state for no reason.
    pub fn ready(&mut self) -> Option<Frame> {
        if self.started {
            return None;
        }
        self.started = true;
        Some(Frame::Ready)
    }

    /// Records one raw event; ignored once the generation has ended.
    pub fn observe(&mut self, changed: Option<&str>) -> Relevance {
        if self.ended {
            return Relevance::Unrelated;
        }
        self.changes.observe(changed)
    }

    /// Ends the generation: nothing after this is queued or delivered.
    pub fn end(&mut self) {
        self.ended = true;
    }

    /// Whether `ready` has been sent.
    #[must_use]
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Whether the generation has ended.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// The plan this generation watches.
    #[must_use]
    pub fn plan(&self) -> &WatchPlan {
        &self.plan
    }

    /// The dirty queue, for diagnostics and for tests that want to see coalescing directly.
    #[must_use]
    pub fn changes(&self) -> &Changes {
        &self.changes
    }

    /// Stats every dirty path and returns the frames to send.
    ///
    /// Nothing is produced after [`Self::end`]: a consumer that already ended must not receive a
    /// frame that arrives while its teardown is in flight.
    pub fn drain<F>(&mut self, stat: F) -> Flushed
    where
        F: FnMut(&str) -> Option<FsInfo>,
    {
        if self.ended {
            return Flushed::default();
        }
        self.changes.drain(stat)
    }
}

/// A `FsInfo` built by hand, for tests and for callers that have metadata but no `FileSystem`.
#[must_use]
pub fn info(version: &str, file_type: FsType) -> FsInfo {
    FsInfo {
        version: FsVersion(version.to_owned()),
        file_type,
        size: 0,
    }
}
