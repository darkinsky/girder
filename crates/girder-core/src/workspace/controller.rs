//! The `workspace` state stream and the host directory-picking namespace.
//!
//! # What upstream carries, measured
//!
//! `packages/api/workspace-controller` is 1,960 lines over 10 sources and
//! `packages/api/workspace-files` is 1,624 lines over 9. Together 3,584 lines in five families:
//!
//! | Family | Upstream source | Lines | What it does |
//! | --- | --- | --- | --- |
//! | Commands | `commands.ts`, `index.ts` | ~470 | Eleven verbs over the registry: create/resolve, rename, delete, two orderings, archive/unarchive, pin/unpin, `initializeDefault`, and the streaming `follow` |
//! | State stream | `feed.ts`, `index.ts` (`follow`) | ~320 | One baseline then ordered increments, reconnect-safe, filtered by the caller's access |
//! | Directory policy | `default-directory.ts`, `default-workspace.ts` | 108 | The Documents directory probe, its validation, and the fixed first-use leaf name and title |
//! | Directory picker | `directory-picker.ts` | 174 | A `directoryPicker` namespace: `pick`, `list`, `createDirectory`, capability-gated with a closed failure vocabulary |
//! | Files | `index.ts`, `changes.ts`, `search.ts`, `client/*` | ~1,400 | `read`/`readBytes`/`stat`/`list`/`search` plus the target-scoped `changes` watch stream and the client change feed |
//!
//! # What this crate already covers
//!
//! `workspace_rpc_extra.rs` implements seven of the eleven controller verbs (`rename`,
//! `insertBefore`, `insertSessionBefore`, `archiveSession`, `unarchiveSession`, `pinSession`,
//! `unpinSession`) over a [`WorkspacePreferences`] seam, and names the remaining three as honest
//! gaps in its own header: `initializeDefault`, `follow` and the picker namespace.
//! `workspace_files_rpc.rs` and `workspace_files_extra.rs` cover `tree`, `readFile`, `stat`,
//! `list`, `read` and a bounded `search` (name-substring walk, no ripgrep, no exclusions).
//!
//! # The gap this module closes
//!
//! 1. **`workspace.follow`** — the state stream. Its load-bearing behaviour is not "send
//!    changes": it is that a client which reconnects must be able to reconstruct state from a
//!    *complete* baseline plus ordered increments, and that a client's view never contains a row
//!    it is not allowed to see. This module ports that contract as a cursor subscription because
//!    [`RemoteRegistry`] has no stream mode; the same choice `session.follow` and `job.follow`
//!    already made. A cursor is not a different contract — it is the same frames with the
//!    resumption token made explicit instead of implied by a live connection.
//! 2. **`directoryPicker`** — the whole namespace, over a [`DirectoryPickerBackend`] seam. The
//!    `browse` capability is implemented against the host filesystem here; the `native` one is a
//!    deployment seam, because a native chooser is a window this crate cannot open.
//! 3. **`initializeDefault`** and the fixed first-use directory policy, over a
//!    [`DocumentsDirectory`] seam.
//!
//! The files-side `changes` watch stream is the largest remaining gap and is *not* claimed here:
//! a real filesystem watch needs a platform notifier, and a polling fake would answer a different
//! question than the one a client asks. Stating that is better than shipping a subscription that
//! fires on a timer and calls itself a watcher.
//!
//! # Why the stream is a journal rather than a broadcast channel
//!
//! A broadcast channel is the obvious shape and the wrong one: a subscriber that is slow loses
//! frames silently, and the client cannot tell which. Upstream solves this by sending a `replace`
//! frame carrying the complete filtered state whenever it cannot express a change as an increment.
//! A bounded journal plus a cursor gives the same guarantee — a cursor older than the retained
//! window gets a `replace` and resynchronizes — without a task per subscriber, and it makes the
//! resync path testable by calling one function.
//!
//! # The access rules, ported verbatim
//!
//! Five rules decide what a caller may see, and each fails silently if it regresses:
//!
//! 1. A workspace the caller may not see is never upserted, and its sessions are filtered from
//!    every row that is.
//! 2. A workspace that was already delivered and then stops being admitted is answered with a
//!    `replace`, never a `remove`: the client did not delete it, so a `remove` would be a lie.
//! 3. A `remove` reaches only a caller that received the workspace, so a caller never learns of a
//!    deletion it was not shown.
//! 4. An `order` frame that contains a delivered-but-hidden workspace becomes a `replace`; the
//!    otherwise-visible ids are filtered out of the order.
//! 5. Archived and pinned sets are filtered by session access, because a pinned session the caller
//!    may not read is a row the client would fail to resolve.
//!
//! # Wire shapes
//!
//! Request arguments are camelCase, as upstream spells them (`afterSeq`, `limit`). Response keys
//! are `snake_case`, which is what `workspace.list`, `workspace.create`, and the seven methods in
//! [`crate::workspace::rpc_extra`] already emit: mixing upstream's camelCase into one namespace
//! would force a client to decode two shapes for the same row. Frames therefore carry
//! `workspace_id`, `session_ids`, `archived_session_ids`, `pinned_session_ids`, and
//! `workspace_ids`, and each frame names its kind in a `type` field exactly as upstream does.
//!
//! # Compilation note
//!
//! Like the other `*_gap` modules, this one is not declared from the crate root yet: adding
//! `mod workspace_controller_gap;` and its re-exports is the integrator's change. Its tests include
//! it by path, so they compile against the code as written rather than against a re-export that
//! may not exist yet.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};

use girder_core::api::config::{Workspace, WorkspaceRegistry};
use girder_core::{
    Declaration, RemoteCall, RemoteError, RemoteRegistry, WorkspacePreferences, now_millis,
};

// ---- constants --------------------------------------------------------------------------------

/// The `workspace` namespace the stream lives in.
pub const NAMESPACE: &str = "workspace";

/// The streaming method's name.
pub const FOLLOW_METHOD: &str = "follow";

/// Initializes or reuses the first-use workspace.
pub const INITIALIZE_DEFAULT_METHOD: &str = "initializeDefault";

/// The namespace the picking verbs live in.
pub const PICKER_NAMESPACE: &str = "directoryPicker";

/// How many frames one poll returns when the caller names no limit.
///
/// 64, the same budget `session.follow` uses: a page that returned thousands would make one frame
/// enormous and the next poll empty, which a client reads as a stall.
pub const DEFAULT_FOLLOW_FRAMES: usize = 64;

