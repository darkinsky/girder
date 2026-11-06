//! Remote method dispatch: the `namespace.method` surface a real client calls.
//!
//! Upstream generates these from TypeScript decorators — `@Remote('list')` on a service method —
//! and Typert projects each one into a client-side `remote.<namespace>.<method>()`. This module is
//! the Rust equivalent without the code generation: a method is registered by name, and dispatch is
//! a lookup.
//!
//! # What this closes and what it does not
//!
//! `docs/phase6-scope.md` said the RPC layer was protocol connectivity, not a working GUI, because
//! a real client calls methods that did not exist. This module makes them exist for the subset
//! below. It does **not** make the upstream GUI work: the GUI calls all twenty `session.*` methods
//! plus eleven `workspace.*`, and this crate implements the ones a browser needs to list sessions
//! and send a prompt. The rest fail with a clear "not implemented" rather than a 404, so a
//! half-working client says which call is missing instead of looking broken.
//!
//! # Why arguments stay JSON
//!
//! Each method parses its own argument object into a typed request. Validating centrally would mean
//! a schema language, which is what Typert exists to avoid reimplementing; per-method `serde`
//! parsing puts the error message next to the field it is about.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::scope::BoxFut;

use crate::session::SessionEvent;

/// A failure a Remote method may report.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// No method is registered under that name.
    #[error("unknown method `{namespace}.{method}`")]
    UnknownMethod {
        /// Requested namespace.
        namespace: String,
        /// Requested method.
        method: String,
    },

    /// The method exists but this build does not implement it.
    ///
    /// Distinct from [`RemoteError::UnknownMethod`] because the two mean different things to a
    /// client author: one is a typo, the other is a gap.
    #[error("`{namespace}.{method}` is not implemented in this build")]
    NotImplemented {
        /// Requested namespace.
        namespace: String,
        /// Requested method.
        method: String,
    },

    /// The arguments did not match the method's request type.
    #[error("invalid arguments for `{namespace}.{method}`: {message}")]
    InvalidArguments {
        /// Requested namespace.
        namespace: String,
        /// Requested method.
        method: String,
        /// What was wrong.
        message: String,
    },

    /// The method failed while running.
    #[error("{0}")]
    Failed(String),
}

impl RemoteError {
    /// The stable code a client may branch on.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownMethod { .. } => "UNKNOWN_METHOD",
            Self::NotImplemented { .. } => "NOT_IMPLEMENTED",
            Self::InvalidArguments { .. } => "INVALID_ARGUMENTS",
            Self::Failed(_) => "FAILED",
        }
    }
}

/// One session list row.
///
/// Mirrors upstream's `SessionSummary` for the fields a list view renders. The optional ones are
/// omitted when unknown rather than sent as `null`, because a client that distinguishes "no value"
/// from "null value" would otherwise mis-render.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session identity.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// When the session last advanced, in milliseconds since the epoch.
    #[serde(rename = "updatedAt")]
    pub updated_at: u64,
    /// Whether an agent is currently attached.
    #[serde(rename = "agentAvailable")]
    pub agent_available: bool,
    /// Whether the agent is running a turn.
    pub running: bool,
    /// Whether the session has no messages yet.
    pub blank: bool,
    /// Working directory, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl SessionSummary {
    /// Builds a summary from a session's event log.
    ///
    /// `updated_at` comes from the caller rather than the log: the log records ordering, not wall
    /// time, and inventing a timestamp here would make two hosts disagree about the same session.
    #[must_use]
    pub fn from_events(
        session_id: impl Into<String>,
        events: &[SessionEvent],
        updated_at: u64,
    ) -> Self {
        let has_messages = events
            .iter()
            .any(|event| matches!(event, SessionEvent::UserMessage { .. }));
        Self {
            session_id: session_id.into(),
            updated_at,
            // Nothing in this build keeps a live agent registry; a stored session is not attached.
            agent_available: false,
            running: false,
            blank: !has_messages,
            cwd: None,
        }
    }

    /// Builds a summary from a catalog row.
    ///
    /// Uses the row's `blank` flag rather than its `events`, which `list` leaves empty.
    #[must_use]
    pub fn from_record(record: &SessionRecord) -> Self {
        Self {
            session_id: record.id.clone(),
            updated_at: record.updated_at,
            agent_available: false,
            running: false,
            blank: record.blank,
            cwd: None,
        }
    }
}

