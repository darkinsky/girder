//! The rest of the `workspace` Remote surface: renaming, ordering, pinning, and archiving.
//!
//! Upstream's workspace controller declares eleven `@Remote` methods in this checkout: `create`,
//! `initializeDefault`, `rename`, `delete`, `insertBefore`, `insertSessionBefore`, `archiveSession`,
//! `unarchiveSession`, `pinSession`, `unpinSession`, and the streaming `follow`. This build had
//! `create`, `list`, and `delete`. This module adds the seven that are mutations of state the
//! registry can hold — `rename`, `insertBefore`, `insertSessionBefore`, `archiveSession`,
//! `unarchiveSession`, `pinSession`, `unpinSession` — and leaves two honest gaps:
//!
//! - `initializeDefault` is absent because it needs the host's documents-directory policy and the
//!   "first use" rule (only while the registry and session history are both empty), which is a
//!   deployment decision this module cannot make. Registering it with a guessed directory would
//!   create a workspace nobody asked for.
//! - `follow` is a stream, and this module registers request/response methods only.
//!
//! # What `WorkspaceRegistry` can and cannot hold
//!
//! [`WorkspaceRegistry`] stores one row per registration: id, path, title, `session_ids`,
//! `created_at`, `updated_at`. Three of the seven methods map onto that row directly and are
//! implemented against the registry's own writes, so `workspace.list` sees the change immediately:
//!
//! | method | storage | verdict |
//! |---|---|---|
//! | `rename` | [`WorkspaceRegistry::rename`] | direct |
//! | `insertBefore` | [`WorkspaceRegistry::insert_before`] | direct, with an append composed from it (see [`insert_before`]) |
//! | `insertSessionBefore` | `Workspace`'s `session_ids`, via detach/attach | composed (see [`insert_session_before`]) |
//!
//! Four do not, and they are the reason this module exists:
//!
//! | method | what it needs | what the registry has |
//! |---|---|---|
//! | `archiveSession` | a registry-global archive set | nothing — [`WorkspaceRegistry::archive_session`] is *workspace-scoped* and removes the session from `session_ids`, which would lose the position upstream promises to keep |
//! | `unarchiveSession` | the same set | nothing |
//! | `pinSession` | a registry-global pin set, most recently pinned first | nothing |
//! | `unpinSession` | the same set | nothing |
//!
//! `Workspace` has no metadata field, no sidecar, and no extension point, and this module may not add
//! one: the registry is another module's type. So the storage is an explicit seam,
//! [`WorkspacePreferences`], and the four methods above go through it. Two implementations ship
//! here: [`InMemoryPreferences`], which is durable for the life of the process and not a moment
//! longer, and [`UnavailablePreferences`], which refuses every write and says what is missing.
//!
//! # The known limitation, stated plainly
//!
//! **A pin or an archive recorded by [`InMemoryPreferences`] is lost on restart.** Nothing in this
//! build writes that set to disk. A user who pins a session and restarts sees it unpinned. That is a
//! real defect and it is stated here, in [`InMemoryPreferences`], and in the response docs rather
//! than papered over with a "saved" that only holds until the process exits.
//!
//! Note that the registry itself has the same property today: the deployment creates a fresh
//! [`WorkspaceRegistry`] at startup, so titles and order are in-memory as well. This module does not
//! make that worse, and it does not pretend otherwise. A deployment that persists workspaces should
//! also pass a [`WorkspacePreferences`] that writes beside them; the trait's six methods are the
//! whole contract.
//!
//! # Why `stopActivity` is refused instead of ignored
//!
//! Upstream's `archiveSession` accepts `stopActivity: true` to stop a session's running work
//! (turn, subagents, background jobs, schedules) instead of refusing the archive as
//! `workspace/session-active`. This build has neither the activity waterfall nor a stop channel, so
//! it can neither detect running work nor stop it. Answering such a request with a success would
//! tell the client its work was stopped while the work kept running against an archived session —
//! so the flag is refused with a reason, the way [`crate::session_lifecycle_rpc`] refuses `steer`
//! and `UnavailableArchiveStore` refuses a mark. Archiving without the flag still works: this build
//! performs no activity check at all, which is a gap rather than a refusal.
//!
//! # Wire shapes
//!
//! Request arguments are camelCase, as upstream spells them (`workspaceId`, `sessionId`,
//! `beforeWorkspaceId`, `beforeSessionId`, `stopActivity`). Response keys are snake_case, which is
//! what `workspace.list` and `workspace.create` in [`crate::remote`] already emit: mixing upstream's
//! camelCase into four methods of one namespace would force a client to decode two shapes for the
//! same row. The keys are therefore `workspace`, `workspace_id`, `session_ids`, `created_at`,
//! `updated_at`, `workspace_ids`, `archived_session_ids`, and `pinned_session_ids`.
//!
//! # Why the seams are parameters
//!
//! Every `register_*` function in this crate takes its capabilities as arguments, and this module is
//! no different: the host owns the registry, the preference store, and the session catalog, and a
//! module that reached for a global would give two registries in one process different answers about
//! the same pins.

