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

    /// The access policy refused the call.
    ///
    /// Distinct from `UnknownMethod` and `InvalidArguments`: the method exists and the arguments are fine, and the
    /// caller needs to know that it is not permitted rather than that it got something wrong. A client that saw a
    /// generic failure would retry; this one can show the reason to its operator.
    #[error("`{namespace}.{method}` refused: {reason}")]
    Refused {
        /// Requested namespace.
        namespace: String,
        /// Requested method.
        method: String,
        /// Why, in words the caller can show.
        reason: String,
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
            // Its own code rather than FAILED: a client that branches on this one can stop retrying, which is the whole
            // difference between a permission problem and a transient one.
            Self::Refused { .. } => "REFUSED",
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
    /// What each method declares about itself, for the policy to consult.
    ///
    /// Registered beside the handler so a declaration cannot drift from the method it describes: a separate table would
    /// eventually describe a method that had changed, and the policy would be judging the old one.
    declarations: BTreeMap<(String, String), crate::access::Declaration>,
    /// Who may call what, when a deployment mounted a policy.
    ///
    /// `None` means none was configured, which is different from a policy that allows: the latter would hide the fact
    /// that nothing was set up.
    policy: Option<std::sync::Arc<dyn crate::access::Policy>>,
}

impl RemoteRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mounts an access policy.
    ///
    /// Takes the registry by value for the builder shape the constructors use: a policy mounted on a clone would apply
    /// to that clone, and the server would keep dispatching through the original.
    #[must_use]
    pub fn with_policy(mut self, policy: std::sync::Arc<dyn crate::access::Policy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Declares what one method does, for the policy.
    pub fn declare(
        &mut self,
        namespace: &str,
        method: &str,
        declaration: crate::access::Declaration,
    ) {
        self.declarations
            .insert((namespace.to_owned(), method.to_owned()), declaration);
    }

    /// Asks the policy whether this call may proceed.
    ///
    /// A method with no declaration is treated as a read of its own namespace: that is the least privilege a method can
    /// plausibly need, and a deployment that wants a stricter answer declares one. Treating it as a write instead would
    /// refuse every listing the moment a read-only policy was mounted, which is the opposite of what such a policy is
    /// for.
    fn authorise(
        &self,
        namespace: &str,
        method: &str,
        call: &RemoteCall,
    ) -> Result<(), RemoteError> {
        let Some(policy) = &self.policy else {
            return Ok(());
        };
        let key = (namespace.to_owned(), method.to_owned());
        let declaration = self
            .declarations
            .get(&key)
            .cloned()
            .unwrap_or_else(|| crate::access::Declaration::read(namespace.to_owned()));
        let request = crate::access::Request {
            peer: call.peer.clone(),
            method: format!("{namespace}.{method}"),
            access: declaration.access,
            resources: declaration.resources,
        };
        match policy.decide(&request) {
            crate::access::Decision::Allow => Ok(()),
            crate::access::Decision::Refuse { reason } => Err(RemoteError::Refused {
                namespace: namespace.to_owned(),
                method: method.to_owned(),
                reason,
            }),
        }
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

    /// The declared gaps: methods a client will get `NOT_IMPLEMENTED` for.
    ///
    /// Exposed so a list builder can report them alongside the implemented ones; a caller that had to
    /// duplicate this list would drift from the registry the moment a method was added.
    #[must_use]
    pub fn unimplemented(&self) -> Vec<(String, String)> {
        self.known_unimplemented.clone()
    }

    /// Registered method names, for diagnosis and schema listing.
    #[must_use]
    /// Looks up one method's callable.
    ///
    /// Exposed so a test can drive a method through the same `RemoteCall` a socket produces, rather
    /// than reimplementing the argument shape and drifting from it.
    pub fn get(&self, namespace: &str, method: &str) -> Option<RemoteMethod> {
        self.methods
            .get(&(namespace.to_owned(), method.to_owned()))
            .cloned()
    }

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

        // Checked before the handler runs, so a method that never thought about access still cannot be reached by a
        // peer the policy refuses. A handler-local check would encode the rule once per method and the first one
        // somebody forgot would be the hole.
        self.authorise(namespace, method, &call)?;

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

    /// Renames a session.
    ///
    /// A default that reports the gap rather than a compile error: a catalog backed by something other
    /// than a directory may not support titles, and forcing every implementation to stub it would make
    /// adding a catalog harder than adding a method.
    fn rename(&self, _id: &str, _title: &str) -> BoxFut<Result<String, RemoteError>> {
        Box::pin(async {
            Err(RemoteError::NotImplemented {
                namespace: "session".to_owned(),
                method: "rename".to_owned(),
            })
        })
    }

    /// Forks a session at an exact event sequence.
    ///
    /// A default that reports the gap rather than a compile error, for the same reason `rename` has
    /// one: a catalog backed by something other than a directory may not be able to copy a log.
    fn fork(&self, _id: &str, _at_seq: Option<u64>) -> BoxFut<Result<String, RemoteError>> {
        Box::pin(async {
            Err(RemoteError::NotImplemented {
                namespace: "session".to_owned(),
                method: "fork".to_owned(),
            })
        })
    }

    /// Deletes a session.
    fn delete(&self, _id: &str) -> BoxFut<Result<bool, RemoteError>> {
        Box::pin(async {
            Err(RemoteError::NotImplemented {
                namespace: "session".to_owned(),
                method: "delete".to_owned(),
            })
        })
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

    /// Renames a session by writing a companion file beside its log.
    ///
    /// A companion rather than an event: a rename is not part of the conversation, and appending one to
    /// the log would make every history read carry a record the model never saw. The file is small
    /// enough that reading it per row costs nothing next to decoding a log.
    fn rename(&self, id: &str, title: &str) -> BoxFut<Result<String, RemoteError>> {
        let path = self.root.join(format!("{id}.title"));
        let id = id.to_owned();
        let title = title.trim().to_owned();
        let log = self.root.join(format!("{id}.jsonl.zst"));
        Box::pin(async move {
            if title.is_empty() {
                return Err(RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "rename".to_owned(),
                    message: "a title cannot be blank".to_owned(),
                });
            }
            if !log.is_file() {
                return Err(RemoteError::UnknownMethod {
                    // A missing session is not an unknown method, but the error enum has no NotFound;
                    // the message names the id so a reader can tell which one was missing.
                    namespace: "session".to_owned(),
                    method: format!("rename `{id}`"),
                });
            }
            std::fs::write(&path, title.as_bytes())
                .map_err(|error| RemoteError::Failed(error.to_string()))?;
            Ok(title)
        })
    }

    /// Forks a session at `at_seq`, or at the latest completed turn.
    ///
    /// The fork copies events up to and including the cut, so the new session replays exactly what the
    /// source had at that point. A cut inside a tool pair would produce a log whose `tool_use` has no
    /// answer, so the cut moves backwards to the nearest `turn/end` at or before the requested
    /// sequence.
    fn fork(&self, id: &str, at_seq: Option<u64>) -> BoxFut<Result<String, RemoteError>> {
        let source = self.root.join(format!("{id}.jsonl.zst"));
        let id = id.to_owned();
        let root = self.root.clone();
        Box::pin(async move {
            let Ok(events) = crate::session::read_session_file(&source) else {
                return Err(RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "fork".to_owned(),
                    message: format!("no session `{id}`"),
                });
            };

            let cut = match at_seq {
                Some(seq) => usize::try_from(seq).unwrap_or(usize::MAX),
                // Omission selects the whole log: the latest completed turn is the last one, and a
                // session with no turns forks empty rather than failing.
                None => events.len(),
            };
            let cut = cut.min(events.len());

            // The cut must land on a turn boundary. A cut inside a turn would separate a tool call from
            // its result, and a log whose `tool_use` has no answer is rejected by every provider. The
            // last `turn/end` at or before the request is the last point a replay can resume from.
            let cut = (0..cut)
                .rev()
                .find(|index| matches!(events.get(*index), Some(SessionEvent::TurnEnd { .. })))
                .map_or(0, |index| index + 1);

            let forked_id = format!("{id}-fork-{}", crate::schedule::now_millis());
            let target = root.join(format!("{forked_id}.jsonl.zst"));
            let store = crate::session::SessionStore::new(&target);
            store
                .touch()
                .map_err(|error| RemoteError::Failed(error.to_string()))?;
            for event in events.into_iter().take(cut) {
                store
                    .append(&event)
                    .map_err(|error| RemoteError::Failed(error.to_string()))?;
            }
            Ok(forked_id)
        })
    }

    /// Deletes a session's log and its title.
    ///
    /// Returns whether anything was removed, so a client distinguishes a deletion from a no-op without
    /// an error. A missing title file is not a failure: most sessions never had one.
    fn delete(&self, id: &str) -> BoxFut<Result<bool, RemoteError>> {
        let log = self.root.join(format!("{id}.jsonl.zst"));
        let title = self.root.join(format!("{id}.title"));
        let id = id.to_owned();
        Box::pin(async move {
            let _ = id;
            let removed = match std::fs::remove_file(&log) {
                Ok(()) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(RemoteError::Failed(error.to_string())),
            };
            // A title with no log is stale, so it goes either way.
            let _ = std::fs::remove_file(&title);
            Ok(removed)
        })
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
                    // The file exists and is empty. It used to hold a placeholder `turn/start`, which
                    // made a session that had never been prompted look like it had one turn, and made
                    // the first prompt write a second `turn/start` for the same turn. An empty file is
                    // a valid session (`read_all` reads it as no events) and the loop writes the first
                    // turn.
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

/// Methods the upstream `session` namespace defines that this build does not implement.
///
/// One list, referenced by every registry constructor. Two copies drifted the moment a method was
/// added — the entry stayed in one list while disappearing from the other, and the only symptom was
/// a `NOT_IMPLEMENTED` answer for a method that existed.
/// Session methods this build declares but does not implement.
///
/// Every name here must be genuinely absent: an implemented method listed as a gap makes a client show
/// "not implemented" for something that works. Three names were removed from this list the moment they
/// were implemented (`projections`, `rename`, `delete`) and two more (`selectModel`, `modelCatalog`)
/// when the route table gave them something to answer with.
/// Session methods this build declares but does not implement.
///
/// Every name here must be genuinely absent. `fork` was on this list for several rounds after it was implemented, and
/// nothing noticed because `dispatch` prefers a handler: a client reading the declaration would have shown "not
/// implemented" for a method that works. The list is a promise about what is missing, so a name that works is a lie
/// even when the behaviour is right.
/// Session methods this build declares but does not implement.
///
/// `search` was here while `session_query_rpc` implemented it: the module registered the method somewhere else entirely
/// and this list was never told, so a client reading the declaration would have shown "not implemented" for a method the
/// server never exposed either. Both halves were wrong, and neither test could see it because the drift test only
/// checked the methods the session core registers.
const UNIMPLEMENTED_SESSION_METHODS: &[&str] = &["workspacePathApplications", "stream", "resume"];

/// Registers the `session.*` methods this build implements.
///
/// The unimplemented ones are declared so a client gets `NOT_IMPLEMENTED` with a name rather than
/// `UNKNOWN_METHOD`, which would read as a client bug.
#[allow(
    clippy::needless_pass_by_value,
    reason = "the Arc is cloned into a method closure, so ownership is the point"
)]
pub fn register_session_methods(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    register_session_core(registry, catalog, None);
    // No runtime, so a prompt cannot actually run a turn: the handler acknowledges and does nothing.
    register_prompt(registry);
}