/// The response to `session.list`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionListValue {
    /// Rows, ordered by `updated_at` descending.
    pub items: Vec<SessionSummary>,
}

/// The request to `session.prompt`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptRequest {
    /// Session to prompt.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// The user's text.
    #[serde(default)]
    pub text: String,
}

/// What `session.prompt` returns.
///
/// The turn runs asynchronously and its output reaches the client through the event stream — that is
/// what `session/event` is for upstream. Returning the accepted prompt here mirrors the API: the
/// call's job is to enqueue, not to wait for the model.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionPromptValue {
    /// Whether the prompt was accepted.
    pub accepted: bool,
    /// Turn number it will run as.
    pub turn: u64,
}

/// Everything a Remote method receives.
pub struct RemoteCall {
    /// Named arguments as sent by the client.
    pub args: Value,
    /// The peer the call speaks for. Carried, never verified — see `docs/phase6-scope.md`.
    pub peer: String,
}

/// A method implementation.
///
/// `Arc<dyn Fn>` rather than an `async fn` in a trait: methods are stored by name in a map, so the
/// callable must be object-safe. That is migration-plan §1.3 constraint one again.
pub type RemoteMethod = Arc<dyn Fn(RemoteCall) -> BoxFut<Result<Value, RemoteError>> + Send + Sync>;

/// The method registry.
#[derive(Default)]
pub struct RemoteRegistry {
    methods: BTreeMap<(String, String), RemoteMethod>,
    /// Methods the upstream API defines that this build does not implement.
    ///
    /// Recorded explicitly so dispatch can distinguish "not implemented" from "typo". A hardcoded
    /// list is the honest option: guessing from a missing entry cannot tell the two apart.
    known_unimplemented: Vec<(String, String)>,
}

impl RemoteRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a method.
    pub fn register<F>(&mut self, namespace: &str, method: &str, handler: F)
    where
        F: Fn(RemoteCall) -> BoxFut<Result<Value, RemoteError>> + Send + Sync + 'static,
    {
        self.methods
            .insert((namespace.to_owned(), method.to_owned()), Arc::new(handler));
    }

    /// Declares a method the upstream API has but this build does not implement.
    pub fn declare_unimplemented(&mut self, namespace: &str, methods: &[&str]) {
        for method in methods {
            self.known_unimplemented
                .push((namespace.to_owned(), (*method).to_owned()));
        }
    }

    /// Registered method names, for diagnosis and schema listing.
    #[must_use]
    pub fn names(&self) -> Vec<(String, String)> {
        self.methods.keys().cloned().collect()
    }

    /// Number of registered methods.
    #[must_use]
    pub fn len(&self) -> usize {
        self.methods.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.methods.is_empty()
    }

    /// Dispatches one call.
    ///
    /// # Errors
    ///
    /// Returns [`RemoteError::UnknownMethod`] for a name nobody defined,
    /// [`RemoteError::NotImplemented`] for a defined-but-absent one, and whatever the method reports.
    pub async fn dispatch(
        &self,
        namespace: &str,
        method: &str,
        call: RemoteCall,
    ) -> Result<Value, RemoteError> {
        let key = (namespace.to_owned(), method.to_owned());
        if let Some(handler) = self.methods.get(&key) {
            return handler(call).await;
        }
        if self.known_unimplemented.contains(&key) {
            return Err(RemoteError::NotImplemented {
                namespace: namespace.to_owned(),
                method: method.to_owned(),
            });
        }
        Err(RemoteError::UnknownMethod {
            namespace: namespace.to_owned(),
            method: method.to_owned(),
        })
    }
}