use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};

use girder_core::api_config::{Workspace, WorkspaceRegistry};
use girder_core::{
    Access, Declaration, RemoteCall, RemoteError, RemoteRegistry, SessionCatalog, now_millis,
};

/// The namespace every method here lives under.
pub const NAMESPACE: &str = "workspace";

/// Renames a workspace.
pub const RENAME_METHOD: &str = "rename";

/// Moves a workspace within the registry's manual order.
pub const INSERT_BEFORE_METHOD: &str = "insertBefore";

/// Moves a session within its workspace's manual order.
pub const INSERT_SESSION_BEFORE_METHOD: &str = "insertSessionBefore";

/// Adds a session to the archive set.
pub const ARCHIVE_SESSION_METHOD: &str = "archiveSession";

/// Drops a session from the archive set.
pub const UNARCHIVE_SESSION_METHOD: &str = "unarchiveSession";

/// Adds a session to the pin set.
pub const PIN_SESSION_METHOD: &str = "pinSession";

/// Drops a session from the pin set.
pub const UNPIN_SESSION_METHOD: &str = "unpinSession";

/// The resource kind these methods touch, as reported to an access policy.
const RESOURCE_KIND: &str = "workspace";

/// What every method here declares to an access policy.
///
/// All seven change what a client will be shown later — a title, an order, or a hidden session — so
/// all seven are writes. None deletes anything: a registration, a title, and a manual order are all
/// recoverable by asking again, and a read-only mirror must refuse them all the same.
pub const DECLARED_ACCESS: Access = Access::Write;

/// The declaration every registration uses.
///
/// Built from [`DECLARED_ACCESS`] rather than spelling [`Declaration::write`] seven times, so the
/// constant and what the registry was told cannot say different things.
fn declaration() -> Declaration {
    match DECLARED_ACCESS {
        Access::Write => Declaration::write(RESOURCE_KIND),
        Access::Destructive => Declaration::destructive(RESOURCE_KIND),
        Access::Read => Declaration::read(RESOURCE_KIND),
    }
}

// ---- preference storage -----------------------------------------------------------------------

/// What a [`WorkspacePreferences`] write can fail with.
///
/// Three variants rather than one string because they call for different answers: no storage is a
/// deployment mistake an operator fixes, an archived id is a request the caller can change, and a
/// failed write is a condition a caller retries.
#[derive(Debug, thiserror::Error)]
pub enum PreferenceError {
    /// The deployment has nowhere to record pins and archives.
    #[error("no workspace preference storage is configured: {reason}")]
    Unavailable {
        /// What is missing, in words an operator can act on.
        reason: String,
    },

    /// The id is archived, and pinning and archival are mutually exclusive upstream.
    #[error("session `{session_id}` is archived")]
    Archived {
        /// The id the caller asked to pin.
        session_id: String,
    },

    /// The store exists but could not record the change.
    #[error("{0}")]
    Failed(String),
}

impl PreferenceError {
    /// A store that is not configured.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    /// A failure while writing.
    ///
    /// Constructed by a store that writes to disk; kept public so such a store does not have to
    /// stringify its own I/O error into the variant above.
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    /// Renders the failure as a [`RemoteError`] naming the method that hit it.
    ///
    /// `Unavailable` becomes [`RemoteError::Failed`] rather than
    /// [`RemoteError::NotImplemented`]: the method exists and is wired, and telling a client author
    /// to stop calling it would be wrong — the deployment is what is incomplete.
    fn into_remote(self, method: &str) -> RemoteError {
        match self {
            Self::Unavailable { reason } => RemoteError::Failed(format!(
                "`{NAMESPACE}.{method}` has nowhere to record its change: {reason}"
            )),
            Self::Archived { session_id } => invalid(
                method,
                format!("session `{session_id}` is archived; unarchive it before pinning"),
            ),
            Self::Failed(message) => {
                RemoteError::Failed(format!("`{NAMESPACE}.{method}` failed: {message}"))
            }
        }
    }
}

/// Where the registry-global pin set and archive set live.
///
/// A trait rather than a field on [`Workspace`] because the registry is another module's type and
/// has no metadata surface, and this module may not add one. The alternative to a seam would be an
/// in-memory flag dressed up as storage, which is exactly the false promise the module docs warn
/// about.
///
/// Implementations must be idempotent: a repeat of a state the store already holds is `Ok(false)`,
/// not an error, because a request that arrives twice after a timeout must not be shown as a
/// failure. They must also keep the two sets disjoint — archiving drops a pin — because upstream
/// defines pinning an archived session as a refusal, and a store that allowed both would leave a
/// hidden session on a surface reserved for visible ones.
pub trait WorkspacePreferences: Send + Sync {
    /// The pin set, most recently pinned first.
    ///
    /// A read: an id that was never pinned is not pinned, and a deployment with no store answers
    /// with an empty list rather than an error, so a listing that consults this still works.
    fn pinned_session_ids(&self) -> Vec<String>;