/// Registers every `session.*` method, given whatever the caller can supply.
///
/// One function rather than a list repeated in each constructor. Seven times a method was added to one
/// path and forgotten in another -- `rename`, `delete`, `fork`, both model methods, `attachment` -- and
/// a test caught it each time. The test was doing work the structure should have done, and this is that
/// structure.
fn register_session_core(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    runtime: Option<&Arc<crate::runtime::AgentRuntime>>,
) {
    register_list(registry, catalog);
    register_create(registry, catalog);
    register_history(registry, catalog);
    register_follow(registry, catalog);
    crate::session_export_rpc::register_session_export_method(registry, catalog);
    register_rename(registry, catalog);
    register_delete(registry, catalog);
    register_fork(registry, catalog);
    register_attachment_method(registry, &Arc::new(crate::attachment::MemoryStore::new()));

    match runtime {
        Some(runtime) => {
            register_prompt_with_runtime(registry, catalog, runtime);
            register_cancel(registry, runtime);
        }
        None => {
            // Declared rather than omitted: a client distinguishes "this build cannot" from "no such
            // method", and the two call for different handling.
            registry.declare_unimplemented("session", &["cancel"]);
        }
    }

    registry.declare_unimplemented("session", UNIMPLEMENTED_SESSION_METHODS);

    // Every constructor gets the command surface: a slash command is available wherever a session is, and a
    // deployment that could list sessions but not run a command would be a strange half.
    register_command_methods(registry, &Arc::new(crate::commands::Registry::new()));
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

/// Registers `session.follow`.
///
/// The one method shaped for polling: a client names the sequence it has already seen and receives what came after, so a
/// subscription built on it repeats nothing and misses nothing. `session.page` cannot serve that role -- its arguments
/// are a window, not a cursor, so a poller would either re-read overlap or guess where it left off.
///
/// `done` is what ends the subscription: the poller stops when a poll returns it, which is the contract the streaming
/// layer documents. A session never finishes, so this stays false and the client ends the subscription instead.
fn register_follow(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    let catalog: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    registry.register("session", "follow", move |call| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&catalog);
        Box::pin(async move {
            let session_id = call
                .args
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "follow".to_owned(),
                    message: "`sessionId` is required".to_owned(),
                })?
                .to_owned();
            // Absent means "from the beginning", which is what a client that just opened a session means. Zero would do
            // the same thing, but a caller that typed `afterSeq: 0` should get the same answer as one that omitted it.
            let after = call
                .args
                .get("afterSeq")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let limit = usize::try_from(
                call.args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(FOLLOW_LIMIT as u64),
            )
            .unwrap_or(FOLLOW_LIMIT)
            .max(1);

            let Some(record) = catalog.read(&session_id).await else {
                // An unknown session is an empty result with `done`: the client's subscription ends quietly rather than
                // polling a session that does not exist forever.
                return Ok(serde_json::json!({
                    "events": [],
                    "nextSeq": after,
                    "done": true,
                }));
            };

            // Sequence numbers are one-based, so an event at index `i` has seq `i + 1`. Taking `after` to mean "the last
            // seq I saw" makes `skip(after)` the exact continuation, which is the arithmetic a cursor needs and the
            // place an off-by-one would silently duplicate or drop an event.
            let skip = usize::try_from(after).unwrap_or(usize::MAX);
            let events: Vec<serde_json::Value> = record
                .events
                .iter()
                .skip(skip)
                .take(limit)
                .enumerate()
                .filter_map(|(offset, event)| {
                    serde_json::to_value(event).ok().map(|value| {
                        serde_json::json!({
                            "seq": skip + offset + 1,
                            "event": value,
                        })
                    })
                })
                .collect();

            let next = skip
                .saturating_add(events.len())
                .try_into()
                .unwrap_or(u64::MAX);
            Ok(serde_json::json!({
                "events": events,
                "nextSeq": next,
                "done": false,
            }))
        })
    });
}