/// How many increments the feed retains for a reconnecting cursor.
///
/// 4,096: large enough that a client which drops for a while resumes without a resync, small
/// enough that one forgotten subscriber cannot grow the process without bound. A cursor older than
/// the retained window is answered with a `replace`, which is correct rather than merely cheap.
pub const DEFAULT_JOURNAL_CAPACITY: usize = 4_096;

/// The leaf directory name of the first-use workspace.
///
/// Language-neutral, so one installation keeps one on-disk path across language switches. The
/// registry derives the initial title from this same segment, which is the title
/// [`workspace_display_title`] recognizes as automatic.
pub const DEFAULT_WORKSPACE_DIRECTORY: &str = "default-workspace";

/// The directory under Documents that holds harness data.
pub const HARNESS_DIRECTORY: &str = "deepseek-harness";

/// How many child directories one picker listing returns.
pub const MAX_DIRECTORY_ENTRIES: usize = 512;

/// The picker failure code for a directory that cannot be read.
pub const PICKER_UNREADABLE: &str = "directory-picker/unreadable";

/// The picker failure code for an existing child.
pub const PICKER_EXISTS: &str = "directory-picker/exists";

/// The picker failure code for a creation that failed.
pub const PICKER_CREATE_FAILED: &str = "directory-picker/create-failed";

/// The picker failure code for a verb the composed backend cannot serve.
pub const PICKER_UNAVAILABLE: &str = "directory-picker/unavailable";

/// The internal code used for a malformed request, which maps to `InvalidArguments`.
pub const PICKER_INVALID_REQUEST: &str = "gateway/bad-request";

// ---- access -----------------------------------------------------------------------------------

/// Who may see what inside one subscription.
///
/// A seam rather than a field because upstream consults the host's remote-access service, and this
/// crate has no such service to call: the deployment knows its own peer and grants. Both methods
/// default to allowing, so a deployment that has not built a policy gets a working stream rather
/// than a surprise empty one — a stream that hides everything by default is indistinguishable from
/// a broken one, and the second is what an operator would investigate.
pub trait FollowAccess: Send + Sync {
    /// Whether this caller may see one workspace row at all.
    fn allows_workspace(&self, workspace_id: &str) -> bool {
        let _ = workspace_id;
        true
    }

    /// Whether this caller may see one session id inside a row, an archive set, or a pin set.
    fn allows_session(&self, session_id: &str) -> bool {
        let _ = session_id;
        true
    }
}

/// The access answer of a deployment with no policy.
///
/// Named rather than implied so a registration site says out loud that it grants everything; a
/// bare `()` would be a trait bound nobody reads as a decision.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

impl FollowAccess for AllowAll {}

/// Hides the workspaces and sessions it was told to hide.
///
/// Shipped because it is the smallest honest policy that exercises every filtering rule, and
/// because the rules are the interesting part of the stream: a test that cannot hide anything
/// cannot show that a `replace` is sent instead of a `remove`.
#[derive(Debug, Default, Clone)]
pub struct Hidden {
    workspaces: BTreeSet<String>,
    sessions: BTreeSet<String>,
}

impl Hidden {
    /// Hides nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Hides one workspace.
    #[must_use]
    pub fn without_workspace(mut self, workspace_id: impl Into<String>) -> Self {
        self.workspaces.insert(workspace_id.into());
        self
    }

    /// Hides one session.
    #[must_use]
    pub fn without_session(mut self, session_id: impl Into<String>) -> Self {
        self.sessions.insert(session_id.into());
        self
    }
}

impl FollowAccess for Hidden {
    fn allows_workspace(&self, workspace_id: &str) -> bool {
        !self.workspaces.contains(workspace_id)
    }

    fn allows_session(&self, session_id: &str) -> bool {
        !self.sessions.contains(session_id)
    }
}

/// An access seam whose answers can change while a subscription is open.
///
/// The `replace` path exists for exactly this: a grant revoked between two polls means the client
/// holds a row it may no longer see, and the next frame must supersede its state rather than
/// remove the row as if it had been deleted.
#[derive(Debug, Default)]
pub struct MutableAccess {
    workspaces: Mutex<BTreeSet<String>>,
    sessions: Mutex<BTreeSet<String>>,
}

impl MutableAccess {
    /// Allows everything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stops admitting one workspace.
    pub fn hide_workspace(&self, workspace_id: impl Into<String>) {
        lock(&self.workspaces).insert(workspace_id.into());
    }

    /// Admits one workspace again.
    pub fn show_workspace(&self, workspace_id: &str) {
        lock(&self.workspaces).remove(workspace_id);
    }

    /// Stops admitting one session.
    pub fn hide_session(&self, session_id: impl Into<String>) {
        lock(&self.sessions).insert(session_id.into());
    }
}

impl FollowAccess for MutableAccess {
    fn allows_workspace(&self, workspace_id: &str) -> bool {
        !lock(&self.workspaces).contains(workspace_id)
    }

    fn allows_session(&self, session_id: &str) -> bool {
        !lock(&self.sessions).contains(session_id)
    }
}

/// Locks a set, recovering from a poisoned mutex.
///
/// Recovered rather than propagated: a panic while holding this table leaves a set of ids, not a
/// half-written one, and refusing every later call would turn one panic into a permanent outage.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- frames -----------------------------------------------------------------------------------

/// One ordered change the feed retains and replays.
///
/// Separate from the wire frames because an increment is a fact about the registry, while a
/// `replace` is a decision about one caller's view. Keeping the decision out of the journal is
/// what makes the journal shareable between callers with different access.
#[derive(Debug, Clone)]
enum Increment {
    /// A row was added or changed.
    Upsert(Workspace),
    /// A row was deleted.
    Remove(String),
    /// The complete row order changed.
    Order(Vec<String>),
    /// The complete archive set changed.
    Archived(Vec<String>),
    /// The complete pin set changed.
    Pinned(Vec<String>),
}

/// One journaled increment and the sequence number a cursor names it by.
#[derive(Debug, Clone)]
struct JournalEntry {
    seq: u64,
    increment: Increment,
}

/// What projecting one increment produced for one caller.
enum Emitted {
    /// A frame this caller may receive.
    Frame(Value),
    /// Nothing this caller may receive, and nothing it needs to resynchronize.
    Skip,
    /// This caller's view can no longer be expressed as an increment.
    Replace,
}