impl std::fmt::Debug for RemoteRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteRegistry")
            .field("methods", &self.names())
            .finish_non_exhaustive()
    }
}

/// Parses a method's arguments, naming the method when it fails.
fn parse_args<T: for<'de> Deserialize<'de>>(
    namespace: &str,
    method: &str,
    args: Value,
) -> Result<T, RemoteError> {
    serde_json::from_value(args).map_err(|error| RemoteError::InvalidArguments {
        namespace: namespace.to_owned(),
        method: method.to_owned(),
        message: error.to_string(),
    })
}

/// A session the `session.*` methods operate on.
#[derive(Clone, Debug)]
pub struct SessionRecord {
    /// Identity.
    pub id: String,
    /// When it last advanced, milliseconds since the epoch.
    pub updated_at: u64,
    /// Whether the conversation has started.
    ///
    /// Carried rather than derived from `events` because [`SessionCatalog::list`] deliberately leaves
    /// `events` empty: decoding every event of every session to compute this one bit is
    /// O(sessions x events) of work for a list view.
    pub blank: bool,
    /// Its durable events. Empty for a row returned by `list`.
    pub events: Vec<SessionEvent>,
}

/// What the session methods need from the host.
pub trait SessionCatalog: Send + Sync {
    /// Every visible session.
    fn list(&self) -> BoxFut<Vec<SessionRecord>>;

    /// Reads one session's events, or `None` when no session has that id.
    ///
    /// Separate from [`SessionCatalog::list`] because a history read must not scan every log: a
    /// client opens one session at a time, and a directory of thousands makes the difference
    /// between one file and all of them.
    fn read(&self, id: &str) -> BoxFut<Option<SessionRecord>>;

    /// Creates a session, returning its id.
    ///
    /// Idempotent per id: creating one that exists reuses it rather than failing, which is what
    /// upstream's `create` does for a blank session.
    fn create(&self, id: &str) -> BoxFut<Result<SessionRecord, RemoteError>>;

    /// Opens the writable log for `id`, or `None` when no session has that id.
    ///
    /// The read methods return a *snapshot*; a prompt has to append to the live log, so it needs the
    /// handle rather than a copy of the events. Returning `None` rather than creating on demand is
    /// deliberate: prompting a session that does not exist should fail, not silently mint one.
    fn open(&self, _id: &str) -> Option<Arc<dyn crate::session::Session>> {
        None
    }
}

/// One durable history record, in the shape a client's page reader expects.
///
/// Upstream wraps each event in `{ type: 'event', event }` so a page can later carry non-event
/// records without a breaking change to the array's element type.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionHistoryRecord {
    /// Always `event` today.
    #[serde(rename = "type")]
    pub kind: String,
    /// The durable event.
    pub event: SessionEvent,
}

impl SessionHistoryRecord {
    /// Wraps one event.
    #[must_use]
    pub fn event(event: SessionEvent) -> Self {
        Self {
            kind: "event".to_owned(),
            event,
        }
    }
}

/// One page of session history.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionEventsPage {
    /// Records in chronological order.
    pub records: Vec<SessionHistoryRecord>,
    /// Whether older records remain before this page.
    #[serde(rename = "hasMore")]
    pub has_more: bool,
    /// Inclusive sequence number of the last record, or 0 for an empty page.
    ///
    /// A client pins this to read a fixed cut later; without it a follow read would drift as new
    /// events arrive.
    pub cursor: u64,
}

/// The request to `session.page` and `session.events`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionPageRequest {
    /// Which session to read.
    pub address: SessionAddress,
    /// Stop at this sequence, inclusive. Omitted means the current end.
    #[serde(
        rename = "throughSeq",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub through_seq: Option<u64>,
    /// Read records strictly before this sequence.
    #[serde(rename = "beforeSeq", default, skip_serializing_if = "Option::is_none")]
    pub before_seq: Option<u64>,
    /// Maximum records to return.
    #[serde(
        rename = "maxMessages",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub max_messages: Option<usize>,
}