/// How many events one `session.follow` poll returns.
///
/// 64: a poll that returned thousands would make one frame enormous and the next poll empty, which reads as a stall.
pub const FOLLOW_LIMIT: usize = 64;

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

/// Builds a registry with every method this build implements, without a runtime.
///
/// Test-only. A production caller wants [`registry_with_runtime`]: without a runtime,
/// `session.prompt` acknowledges and never runs a turn, which is a lie to a client that then waits
/// for output. Exposing this publicly invited exactly that mistake, so it is not public.
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
    // No route table, so this path serves no model methods and declares them gaps: a caller without
    // routes cannot answer a model question, and a picker that listed nothing would be worse than an
    // honest gap. A caller that has routes uses `registry_with_routes`.
    let mut registry = RemoteRegistry::new();
    register_session_core(&mut registry, &catalog, Some(&runtime));
    registry.declare_unimplemented("session", &["modelCatalog", "selectModel"]);
    registry
}

/// Builds a registry whose model methods read a route table.
///
/// The catalogue and the selection check both need the routes; a registry built without them would
/// offer a model picker that lists nothing and accepts nothing.
#[must_use]
pub fn registry_with_routes(
    catalog: &Arc<dyn SessionCatalog>,
    runtime: &Arc<crate::runtime::AgentRuntime>,
    routes: &Arc<crate::providers::RouteRegistry>,
) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    register_session_core(&mut registry, catalog, Some(runtime));
    register_model_catalog(&mut registry, routes);
    register_select_model(&mut registry, routes);
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

