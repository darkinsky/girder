//! The `session.duplicate` and `session.archive` Remote methods: copying a conversation, and
//! putting one out of the way without destroying it.
//!
//! Both names are declared as gaps by [`crate::remote::register_session_methods`] today, so a client
//! calling them receives `NOT_IMPLEMENTED`. This module gives two of them a behaviour.
//!
//! # What `duplicate` is, and why it is not `fork`
//!
//! `session.fork` (`SessionCatalog::fork`) cuts a log at a turn boundary: its whole purpose is to
//! produce a session that is *shorter* than the source so a replay can branch from an earlier point.
//! A duplicate keeps everything. The distinction matters for the implementation: a fork has to
//! reason about which events may be separated, while a copy is a plain replay of one log into
//! another, and a copy that dropped or reordered an event would be a silently different
//! conversation. It also has to name itself: a list holding two identically titled rows is a list
//! the user cannot navigate, so the copy is titled `"<source title> (copy)"`.
//!
//! # What `SessionCatalog` does and does not offer
//!
//! This is the important part of the module, because it decides how honest each method can be.
//!
//! | what the method needs | what the trait has | verdict |
//! |---|---|---|
//! | read the source's events | [`SessionCatalog::read`] | present |
//! | append events to a new session | [`SessionCatalog::open`] | present as a *defaulted* method that returns `None`; a catalog without it cannot host a copy |
//! | create the new id | [`SessionCatalog::create`] | present, but documented as *idempotent per id*: it reuses an existing session rather than refusing, so it guarantees nothing about uniqueness |
//! | give the copy a title | [`SessionCatalog::rename`] | present as a defaulted method that reports `NotImplemented` on catalogs that keep no titles |
//! | read the source's current title | — | **absent** |
//! | mark a session archived | — | **absent** |
//! | hide archived sessions from a listing | — | **absent** ([`SessionRecord`] has no such field, and `list` cannot filter) |
//!
//! Three consequences follow, and none of them is papered over:
//!
//! 1. **The copy's title is derived, not read.** [`SessionCatalog`] has no `title(id)`. The title a
//!    caller set through `session.rename` lives in a companion file that only `DirectoryCatalog`
//!    knows about, and this module must not reach into a concrete catalog's layout. So the base
//!    title comes from what *is* in the log: the last [`SessionEvent::SessionTitle`] event, else the
//!    first user message, else the source id. A session titled only through `session.rename` copies
//!    as the first line of its conversation — accurate about the conversation, not about the label.
//!    Closing that gap needs one method on the catalog, `title(&self, id) -> Option<String>`.
//! 2. **Uniqueness is manufactured here.** Because `create` reuses an existing id, a copy must
//!    choose an id that no session holds *before* creating it, and must verify afterwards that the
//!    row it got back is empty (an empty row is what a fresh session looks like; a reused one
//!    already has events). The candidate id carries the process id, a nanosecond timestamp, and a
//!    process-wide sequence counter, so two copies — in this process or another — cannot agree on
//!    one. The pre-check still cannot be atomic, which is exactly why the post-check exists.
//! 3. **`session.archive` has nowhere to write.** The trait exposes no metadata surface at all: no
//!    archive flag, no sidecar of its own, no directory to put one in. Inventing a location would
//!    mean guessing a catalog's on-disk layout, and inventing an in-memory flag would report
//!    "archived" for a state a restart loses. So the storage is an explicit seam
//!    ([`ArchiveStore`]), and the two-argument registration installs
//!    [`UnavailableArchiveStore`], which refuses with a message naming what a host must provide.
//!    A host that has a metadata directory passes [`MemoryArchiveStore`] or its own implementation
//!    through [`register_session_lifecycle_methods_with_archive`], and — to make the archive
//!    actually hide anything — filters its list with [`retain_visible`], since the catalog's own
//!    `list` cannot know about this module's marks.
//!
//! # Why `archive` verifies the session exists first
//!
//! A mark for an id no session holds is a promise about nothing: it would keep hiding a session if
//! that id were later reused, and the caller would learn nothing from the success. So an archive for
//! an unknown id is an error, and the mark is written only after the session has been found.
//!
//! # Why `archive` is idempotent
//!
//! Archiving is a state, not an event: "this session is archived" is true however many times it is
//! asked for. A second call therefore succeeds, reports `archived: true` again, and the store
//! returns `Ok(false)` to say nothing changed. A client that retried after a timeout — the ordinary
//! reason a request arrives twice — must not be shown a failure for a state it already has.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use serde_json::{Value, json};