/// The feed's mutable state, held behind one lock so a page and its cursor are always consistent.
#[derive(Debug)]
struct FeedState {
    /// Every known row, by id.
    rows: BTreeMap<String, Workspace>,
    /// The complete manual order, as the registry holds it.
    order: Vec<String>,
    /// The complete archive set.
    archived: Vec<String>,
    /// The complete pin set, most recently pinned first.
    pinned: Vec<String>,
    /// Retained increments, oldest first.
    journal: VecDeque<JournalEntry>,
    /// The next sequence number to assign.
    next_seq: u64,
    /// Workspace ids that existed *before* the oldest retained increment.
    ///
    /// Access-independent on purpose: it is a fact about the registry, not about one caller. A
    /// replay turns it into a caller's delivered set by intersecting it with what that caller may
    /// see, which is exactly the set a baseline taken at that point would have handed them.
    ///
    /// Advanced only when the oldest entry is pruned, by applying that entry; that is what makes a
    /// replay from the middle of the journal exact rather than a guess.
    front_ids: BTreeSet<String>,
}

impl FeedState {
    /// The ids a baseline taken at the journal's front would have delivered, for one caller.
    fn delivered_at_front(&self, access: &dyn FollowAccess) -> BTreeSet<String> {
        self.front_ids
            .iter()
            .filter(|workspace_id| access.allows_workspace(workspace_id))
            .cloned()
            .collect()
    }
}

// ---- projection -------------------------------------------------------------------------------

/// Projects one row for the wire, hiding archived and inaccessible sessions.
fn project_workspace(
    workspace: &Workspace,
    archived: &[String],
    access: &dyn FollowAccess,
) -> Value {
    let session_ids: Vec<String> = workspace
        .session_ids
        .iter()
        .filter(|session_id| !archived.iter().any(|hidden| hidden == *session_id))
        .filter(|session_id| access.allows_session(session_id))
        .cloned()
        .collect();
    json!({
        "workspace_id": workspace.id,
        "path": workspace.path.display().to_string(),
        "title": workspace.title,
        "session_ids": session_ids,
        "created_at": workspace.created_at,
        "updated_at": workspace.updated_at,
    })
}

/// Filters one session-id list to what a caller may see.
fn visible_sessions(session_ids: &[String], access: &dyn FollowAccess) -> Vec<String> {
    session_ids
        .iter()
        .filter(|session_id| access.allows_session(session_id))
        .cloned()
        .collect()
}

/// Builds the complete filtered state a generation starts from.
fn baseline_value(state: &FeedState, access: &dyn FollowAccess) -> Value {
    let items: Vec<Value> = state
        .order
        .iter()
        .filter(|workspace_id| access.allows_workspace(workspace_id))
        .filter_map(|workspace_id| state.rows.get(workspace_id))
        .map(|workspace| project_workspace(workspace, &state.archived, access))
        .collect();
    json!({
        "items": items,
        "archived_session_ids": visible_sessions(&state.archived, access),
        "pinned_session_ids": visible_sessions(&state.pinned, access),
    })
}

/// Projects one increment, maintaining the ids this caller is known to have received.
fn project_increment(
    increment: &Increment,
    archived: &[String],
    access: &dyn FollowAccess,
    delivered: &mut BTreeSet<String>,
    front_ids: &BTreeSet<String>,
) -> Emitted {
    match increment {
        Increment::Upsert(workspace) => {
            if access.allows_workspace(&workspace.id) {
                delivered.insert(workspace.id.clone());
                Emitted::Frame(json!({
                    "type": "upsert",
                    "workspace": project_workspace(workspace, archived, access),
                }))
            } else if delivered.remove(&workspace.id) || front_ids.contains(&workspace.id) {
                // Rule 2: it was delivered once, so the caller holds it. A `remove` would say the
                // workspace was deleted; what happened is that this caller may no longer see it.
                //
                // A row that existed at the journal's front is treated as possibly delivered even
                // when the caller's current grants hide it, because this build cannot reconstruct
                // which grants a caller held when it took its baseline. A redundant `replace`
                // supersedes a view that was already correct; a missing one leaves a stale row on
                // screen, so the ambiguity is resolved toward the safe answer.
                Emitted::Replace
            } else {
                // It did not exist at the front and is not visible now, so this caller has never
                // been shown it and must not learn of it by its absence.
                Emitted::Skip
            }
        }
        Increment::Remove(workspace_id) => {
            // Rule 3: a deletion reaches only a caller that received the row. Deliberately no
            // `front_ids` fallback here, unlike the upsert arm: a row that existed at the front but
            // was hidden from this caller was never delivered, so reporting its deletion would tell
            // the caller that a row it never saw had existed.
            if delivered.remove(workspace_id) {
                Emitted::Frame(json!({ "type": "remove", "workspace_id": workspace_id }))
            } else {
                Emitted::Skip
            }
        }
        Increment::Order(workspace_ids) => {
            if workspace_ids.iter().any(|workspace_id| {
                delivered.contains(workspace_id) && !access.allows_workspace(workspace_id)
            }) {
                Emitted::Replace
            } else {
                let visible: Vec<String> = workspace_ids
                    .iter()
                    .filter(|workspace_id| access.allows_workspace(workspace_id))
                    .cloned()
                    .collect();
                Emitted::Frame(json!({ "type": "order", "workspace_ids": visible }))
            }
        }
        Increment::Archived(session_ids) => Emitted::Frame(json!({
            "type": "archived",
            "archived_session_ids": visible_sessions(session_ids, access),
        })),
        Increment::Pinned(session_ids) => Emitted::Frame(json!({
            "type": "pinned",
            "pinned_session_ids": visible_sessions(session_ids, access),
        })),
    }
}

// ---- the feed ---------------------------------------------------------------------------------

/// One page of a `workspace.follow` subscription.
#[derive(Debug, Clone, PartialEq)]
pub struct FollowPage {
    /// The frames in order; the first page of a generation starts with exactly one `baseline`.
    pub frames: Vec<Value>,
    /// The cursor to send back as `afterSeq` on the next poll.
    pub next_seq: u64,
    /// Always false: this subscription ends when the caller stops asking.
    ///
    /// Present because a client that read `session.follow` expects the field, and a client that
    /// had to special-case its absence would encode this module's internal honesty as a branch.
    pub done: bool,
}

impl FollowPage {
    /// Renders the page for the wire.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "frames": self.frames,
            "next_seq": self.next_seq,
            "done": self.done,
        })
    }
}