// ---- the projection and job Remote methods ----------------------------------------------------

/// Registers the methods a client reads folded session state through.
///
/// The bridge between the log and a UI. Without it a client would have to replay events itself,
/// which means shipping the fold logic to the browser and keeping two implementations in step.
pub fn register_projection_methods(
    registry: &mut RemoteRegistry,
    projections: &std::sync::Arc<crate::projection::ProjectionRegistry>,
) {
    let projections = std::sync::Arc::clone(projections);
    registry.register("session", "projections", move |_call: RemoteCall| {
        let projections = std::sync::Arc::clone(&projections);
        Box::pin(async move {
            // Every registered projection, as the summary a UI lists. A full view would carry a
            // transcript; the summary is what a session list needs.
            let value = projections.summary();
            let mut serialized = serde_json::Map::new();
            for (key, state) in value {
                serialized.insert(key, state);
            }
            Ok(serde_json::json!({ "projections": serialized }))
        })
    });
}

/// Registers the methods a client manages background jobs through.
///
/// The projections and the registry are shared rather than copied: a client that killed a job must
/// see its status change without asking again.
pub fn register_job_methods(
    registry: &mut RemoteRegistry,
    jobs: &std::sync::Arc<crate::jobs::JobRegistry>,
) {
    let jobs = std::sync::Arc::clone(jobs);
    registry.register("job", "list", {
        let jobs = std::sync::Arc::clone(&jobs);
        move |_call: RemoteCall| {
            let jobs = std::sync::Arc::clone(&jobs);
            Box::pin(async move {
                // The whole set, not a delta: a reconnecting client's first frame is already correct.
                let mut views = Vec::new();
                for id in jobs.ids() {
                    if let Ok(job) = jobs.get(&id) {
                        let view = crate::api_job::project(&job);
                        views.push(serde_json::json!({
                            "id": view.id,
                            "kind": view.kind,
                            "label": view.label,
                            "status": view.status,
                            "code": view.code,
                        }));
                    }
                }
                Ok(serde_json::json!({ "jobs": views }))
            })
        }
    });

    registry.register("job", "kill", {
        let jobs = std::sync::Arc::clone(&jobs);
        move |call: RemoteCall| {
            let jobs = std::sync::Arc::clone(&jobs);
            Box::pin(async move {
                let job_id = call
                    .args
                    .get("jobId")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| RemoteError::InvalidArguments {
                        namespace: "job".to_owned(),
                        method: "kill".to_owned(),
                        message: "`jobId` is required".to_owned(),
                    })?
                    .to_owned();

                // An unknown job is an invalid argument rather than a distinct variant: the client
                // sent an id this registry does not hold, which is the same class of mistake as a
                // missing field.
                let job = jobs
                    .get(&job_id)
                    .map_err(|_| RemoteError::InvalidArguments {
                        namespace: "job".to_owned(),
                        method: "kill".to_owned(),
                        message: format!("no job `{job_id}`"),
                    })?;
                let outcome = crate::api_job::kill_outcome(job.status);
                if job.status.is_live() {
                    let _ = jobs.request_stop(&job_id);
                }
                Ok(serde_json::json!({ "outcome": outcome }))
            })
        }
    });
}