// Paths go through the crate's own re-exports rather than `girder_core::remote`: this file is also
// compiled as a module of the integration test by path, where the private module is unreachable and
// only the re-exports are.
use girder_core::{
    Access, Declaration, RemoteCall, RemoteError, RemoteRegistry, SessionCatalog, SessionEvent,
    SessionRecord,
};

/// The namespace both methods live under.
pub const NAMESPACE: &str = "session";

/// The copy method, spelled as upstream spells it.
pub const DUPLICATE_METHOD: &str = "duplicate";

/// The archive method, spelled as upstream spells it.
pub const ARCHIVE_METHOD: &str = "archive";

/// The resource kind these methods touch, as reported to an access policy.
///
/// Both change a session, so both declare a write. Without a declaration the policy would read them
/// as a listing and a read-only mirror could copy or hide a user's conversations.
const RESOURCE_KIND: &str = "session";

/// What both methods declare to an access policy.
///
/// A duplicate creates a session and an archive changes one; neither is destructive, because
/// neither removes history, so a deployment that allows a peer to write but not to delete can allow
/// both. The `Declaration` next to each registration uses the same value, so a host comparing
/// against this cannot disagree with what the registry was told.
pub const DECLARED_ACCESS: Access = Access::Write;

/// The declaration both registrations use.
///
/// Built from [`DECLARED_ACCESS`] rather than spelling `Declaration::write` twice, so the constant
/// and what the registry is told cannot say different things.
fn declaration() -> Declaration {
    match DECLARED_ACCESS {
        Access::Write => Declaration::write(RESOURCE_KIND),
        Access::Destructive => Declaration::destructive(RESOURCE_KIND),
        Access::Read => Declaration::read(RESOURCE_KIND),
    }
}

/// Appended to the source's title to name the copy.
///
/// The suffix rather than a numeric prefix because a list sorts and reads by the beginning of a
/// title: copies stay next to their source instead of collecting in a separate run of the list.
pub const COPY_SUFFIX: &str = " (copy)";

/// The longest title this module writes, in characters.
///
/// A title is a line in a list, and the source of the base title — a log's title event, or a user's
/// first message — has no length bound of its own.
pub const MAX_COPY_TITLE_CHARS: usize = 120;

/// How much of a first user message becomes a derived title.
///
/// Shorter than [`MAX_COPY_TITLE_CHARS`] because the suffix still has to fit inside it.
const DERIVED_TITLE_CHARS: usize = 80;

/// How many candidate ids to try before giving up.
///
/// A collision needs two copies of one session to agree on process id, nanosecond, and sequence
/// number, so the first attempt practically always succeeds; the bound exists so a catalog that
/// answers `read` for every id cannot loop forever.
const ID_ATTEMPTS: u32 = 8;

/// Distinguishes two ids generated in the same nanosecond.
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// The arguments of both methods.
///
/// Typed and `serde`-parsed, like every other method here, so a wrong type names the field instead
/// of silently defaulting. `sessionId` is `Option` rather than required so a missing field can be
/// reported with the same words as a blank one.
#[derive(Debug, Deserialize)]
struct SessionIdRequest {
    /// The session to copy or archive.
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
}

/// What an [`ArchiveStore`] can fail with.
///
/// Two variants rather than one string because they call for different answers: no storage is a
/// deployment mistake an operator fixes, and a failed write is an I/O condition a caller retries.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// The deployment has nowhere to record an archive mark.
    #[error("no archive storage is configured: {reason}")]
    Unavailable {
        /// What is missing, in words a host operator can act on.
        reason: String,
    },

    /// The store exists but could not record the mark.
    #[error("{0}")]
    Failed(String),
}