/// The reconnect-safe state feed behind `workspace.follow`.
///
/// One feed serves every subscriber: increments are journaled once and projected per caller, so two
/// clients with different access see the same facts through different filters.
#[derive(Debug)]
pub struct WorkspaceFeed {
    state: Mutex<FeedState>,
    capacity: usize,
}

impl WorkspaceFeed {
    /// Snapshots the registry and the preference sets as the feed's starting state.
    ///
    /// The snapshot is not an increment: a caller's first page is a `baseline` built from the
    /// current state, so nothing that happened before the feed existed has to be replayed.
    #[must_use]
    pub fn new(workspaces: &WorkspaceRegistry, preferences: &dyn WorkspacePreferences) -> Self {
        Self::with_capacity(workspaces, preferences, DEFAULT_JOURNAL_CAPACITY)
    }

    /// Same, with an explicit journal capacity.
    ///
    /// Exposed so a test can prove that a cursor older than the retained window resynchronizes
    /// with a `replace` instead of silently losing frames; a smaller window is also a legitimate
    /// deployment choice for a process that would rather resync than hold memory.
    #[must_use]
    pub fn with_capacity(
        workspaces: &WorkspaceRegistry,
        preferences: &dyn WorkspacePreferences,
        capacity: usize,
    ) -> Self {
        let listed = workspaces.list();
        let order: Vec<String> = listed
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect();
        // Every row that exists when the feed is created is in the first baseline, so it counts as
        // delivered from the journal's point of view. Seeding it here is what keeps a later
        // `replace` exact instead of merely frequent.
        let front_ids: BTreeSet<String> = order.iter().cloned().collect();
        let rows: BTreeMap<String, Workspace> = listed
            .into_iter()
            .map(|workspace| (workspace.id.clone(), workspace))
            .collect();
        Self {
            state: Mutex::new(FeedState {
                rows,
                order,
                archived: preferences.archived_session_ids(),
                pinned: preferences.pinned_session_ids(),
                journal: VecDeque::new(),
                // Starts at 1 so that `afterSeq: 0` can mean "start a generation" while a
                // baseline's returned cursor is already nonzero. Without that, a client that had
                // just received a baseline would send `afterSeq: 0` again and be sent a second one.
                next_seq: 1,
                front_ids,
            }),
            capacity: capacity.max(1),
        }
    }

    /// Brings the feed up to date with the registry and the preference sets.
    ///
    /// Called before every page rather than from a background listener because this crate has no
    /// domain-change event to subscribe to; the registry *is* the authority, so diffing it is the
    /// faithful port of upstream's `domain/changed` handler. The diff publishes only what changed:
    /// a row that was re-listed unchanged produces no frame, which is what keeps a poll idempotent.
    ///
    /// Deliberately takes no [`FollowAccess`]: what changed is a fact about the registry, and the
    /// same increment is journaled once and projected differently for each caller. Threading access
    /// in here would journal a filtered fact and make the journal useless to a second caller.
    pub fn refresh(&self, workspaces: &WorkspaceRegistry, preferences: &dyn WorkspacePreferences) {
        let listed = workspaces.list();
        let next_order: Vec<String> = listed
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect();
        let archived = preferences.archived_session_ids();
        let pinned = preferences.pinned_session_ids();

        let mut state = lock(&self.state);

        let mut pending: Vec<Increment> = Vec::new();
        for workspace_id in state.rows.keys() {
            if !next_order.iter().any(|known| known == workspace_id) {
                pending.push(Increment::Remove(workspace_id.clone()));
            }
        }
        for workspace in &listed {
            if state.rows.get(&workspace.id) != Some(workspace) {
                pending.push(Increment::Upsert(workspace.clone()));
            }
        }
        if state.order != next_order {
            pending.push(Increment::Order(next_order.clone()));
        }
        if state.archived != archived {
            pending.push(Increment::Archived(archived.clone()));
        }
        if state.pinned != pinned {
            pending.push(Increment::Pinned(pinned.clone()));
        }
        if pending.is_empty() {
            return;
        }

        let removed: Vec<String> = state
            .rows
            .keys()
            .filter(|workspace_id| !next_order.iter().any(|known| known == *workspace_id))
            .cloned()
            .collect();
        for workspace_id in removed {
            state.rows.remove(&workspace_id);
        }
        for workspace in &listed {
            state.rows.insert(workspace.id.clone(), workspace.clone());
        }
        state.order = next_order;
        state.archived = archived;
        state.pinned = pinned;

        for increment in pending {
            Self::publish(&mut state, increment, self.capacity);
        }
    }

    /// Appends one increment to the journal, pruning the oldest when it is full.
    fn publish(state: &mut FeedState, increment: Increment, capacity: usize) {
        state.next_seq = state.next_seq.saturating_add(1);
        let seq = state.next_seq;
        state.journal.push_back(JournalEntry { seq, increment });
        while state.journal.len() > capacity {
            if let Some(pruned) = state.journal.pop_front() {
                // The new front's "already delivered" set is the old front's plus the pruned
                // increment's effect. This is the only place the set moves, which is why it stays
                // exact no matter how much of the journal is discarded.
                apply_delivered(&pruned.increment, &mut state.front_ids);
            }
        }
    }

    /// The complete filtered state as one baseline.
    #[must_use]
    pub fn baseline(&self, access: &dyn FollowAccess) -> Value {
        let state = lock(&self.state);
        baseline_value(&state, access)
    }