    /// The archive set, in archive order.
    fn archived_session_ids(&self) -> Vec<String>;

    /// Prepends one id to the pin set.
    ///
    /// `Ok(true)` when it was added, `Ok(false)` when it was already pinned — a repeat must not
    /// reorder, because the set's order is "most recently pinned first" and a retry was not a
    /// second pin.
    fn pin_session(&self, session_id: &str) -> Result<bool, PreferenceError>;

    /// Drops one id from the pin set; `Ok(false)` when it was not there.
    fn unpin_session(&self, session_id: &str) -> Result<bool, PreferenceError>;

    /// Appends one id to the archive set, dropping any pin in the same write.
    fn archive_session(&self, session_id: &str) -> Result<bool, PreferenceError>;

    /// Drops one id from the archive set; `Ok(false)` when it was not there.
    fn unarchive_session(&self, session_id: &str) -> Result<bool, PreferenceError>;
}

/// The state [`InMemoryPreferences`] holds.
#[derive(Debug, Default)]
struct PreferenceState {
    /// Pin set, most recently pinned first.
    pinned: Vec<String>,
    /// Archive set, in archive order.
    archived: Vec<String>,
}

/// A preference store held in memory.
///
/// **Durable for the life of the process and not a moment longer.** Nothing here writes to disk, so
/// a pin or an archive recorded by this store is gone after a restart; a user who pinned a session
/// and restarted sees it unpinned. That is a known limitation, stated here rather than implied.
///
/// It exists for tests and for a host that already keeps workspace metadata somewhere and would
/// rather lend this module a table than write a type. A host that wants the sets to survive a
/// restart implements [`WorkspacePreferences`] over its own store; the six methods are the whole
/// contract.
#[derive(Debug, Default)]
pub struct InMemoryPreferences {
    state: Mutex<PreferenceState>,
}

impl InMemoryPreferences {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // Poisoning is recovered rather than propagated: a panic while holding this table leaves a list
    // of ids, not a half-written one, and refusing every later call would turn one panic into a
    // permanently unusable preference surface.
    fn lock(&self) -> std::sync::MutexGuard<'_, PreferenceState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl WorkspacePreferences for InMemoryPreferences {
    fn pinned_session_ids(&self) -> Vec<String> {
        self.lock().pinned.clone()
    }

    fn archived_session_ids(&self) -> Vec<String> {
        self.lock().archived.clone()
    }

    fn pin_session(&self, session_id: &str) -> Result<bool, PreferenceError> {
        let mut state = self.lock();
        // Checked under the same lock as the write: a check made by the caller would race a
        // concurrent archive, and the pair would then leave the id in both sets.
        if state.archived.iter().any(|id| id == session_id) {
            return Err(PreferenceError::Archived {
                session_id: session_id.to_owned(),
            });
        }
        if state.pinned.iter().any(|id| id == session_id) {
            return Ok(false);
        }
        state.pinned.insert(0, session_id.to_owned());
        Ok(true)
    }

    fn unpin_session(&self, session_id: &str) -> Result<bool, PreferenceError> {
        let mut state = self.lock();
        let before = state.pinned.len();
        state.pinned.retain(|id| id != session_id);
        Ok(state.pinned.len() != before)
    }

    fn archive_session(&self, session_id: &str) -> Result<bool, PreferenceError> {
        let mut state = self.lock();
        if state.archived.iter().any(|id| id == session_id) {
            return Ok(false);
        }
        // One write, because the two sets are a single invariant: an id must never be both hidden
        // and pinned, and two writes could be observed in between.
        state.pinned.retain(|id| id != session_id);
        state.archived.push(session_id.to_owned());
        Ok(true)
    }

    fn unarchive_session(&self, session_id: &str) -> Result<bool, PreferenceError> {
        let mut state = self.lock();
        let before = state.archived.len();
        state.archived.retain(|id| id != session_id);
        Ok(state.archived.len() != before)
    }
}

/// The preference store a deployment gets when it supplies none.
///
/// It refuses every write and says why. Reads answer with empty sets rather than an error: a listing
/// that consults this must not fail because a deployment has not configured a store, and "nothing is
/// pinned or archived" is the truth — this store never accepted a mark.
#[derive(Debug)]
pub struct UnavailablePreferences {
    reason: String,
}