impl ArchiveError {
    /// The store the default registration installs cannot fail any other way, so this is the
    /// variant a caller is most likely to meet.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    /// A failure while writing the mark.
    ///
    /// Constructed by a store that writes to disk; kept public so such a store does not have to
    /// stringify its own I/O error into the variant above.
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    /// Renders the failure as a [`RemoteError`] naming the method that hit it.
    ///
    /// Both variants become [`RemoteError::Failed`]: the method itself exists, so
    /// `NOT_IMPLEMENTED` would tell a client author to stop using it, and the message already says
    /// whether the operator or the next attempt is the answer.
    fn into_remote(self, method: &str) -> RemoteError {
        RemoteError::Failed(format!("`{NAMESPACE}.{method}` failed: {self}"))
    }
}

/// Where archive marks live.
///
/// A trait rather than a field on the catalog because [`SessionCatalog`] has no metadata surface and
/// this module may not add one: the catalog is another module's type, and the alternative to a seam
/// would be guessing where a particular implementation keeps its files.
///
/// Every implementation must answer [`ArchiveStore::mark_archived`] idempotently — a second mark for
/// an id that is already marked is `Ok(false)`, not an error — because that is what makes the method
/// safe to retry.
pub trait ArchiveStore: Send + Sync {
    /// Whether this session is archived.
    ///
    /// A read, and the one a listing filters with. An id that was never marked is not archived,
    /// including one no session holds: this answers about the mark, not about the session.
    fn is_archived(&self, session_id: &str) -> bool;

    /// Marks a session archived.
    ///
    /// `Ok(true)` when the mark was written, `Ok(false)` when it was already there. Any `Err` means
    /// the state did not change.
    fn mark_archived(&self, session_id: &str) -> Result<bool, ArchiveError>;

    /// Every archived id.
    ///
    /// Exposed so a host can filter a listing it built elsewhere without asking once per row, and
    /// so a diagnostic can show what is hidden.
    fn archived_ids(&self) -> Vec<String>;
}

/// The archive store a deployment gets when it supplies none.
///
/// It refuses every mark, and says why. The alternative — silently keeping the mark in memory —
/// would report a state that a restart loses, and a user whose session came back in the list a day
/// later would have been told something untrue. The reason is a value rather than a constant so a
/// host can name its own missing piece.
#[derive(Debug)]
pub struct UnavailableArchiveStore {
    reason: String,
}

impl UnavailableArchiveStore {
    /// Creates a store that refuses with `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The reason the default registration uses.
    ///
    /// It names the two things a host can supply, because a message that only said "not supported"
    /// would leave an operator guessing which half to build.
    #[must_use]
    pub fn default_reason() -> Self {
        Self::new(
            "SessionCatalog has no archive metadata surface (no field on SessionRecord, no \
             sidecar path, and no filtering in list), and this module has no directory of its own; \
             a host must supply an ArchiveStore via \
             register_session_lifecycle_methods_with_archive",
        )
    }
}

impl ArchiveStore for UnavailableArchiveStore {
    fn is_archived(&self, _session_id: &str) -> bool {
        // Nothing is archived, because nothing can be marked. Reporting a mark this store never
        // accepted would make a listing hide a session for no recorded reason.
        false
    }

    fn mark_archived(&self, _session_id: &str) -> Result<bool, ArchiveError> {
        Err(ArchiveError::unavailable(self.reason.clone()))
    }

    fn archived_ids(&self) -> Vec<String> {
        Vec::new()
    }
}

/// An archive store held in memory.
///
/// Durable for the life of the process and not a moment longer, which is stated rather than implied:
/// a host that wants an archive to survive a restart puts the marks in its own metadata directory
/// and passes a store that writes there. This one exists for tests and for a host that already keeps
/// session metadata somewhere and would rather lend this module the table than write a type.
#[derive(Debug, Default)]
pub struct MemoryArchiveStore {
    ids: Mutex<BTreeSet<String>>,
}