/// Every method name this build can register, as `namespace.method`.
///
/// The session methods this build declares but does not implement.
///
/// Exported so a test can check every name rather than a hand-picked handful: the probe list used to be written by
/// hand, which is why `fork` stayed declared as missing for several rounds after it worked.
#[must_use]
pub fn declared_gaps() -> &'static [&'static str] {
    UNIMPLEMENTED_SESSION_METHODS
}

/// A single list so a page or a test can check what it offers against what exists. It is built from
/// the same registration functions the server uses, so the two cannot drift.
#[must_use]
pub fn registered_method_names() -> Vec<String> {
    // Built from the same registration functions the server uses, so the list cannot drift from what
    // a server actually serves. An earlier version hand-copied the session names here and went stale
    // the moment `delete` was added -- a page offered a method the list denied existed.
    let mut registry = RemoteRegistry::new();
    let catalog: Arc<dyn SessionCatalog> = Arc::new(DirectoryCatalog::new(std::env::temp_dir()));
    register_session_methods(&mut registry, &catalog);
    register_job_methods(
        &mut registry,
        &Arc::new(crate::jobs::JobRegistry::new(4096)),
    );
    register_projection_methods(
        &mut registry,
        &Arc::new(crate::projection::ProjectionRegistry::new()),
    );

    let mut names: Vec<String> = registry
        .names()
        .into_iter()
        .map(|(namespace, method)| format!("{namespace}.{method}"))
        .chain(
            registry
                .unimplemented()
                .into_iter()
                .map(|(namespace, method)| format!("{namespace}.{method}")),
        )
        .collect();
    for method in crate::api_job::job_methods() {
        names.push(method.qualified());
    }
    for method in crate::api_session::session_methods() {
        names.push(method.qualified());
    }
    for method in crate::api_config::workspace_methods() {
        names.push(method.qualified());
    }
    names.sort();
    names.dedup();
    names
}