    /// Reads one page after `after`.
    ///
    /// `after == 0` starts a generation: the page is a single `baseline` carrying current state,
    /// and the cursor it returns is already caught up, so a client never re-applies history it
    /// just received in full. A nonzero cursor continues the journal; a cursor older than the
    /// retained window, or one whose replay would have produced a `replace` it never saw, is
    /// answered with a `replace` carrying a fresh baseline.
    #[must_use]
    pub fn page(&self, after: u64, limit: usize, access: &dyn FollowAccess) -> FollowPage {
        let state = lock(&self.state);
        let limit = limit.max(1);
        if after == 0 {
            return FollowPage {
                frames: vec![json!({
                    "type": "baseline",
                    "value": baseline_value(&state, access),
                })],
                next_seq: state.next_seq,
                done: false,
            };
        }
        if after >= state.next_seq {
            return FollowPage {
                frames: Vec::new(),
                next_seq: state.next_seq,
                done: false,
            };
        }
        let front = state
            .journal
            .front()
            .map_or(state.next_seq + 1, |entry| entry.seq);
        if after.saturating_add(1) < front {
            return replace_page(&state, access);
        }

        let mut delivered = state.delivered_at_front(access);
        let mut frames = Vec::new();
        let mut cursor = after;
        let mut truncated = false;
        for entry in &state.journal {
            let seen = entry.seq <= after;
            match project_increment(
                &entry.increment,
                &state.archived,
                access,
                &mut delivered,
                &state.front_ids,
            ) {
                Emitted::Skip => {}
                Emitted::Frame(value) => {
                    if seen {
                        continue;
                    }
                    if frames.len() == limit {
                        truncated = true;
                        break;
                    }
                    frames.push(value);
                    cursor = entry.seq;
                }
                Emitted::Replace => {
                    if seen {
                        // The caller already received the resync this entry implies, and the
                        // replay is only walking it to rebuild its delivered set. Superseding
                        // again would send a redundant baseline on every page.
                        continue;
                    }
                    // The caller's view cannot be reconstructed from increments: it holds a row it
                    // may no longer see, or missed a frame this journal no longer retains.
                    return replace_page(&state, access);
                }
            }
        }
        FollowPage {
            frames,
            next_seq: if truncated { cursor } else { state.next_seq },
            done: false,
        }
    }

    /// The cursor a client should send to begin, for documentation and for tests.
    #[must_use]
    pub fn head(&self) -> u64 {
        lock(&self.state).next_seq
    }

    /// How many increments the journal currently retains.
    #[must_use]
    pub fn retained(&self) -> usize {
        lock(&self.state).journal.len()
    }
}

/// Applies one pruned increment to the registry-level front id set.
///
/// Deliberately access-blind: this set answers "which workspaces existed at this point in the
/// journal", which is the same answer for every caller. Projecting it through a caller's access
/// happens in [`FeedState::delivered_at_front`], so two callers with different grants cannot corrupt
/// each other's replay by sharing this field.
fn apply_delivered(increment: &Increment, front_ids: &mut BTreeSet<String>) {
    match increment {
        Increment::Upsert(workspace) => {
            front_ids.insert(workspace.id.clone());
        }
        Increment::Remove(workspace_id) => {
            front_ids.remove(workspace_id);
        }
        Increment::Order(_) | Increment::Archived(_) | Increment::Pinned(_) => {}
    }
}

/// One complete state that supersedes a caller's view.
fn replace_page(state: &FeedState, access: &dyn FollowAccess) -> FollowPage {
    FollowPage {
        frames: vec![json!({
            "type": "replace",
            "value": baseline_value(state, access),
        })],
        next_seq: state.next_seq,
        done: false,
    }
}

// ---- the follow registration ------------------------------------------------------------------

/// Registers `workspace.follow`.
///
/// The handler refreshes the feed from the registry and the preference sets, then returns one
/// page. That ordering matters: a page computed from a stale feed would tell a client it is caught
/// up while the registry had already moved, and the next poll would then look like a skipped
/// change.
pub fn register_workspace_follow(
    registry: &mut RemoteRegistry,
    workspaces: &Arc<WorkspaceRegistry>,
    preferences: &Arc<dyn WorkspacePreferences>,
    feed: &Arc<WorkspaceFeed>,
    access: Arc<dyn FollowAccess>,
) {
    let workspaces = Arc::clone(workspaces);
    let preferences = Arc::clone(preferences);
    let feed = Arc::clone(feed);
    registry.declare(NAMESPACE, FOLLOW_METHOD, Declaration::read(NAMESPACE));
    registry.register(NAMESPACE, FOLLOW_METHOD, move |call: RemoteCall| {
        let workspaces = Arc::clone(&workspaces);
        let preferences = Arc::clone(&preferences);
        let feed = Arc::clone(&feed);
        let access = Arc::clone(&access);
        Box::pin(async move {
            let after = call
                .args
                .get("afterSeq")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let limit = call
                .args
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(DEFAULT_FOLLOW_FRAMES);
            feed.refresh(workspaces.as_ref(), preferences.as_ref());
            Ok(feed.page(after, limit, access.as_ref()).to_json())
        })
    });
}

/// Registers `workspace.follow` granting every caller full visibility.
pub fn register_workspace_follow_allow_all(
    registry: &mut RemoteRegistry,
    workspaces: &Arc<WorkspaceRegistry>,
    preferences: &Arc<dyn WorkspacePreferences>,
    feed: &Arc<WorkspaceFeed>,
) {
    register_workspace_follow(registry, workspaces, preferences, feed, Arc::new(AllowAll));
}

// ---- first-use directory policy ---------------------------------------------------------------

/// Where the first-use Documents directory comes from.
///
/// A seam because the answer is a host fact: a deployment pins it in configuration, a desktop reads
/// it from the operating system, and a test supplies it. Nothing here creates the directory by
/// itself, which is the upstream rule and the reason a customer's Documents folder is never
/// surprised by an installation.
pub trait DocumentsDirectory: Send + Sync {
    /// The fully qualified directory, validated.
    ///
    /// # Errors
    ///
    /// Returns a reason a caller can show when the account has no usable Documents directory.
    fn resolve(&self) -> Result<PathBuf, String>;
}

/// A Documents directory the deployment named.
#[derive(Debug, Clone)]
pub struct ConfiguredDocuments {
    /// The configured spelling.
    pub directory: PathBuf,
}

impl ConfiguredDocuments {
    /// Pins one directory.
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }
}

impl DocumentsDirectory for ConfiguredDocuments {
    fn resolve(&self) -> Result<PathBuf, String> {
        validate_documents_directory(&self.directory.display().to_string())
    }
}

/// The operating system's Documents directory.
///
/// Probes the same three commands upstream does, and refuses rather than guessing when the account
/// has disabled the user directory: XDG answers with the home directory in that case, and treating
/// home as Documents would create a workspace one level above where anyone would look for it.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemDocuments;

impl DocumentsDirectory for SystemDocuments {
    fn resolve(&self) -> Result<PathBuf, String> {
        let candidate = system_documents_directory()?;
        validate_documents_directory(&candidate)
    }
}