impl MemoryArchiveStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // Poisoning is recovered rather than propagated: a panic while holding this set leaves a table
    // of ids, not a half-written one, and refusing every later call would turn one panic into a
    // permanently unusable archive surface.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        self.ids.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl ArchiveStore for MemoryArchiveStore {
    fn is_archived(&self, session_id: &str) -> bool {
        self.lock().contains(session_id)
    }

    fn mark_archived(&self, session_id: &str) -> Result<bool, ArchiveError> {
        // `BTreeSet::insert` is the idempotency: it returns false when the id was already present,
        // so a retried archive is a no-op that still reports success.
        Ok(self.lock().insert(session_id.to_owned()))
    }

    fn archived_ids(&self) -> Vec<String> {
        self.lock().iter().cloned().collect()
    }
}

/// Removes archived sessions from a listing.
///
/// The catalog's own `list` cannot do this: it is another module's method, its row type
/// ([`SessionRecord`]) has no archived field, and it has no access to this module's marks. So the
/// filtering is a function a host applies to the rows it got, which keeps the rule in one place
/// instead of once per view.
///
/// Called with the rows and the store rather than reading the store here, because a caller that
/// already knows what is archived — a page that has the store in hand — should not be forced
/// through an indirection that would only re-read the same set.
pub fn retain_visible(records: &mut Vec<SessionRecord>, archive: &dyn ArchiveStore) {
    records.retain(|record| !archive.is_archived(&record.id));
}

/// Registers both methods with no archive storage.
///
/// `session.duplicate` works fully: it needs only [`SessionCatalog`]. `session.archive` is
/// registered and refuses with a message naming what is missing, so a client gets a reason rather
/// than `UNKNOWN_METHOD`, and an operator learns which seam to fill. See the module documentation
/// for why the storage is not invented here.
pub fn register_session_lifecycle_methods(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
) {
    let archive: Arc<dyn ArchiveStore> = Arc::new(UnavailableArchiveStore::default_reason());
    register_session_lifecycle_methods_with_archive(registry, catalog, &archive);
}

/// Registers both methods against a host-supplied archive store.
///
/// The arguments are references and are cloned into the closures, following the other `register_*`
/// functions: the host keeps ownership of both seams, and the registry stays the only thing this
/// module mutates.
pub fn register_session_lifecycle_methods_with_archive(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    archive: &Arc<dyn ArchiveStore>,
) {
    let duplicate_catalog = Arc::clone(catalog);
    let archive_catalog = Arc::clone(catalog);
    let archive_store = Arc::clone(archive);

    // Declared before the handlers, so the policy judges a call the handler would otherwise accept.
    registry.declare(NAMESPACE, DUPLICATE_METHOD, declaration());
    registry.declare(NAMESPACE, ARCHIVE_METHOD, declaration());

    registry.register(NAMESPACE, DUPLICATE_METHOD, move |call: RemoteCall| {
        let catalog = Arc::clone(&duplicate_catalog);
        Box::pin(async move { duplicate(&call.args, &catalog).await })
    });

    registry.register(NAMESPACE, ARCHIVE_METHOD, move |call: RemoteCall| {
        let catalog = Arc::clone(&archive_catalog);
        let archive = Arc::clone(&archive_store);
        Box::pin(async move { archive_session(&call.args, &catalog, &archive).await })
    });
}