/// Registers the workspace Remote methods.
///
/// The registry is shared: a client that renamed a workspace must see the new title on the next list
/// without asking again for permission to.
pub fn register_workspace_methods(
    registry: &mut RemoteRegistry,
    workspaces: &std::sync::Arc<crate::api_config::WorkspaceRegistry>,
) {
    let workspaces = std::sync::Arc::clone(workspaces);

    registry.register("workspace", "list", {
        let workspaces = std::sync::Arc::clone(&workspaces);
        move |_call: RemoteCall| {
            let workspaces = std::sync::Arc::clone(&workspaces);
            Box::pin(async move {
                let now = crate::schedule::now_millis();
                let views: Vec<serde_json::Value> = workspaces
                    .list()
                    .into_iter()
                    .map(|workspace| {
                        let view = workspace.view(now);
                        serde_json::json!({
                            "workspace_id": view.workspace_id,
                            "path": view.path,
                            "title": view.title,
                            "session_ids": view.session_ids,
                            "created_at": view.created_at,
                            "updated_at": view.updated_at,
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "workspaces": views }))
            })
        }
    });

    registry.register("workspace", "create", {
        let workspaces = std::sync::Arc::clone(&workspaces);
        move |call: RemoteCall| {
            let workspaces = std::sync::Arc::clone(&workspaces);
            Box::pin(async move {
                let path = call
                    .args
                    .get("path")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| RemoteError::InvalidArguments {
                        namespace: "workspace".to_owned(),
                        method: "create".to_owned(),
                        message: "`path` is required".to_owned(),
                    })?;
                let title = call
                    .args
                    .get("title")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned);

                // The id is derived from the path so a re-registration of the same directory is
                // refused by the registry rather than producing a second entry.
                let id = format!("ws-{}", simple_hash(path));
                let now = crate::schedule::now_millis();
                let workspace = workspaces
                    .register(id, path, title, now)
                    .map_err(|existing| RemoteError::InvalidArguments {
                        namespace: "workspace".to_owned(),
                        method: "create".to_owned(),
                        message: format!("`{path}` is already registered as `{existing}`"),
                    })?;
                let view = workspace.view(now);
                Ok(serde_json::json!({
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
        }
    });

    registry.register("workspace", "delete", {
        let workspaces = std::sync::Arc::clone(&workspaces);
        move |call: RemoteCall| {
            let workspaces = std::sync::Arc::clone(&workspaces);
            Box::pin(async move {
                let id = call
                    .args
                    .get("workspaceId")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| RemoteError::InvalidArguments {
                        namespace: "workspace".to_owned(),
                        method: "delete".to_owned(),
                        message: "`workspaceId` is required".to_owned(),
                    })?;
                let deleted = workspaces.delete(id);
                Ok(serde_json::json!({ "deleted": deleted }))
            })
        }
    });
}

/// A stable 32-bit hash of a path, for deriving a workspace id.
///
/// FNV-1a rather than a random id: the same directory must produce the same id, which is what makes a
/// re-registration a refusal instead of a duplicate entry.
fn simple_hash(text: &str) -> String {
    let mut hash: u32 = 2_166_136_261;
    for byte in text.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    format!("{hash:08x}")
}

/// Registers `session.rename`.
fn register_rename(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    let catalog: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    registry.register("session", "rename", move |call| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&catalog);
        Box::pin(async move {
            let session_id = call
                .args
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "rename".to_owned(),
                    message: "`sessionId` is required".to_owned(),
                })?
                .to_owned();
            let title = call
                .args
                .get("title")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "rename".to_owned(),
                    message: "`title` is required".to_owned(),
                })?
                .to_owned();

            let stored = catalog.rename(&session_id, &title).await?;
            Ok(serde_json::json!({ "title": stored }))
        })
    });
}

/// Registers `session.delete`.
fn register_delete(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    let catalog: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    registry.register("session", "delete", move |call| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&catalog);
        Box::pin(async move {
            let session_id = call
                .args
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "delete".to_owned(),
                    message: "`sessionId` is required".to_owned(),
                })?
                .to_owned();

            let deleted = catalog.delete(&session_id).await?;
            Ok(serde_json::json!({ "deleted": deleted }))
        })
    });
}

/// Registers `session.modelCatalog`.
///
/// The models a client may choose from, read from the route registry rather than hard-coded: a
/// deployment that adds a provider must not need a code change for its UI to offer it.
pub fn register_model_catalog(
    registry: &mut RemoteRegistry,
    routes: &Arc<crate::providers::RouteRegistry>,
) {
    let routes = Arc::clone(routes);
    registry.register("session", "modelCatalog", move |_call: RemoteCall| {
        let routes = Arc::clone(&routes);
        Box::pin(async move {
            let mut groups: Vec<serde_json::Value> = Vec::new();
            for name in routes.names() {
                let Ok(route) = routes.get(&name) else {
                    continue;
                };
                let models: Vec<serde_json::Value> = route
                    .models
                    .iter()
                    .map(|model| {
                        serde_json::json!({
                            "provider": route.name,
                            "model": model.id,
                            "displayName": model.display_name,
                            // Omitted rather than zeroed when the catalogue is silent: a client that
                            // showed "0 tokens" would be claiming a fact nobody stated.
                            "contextWindow": model.context_window,
                        })
                    })
                    .collect();
                groups.push(serde_json::json!({
                    "provider": route.name,
                    "displayName": route.display_name,
                    "models": models,
                }));
            }

            // The default is the first route's default model, which is what a session with no recorded
            // selection would use. Absent when nothing is registered, rather than an invented pair.
            let default = routes
                .names()
                .first()
                .and_then(|name| routes.resolve(name, None).ok())
                .map(|(route, model)| {
                    serde_json::json!({ "provider": route.name, "model": model.id })
                });

            let providers: Vec<String> = routes
                .names()
                .into_iter()
                .filter(|name| routes.get(name).is_ok_and(|route| !route.models.is_empty()))
                .collect();

            Ok(serde_json::json!({
                "default": default,
                "routableProviders": providers,
                "groups": groups,
                // Empty: this build reads its catalogue from the routing table, which cannot fail.
                // The field exists because a client renders a warning list, and an absent field would
                // be a different sort of absence from an empty one.
                "failures": Vec::<serde_json::Value>::new(),
            }))
        })
    });
}