/// A durable session address.
///
/// Only the ordinary case is modelled. Upstream also addresses a subagent child by parent and child
/// id; this build has no such registry, so the variant is rejected as invalid arguments rather than
/// silently reading the wrong log.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionAddress {
    /// `session` for an ordinary session.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// The session id.
    #[serde(rename = "sessionId", default)]
    pub session_id: String,
}

fn default_kind() -> String {
    "session".to_owned()
}

/// The request to `session.create`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionCreateRequest {
    /// Working directory the session owns, when the client names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Identity to create, or a blank session to reuse.
    #[serde(rename = "sessionId", default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Preset the session runs.
    #[serde(
        rename = "agentPreset",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_preset: Option<String>,
}

/// The request to `session.cancel`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionCancelRequest {
    /// Session whose running turn is cancelled.
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

/// What `session.cancel` returns.
///
/// `accepted` acknowledges the request, not its completion: cancellation lands at the next step
/// boundary, so a client that needs to know the turn actually stopped watches for `turn-ended`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionCancelValue {
    /// Always true; a refusal is an error, not a false.
    pub accepted: bool,
}

/// What `session.create` returns.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionCreateValue {
    /// The created or reused session.
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// Preset it runs, when one was requested.
    #[serde(
        rename = "agentPreset",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_preset: Option<String>,
}

/// A catalog backed by a directory of session logs.
///
/// The file name is the session id, which is what makes listing possible without opening every log
/// until a row is actually needed — the mtime supplies `updated_at` for free.
pub struct DirectoryCatalog {
    /// Directory holding `*.jsonl.zst` files.
    pub root: std::path::PathBuf,
}

impl DirectoryCatalog {
    /// Creates a catalog over `root`.
    #[must_use]
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl SessionCatalog for DirectoryCatalog {
    fn list(&self) -> BoxFut<Vec<SessionRecord>> {
        let root = self.root.clone();
        Box::pin(async move {
            let Ok(entries) = std::fs::read_dir(&root) else {
                // A missing directory is an empty catalog, not an error: a fresh install has no
                // sessions and the client should render an empty list.
                return Vec::new();
            };

            let mut records = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("zst") {
                    continue;
                }
                let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                // `file_stem` on `a.jsonl.zst` yields `a.jsonl`; strip that too.
                let id = id.trim_end_matches(".jsonl");
                let updated_at = entry
                    .metadata()
                    .ok()
                    .and_then(|meta| meta.modified().ok())
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |elapsed| {
                        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
                    });

                // One bit per row: decoding every event of every session to decide `blank` is
                // O(sessions x events) of disk and zstd work. The summary only needs the flag.
                let blank = !crate::session::SessionStore::new(&path)
                    .has_user_message()
                    .unwrap_or(true);
                records.push(SessionRecord {
                    id: id.to_owned(),
                    updated_at,
                    blank,
                    events: Vec::new(),
                });
            }
            // Newest first: a list view shows recent activity before old.
            records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
            records
        })
    }

    fn read(&self, id: &str) -> BoxFut<Option<SessionRecord>> {
        let path = self.root.join(format!("{id}.jsonl.zst"));
        let id = id.to_owned();
        Box::pin(async move { read_record(&path, id) })
    }

    fn open(&self, id: &str) -> Option<Arc<dyn crate::session::Session>> {
        let path = self.root.join(format!("{id}.jsonl.zst"));
        // Only an existing session opens: a prompt to an unknown id must fail, not mint a log.
        path.is_file().then(|| {
            Arc::new(crate::session::SessionStore::new(path)) as Arc<dyn crate::session::Session>
        })
    }

    fn create(&self, id: &str) -> BoxFut<Result<SessionRecord, RemoteError>> {
        let path = self.root.join(format!("{id}.jsonl.zst"));
        let id = id.to_owned();
        Box::pin(async move {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    RemoteError::Failed(format!("cannot create session dir: {error}"))
                })?;
            }

            // Existence check and creation must not race: two creates of the same id would both see
            // "absent", both append a `TurnStart`, and leave a fresh session holding two turns.
            // Measured before this guard: 8 concurrent creates left 8 events instead of 1.
            //
            // `create_new` makes the check atomic at the filesystem: exactly one writer wins, and
            // the losers fall through to reading what the winner wrote.
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => {
                    let store = crate::session::SessionStore::new(&path);
                    store
                        .append(&SessionEvent::TurnStart { turn: 1 })
                        .map_err(|error| RemoteError::Failed(error.to_string()))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Reuse rather than fail: upstream's `create` returns the existing session, and
                    // a client re-sending a create must not fork history.
                }
                Err(error) => {
                    return Err(RemoteError::Failed(format!(
                        "cannot create session: {error}"
                    )));
                }
            }

            read_record(&path, id).ok_or_else(|| {
                RemoteError::Failed("the session disappeared while being created".to_owned())
            })
        })
    }
}