/// Runs the platform's Documents lookup.
fn system_documents_directory() -> Result<String, String> {
    let (program, arguments): (&str, &[&str]) = if cfg!(target_os = "macos") {
        (
            "osascript",
            &[
                "-e",
                "POSIX path of (path to documents folder from user domain without folder creation)",
            ],
        )
    } else if cfg!(target_os = "windows") {
        (
            "powershell.exe",
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[Environment]::GetFolderPath([Environment+SpecialFolder]::MyDocuments, [Environment+SpecialFolderOption]::DoNotVerify)",
            ],
        )
    } else {
        ("xdg-user-dir", &["DOCUMENTS"])
    };

    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("system Documents directory is unavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{program}` could not report the Documents directory (status {})",
            output.status
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let directory = text.trim_end_matches(['\r', '\n']);
    if directory.is_empty() {
        return Err("system Documents directory is unavailable".to_owned());
    }
    Ok(directory.to_owned())
}

/// Validates a Documents directory without resolving it against the working directory.
///
/// A relative spelling is refused rather than joined: `Documents` under whatever directory the
/// process happened to start in would register a workspace that moves when the launcher does.
///
/// # Errors
///
/// Returns a reason when the spelling is not fully qualified.
pub fn validate_documents_directory(directory: &str) -> Result<PathBuf, String> {
    validate_documents_directory_for(directory, cfg!(windows))
}

/// Same, for a named platform, so the rule is testable on any host.
///
/// # Errors
///
/// Returns a reason when the spelling is not fully qualified, or when a Windows drive is named
/// without a directory below it.
pub fn validate_documents_directory_for(directory: &str, windows: bool) -> Result<PathBuf, String> {
    let path = Path::new(directory);
    // A drive-rooted spelling is absolute on Windows and unusable on any other host, so the check
    // is made on the spelling rather than through `Path::is_absolute`, which answers for the host
    // this build happens to run on. Validating Windows configuration on a Linux CI machine must
    // give the same verdict it would give on Windows.
    let drive_rooted = {
        let bytes = directory.as_bytes();
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
    };
    if !(path.is_absolute() || (windows && drive_rooted)) {
        return Err(format!(
            "Documents directory must be fully qualified: `{directory}`"
        ));
    }
    if windows {
        // `C:\` and `\` are drive roots, not a user's Documents directory: joining a leaf onto a
        // root would put one installation's workspace at the top of a volume shared with others.
        let text = directory.trim_end_matches(['\\', '/']);
        let bytes = text.as_bytes();
        let drive_root = bytes.len() == 2 && bytes[1] == b':';
        if text.is_empty() || drive_root {
            return Err(format!(
                "Documents directory must not be a drive root: `{directory}`"
            ));
        }
    }
    Ok(normalize_lexically(path))
}

/// The first-use workspace directory under a valid Documents directory.
#[must_use]
pub fn default_workspace_path(documents: &Path) -> PathBuf {
    documents
        .join(HARNESS_DIRECTORY)
        .join(DEFAULT_WORKSPACE_DIRECTORY)
}

/// The text one workspace is labeled with.
///
/// A workspace still carrying the automatic first-use title reads as the caller's localized
/// default name; every other title reads verbatim in every language. A title the user typed as
/// exactly [`DEFAULT_WORKSPACE_DIRECTORY`] is labeled as the default too, and nothing else depends
/// on the distinction.
#[must_use]
pub fn workspace_display_title(title: &str, localized_default: &str) -> String {
    if title == DEFAULT_WORKSPACE_DIRECTORY {
        localized_default.to_owned()
    } else {
        title.to_owned()
    }
}

/// Whether a title is the automatic first-use one.
#[must_use]
pub fn is_default_workspace_title(title: &str) -> bool {
    title == DEFAULT_WORKSPACE_DIRECTORY
}

/// The registry id a path derives.
///
/// The same FNV-1a fold `workspace.create` uses, so a workspace initialized here and one created
/// through that method land on the same id for the same directory instead of registering it twice.
#[must_use]
pub fn workspace_id_for_path(path: &str) -> String {
    let mut hash: u32 = 2_166_136_261;
    for byte in path.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    format!("ws-{hash}")
}

/// Drops `.` and `..` segments without touching the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Registers `workspace.initializeDefault`.
///
/// Ineligible while the registry already holds a row, which is the upstream rule: first use means
/// first, and re-running initialization would relocate a workspace the user has been working in.
/// Returns `{"workspace": null}` when ineligible rather than an error, because a client calling
/// this on every boot must not see a failure on every boot after the first.
pub fn register_initialize_default(
    registry: &mut RemoteRegistry,
    workspaces: &Arc<WorkspaceRegistry>,
    documents: Arc<dyn DocumentsDirectory>,
) {
    let workspaces = Arc::clone(workspaces);
    registry.declare(
        NAMESPACE,
        INITIALIZE_DEFAULT_METHOD,
        Declaration::write(NAMESPACE),
    );
    registry.register(
        NAMESPACE,
        INITIALIZE_DEFAULT_METHOD,
        move |_call: RemoteCall| {
            let workspaces = Arc::clone(&workspaces);
            let documents = Arc::clone(&documents);
            Box::pin(async move {
                if !workspaces.list().is_empty() {
                    return Ok(json!({ "workspace": Value::Null }));
                }
                let root = documents.resolve().map_err(|reason| {
                    RemoteError::Failed(format!(
                        "`{NAMESPACE}.{INITIALIZE_DEFAULT_METHOD}` cannot locate a Documents \
                         directory: {reason}"
                    ))
                })?;
                let path = default_workspace_path(&root);
                tokio::fs::create_dir_all(&path).await.map_err(|error| {
                    RemoteError::Failed(format!(
                        "`{NAMESPACE}.{INITIALIZE_DEFAULT_METHOD}` cannot create `{}`: {error}",
                        path.display()
                    ))
                })?;
                let spelling = path.display().to_string();
                let id = workspace_id_for_path(&spelling);
                let now = now_millis();
                let workspace = match workspaces.register(id, &spelling, None, now) {
                    Ok(created) => created,
                    // Already registered is the idempotent arm, not a failure: the registry is the
                    // authority on identity, and a second boot must reuse the row it made.
                    Err(existing) => workspaces.get(&existing).ok_or_else(|| {
                        RemoteError::Failed(format!(
                            "workspace registry reported `{existing}` for `{spelling}` but holds no such row"
                        ))
                    })?,
                };
                let view = workspace.view(now);
                Ok(json!({
                    "workspace": {
                        "workspace_id": view.workspace_id,
                        "path": view.path,
                        "title": view.title,
                        "session_ids": view.session_ids,
                        "created_at": view.created_at,
                        "updated_at": view.updated_at,
                    }
                }))
            })
        },
    );
}

// ---- directory picker -------------------------------------------------------------------------