/// Runs one `session.duplicate` call.
///
/// The order is deliberate: every capability the copy needs is verified against the *source* before
/// anything is created, so a catalog that cannot host a copy is refused without leaving an empty
/// session behind.
async fn duplicate(args: &Value, catalog: &Arc<dyn SessionCatalog>) -> Result<Value, RemoteError> {
    let request: SessionIdRequest = parse_request(args, DUPLICATE_METHOD)?;
    let source_id = require_session_id(&request, DUPLICATE_METHOD)?;

    // A snapshot rather than a live handle: the copy must be of the conversation as it is now, and
    // appending would otherwise interleave with a turn still running on the source.
    let Some(source) = catalog.read(&source_id).await else {
        return Err(no_session(DUPLICATE_METHOD, &source_id));
    };

    // `open` is the trait's only write path, and its default returns `None`. Probing with the
    // source — which exists, or `read` would have answered `None` — turns "this catalog cannot write
    // events" into a refusal before a new id is minted.
    if catalog.open(&source_id).is_none() {
        return Err(RemoteError::Failed(format!(
            "cannot duplicate `{source_id}`: this catalog does not open sessions for writing \
             (SessionCatalog::open returned None), so the copy would be empty"
        )));
    }

    let new_id = create_empty(catalog, &source_id).await?;

    let Some(target) = catalog.open(&new_id) else {
        // Unreachable through the probe above for a catalog whose answers are consistent; still an
        // error rather than a blank success, because an empty copy reported as a copy is worse than
        // a failure the caller can see.
        return Err(RemoteError::Failed(format!(
            "session `{new_id}` was created but cannot be opened for writing, so it was left empty"
        )));
    };

    for event in &source.events {
        target.append(event.clone()).await.map_err(|error| {
            RemoteError::Failed(format!(
                "cannot copy event into `{new_id}`: {error}; the copy is incomplete"
            ))
        })?;
    }

    // The stored title first, because it is what the person called the session. Falling straight to the log produced a
    // copy titled with its first message when the source had been renamed -- accurate about the conversation and wrong
    // about the label, which is the one thing a copy's title is for.
    let stored = catalog.title(&source_id).await;
    let title = titled_copy(stored.as_deref(), &source.events, &source_id);
    let stored_title = match catalog.rename(&new_id, &title).await {
        Ok(stored) => Some(stored),
        // A catalog that keeps no titles still gets a correct copy; saying so in the response is
        // better than failing after history has already been written, which would leave both an
        // untitled copy and an error.
        Err(RemoteError::NotImplemented { .. }) => None,
        Err(error) => {
            return Err(RemoteError::Failed(format!(
                "session `{new_id}` was copied but its title could not be stored: {error}"
            )));
        }
    };

    Ok(json!({
        "sessionId": new_id,
        // Null when the catalog has no place to put a title. Present rather than omitted so a client
        // can tell "no title support" from a field it forgot to read.
        "title": stored_title,
    }))
}

/// Runs one `session.archive` call.
async fn archive_session(
    args: &Value,
    catalog: &Arc<dyn SessionCatalog>,
    archive: &Arc<dyn ArchiveStore>,
) -> Result<Value, RemoteError> {
    let request: SessionIdRequest = parse_request(args, ARCHIVE_METHOD)?;
    let session_id = require_session_id(&request, ARCHIVE_METHOD)?;

    // The mark is only worth writing for a session that exists: a mark for an unknown id would hide
    // whatever later took that id, and a caller who mistyped an id would see a success.
    if catalog.read(&session_id).await.is_none() {
        return Err(no_session(ARCHIVE_METHOD, &session_id));
    }

    // `changed` is dropped on purpose: archiving is a state, so a repeat is a success too. The store
    // still reports it for a host that logs transitions.
    archive
        .mark_archived(&session_id)
        .map_err(|error| error.into_remote(ARCHIVE_METHOD))?;

    Ok(json!({
        // Always true, because a refusal above is an error rather than a false: a client that got a
        // response got an archived session.
        "archived": true,
        "sessionId": session_id,
    }))
}

/// Parses a method's arguments, naming the method when they do not fit.
fn parse_request(args: &Value, method: &str) -> Result<SessionIdRequest, RemoteError> {
    serde_json::from_value(args.clone()).map_err(|error| RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: error.to_string(),
    })
}

/// Extracts `sessionId`, refusing a missing or blank one.
///
/// Blank is refused rather than looked up: no session id is empty, and letting it through would make
/// a client bug look like a missing session.
fn require_session_id(request: &SessionIdRequest, method: &str) -> Result<String, RemoteError> {
    match request.session_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => Ok(id.to_owned()),
        _ => Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: "`sessionId` is required".to_owned(),
        }),
    }
}