impl UnavailablePreferences {
    /// Creates a store that refuses with `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The reason the default registration uses.
    ///
    /// It names what a host can supply, because a message that only said "not supported" would
    /// leave an operator guessing which half to build.
    #[must_use]
    pub fn default_reason() -> Self {
        Self::new(
            "WorkspaceRegistry has no pin or archive metadata surface (no field on `Workspace` and \
             no sidecar of its own), and persistence for that set is a deployment decision; a host \
             must supply a WorkspacePreferences (for example InMemoryPreferences, whose contents a \
             restart loses) via register_workspace_extra_methods",
        )
    }
}

impl WorkspacePreferences for UnavailablePreferences {
    fn pinned_session_ids(&self) -> Vec<String> {
        Vec::new()
    }

    fn archived_session_ids(&self) -> Vec<String> {
        Vec::new()
    }

    fn pin_session(&self, _session_id: &str) -> Result<bool, PreferenceError> {
        Err(PreferenceError::unavailable(self.reason.clone()))
    }

    fn unpin_session(&self, _session_id: &str) -> Result<bool, PreferenceError> {
        Err(PreferenceError::unavailable(self.reason.clone()))
    }

    fn archive_session(&self, _session_id: &str) -> Result<bool, PreferenceError> {
        Err(PreferenceError::unavailable(self.reason.clone()))
    }

    fn unarchive_session(&self, _session_id: &str) -> Result<bool, PreferenceError> {
        Err(PreferenceError::unavailable(self.reason.clone()))
    }
}

/// Drops archived sessions from a listing of one workspace's account.
///
/// The archive set is registry-global and lives outside the row, so a caller that renders
/// `session_ids` must filter it. Upstream does this on the client; doing it here as well means a
/// server-side listing and a client-side one cannot disagree about which sessions are hidden.
pub fn retain_visible(session_ids: &mut Vec<String>, preferences: &dyn WorkspacePreferences) {
    let archived = preferences.archived_session_ids();
    session_ids.retain(|id| !archived.contains(id));
}

// ---- registration -----------------------------------------------------------------------------

/// Registers all seven methods against an explicit preference store.
///
/// `sessions` is needed for the two methods that must refuse an id no session holds: a pin or an
/// archive for an unknown id is a promise about nothing, would keep hiding whatever later took that
/// id, and would report a success the caller learns nothing from.
pub fn register_workspace_extra_methods(
    registry: &mut RemoteRegistry,
    workspaces: &Arc<WorkspaceRegistry>,
    preferences: &Arc<dyn WorkspacePreferences>,
    sessions: &Arc<dyn SessionCatalog>,
) {
    register_rename(registry, workspaces);
    register_insert_before(registry, workspaces);
    register_insert_session_before(registry, workspaces);
    register_archive_session(registry, preferences, sessions);
    register_unarchive_session(registry, preferences);
    register_pin_session(registry, preferences, sessions);
    register_unpin_session(registry, preferences);
}

/// Registers all seven methods with [`UnavailablePreferences`].
///
/// The four preference-backed methods stay reachable and refuse with a reason naming the seam, so a
/// client gets an explanation rather than `UNKNOWN_METHOD` and an operator learns what to fill. The
/// three registry-backed methods work fully.
pub fn register_workspace_extra_methods_without_preferences(
    registry: &mut RemoteRegistry,
    workspaces: &Arc<WorkspaceRegistry>,
    sessions: &Arc<dyn SessionCatalog>,
) {
    let preferences: Arc<dyn WorkspacePreferences> =
        Arc::new(UnavailablePreferences::default_reason());
    register_workspace_extra_methods(registry, workspaces, &preferences, sessions);
}

/// Registers `workspace.rename`.
fn register_rename(registry: &mut RemoteRegistry, workspaces: &Arc<WorkspaceRegistry>) {
    let workspaces = Arc::clone(workspaces);
    registry.declare(NAMESPACE, RENAME_METHOD, declaration());
    registry.register(NAMESPACE, RENAME_METHOD, move |call: RemoteCall| {
        let workspaces = Arc::clone(&workspaces);
        Box::pin(async move { rename(&call.args, &workspaces) })
    });
}

/// Registers `workspace.insertBefore`.
fn register_insert_before(registry: &mut RemoteRegistry, workspaces: &Arc<WorkspaceRegistry>) {
    let workspaces = Arc::clone(workspaces);
    registry.declare(NAMESPACE, INSERT_BEFORE_METHOD, declaration());
    registry.register(NAMESPACE, INSERT_BEFORE_METHOD, move |call: RemoteCall| {
        let workspaces = Arc::clone(&workspaces);
        Box::pin(async move { insert_before(&call.args, &workspaces) })
    });
}

/// Registers `workspace.insertSessionBefore`.
fn register_insert_session_before(
    registry: &mut RemoteRegistry,
    workspaces: &Arc<WorkspaceRegistry>,
) {
    let workspaces = Arc::clone(workspaces);
    registry.declare(NAMESPACE, INSERT_SESSION_BEFORE_METHOD, declaration());
    registry.register(
        NAMESPACE,
        INSERT_SESSION_BEFORE_METHOD,
        move |call: RemoteCall| {
            let workspaces = Arc::clone(&workspaces);
            Box::pin(async move { insert_session_before(&call.args, &workspaces).await })
        },
    );
}