/// What one composed picking backend can serve.
///
/// A closed pair because the verbs differ: `pick` needs a window the host opens, while `list` and
/// `createDirectory` need a directory it can read. A backend that has one and not the other says so
/// instead of approximating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerCapability {
    /// An operating-system chooser.
    Native,
    /// In-app browse primitives over a directory the host can read.
    Browse,
}

impl PickerCapability {
    /// The capability's wire name, as upstream spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Browse => "browse",
        }
    }
}

/// One directory row: a listing child or a breadcrumb ancestor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    /// Base name shown in a browser row; a root crumb carries its full path.
    pub name: String,
    /// Absolute host path — clients never join path segments themselves.
    pub path: String,
    /// Hidden by the host platform's convention; the client owns whether to show it.
    pub hidden: bool,
}

impl DirectoryEntry {
    /// Renders the row for the wire.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({ "name": self.name, "path": self.path, "hidden": self.hidden })
    }
}

/// One directory level plus its ancestry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryListing {
    /// Absolute path of the listed directory.
    pub path: String,
    /// The host account's home directory, for breadcrumb rooting.
    pub home: String,
    /// Ancestors from the filesystem root to the listed directory, inclusive.
    pub crumbs: Vec<DirectoryEntry>,
    /// Direct child directories, name-sorted.
    pub entries: Vec<DirectoryEntry>,
    /// Whether the entry cap dropped children.
    pub truncated: bool,
}

impl DirectoryListing {
    /// Renders the listing for the wire.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "home": self.home,
            "crumbs": self.crumbs.iter().map(DirectoryEntry::to_json).collect::<Vec<Value>>(),
            "entries": self.entries.iter().map(DirectoryEntry::to_json).collect::<Vec<Value>>(),
            "truncated": self.truncated,
        })
    }
}

/// One picking failure, carrying the seam's closed code and the path it is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerError {
    /// One of the `directory-picker/*` codes, or [`PICKER_INVALID_REQUEST`].
    pub code: &'static str,
    /// What the failure is about.
    pub path: String,
    /// A message a caller can show.
    pub message: String,
}

impl PickerError {
    /// Builds a failure.
    #[must_use]
    pub fn new(code: &'static str, path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            code,
            message: format!("{code}: {path}"),
            path,
        }
    }

    /// Builds a failure with its own explanation.
    #[must_use]
    pub fn explained(
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            path: path.into(),
            message: message.into(),
        }
    }
}

/// The directory-picking seam.
///
/// The three verbs have defaults that refuse with the capability they need, so a backend
/// implements only what its composition actually serves and every unservable verb answers
/// `directory-picker/unavailable` instead of a confusing success.
pub trait DirectoryPickerBackend: Send + Sync {
    /// What this backend serves.
    fn capability(&self) -> PickerCapability;

    /// Opens the host's chooser.
    ///
    /// # Errors
    ///
    /// Returns a failure when the chooser is unavailable or the operator's session refused it.
    fn pick(&self) -> Result<Option<String>, PickerError> {
        Err(PickerError::new(PICKER_UNAVAILABLE, ""))
    }

    /// Lists one level.
    ///
    /// # Errors
    ///
    /// Returns a failure when the directory cannot be read.
    fn list(&self, path: Option<&str>) -> Result<DirectoryListing, PickerError> {
        let _ = path;
        Err(PickerError::new(PICKER_UNAVAILABLE, ""))
    }

    /// Creates one child directory and returns its absolute path.
    ///
    /// # Errors
    ///
    /// Returns a failure when the parent is not usable or the child already exists.
    fn create_directory(&self, path: &str, name: &str) -> Result<String, PickerError> {
        let _ = (path, name);
        Err(PickerError::new(PICKER_UNAVAILABLE, ""))
    }
}

/// The `browse` capability over the host filesystem.
///
/// Reads one level at a time because that is what an in-app browser asks for: a tree at the right
/// depth is a whole subtree, and a browser refreshing one folder should not re-walk it.
#[derive(Debug, Default, Clone)]
pub struct LocalDirectoryPicker {
    home: Option<PathBuf>,
}

impl LocalDirectoryPicker {
    /// Uses the account's home directory from the environment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pins the home directory, for a host that knows it better than the environment does.
    #[must_use]
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self {
            home: Some(home.into()),
        }
    }

    /// The home directory this picker reports.
    fn home(&self) -> Result<PathBuf, PickerError> {
        self.home.clone().or_else(home_directory).ok_or_else(|| {
            PickerError::explained(
                PICKER_UNREADABLE,
                "",
                "the host account has no home directory",
            )
        })
    }
}

impl DirectoryPickerBackend for LocalDirectoryPicker {
    fn capability(&self) -> PickerCapability {
        PickerCapability::Browse
    }