/// The error for an id no session holds.
///
/// [`RemoteError::InvalidArguments`] rather than [`RemoteError::Failed`], following `session.fork`:
/// the method and the deployment are fine, and the id the caller sent is what does not match
/// anything. The enum has no `NotFound`, which is why the message has to carry the id.
fn no_session(method: &str, session_id: &str) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: format!("no session `{session_id}`"),
    }
}

/// Creates an empty session under an id nobody holds, and returns the id.
///
/// `SessionCatalog::create` is idempotent per id — it reuses a session that already exists — so it
/// cannot be asked for "a new one". The uniqueness is therefore built here, in two parts:
///
/// - an id that is practically impossible to repeat: process id, nanoseconds, and a process-wide
///   counter, so two copies collide only if they share a process, a nanosecond, and a counter value;
/// - a check before *and* after the create. The before-check skips an id that is already taken; the
///   after-check refuses a row that came back with events, which is what a reused session looks
///   like. The pre-check cannot be atomic with the create, and that race is precisely why the
///   post-check exists.
async fn create_empty(
    catalog: &Arc<dyn SessionCatalog>,
    source_id: &str,
) -> Result<String, RemoteError> {
    for _ in 0..ID_ATTEMPTS {
        let candidate = format!(
            "{source_id}-copy-{}-{}-{}",
            std::process::id(),
            now_nanos(),
            NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        if catalog.read(&candidate).await.is_some() {
            continue;
        }
        let record = catalog.create(&candidate).await?;
        if record.events.is_empty() {
            return Ok(candidate);
        }
        // The id was taken between the check and the create: `create` reused that session rather
        // than minting a new one, so the events are somebody else's.
    }
    Err(RemoteError::Failed(format!(
        "could not find an unused id for a copy of `{source_id}` after {ID_ATTEMPTS} attempts"
    )))
}

/// Current time in nanoseconds since the epoch, or `0` when the clock is before it.
///
/// Nanoseconds rather than the milliseconds `fork` uses: a fork's id only has to be unique against
/// other forks of one session made by hand, while copies are cheap enough for a client to issue in a
/// burst.
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

/// The title a copy carries: the source's title with [`COPY_SUFFIX`] appended.
///
/// The base comes from the log because the catalog cannot be asked for a title (see the module
/// documentation): the last title event wins, since a rename appends a new one, then the first user
/// message, which is what a reader would call the conversation. The source id is the last resort so
/// the result is never blank.
///
/// A copy of a copy ends up with two suffixes rather than a counter: the intent is "this is not the
/// original", and a number would imply an ordering the log does not record.
fn titled_copy(stored: Option<&str>, events: &[SessionEvent], source_id: &str) -> String {
    // The stored title wins: it is the label the person gave the session. The log is the fallback for a catalog that
    // keeps no titles, and the source id is the last resort so the result is never blank.
    let base = stored
        .map(str::to_owned)
        .or_else(|| derived_title(events))
        .unwrap_or_else(|| source_id.to_owned());

    // Cut on a character boundary, and leave room for the suffix: the base is user text with no
    // length bound of its own, and a title longer than the list that renders it is not a title.
    let room = MAX_COPY_TITLE_CHARS.saturating_sub(COPY_SUFFIX.chars().count());
    let mut title: String = base.chars().take(room).collect();
    title.push_str(COPY_SUFFIX);
    title
}

/// The title the log itself records, when it records one.
///
/// A session that was never renamed has no stored title and no `SessionTitle` event, so its first user message is the
/// closest thing to a label the conversation contains.
fn derived_title(events: &[SessionEvent]) -> Option<String> {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            SessionEvent::SessionTitle { title, .. } if !title.trim().is_empty() => {
                Some(title.trim().to_owned())
            }
            _ => None,
        })
        .or_else(|| {
            events.iter().find_map(|event| match event {
                SessionEvent::UserMessage { message } => {
                    let text =
                        girder_core::session_excerpt(&message.text(), DERIVED_TITLE_CHARS, true);
                    (!text.is_empty()).then_some(text)
                }
                _ => None,
            })
        })
}