/// Registers `workspace.archiveSession`.
fn register_archive_session(
    registry: &mut RemoteRegistry,
    preferences: &Arc<dyn WorkspacePreferences>,
    sessions: &Arc<dyn SessionCatalog>,
) {
    let preferences = Arc::clone(preferences);
    let sessions = Arc::clone(sessions);
    registry.declare(NAMESPACE, ARCHIVE_SESSION_METHOD, declaration());
    registry.register(NAMESPACE, ARCHIVE_SESSION_METHOD, move |call: RemoteCall| {
        let preferences = Arc::clone(&preferences);
        let sessions = Arc::clone(&sessions);
        Box::pin(async move { archive_session(&call.args, &preferences, &sessions).await })
    });
}

/// Registers `workspace.unarchiveSession`.
fn register_unarchive_session(
    registry: &mut RemoteRegistry,
    preferences: &Arc<dyn WorkspacePreferences>,
) {
    let preferences = Arc::clone(preferences);
    registry.declare(NAMESPACE, UNARCHIVE_SESSION_METHOD, declaration());
    registry.register(NAMESPACE, UNARCHIVE_SESSION_METHOD, move |call: RemoteCall| {
        let preferences = Arc::clone(&preferences);
        Box::pin(async move { unarchive_session(&call.args, &preferences) })
    });
}

/// Registers `workspace.pinSession`.
fn register_pin_session(
    registry: &mut RemoteRegistry,
    preferences: &Arc<dyn WorkspacePreferences>,
    sessions: &Arc<dyn SessionCatalog>,
) {
    let preferences = Arc::clone(preferences);
    let sessions = Arc::clone(sessions);
    registry.declare(NAMESPACE, PIN_SESSION_METHOD, declaration());
    registry.register(NAMESPACE, PIN_SESSION_METHOD, move |call: RemoteCall| {
        let preferences = Arc::clone(&preferences);
        let sessions = Arc::clone(&sessions);
        Box::pin(async move { pin_session(&call.args, &preferences, &sessions).await })
    });
}

/// Registers `workspace.unpinSession`.
fn register_unpin_session(
    registry: &mut RemoteRegistry,
    preferences: &Arc<dyn WorkspacePreferences>,
) {
    let preferences = Arc::clone(preferences);
    registry.declare(NAMESPACE, UNPIN_SESSION_METHOD, declaration());
    registry.register(NAMESPACE, UNPIN_SESSION_METHOD, move |call: RemoteCall| {
        let preferences = Arc::clone(&preferences);
        Box::pin(async move { unpin_session(&call.args, &preferences) })
    });
}

// ---- handlers ---------------------------------------------------------------------------------

/// Runs one `workspace.rename` call.
///
/// The title is trimmed before it is stored, and a blank one is refused rather than stored: a
/// sidebar row with no label is a row the user cannot identify, and trimming here is what makes
/// "  " and "" the same refusal. A title another workspace already holds is refused too, because two
/// rows with one name are indistinguishable — upstream answers `workspace/name-conflict` for it, and
/// this error carries the same "already in use" wording since [`RemoteError`] has no code slot.
fn rename(args: &Value, workspaces: &Arc<WorkspaceRegistry>) -> Result<Value, RemoteError> {
    let workspace_id = required_str(args, "workspaceId", RENAME_METHOD)?;
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(RENAME_METHOD, "`title` is required"))?
        .trim();

    if title.is_empty() {
        return Err(invalid(RENAME_METHOD, "a rename requires a non-blank title"));
    }

    let current = workspaces
        .get(workspace_id)
        .ok_or_else(|| unknown_workspace(RENAME_METHOD, workspace_id))?;

    if let Some(conflict) = workspaces
        .list()
        .into_iter()
        .find(|other| other.id != current.id && other.title == title)
    {
        return Err(invalid(
            RENAME_METHOD,
            format!(
                "workspace name `{title}` is already in use by `{}`",
                conflict.id
            ),
        ));
    }

    // An unchanged title is not a write: moving `updated_at` for a no-op would make "last changed"
    // mean "last rendered", and a client that retried a rename would reorder its own history.
    if current.title == title {
        return Ok(workspace_value(&current));
    }

    let renamed = workspaces
        .rename(workspace_id, title, now_millis())
        .ok_or_else(|| unknown_workspace(RENAME_METHOD, workspace_id))?;
    Ok(workspace_value(&renamed))
}