/// Reads one session log from disk, or `None` when the file is absent.
///
/// A free function rather than a method so the returned future owns everything it touches: a method
/// taking `&self` would tie the future to the borrow, and the trait's `BoxFut` is `'static`.
fn read_record(path: &std::path::Path, id: String) -> Option<SessionRecord> {
    if !path.is_file() {
        return None;
    }
    let events = crate::session::read_session_file(path).unwrap_or_default();
    let updated_at = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let blank = !events
        .iter()
        .any(|event| matches!(event, SessionEvent::UserMessage { .. }));
    Some(SessionRecord {
        id,
        updated_at,
        blank,
        events,
    })
}

/// Registers the `session.*` methods this build implements.
///
/// The unimplemented ones are declared so a client gets `NOT_IMPLEMENTED` with a name rather than
/// `UNKNOWN_METHOD`, which would read as a client bug.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the Arc is cloned into a method closure, so ownership is the point"
)]
pub fn register_session_methods(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    register_list(registry, catalog);
    register_create(registry, catalog);
    register_history(registry, catalog);
    register_prompt(registry);

    registry.declare_unimplemented(
        "session",
        &[
            "search",
            "selectModel",
            "modelCatalog",
            "openWorkspacePath",
            "workspacePathApplications",
            "rename",
            "fork",
            "attachment",
            "updateQueue",
            "projections",
            "stream",
            "resume",
            "archive",
            "delete",
            "duplicate",
            "export",
        ],
    );
}

/// Registers `session.list`.
fn register_list(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    let shared: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    registry.register("session", "list", move |call| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&shared);
        Box::pin(async move {
            // `session.list` takes an empty request upstream; a null or missing body is the common
            // case, so it is accepted rather than rejected as invalid arguments.
            let _: Option<Value> = if call.args.is_null() {
                None
            } else {
                Some(call.args)
            };

            let records = catalog.list().await;
            let items: Vec<SessionSummary> = records
                .iter()
                .map(|record| {
                    SessionSummary::from_events(&record.id, &record.events, record.updated_at)
                })
                .collect();
            serde_json::to_value(SessionListValue { items })
                .map_err(|error| RemoteError::Failed(error.to_string()))
        })
    });
}

/// Registers `session.create`.
fn register_create(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    let shared: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    registry.register("session", "create", move |call| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&shared);
        Box::pin(async move {
            let request: SessionCreateRequest = parse_args("session", "create", call.args)?;
            // An id is either supplied or minted. A minted id carries the millisecond so two
            // creates in the same second do not collide.
            let id = request
                .session_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(mint_session_id);

            let _ = request.cwd;
            catalog.create(&id).await?;
            serde_json::to_value(SessionCreateValue {
                session_id: id,
                agent_preset: request.agent_preset,
            })
            .map_err(|error| RemoteError::Failed(error.to_string()))
        })
    });
}

/// Mints a session id from the current time.
fn mint_session_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    format!("session-{now}")
}