    fn list(&self, path: Option<&str>) -> Result<DirectoryListing, PickerError> {
        let home = self.home()?;
        let requested = match path {
            Some(spelling) if !spelling.trim().is_empty() => PathBuf::from(spelling),
            _ => home.clone(),
        };
        if !requested.is_absolute() {
            return Err(PickerError::explained(
                PICKER_UNREADABLE,
                requested.display().to_string(),
                "the directory to list must be fully qualified",
            ));
        }
        let listed = std::fs::canonicalize(&requested).map_err(|error| {
            PickerError::explained(
                PICKER_UNREADABLE,
                requested.display().to_string(),
                format!("cannot read `{}`: {error}", requested.display()),
            )
        })?;
        let reader = std::fs::read_dir(&listed).map_err(|error| {
            PickerError::explained(
                PICKER_UNREADABLE,
                listed.display().to_string(),
                format!("cannot read `{}`: {error}", listed.display()),
            )
        })?;

        let mut children: Vec<(String, DirectoryEntry)> = Vec::new();
        for entry in reader.flatten() {
            let is_directory = entry.metadata().is_ok_and(|metadata| metadata.is_dir());
            // Only directories are rows: a picker that showed files would offer targets that
            // cannot be picked, and the caller would have to filter them out again.
            if !is_directory {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            children.push((
                name.clone(),
                DirectoryEntry {
                    hidden: name.starts_with('.'),
                    name,
                    path: entry.path().display().to_string(),
                },
            ));
        }
        // Name-sorted before the cut, so what the cap drops is the tail of a stable order rather
        // than whichever entries the filesystem happened to return last.
        children.sort_by(|left, right| left.0.cmp(&right.0));
        let truncated = children.len() > MAX_DIRECTORY_ENTRIES;
        let entries = children
            .into_iter()
            .take(MAX_DIRECTORY_ENTRIES)
            .map(|(_, entry)| entry)
            .collect();

        Ok(DirectoryListing {
            path: listed.display().to_string(),
            home: home.display().to_string(),
            crumbs: breadcrumbs(&listed),
            entries,
            truncated,
        })
    }

    fn create_directory(&self, path: &str, name: &str) -> Result<String, PickerError> {
        if !Path::new(path).is_absolute() {
            return Err(PickerError::explained(
                PICKER_INVALID_REQUEST,
                path,
                "the parent directory must be fully qualified",
            ));
        }
        let target = Path::new(path).join(name);
        if target.exists() {
            return Err(PickerError::explained(
                PICKER_EXISTS,
                target.display().to_string(),
                format!("`{}` already exists", target.display()),
            ));
        }
        std::fs::create_dir(&target).map_err(|error| {
            PickerError::explained(
                PICKER_CREATE_FAILED,
                target.display().to_string(),
                format!("cannot create `{}`: {error}", target.display()),
            )
        })?;
        Ok(target.display().to_string())
    }
}

/// The account's home directory, when the environment names one.
fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// The ancestor chain from the filesystem root to `path`, inclusive.
fn breadcrumbs(path: &Path) -> Vec<DirectoryEntry> {
    let mut chain: Vec<DirectoryEntry> = path
        .ancestors()
        .map(|ancestor| DirectoryEntry {
            name: ancestor.file_name().map_or_else(
                || ancestor.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
            path: ancestor.display().to_string(),
            // A breadcrumb is a jump target the client shows unconditionally; marking a crumb
            // hidden would let a hidden-directory filter remove the route back to the root.
            hidden: false,
        })
        .collect();
    chain.reverse();
    chain
}

/// Validates one child name: a single, non-blank path segment.
///
/// The rule is checked before the filesystem is touched, and the same rule upstream states: a name
/// carrying a separator is a path, and accepting it would let a caller create a directory outside
/// the parent it named.
///
/// # Errors
///
/// Returns a `gateway/bad-request` failure naming what was wrong.
pub fn validate_directory_name(name: &str) -> Result<(), PickerError> {
    let rejected = name.trim().is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\');
    if rejected {
        return Err(PickerError::explained(
            PICKER_INVALID_REQUEST,
            name,
            "createDirectory requires a single non-blank path segment name",
        ));
    }
    Ok(())
}

/// Maps one seam failure onto the Remote error a caller discriminates on.
fn picker_failure(error: PickerError) -> RemoteError {
    if error.code == PICKER_INVALID_REQUEST {
        return RemoteError::InvalidArguments {
            namespace: PICKER_NAMESPACE.to_owned(),
            method: "createDirectory".to_owned(),
            message: error.message,
        };
    }
    // The code is part of the message because [`RemoteError`] carries no code field: a client that
    // had to match on prose would break the moment somebody reworded a sentence, and the closed
    // `directory-picker/*` vocabulary is the part upstream promises to keep stable.
    RemoteError::Failed(format!("{}: {}", error.code, error.message))
}

/// The refusal for a verb the composed backend cannot serve.
fn unavailable(method: &str, needed: PickerCapability, served: PickerCapability) -> RemoteError {
    RemoteError::Failed(format!(
        "{PICKER_UNAVAILABLE}: directoryPicker.{method} needs the {} capability; the composed \
         picker serves \"{}\"",
        needed.name(),
        served.name()
    ))
}

/// Registers `directoryPicker.pick`, `list`, and `createDirectory`.
///
/// The namespace is registered whole. Upstream registers it only once a backend is composed,
/// because a verb nobody can serve is worse than an absent namespace; a deployment that has no
/// chooser should not call this at all, and the capability refusal covers the mixed case where a
/// browse-only backend is composed but a client asks for `pick`.
pub fn register_directory_picker(
    registry: &mut RemoteRegistry,
    backend: &Arc<dyn DirectoryPickerBackend>,
) {
    let picker = Arc::clone(backend);
    registry.declare(
        PICKER_NAMESPACE,
        "pick",
        Declaration::read("directory-picker"),
    );
    registry.register(PICKER_NAMESPACE, "pick", move |_call: RemoteCall| {
        let picker = Arc::clone(&picker);
        Box::pin(async move {
            let served = picker.capability();
            if served != PickerCapability::Native {
                return Err(unavailable("pick", PickerCapability::Native, served));
            }
            let selected = picker.pick().map_err(picker_failure)?;
            Ok(json!({ "path": selected }))
        })
    });

    let lister = Arc::clone(backend);
    registry.declare(
        PICKER_NAMESPACE,
        "list",
        Declaration::read("directory-picker"),
    );
    registry.register(PICKER_NAMESPACE, "list", move |call: RemoteCall| {
        let lister = Arc::clone(&lister);
        Box::pin(async move {
            let served = lister.capability();
            if served != PickerCapability::Browse {
                return Err(unavailable("list", PickerCapability::Browse, served));
            }
            let path = call.args.get("path").and_then(Value::as_str);
            lister
                .list(path)
                .map(|listing| listing.to_json())
                .map_err(picker_failure)
        })
    });

    let creator = Arc::clone(backend);
    registry.declare(
        PICKER_NAMESPACE,
        "createDirectory",
        Declaration::write("directory-picker"),
    );
    registry.register(
        PICKER_NAMESPACE,
        "createDirectory",
        move |call: RemoteCall| {
            let creator = Arc::clone(&creator);
            Box::pin(async move {
                let served = creator.capability();
                if served != PickerCapability::Browse {
                    return Err(unavailable(
                        "createDirectory",
                        PickerCapability::Browse,
                        served,
                    ));
                }
                let path = call
                    .args
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RemoteError::InvalidArguments {
                        namespace: PICKER_NAMESPACE.to_owned(),
                        method: "createDirectory".to_owned(),
                        message: "`path` is required".to_owned(),
                    })?;
                let name = call
                    .args
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RemoteError::InvalidArguments {
                        namespace: PICKER_NAMESPACE.to_owned(),
                        method: "createDirectory".to_owned(),
                        message: "`name` is required".to_owned(),
                    })?;
                validate_directory_name(name).map_err(picker_failure)?;
                creator
                    .create_directory(path, name)
                    .map(|created| json!({ "path": created }))
                    .map_err(picker_failure)
            })
        },
    );
}