/// Runs one `workspace.insertBefore` call.
///
/// With an anchor the registry's own [`WorkspaceRegistry::insert_before`] does the move. Without one
/// upstream appends, and the registry has no append primitive, so the move is composed from the one
/// it does have: every workspace after the moved one is placed before it in turn, which shifts it to
/// the end without touching the others' relative order.
///
/// The composition is several lock acquisitions rather than one, so a concurrent `workspace.list`
/// can observe an intermediate order. It cannot observe an order that loses or duplicates a
/// workspace — each step is a whole-list move — and the final order is the one the caller asked for.
fn insert_before(args: &Value, workspaces: &Arc<WorkspaceRegistry>) -> Result<Value, RemoteError> {
    let workspace_id = required_str(args, "workspaceId", INSERT_BEFORE_METHOD)?;
    let anchor = optional_str(args, "beforeWorkspaceId");

    if workspaces.get(workspace_id).is_none() {
        return Err(unknown_workspace(INSERT_BEFORE_METHOD, workspace_id));
    }

    let now = now_millis();
    match anchor {
        Some(anchor) => {
            if workspaces.get(anchor).is_none() {
                return Err(unknown_workspace(INSERT_BEFORE_METHOD, anchor));
            }
            // An anchor equal to the moved id is a no-op, which the registry already answers by
            // returning the current order.
            workspaces.insert_before(workspace_id, anchor, now).ok_or_else(|| {
                unknown_workspace(INSERT_BEFORE_METHOD, workspace_id)
            })?;
        }
        None => append_last(workspaces, workspace_id, now)?,
    }

    Ok(order_value(workspaces))
}

/// Moves one workspace to the end of the registry order.
///
/// Composed rather than primitive; see [`insert_before`] for why and for the visibility caveat. Each
/// iteration is guaranteed to move the id one place right, so the loop cannot spin: the id reaches
/// the end after at most `len` steps.
fn append_last(
    workspaces: &WorkspaceRegistry,
    workspace_id: &str,
    now: u64,
) -> Result<(), RemoteError> {
    let ids = ordered_ids(workspaces);
    let Some(position) = ids.iter().position(|id| id == workspace_id) else {
        return Err(unknown_workspace(INSERT_BEFORE_METHOD, workspace_id));
    };
    for other in &ids[position + 1..] {
        workspaces
            .insert_before(other, workspace_id, now)
            .ok_or_else(|| unknown_workspace(INSERT_BEFORE_METHOD, other))?;
    }
    Ok(())
}

/// Runs one `workspace.insertSessionBefore` call.
///
/// The order it changes is `Workspace::session_ids`, which the registry owns. It has no reorder
/// method for that list, only a detach ([`WorkspaceRegistry::archive_session`], which removes one
/// id) and an append ([`WorkspaceRegistry::attach_session`]), so the move is composed: everything
/// from the first position that differs is detached in order and re-attached in the desired order.
/// The prefix that already matches is not touched, which keeps the number of writes proportional to
/// the part that actually moves.
///
/// Two consequences are stated rather than hidden:
///
/// 1. **The move is not atomic.** Each registry call takes the lock separately, so a concurrent
///    `workspace.list` can see the moving tail missing for a moment. The final state is the order
///    the caller asked for, and no id is lost or duplicated.
/// 2. **The registry's `archive_session` is used here as a detach, not as an archive.** It removes
///    the id from this workspace's account; it is unrelated to `workspace.archiveSession`, which
///    keeps the id in place and hides it through [`WorkspacePreferences`]. The two are named alike
///    and mean different things, which is exactly why this note exists.
///
/// A session or anchor that the workspace does not account for is refused before anything is
/// written, so a bad request cannot leave the list half-moved.
async fn insert_session_before(
    args: &Value,
    workspaces: &Arc<WorkspaceRegistry>,
) -> Result<Value, RemoteError> {
    let workspace_id = required_str(args, "workspaceId", INSERT_SESSION_BEFORE_METHOD)?;
    let session_id = required_str(args, "sessionId", INSERT_SESSION_BEFORE_METHOD)?;
    let anchor = optional_str(args, "beforeSessionId");

    let workspace = workspaces
        .get(workspace_id)
        .ok_or_else(|| unknown_workspace(INSERT_SESSION_BEFORE_METHOD, workspace_id))?;

    if !workspace.session_ids.iter().any(|id| id == session_id) {
        return Err(invalid(
            INSERT_SESSION_BEFORE_METHOD,
            format!("session `{session_id}` is not accounted by workspace `{workspace_id}`"),
        ));
    }
    if let Some(anchor) = anchor {
        if !workspace.session_ids.iter().any(|id| id == anchor) {
            return Err(invalid(
                INSERT_SESSION_BEFORE_METHOD,
                format!(
                    "session `{anchor}` is not accounted by workspace `{workspace_id}`, so it \
                     cannot be an anchor"
                ),
            ));
        }
    }

    // An anchor equal to the moved session is a no-op upstream, and treating it as "append" here
    // would silently do the opposite of what was asked.
    if anchor == Some(session_id) {
        return Ok(workspace_value(&workspace));
    }

    let desired = moved(&workspace.session_ids, session_id, anchor);
    let Some(start) = workspace
        .session_ids
        .iter()
        .zip(desired.iter())
        .position(|(current, wanted)| current != wanted)
    else {
        // The list already has the requested order; nothing is written, so `updated_at` stays put.
        return Ok(workspace_value(&workspace));
    };

    let now = now_millis();
    for id in &workspace.session_ids[start..] {
        if workspaces.archive_session(workspace_id, id, now).is_none() {
            return Err(partial_move(workspace_id, INSERT_SESSION_BEFORE_METHOD));
        }
    }
    for id in &desired[start..] {
        if workspaces.attach_session(workspace_id, id, now).is_none() {
            return Err(partial_move(workspace_id, INSERT_SESSION_BEFORE_METHOD));
        }
    }

    let updated = workspaces
        .get(workspace_id)
        .ok_or_else(|| unknown_workspace(INSERT_SESSION_BEFORE_METHOD, workspace_id))?;
    Ok(workspace_value(&updated))
}