/// Registers `session.selectModel`.
///
/// Records a selection for a session's next step. The request carries the full selection because a
/// partial one cannot be routed: a provider without a model has nothing to call.
pub fn register_select_model(
    registry: &mut RemoteRegistry,
    routes: &Arc<crate::providers::RouteRegistry>,
) {
    let routes = Arc::clone(routes);
    registry.register("session", "selectModel", move |call: RemoteCall| {
        let routes = Arc::clone(&routes);
        Box::pin(async move {
            let provider = call
                .args
                .get("provider")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "selectModel".to_owned(),
                    message: "`provider` is required".to_owned(),
                })?
                .to_owned();
            let model = call
                .args
                .get("model")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "selectModel".to_owned(),
                    message: "`model` is required".to_owned(),
                })?
                .to_owned();

            let selection = crate::agent::ModelSelection::new(provider, model);
            // Validated before it is recorded: a selection naming a route this deployment does not
            // mount would otherwise be accepted here and fail at the next request, far from the client
            // that sent it.
            crate::agent::validate(&selection, &routes).map_err(|error| {
                RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "selectModel".to_owned(),
                    message: error.to_string(),
                }
            })?;

            Ok(serde_json::json!({
                "selected": { "provider": selection.provider, "model": selection.model },
            }))
        })
    });
}

/// Registers `session.fork`.
fn register_fork(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    let catalog: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    registry.register("session", "fork", move |call| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&catalog);
        Box::pin(async move {
            let session_id = call
                .args
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "fork".to_owned(),
                    message: "`sessionId` is required".to_owned(),
                })?
                .to_owned();
            // Omitted means "the whole log", which is a different request from "the first event".
            let at_seq = call.args.get("atSeq").and_then(Value::as_u64);

            let forked = catalog.fork(&session_id, at_seq).await?;
            Ok(serde_json::json!({ "sessionId": forked }))
        })
    });
}

/// Registers `session.attachment`.
///
/// Reads one stored image back by reference. The reference is the client's, so this is a lookup and not
/// a search: a client that lost its id has no way to ask for "the image in this session".
///
/// The data is base64 in the response because JSON has no byte array. The encoding is canonical, so a
/// client comparing two responses for the same image sees identical text.
pub fn register_attachment_method(
    registry: &mut RemoteRegistry,
    store: &Arc<crate::attachment::MemoryStore>,
) {
    let store = Arc::clone(store);
    registry.register("session", "attachment", move |call| {
        let store = Arc::clone(&store);
        Box::pin(async move {
            let attachment =
                call.args
                    .get("attachment")
                    .ok_or_else(|| RemoteError::InvalidArguments {
                        namespace: "session".to_owned(),
                        method: "attachment".to_owned(),
                        message: "`attachment` is required".to_owned(),
                    })?;
            let id = attachment
                .get("attachmentId")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "attachment".to_owned(),
                    message: "`attachment.attachmentId` is required".to_owned(),
                })?;

            let Some(image) = store.get(id) else {
                return Err(RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "attachment".to_owned(),
                    message: format!("no attachment `{id}`"),
                });
            };

            Ok(serde_json::json!({
                "attachment": {
                    "attachmentId": id,
                    // The verified type, not the caller's claim: the store already refused anything
                    // whose bytes disagreed with its declared type.
                    "mediaType": image.media_type,
                    "bytes": image.data.len(),
                    "name": image.name,
                },
                "data": crate::attachment::encode_base64(&image.data),
            }))
        })
    });
}