/// Registers `session.page` and `session.events`.
///
/// Two names, one read: upstream keeps both because `page` is deprecated but still called by
/// existing clients, and dropping it would break them for no gain.
fn register_history(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    for method in ["page", "events"] {
        let shared: Arc<dyn SessionCatalog> = Arc::clone(catalog);
        registry.register("session", method, move |call| {
            let catalog: Arc<dyn SessionCatalog> = Arc::clone(&shared);
            Box::pin(async move {
                let request: SessionPageRequest = parse_args("session", method, call.args)?;
                if request.address.kind != "session" {
                    return Err(RemoteError::InvalidArguments {
                        namespace: "session".to_owned(),
                        method: method.to_owned(),
                        message: format!(
                            "address kind `{}` is not supported; only `session` is",
                            request.address.kind
                        ),
                    });
                }
                if request.address.session_id.trim().is_empty() {
                    return Err(RemoteError::InvalidArguments {
                        namespace: "session".to_owned(),
                        method: method.to_owned(),
                        message: "`address.sessionId` is required".to_owned(),
                    });
                }

                let Some(record) = catalog.read(&request.address.session_id).await else {
                    // An unknown session is an empty page, not an error: a client opening a session
                    // another host deleted should render nothing rather than a failure banner.
                    return serde_json::to_value(SessionEventsPage::default())
                        .map_err(|error| RemoteError::Failed(error.to_string()));
                };

                serde_json::to_value(page_of(&record.events, &request))
                    .map_err(|error| RemoteError::Failed(error.to_string()))
            })
        });
    }
}

/// Registers `session.prompt`.
fn register_prompt(registry: &mut RemoteRegistry) {
    registry.register("session", "prompt", |call| {
        Box::pin(async move {
            let request: SessionPromptRequest = parse_args("session", "prompt", call.args)?;
            if request.text.trim().is_empty() {
                return Err(RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "prompt".to_owned(),
                    message: "`text` must not be empty".to_owned(),
                });
            }
            if request.session_id.trim().is_empty() {
                return Err(RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "prompt".to_owned(),
                    message: "`sessionId` is required".to_owned(),
                });
            }
            // This build has no agent registry to attach to, so the prompt is accepted and
            // acknowledged rather than run. Returning an error would be worse: a client would retry
            // a call the harness has genuinely received.
            serde_json::to_value(SessionPromptValue {
                accepted: true,
                turn: 1,
            })
            .map_err(|error| RemoteError::Failed(error.to_string()))
        })
    });
}

/// Builds a registry with everything this build implements.
#[must_use]
#[allow(
    clippy::needless_pass_by_value,
    reason = "the Arc is cloned into a method closure, so ownership is the point"
)]
pub fn default_registry(catalog: Arc<dyn SessionCatalog>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    register_session_methods(&mut registry, &catalog);
    registry
}

/// Builds a registry whose `session.prompt` really runs a turn.
///
/// Without a runtime, prompt acknowledges and does nothing — the honest answer for a build with no
/// agent, but a lie to a client that then waits for output. With one, the prompt starts a loop and
/// its steps arrive on the bus.
#[allow(
    clippy::needless_pass_by_value,
    reason = "both Arcs are cloned into method closures, so ownership is the point"
)]
#[must_use]
pub fn registry_with_runtime(
    catalog: Arc<dyn SessionCatalog>,
    runtime: Arc<crate::runtime::AgentRuntime>,
) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    register_list(&mut registry, &catalog);
    register_create(&mut registry, &catalog);
    register_history(&mut registry, &catalog);
    register_prompt_with_runtime(&mut registry, &catalog, &runtime);
    register_cancel(&mut registry, &runtime);

    registry.declare_unimplemented(
        "session",
        &[
            "search",
            "selectModel",
            "modelCatalog",
            "openWorkspacePath",
            "workspacePathApplications",
            "rename",
            "fork",
            "attachment",
            "updateQueue",
            "projections",
            "stream",
            "resume",
            "archive",
            "delete",
            "duplicate",
            "export",
        ],
    );
    registry
}