/// Runs one `workspace.archiveSession` call.
///
/// The archive set is global to the registry, and archiving leaves the session's account — its slot
/// in `session_ids` — untouched, which is what lets an unarchive return it to its old position. That
/// is why this does not go through [`WorkspaceRegistry::archive_session`]: that method removes the
/// slot, which would make the position unrecoverable.
///
/// The existence check runs before the write for the reason upstream gives: a mark for an id no
/// session holds hides whatever later takes that id, and the caller learns nothing from the success.
/// A repeat for an id that is already archived is a success without a write and without a check,
/// because archiving is a state rather than an event.
async fn archive_session(
    args: &Value,
    preferences: &Arc<dyn WorkspacePreferences>,
    sessions: &Arc<dyn SessionCatalog>,
) -> Result<Value, RemoteError> {
    let session_id = required_str(args, "sessionId", ARCHIVE_SESSION_METHOD)?.to_owned();

    // See the module docs: this build cannot stop a session's work, so it must not answer a request
    // that asked it to.
    match args.get("stopActivity") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            return Err(RemoteError::Failed(format!(
                "`{NAMESPACE}.{ARCHIVE_SESSION_METHOD}` cannot stop a session's running work: this \
                 build has no workspace/session-activity provider and no workspace/session-stop \
                 channel, so archiving with `stopActivity` would report a stop that never happened; \
                 retry without `stopActivity`"
            )));
        }
        Some(other) => {
            return Err(invalid(
                ARCHIVE_SESSION_METHOD,
                format!("`stopActivity` must be a boolean, got `{other}`"),
            ));
        }
    }

    if preferences
        .archived_session_ids()
        .iter()
        .any(|archived| archived == &session_id)
    {
        return Ok(archive_value(preferences));
    }

    if sessions.read(&session_id).await.is_none() {
        return Err(invalid(
            ARCHIVE_SESSION_METHOD,
            format!("no session `{session_id}`, so there is nothing to archive"),
        ));
    }

    preferences
        .archive_session(&session_id)
        .map_err(|error| error.into_remote(ARCHIVE_SESSION_METHOD))?;
    Ok(archive_value(preferences))
}

/// Runs one `workspace.unarchiveSession` call.
///
/// Idempotent, and deliberately without an existence check: removing an id cannot introduce an
/// unknown one, so an entry whose session has since gone must still be removable. Upstream makes
/// the same choice for the same reason.
async fn unarchive_session(
    args: &Value,
    preferences: &Arc<dyn WorkspacePreferences>,
) -> Result<Value, RemoteError> {
    let session_id = required_str(args, "sessionId", UNARCHIVE_SESSION_METHOD)?;
    preferences
        .unarchive_session(session_id)
        .map_err(|error| error.into_remote(UNARCHIVE_SESSION_METHOD))?;
    Ok(archive_value(preferences))
}

/// Runs one `workspace.pinSession` call.
///
/// The set is ordered "most recently pinned first", so an id that is already pinned is answered
/// without a write: a retry is not a second pin and must not reorder the surface. Pinning an
/// archived session is refused, and the refusal is placed before the existence check so the caller
/// is told the thing it can act on.
async fn pin_session(
    args: &Value,
    preferences: &Arc<dyn WorkspacePreferences>,
    sessions: &Arc<dyn SessionCatalog>,
) -> Result<Value, RemoteError> {
    let session_id = required_str(args, "sessionId", PIN_SESSION_METHOD)?.to_owned();

    if preferences
        .pinned_session_ids()
        .iter()
        .any(|pinned| pinned == &session_id)
    {
        return Ok(pin_value(preferences));
    }
    if preferences
        .archived_session_ids()
        .iter()
        .any(|archived| archived == &session_id)
    {
        return Err(invalid(
            PIN_SESSION_METHOD,
            format!("session `{session_id}` is archived; unarchive it before pinning"),
        ));
    }
    if sessions.read(&session_id).await.is_none() {
        return Err(invalid(
            PIN_SESSION_METHOD,
            format!("no session `{session_id}`, so there is nothing to pin"),
        ));
    }

    preferences
        .pin_session(&session_id)
        .map_err(|error| error.into_remote(PIN_SESSION_METHOD))?;
    Ok(pin_value(preferences))
}