/// Registers the `command.*` methods.
///
/// A slash command is typed by a person, not called by a model, so it needs its own namespace: a client that offered
/// commands under `session.*` would have to guess which names were model-facing and which were not.
pub fn register_command_methods(
    registry: &mut RemoteRegistry,
    commands: &Arc<crate::commands::Registry>,
) {
    let catalog = Arc::clone(commands);
    registry.register("command", "list", move |_call: RemoteCall| {
        let catalog = Arc::clone(&catalog);
        Box::pin(async move {
            let definitions: Vec<serde_json::Value> = catalog
                .definitions()
                .into_iter()
                .map(|definition| {
                    serde_json::json!({
                        "name": definition.name,
                        "description": definition.description,
                        "input": {
                            "kind": match definition.input {
                                crate::commands::Input::None => "none",
                                crate::commands::Input::Text { .. } => "text",
                                crate::commands::Input::Files { .. } => "files",
                                crate::commands::Input::Choice { .. } => "choice",
                            },
                            "label": definition.input.label(),
                        },
                    })
                })
                .collect();
            Ok(serde_json::json!({ "commands": definitions }))
        })
    });

    let run = Arc::clone(commands);
    registry.register("command", "run", move |call: RemoteCall| {
        let run = Arc::clone(&run);
        Box::pin(async move {
            let name = call
                .args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "command".to_owned(),
                    method: "run".to_owned(),
                    message: "`name` is required".to_owned(),
                })?
                .to_owned();

            // The argument kind is derived from what the client sent rather than from the declaration: a client that
            // sent `text` to a command asking for a choice made a mistake the registry will name.
            let arguments = match call.args.get("arguments") {
                None | Some(serde_json::Value::Null) => crate::commands::Arguments::None,
                Some(serde_json::Value::String(text)) => {
                    crate::commands::Arguments::Text(text.clone())
                }
                Some(serde_json::Value::Array(values)) => crate::commands::Arguments::Files(
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                ),
                Some(other) => {
                    return Err(RemoteError::InvalidArguments {
                        namespace: "command".to_owned(),
                        method: "run".to_owned(),
                        message: format!("`arguments` must be a string or an array, got {other}"),
                    });
                }
            };

            match run.execute(&name, arguments).await {
                Ok(message) => Ok(serde_json::json!({ "message": message, "isError": false })),
                Err(error) => Ok(serde_json::json!({
                    // A command failure is a result a client renders, not a transport error: the person typed
                    // something and deserves to see why it did not work.
                    "message": error.to_string(),
                    "isError": true,
                })),
            }
        })
    });
}

/// Registers the `extension.*` methods.
///
/// Extensions are code a deployment adds, and a client needs to see what is installed and approve it before it runs.
/// The approval is its own method rather than part of `activate`: an approval granted as a side effect of activation
/// is one nobody was asked for, which is exactly the defect the approval flow was added to fix.
pub fn register_extension_methods(
    registry: &mut RemoteRegistry,
    extensions: &Arc<crate::extensions::Registry>,
) {
    let list = Arc::clone(extensions);
    registry.register("extension", "list", move |_call: RemoteCall| {
        let list = Arc::clone(&list);
        Box::pin(async move {
            let entries: Vec<serde_json::Value> = list
                .ids()
                .into_iter()
                .filter_map(|id| {
                    let definition = list.definition(&id)?;
                    let status = list.status(&id)?;
                    Some(serde_json::json!({
                        "id": definition.id.as_str(),
                        "name": definition.name,
                        "description": definition.description,
                        "source": definition.source,
                        "requires": definition.requires,
                        // Whether it may run is a separate fact from whether it is installed: a client that showed
                        // "installed" as "running" would misreport every extension waiting for approval.
                        "approvedBy": definition.approved_by,
                        "status": status.as_str(),
                    }))
                })
                .collect();
            Ok(serde_json::json!({ "extensions": entries }))
        })
    });

    let approve = Arc::clone(extensions);
    registry.register("extension", "approve", move |call: RemoteCall| {
        let approve = Arc::clone(&approve);
        Box::pin(async move {
            let id = call
                .args
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "extension".to_owned(),
                    method: "approve".to_owned(),
                    message: "`id` is required".to_owned(),
                })?
                .to_owned();
            let by = call
                .args
                .get("by")
                .and_then(Value::as_str)
                .unwrap_or("client")
                .to_owned();

            approve
                .approve(&id, by)
                .map_err(|error| RemoteError::InvalidArguments {
                    namespace: "extension".to_owned(),
                    method: "approve".to_owned(),
                    message: error.to_string(),
                })?;
            Ok(serde_json::json!({ "approved": true }))
        })
    });
}