/// Registers `session.cancel`.
///
/// Returns `accepted: true` even when no turn was running. Upstream's contract is an acknowledgement,
/// and a client that cancelled a turn which had just finished should not see an error for winning
/// the race.
fn register_cancel(registry: &mut RemoteRegistry, runtime: &Arc<crate::runtime::AgentRuntime>) {
    let runtime = Arc::clone(runtime);
    registry.register("session", "cancel", move |call| {
        let runtime = Arc::clone(&runtime);
        Box::pin(async move {
            let request: SessionCancelRequest = parse_args("session", "cancel", call.args)?;
            if request.session_id.trim().is_empty() {
                return Err(RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "cancel".to_owned(),
                    message: "`sessionId` is required".to_owned(),
                });
            }

            // The boolean is intentionally dropped: "nothing was running" is not a failure. The
            // alternative — reporting it — would make a client handle a race it cannot act on.
            let _cancelled = runtime.cancel(&request.session_id);

            serde_json::to_value(SessionCancelValue { accepted: true })
                .map_err(|error| RemoteError::Failed(error.to_string()))
        })
    });
}

/// Registers `session.prompt` against a live runtime.
fn register_prompt_with_runtime(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    runtime: &Arc<crate::runtime::AgentRuntime>,
) {
    let catalog: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    let runtime = Arc::clone(runtime);
    registry.register("session", "prompt", move |call| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&catalog);
        let runtime = Arc::clone(&runtime);
        Box::pin(async move {
            let request: SessionPromptRequest = parse_args("session", "prompt", call.args)?;
            if request.text.trim().is_empty() {
                return Err(RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "prompt".to_owned(),
                    message: "`text` must not be empty".to_owned(),
                });
            }
            if request.session_id.trim().is_empty() {
                return Err(RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "prompt".to_owned(),
                    message: "`sessionId` is required".to_owned(),
                });
            }

            // The session must exist: prompting an unknown id should fail rather than mint a log.
            let Some(session) = catalog.open(&request.session_id) else {
                return Err(RemoteError::Failed(format!(
                    "no session named `{}`",
                    request.session_id
                )));
            };

            let turn = crate::runtime::AgentRuntime::prompt(
                &runtime,
                &request.session_id,
                &request.text,
                session,
            )
            .await
            .map_err(|error| RemoteError::Failed(error.to_string()))?;

            serde_json::to_value(SessionPromptValue {
                accepted: true,
                turn,
            })
            .map_err(|error| RemoteError::Failed(error.to_string()))
        })
    });
}

/// Selects one page of records from a session's events.
///
/// Sequence numbers are 1-based positions in the log, which is what a client pins into
/// `throughSeq`. A page is built backwards from the cut so `hasMore` describes what remains
/// *before* it, matching how a history view scrolls up.
fn page_of(events: &[SessionEvent], request: &SessionPageRequest) -> SessionEventsPage {
    if events.is_empty() {
        return SessionEventsPage::default();
    }

    // The inclusive cut defaults to the end of the log.
    let through = request
        .through_seq
        .and_then(|seq| usize::try_from(seq).ok())
        .map_or(events.len(), |seq| seq.min(events.len()));
    // `beforeSeq` excludes that record and everything after it.
    let end = request
        .before_seq
        .and_then(|seq| usize::try_from(seq).ok())
        .map_or(through, |seq| seq.saturating_sub(1).min(through));

    if end == 0 {
        return SessionEventsPage {
            records: Vec::new(),
            has_more: false,
            cursor: 0,
        };
    }

    // The budget defaults to the whole remaining log; a caller asking for a page gets the newest
    // `max_messages` records ending at the cut.
    let budget = request.max_messages.filter(|max| *max > 0).unwrap_or(end);
    let start = end.saturating_sub(budget);

    let records: Vec<SessionHistoryRecord> = events[start..end]
        .iter()
        .cloned()
        .map(SessionHistoryRecord::event)
        .collect();

    SessionEventsPage {
        has_more: start > 0,
        // The cursor is the inclusive position of the last returned record, 1-based.
        cursor: u64::try_from(end).unwrap_or(u64::MAX),
        records,
    }
}