/// Runs one `workspace.unpinSession` call.
///
/// Idempotent and without an existence check, for the same reason [`unarchive_session`] is: dropping
/// an id cannot introduce an unknown one.
async fn unpin_session(
    args: &Value,
    preferences: &Arc<dyn WorkspacePreferences>,
) -> Result<Value, RemoteError> {
    let session_id = required_str(args, "sessionId", UNPIN_SESSION_METHOD)?;
    preferences
        .unpin_session(session_id)
        .map_err(|error| error.into_remote(UNPIN_SESSION_METHOD))?;
    Ok(pin_value(preferences))
}

// ---- projections and helpers ------------------------------------------------------------------

/// Projects one registration onto the wire.
///
/// Hand-built rather than serialized from [`girder_core::api_config::WorkspaceView`] so the keys
/// match what [`crate::remote`] already emits for `workspace.list`; see the module docs.
fn workspace_value(workspace: &Workspace) -> Value {
    let view = workspace.view(now_millis());
    json!({
        "workspace": {
            "workspace_id": view.workspace_id,
            "path": view.path,
            "title": view.title,
            "session_ids": view.session_ids,
            "created_at": view.created_at,
            "updated_at": view.updated_at,
        }
    })
}

/// The complete registry order, which is what a reorder answers with.
fn order_value(workspaces: &WorkspaceRegistry) -> Value {
    json!({ "workspace_ids": ordered_ids(workspaces) })
}

/// The complete archive set.
fn archive_value(preferences: &Arc<dyn WorkspacePreferences>) -> Value {
    json!({ "archived_session_ids": preferences.archived_session_ids() })
}

/// The complete pin set, most recently pinned first.
fn pin_value(preferences: &Arc<dyn WorkspacePreferences>) -> Value {
    json!({ "pinned_session_ids": preferences.pinned_session_ids() })
}

/// Every registered id, in manual order.
fn ordered_ids(workspaces: &WorkspaceRegistry) -> Vec<String> {
    workspaces
        .list()
        .into_iter()
        .map(|workspace| workspace.id)
        .collect()
}

/// `session_ids` with `session_id` moved before `anchor`, or to the end when there is no anchor.
///
/// Pure, so the desired order can be compared against the current one before anything is written.
fn moved(current: &[String], session_id: &str, anchor: Option<&str>) -> Vec<String> {
    let mut without: Vec<String> = current
        .iter()
        .filter(|id| id.as_str() != session_id)
        .cloned()
        .collect();
    // The caller validated the anchor against the current list, so a missing position here means
    // the anchor was the moved id itself; treating that as "append" is the documented upstream
    // no-op, already handled by the caller.
    let at = anchor
        .and_then(|anchor| without.iter().position(|id| id == anchor))
        .unwrap_or(without.len());
    without.insert(at, session_id.to_owned());
    without
}

/// The `workspaceId` / `sessionId` argument.
///
/// A blank value is refused rather than looked up: no id is empty, and letting one through would
/// make a client bug look like a missing workspace.
fn required_str<'a>(args: &'a Value, field: &str, method: &str) -> Result<&'a str, RemoteError> {
    args.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(method, format!("`{field}` is required")))
}

/// An optional string argument.
///
/// A `null` and an absent field mean the same thing here, which is what a client that clears an
/// optional anchor sends.
fn optional_str<'a>(args: &'a Value, field: &str) -> Option<&'a str> {
    args.get(field).and_then(Value::as_str)
}

/// A refusal for a workspace id no registration holds.
fn unknown_workspace(method: &str, workspace_id: &str) -> RemoteError {
    invalid(
        method,
        format!("unknown workspace `{workspace_id}`; it is not registered"),
    )
}

/// A refusal for a request the caller can fix.
fn invalid(method: &str, message: impl Into<String>) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: message.into(),
    }
}

/// A failure after a composed move had already written part of its result.
///
/// Only reachable if the registration vanished between two registry calls, which this crate's
/// writers do not do; it is still an error rather than a silent success, and it says the order may
/// be partial so a caller can re-read instead of trusting its copy.
fn partial_move(workspace_id: &str, method: &str) -> RemoteError {
    RemoteError::Failed(format!(
        "`{NAMESPACE}.{method}` stopped partway through reordering workspace `{workspace_id}`: an \
         id it had just detached or attached was no longer registered, so the manual order is \
         partial; re-read the workspace before retrying"
    ))
}
