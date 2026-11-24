//! Remote method dispatch: the `namespace.method` surface a real client calls.
//!
//! Upstream generates these from TypeScript decorators — `@Remote('list')` on a service method —
//! and Typert projects each one into a client-side `remote.<namespace>.<method>()`. This module is
//! the Rust equivalent without the code generation: a method is registered by name, and dispatch is
//! a lookup.
//!
//! # What dispatch does
//!
//! A method is a name in a namespace. Dispatch looks it up, and the method parses its own JSON
//! arguments. An unknown name is `UNKNOWN_METHOD`. A handler that cannot do the work returns a
//! structured result, so a client can tell a refusal from a typo.
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

use crate::core::scope::BoxFut;

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
    /// Persistence format as the browser list reads it.
    ///
    /// This build writes the current session format, so a listed row is `current`.
    #[serde(rename = "formatStatus")]
    pub format_status: String,

    /// How much of the log could be read.
    ///
    /// `Empty` and `TornTail` both produce no events, and before this field they were the same thing to a caller: a
    /// session that was created and never used, and a session whose last write was interrupted. The second is repairable
    /// and the first needs nothing, so a reader had no way to tell which it was looking at.
    pub integrity: crate::session::log_integrity::Integrity,

    /// Working directory, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Fork parent, when this session was created from another.
    #[serde(
        rename = "parentSessionId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_session_id: Option<String>,
    /// Creation origin, when the session was not opened directly by a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
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
            // A summary built from a log has no live agent. `session.list` overlays the runtime.
            agent_available: false,
            running: false,
            blank: !has_messages,
            format_status: "current".to_owned(),
            // The summary was built from events this caller already decoded, so it can say how many.
            integrity: crate::session::log_integrity::Integrity::Readable {
                events: events.len(),
            },
            cwd: None,
            parent_session_id: None,
            origin: None,
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
            format_status: "current".to_owned(),
            integrity: record.integrity.clone(),
            cwd: None,
            parent_session_id: None,
            origin: None,
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
///
/// The browser sends `requestId`, `mode`, and `content` parts. This struct is the text the runtime
/// actually runs after those parts are read. `text` remains for callers that still send the local
/// one-string form.
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
    /// The peer name as the caller sent it. This field is not checked against a credential.
    pub peer: String,
}

/// A method implementation.
///
/// `Arc<dyn Fn>` rather than an `async fn` in a trait: methods are stored by name in a map, so the
/// callable must be object-safe. That is migration-plan §1.3 constraint one again.
pub type RemoteMethod = Arc<dyn Fn(RemoteCall) -> BoxFut<Result<Value, RemoteError>> + Send + Sync>;

/// The sink a streaming Remote method pushes values through.
///
/// Dropping it ends the stream. A push that returns `false` means the mux stopped reading, which
/// is the host method's signal to return.
pub struct StreamYield {
    tx: tokio::sync::mpsc::UnboundedSender<Result<Value, RemoteError>>,
}

impl StreamYield {
    /// Pushes one value. `false` means the reader is gone.
    pub fn push(&self, value: Value) -> bool {
        self.tx.send(Ok(value)).is_ok()
    }

    /// Ends the stream with a Remote error instead of a normal end.
    pub fn fail(&self, error: RemoteError) -> bool {
        self.tx.send(Err(error)).is_ok()
    }
}

/// What a streaming Remote method receives when the mux opens it.
pub struct StreamOpen {
    /// The client's open payload and peer name.
    pub call: RemoteCall,
    /// Items the client sends after `open`, until it sends `end`.
    pub uplink: Arc<crate::api::extra::UplinkInbox>,
    /// Set when the client sends `cancel` or the socket closes.
    pub cancel: tokio_util::sync::CancellationToken,
    yield_to: StreamYield,
}

impl StreamOpen {
    /// Pairs the host method with the receiver the mux drains onto the socket.
    #[must_use]
    pub fn pair(
        call: RemoteCall,
        uplink: Arc<crate::api::extra::UplinkInbox>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<Result<Value, RemoteError>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                call,
                uplink,
                cancel,
                yield_to: StreamYield { tx },
            },
            rx,
        )
    }

    /// Pushes one item toward the client.
    pub fn push(&self, value: Value) -> bool {
        self.yield_to.push(value)
    }

    /// Ends the stream with a Remote error.
    pub fn fail(&self, error: RemoteError) -> bool {
        self.yield_to.fail(error)
    }
}

/// A method that yields a series of values rather than one.
pub type RemoteStreamMethod = Arc<dyn Fn(StreamOpen) -> BoxFut<()> + Send + Sync>;

/// The method registry.
#[derive(Default)]
pub struct RemoteRegistry {
    methods: BTreeMap<(String, String), RemoteMethod>,
    streams: BTreeMap<(String, String), RemoteStreamMethod>,
    /// Methods the upstream API defines that this build does not implement.
    ///
    /// Recorded explicitly so dispatch can distinguish "not implemented" from "typo". A hardcoded
    /// list is the honest option: guessing from a missing entry cannot tell the two apart.
    known_unimplemented: Vec<(String, String)>,
    /// What each method declares about itself, for the policy to consult.
    ///
    /// Registered beside the handler so a declaration cannot drift from the method it describes: a separate table would
    /// eventually describe a method that had changed, and the policy would be judging the old one.
    declarations: BTreeMap<(String, String), crate::guard::access::Declaration>,
    /// Who may call what, when a deployment mounted a policy.
    ///
    /// `None` means none was configured, which is different from a policy that allows: the latter would hide the fact
    /// that nothing was set up.
    policy: Option<std::sync::Arc<dyn crate::guard::access::Policy>>,
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
    pub fn with_policy(mut self, policy: std::sync::Arc<dyn crate::guard::access::Policy>) -> Self {
        self.set_policy(policy);
        self
    }

    /// Replaces the mounted access policy.
    pub fn set_policy(&mut self, policy: std::sync::Arc<dyn crate::guard::access::Policy>) {
        self.policy = Some(policy);
    }

    /// Declares what one method does, for the policy.
    pub fn declare(
        &mut self,
        namespace: &str,
        method: &str,
        declaration: crate::guard::access::Declaration,
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
            .unwrap_or_else(|| crate::guard::access::Declaration::read(namespace.to_owned()));
        let request = crate::guard::access::Request {
            peer: call.peer.clone(),
            method: format!("{namespace}.{method}"),
            access: declaration.access,
            resources: concrete_resources(declaration.resources, &call.args),
        };
        match policy.decide(&request) {
            crate::guard::access::Decision::Allow => Ok(()),
            crate::guard::access::Decision::Refuse { reason } => Err(RemoteError::Refused {
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

    /// Registers a method whose result is a series of items on the stream mux.
    pub fn register_stream<F>(&mut self, namespace: &str, method: &str, handler: F)
    where
        F: Fn(StreamOpen) -> BoxFut<()> + Send + Sync + 'static,
    {
        self.streams
            .insert((namespace.to_owned(), method.to_owned()), Arc::new(handler));
    }

    /// Whether `namespace.method` was registered with [`Self::register_stream`].
    #[must_use]
    pub fn is_streaming(&self, namespace: &str, method: &str) -> bool {
        self.streams
            .contains_key(&(namespace.to_owned(), method.to_owned()))
    }

    /// Returns the streaming handler after the same access check unary dispatch uses.
    ///
    /// # Errors
    ///
    /// [`RemoteError::UnknownMethod`] when the name is not a stream, and [`RemoteError::Refused`]
    /// when the mounted policy refuses the caller.
    pub fn stream_method(
        &self,
        namespace: &str,
        method: &str,
        call: &RemoteCall,
    ) -> Result<RemoteStreamMethod, RemoteError> {
        self.authorise(namespace, method, call)?;
        let key = (namespace.to_owned(), method.to_owned());
        self.streams
            .get(&key)
            .cloned()
            .ok_or(RemoteError::UnknownMethod {
                namespace: namespace.to_owned(),
                method: method.to_owned(),
            })
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
        let mut names: Vec<(String, String)> = self
            .methods
            .keys()
            .cloned()
            .chain(self.streams.keys().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Number of registered methods, including streams.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names().len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.methods.is_empty() && self.streams.is_empty()
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

        if self.streams.contains_key(&key) {
            return Err(RemoteError::Failed(format!(
                "{namespace}.{method} is a stream and must be opened on the mux"
            )));
        }
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

/// Fills declared resource ids from the call arguments.
///
/// Declarations name a kind. A session or workspace call usually names the identity inside `args`,
/// and a policy that only sees the empty id cannot tell one session from another.
fn concrete_resources(
    resources: Vec<crate::guard::access::Resource>,
    args: &Value,
) -> Vec<crate::guard::access::Resource> {
    let session_id = args
        .get("sessionId")
        .and_then(Value::as_str)
        .or_else(|| {
            args.get("address")
                .and_then(|address| address.get("sessionId"))
                .and_then(Value::as_str)
        })
        .filter(|id| !id.is_empty());
    let workspace_id = args
        .get("workspaceId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    resources
        .into_iter()
        .map(|mut resource| {
            if resource.id.is_empty() {
                if resource.kind == "session" {
                    if let Some(id) = session_id {
                        resource.id = id.to_owned();
                    }
                } else if resource.kind == "workspace" {
                    if let Some(id) = workspace_id {
                        resource.id = id.to_owned();
                    }
                }
            }
            resource
        })
        .collect()
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

/// Durable fields a session list and follow snapshot share.
///
/// Stored beside the log rather than inside it: the event log is the conversation, and these
/// fields describe the file. Optional values stay absent on the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeaderMeta {
    /// Creation time in milliseconds since the epoch.
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    /// Working directory, when the session was placed in one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Session this one was forked from.
    #[serde(
        rename = "parentSession",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_session: Option<String>,
    /// Who created the session, when it was not a direct client session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Preset recorded for the session.
    #[serde(
        rename = "agentPreset",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_preset: Option<String>,
    /// Whether the log starts with events inherited from a parent.
    #[serde(rename = "isSeeded", default)]
    pub is_seeded: bool,
    /// Distance from the root session.
    #[serde(rename = "delegationDepth", default)]
    pub delegation_depth: u64,
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
    /// How much of the log could be read.
    ///
    /// `Empty` and `TornTail` both produce no events, and before this field they were the same thing to a caller: a
    /// session that was created and never used, and a session whose last write was interrupted. The second is repairable
    /// and the first needs nothing, so a reader had no way to tell which it was looking at.
    pub integrity: crate::session::log_integrity::Integrity,
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

    /// Creates a session with immutable metadata in the log header.
    ///
    /// Lightweight catalogs keep the default; persistent catalogs override it so metadata and
    /// events use one durable format.
    fn create_with_header(
        &self,
        id: &str,
        _header: SessionHeaderMeta,
    ) -> BoxFut<Result<SessionRecord, RemoteError>> {
        self.create(id)
    }

    /// Returns the persisted session header, when the catalog stores one.
    ///
    /// The default keeps lightweight in-memory catalogs source-compatible. The directory-backed
    /// catalog reads the header from the log, so a restart restores the fields a list and a
    /// follow snapshot render.
    fn session_header(&self, _id: &str) -> Option<SessionHeaderMeta> {
        None
    }

    /// Persists the session header selected when a session is created or forked.
    fn store_session_header(
        &self,
        _id: &str,
        _header: &SessionHeaderMeta,
    ) -> Result<(), RemoteError> {
        Ok(())
    }

    /// Returns the persisted working directory for a session, when the catalog stores one.
    ///
    /// The default keeps lightweight in-memory catalogs source-compatible; the directory-backed
    /// implementation overrides it so a restart does not lose the workspace shown by the client.
    fn working_directory(&self, id: &str) -> Option<String> {
        self.session_header(id).and_then(|header| header.cwd)
    }

    /// Returns the durable creation time for a session, when the catalog records one.
    fn created_at(&self, id: &str) -> Option<u64> {
        self.session_header(id).map(|header| header.created_at)
    }

    /// Wakes a session stream after the catalog may have changed.
    ///
    /// Directory catalogs watch the log directory. A catalog without a native watcher returns
    /// `None`, and the stream keeps its bounded poll.
    fn changes(&self) -> Option<std::sync::Arc<tokio::sync::Notify>> {
        None
    }

    /// Persists the working directory selected when a session is created.
    ///
    /// Catalogs that do not persist placement may keep the default no-op because the caller still
    /// maintains its live placement index for the lifetime of the process.
    fn set_working_directory(&self, _id: &str, _cwd: &str) -> Result<(), RemoteError> {
        Ok(())
    }

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

    /// The title a session was given, when it has one.
    ///
    /// `rename` writes a title and nothing could read it back, so a caller that wanted the existing label had to either
    /// guess from the log or reach into the directory layout. Guessing produced a copy titled with its first message
    /// when the session had been renamed, which is accurate about the conversation and wrong about the label.
    ///
    /// A default returning `None` rather than an error: a catalog with no titles has no title, and that is an answer.
    fn title(&self, _id: &str) -> BoxFut<Option<String>> {
        Box::pin(async { None })
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
    /// Workspace the session joins. Exclusive with `cwd`.
    #[serde(
        rename = "workspaceId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_id: Option<String>,
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
    /// Wakes session streams after a log in `root` changes.
    changes: std::sync::Arc<tokio::sync::Notify>,
    /// The directory watch. Started on the first stream that asks for it.
    watch: std::sync::Mutex<Option<crate::workspace::native_watch::NativeWatcher>>,
}

impl DirectoryCatalog {
    /// Creates a catalog over `root`.
    #[must_use]
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            root: root.into(),
            changes: std::sync::Arc::new(tokio::sync::Notify::new()),
            watch: std::sync::Mutex::new(None),
        }
    }
}

/// The log path for `id`, refusing an id that would address another directory.
///
/// `SessionLayout` has enforced this since it was written and the catalog never used it: every method joined the root
/// directly, so `create("../escaped")` wrote outside the sessions directory and `read("../secret")` read a file that was
/// never a session. The check belongs on the path because that is the only place it cannot be forgotten.
fn log_path(root: &std::path::Path, id: &str) -> Result<std::path::PathBuf, RemoteError> {
    girder_core::session::layout::validate_id(id).map_err(|error| {
        RemoteError::InvalidArguments {
            namespace: "session".to_owned(),
            method: "id".to_owned(),
            message: error.to_string(),
        }
    })?;
    Ok(root.join(format!("{id}.jsonl.zst")))
}

impl SessionCatalog for DirectoryCatalog {
    fn changes(&self) -> Option<std::sync::Arc<tokio::sync::Notify>> {
        let mut slot = self
            .watch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = crate::workspace::native_watch::NativeWatcher::watch(
                &self.root.to_string_lossy(),
                true,
                std::sync::Arc::clone(&self.changes),
            );
        }
        slot.is_some().then(|| std::sync::Arc::clone(&self.changes))
    }

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
                // `strip_suffix`, not `trim_end_matches`: the latter strips repeatedly, so a session created as
                // `notes.jsonl` listed under `notes` and the row the UI rendered could not be opened.
                let id = id.strip_suffix(".jsonl").unwrap_or(id);
                // An archive is named `{id}.archive-{stamp}.jsonl.zst`, which satisfies the extension filter, so a listing
                // returned it as a session -- openable, countable, a phantom. It is discoverable as an archive; it is not
                // a conversation.
                if id.contains(".archive-") {
                    continue;
                }
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
                // A listing does not read the events, so it cannot say more than whether the file has bytes. That is
                // still the distinction that matters: an empty file is a created session and a non-empty one is a session
                // with content, whatever its state.
                let integrity = match std::fs::metadata(&path) {
                    Ok(meta) if meta.len() == 0 => crate::session::log_integrity::Integrity::Empty,
                    Ok(meta) => {
                        crate::session::log_integrity::Integrity::Unread { bytes: meta.len() }
                    }
                    Err(_) => crate::session::log_integrity::Integrity::Empty,
                };
                records.push(SessionRecord {
                    id: id.to_owned(),
                    updated_at,
                    blank,
                    integrity,
                    events: Vec::new(),
                });
            }
            // Newest first: a list view shows recent activity before old.
            records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
            records
        })
    }

    fn read(&self, id: &str) -> BoxFut<Option<SessionRecord>> {
        // An unusable id is "no such session" rather than an error: the signature has no way to say why, and a caller
        // asking for `../etc/passwd` should get the same answer as one asking for a session that does not exist.
        let Ok(path) = log_path(&self.root, id) else {
            return Box::pin(async { None });
        };
        let id = id.to_owned();
        Box::pin(async move { read_record(&path, id) })
    }

    fn session_header(&self, id: &str) -> Option<SessionHeaderMeta> {
        let log = log_path(&self.root, id).ok()?;
        if let Ok(Some(value)) = crate::session::read_session_header(&log) {
            if let Ok(header) = serde_json::from_value(value) {
                return Some(header);
            }
        }
        let raw = std::fs::read_to_string(self.root.join(format!("{id}.header"))).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn store_session_header(
        &self,
        id: &str,
        header: &SessionHeaderMeta,
    ) -> Result<(), RemoteError> {
        let path = log_path(&self.root, id)?;
        if path.is_file() {
            let existing = crate::session::read_session_header(&path).ok().flatten();
            let document = durable_header(existing.as_ref(), header)?;
            crate::session::SessionStore::new(&path)
                .rewrite_header(&document)
                .map_err(|error| {
                    RemoteError::Failed(format!("cannot update session header: {error}"))
                })?;
        }
        let raw = serde_json::to_string(header).map_err(|error| {
            RemoteError::Failed(format!("cannot encode session header: {error}"))
        })?;
        std::fs::write(self.root.join(format!("{id}.header")), raw)
            .map_err(|error| RemoteError::Failed(format!("cannot persist session header: {error}")))
    }

    fn set_working_directory(&self, id: &str, cwd: &str) -> Result<(), RemoteError> {
        let mut header = self.session_header(id).unwrap_or_default();
        header.cwd = Some(cwd.to_owned());
        self.store_session_header(id, &header)
    }

    /// Renames a session by writing a companion file beside its log.
    ///
    /// A companion rather than an event: a rename is not part of the conversation, and appending one to
    /// the log would make every history read carry a record the model never saw. The file is small
    /// enough that reading it per row costs nothing next to decoding a log.
    fn title(&self, id: &str) -> BoxFut<Option<String>> {
        let path = self.root.join(format!("{id}.title"));
        let log = log_path(&self.root, id).ok();
        let id = id.to_owned();
        Box::pin(async move {
            // A sidecar is a name a person stored. It wins over the fallback written into the log,
            // so a rename is what the list shows after it happens.
            if let Ok(raw) = tokio::fs::read_to_string(&path).await {
                let title = raw.trim().to_owned();
                if !title.is_empty() {
                    return Some(title);
                }
            }
            let Some(log) = log else {
                let _ = id;
                return None;
            };
            let Ok(events) = crate::session::SessionStore::new(log).read_all() else {
                return None;
            };
            events.into_iter().rev().find_map(|event| match event {
                crate::session::SessionEvent::SessionTitle { title, .. }
                    if !title.trim().is_empty() =>
                {
                    Some(title)
                }
                _ => None,
            })
        })
    }

    fn rename(&self, id: &str, title: &str) -> BoxFut<Result<String, RemoteError>> {
        let id = id.to_owned();
        let title = title.trim().to_owned();
        let log = match log_path(&self.root, &id) {
            Ok(path) => path,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let path = self.root.join(format!("{id}.title"));
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
        let source = match log_path(&self.root, id) {
            Ok(path) => path,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let id = id.to_owned();
        let root = self.root.clone();
        let source_header = self.session_header(&id);
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
            let mut header = source_header.unwrap_or_default();
            header.created_at = crate::now_millis();
            header.parent_session = Some(id.clone());
            let mut header_value = serde_json::to_value(&header)
                .map_err(|error| RemoteError::Failed(error.to_string()))?;
            if let Some(object) = header_value.as_object_mut() {
                object.insert(
                    "type".to_owned(),
                    serde_json::Value::String("session".to_owned()),
                );
                object.insert("version".to_owned(), serde_json::Value::from(4_u64));
            }
            let store = crate::session::SessionStore::new(&target);
            store
                .touch_with_header(&header_value)
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
        let log = match log_path(&self.root, id) {
            Ok(path) => path,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        let title = self.root.join(format!("{id}.title"));
        let header = self.root.join(format!("{id}.header"));
        let cwd = self.root.join(format!("{id}.cwd"));
        let created = self.root.join(format!("{id}.created"));
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
            let _ = std::fs::remove_file(&header);
            let _ = std::fs::remove_file(&cwd);
            let _ = std::fs::remove_file(&created);
            Ok(removed)
        })
    }

    fn open(&self, id: &str) -> Option<Arc<dyn crate::session::Session>> {
        // An id that names something outside the directory opens nothing, for the same reason `read` answers `None`.
        let path = log_path(&self.root, id).ok()?;
        // Only an existing session opens: a prompt to an unknown id must fail, not mint a log.
        path.is_file().then(|| {
            Arc::new(crate::session::SessionStore::new(path)) as Arc<dyn crate::session::Session>
        })
    }

    fn create(&self, id: &str) -> BoxFut<Result<SessionRecord, RemoteError>> {
        self.create_with_header(
            id,
            SessionHeaderMeta {
                created_at: crate::now_millis(),
                ..SessionHeaderMeta::default()
            },
        )
    }

    fn create_with_header(
        &self,
        id: &str,
        header: SessionHeaderMeta,
    ) -> BoxFut<Result<SessionRecord, RemoteError>> {
        let path = match log_path(&self.root, id) {
            Ok(path) => path,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
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
            let mut header = serde_json::to_value(&header).map_err(|error| {
                RemoteError::Failed(format!("cannot encode session header: {error}"))
            })?;
            if let Some(object) = header.as_object_mut() {
                object.insert(
                    "type".to_owned(),
                    serde_json::Value::String("session".to_owned()),
                );
                object.insert("version".to_owned(), serde_json::Value::from(4_u64));
            }
            crate::session::SessionStore::new(&path)
                .touch_with_header(&header)
                .map_err(|error| RemoteError::Failed(format!("cannot create session: {error}")))?;

            read_record(&path, id).ok_or_else(|| {
                RemoteError::Failed("the session disappeared while being created".to_owned())
            })
        })
    }
}

/// The log header a reader will see after a metadata update.
///
/// Fields already stored in the log stay unless this update names them. `type` and `version` are
/// put back even when the sidecar shape omitted them, because that is what makes the first line a
/// header rather than an event.
fn durable_header(
    existing: Option<&serde_json::Value>,
    header: &SessionHeaderMeta,
) -> Result<serde_json::Value, RemoteError> {
    let mut object = existing
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    let patch = serde_json::to_value(header)
        .map_err(|error| RemoteError::Failed(format!("cannot encode session header: {error}")))?;
    if let Some(patch) = patch.as_object() {
        for (key, value) in patch {
            object.insert(key.clone(), value.clone());
        }
    }
    object.insert(
        "type".to_owned(),
        serde_json::Value::String("session".to_owned()),
    );
    object.insert("version".to_owned(), serde_json::Value::from(4_u64));
    Ok(serde_json::Value::Object(object))
}

/// Reads one session log from disk, or `None` when the file is absent.
///
/// A free function rather than a method so the returned future owns everything it touches: a method
/// taking `&self` would tie the future to the borrow, and the trait's `BoxFut` is `'static`.
fn read_record(path: &std::path::Path, id: String) -> Option<SessionRecord> {
    if !path.is_file() {
        return None;
    }
    // Not `unwrap_or_default`: that turned a damaged log into `events: []`, which is indistinguishable from a session
    // created and never used, and `session.state` then reported `events: 0` with no error. The integrity check keeps the
    // difference -- a torn tail is readable and repairable, a broken middle is neither -- and the record carries it.
    let bytes = std::fs::read(path).unwrap_or_default();
    let outcome = crate::session::read_session_file(path)
        .map(|events| events.len())
        .map_err(|error| error.to_string());
    let integrity = crate::session::log_integrity::inspect(
        &bytes,
        outcome.as_ref().map(|count| *count).map_err(String::as_str),
    );
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
        integrity,
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
/// Methods this build declares but does not implement.
///
/// Empty: `stream` was the last one and it now answers with frames. The constant stays because the declaration machinery
/// is what turns a missing method into a named `NOT_IMPLEMENTED` rather than an `UNKNOWN_METHOD` that reads as a client
/// bug, and the next gap will want it.
const UNIMPLEMENTED_SESSION_METHODS: &[&str] = &[];

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
    runtime: Option<&Arc<crate::core::runtime::AgentRuntime>>,
) {
    let places = Arc::new(std::sync::Mutex::new(
        crate::session::workspace::Index::new(),
    ));
    register_list(registry, catalog, runtime, &places);
    register_create(registry, catalog, &places, None);
    register_history(registry, catalog);
    register_follow(registry, catalog, &places);
    // The version handshake. A client that speaks an older shape asks once and learns it is old, instead of discovering it
    // from an argument error it cannot interpret -- and a client newer than this build is told to upgrade rather than
    // silently degraded, which is the one failure a version check exists to prevent.
    registry.register("session", "version", move |_call| {
        Box::pin(async move {
            Ok(serde_json::json!({
                "version": crate::api::version::Version::CURRENT.as_u32(),
            }))
        })
    });
    register_stream(registry, catalog);
    crate::session::export_rpc::register_session_export_method(registry, catalog);
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
    //
    // The registry is populated before it is registered. An empty registry behind a working `command.list` is a surface
    // that answers "no commands" to every client, which is indistinguishable from a broken one.
    let commands = Arc::new(crate::interaction::commands::Registry::new());
    crate::interaction::builtin_commands::register_builtin_commands(&commands);
    register_command_methods(registry, &commands);
}

/// Registers `session.list`.
fn register_list(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    runtime: Option<&Arc<crate::core::runtime::AgentRuntime>>,
    places: &Arc<std::sync::Mutex<crate::session::workspace::Index>>,
) {
    let shared: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    let runtime = runtime.cloned();
    let places = Arc::clone(places);
    registry.register("session", "list", move |call| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&shared);
        let runtime = runtime.clone();
        let places = Arc::clone(&places);
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
                    // `from_record`, not `from_events`: `list` deliberately leaves `events` empty, so reading them here
                    // reported every session as blank -- a conversation with seven messages listed as unused. The
                    // constructor written for this case existed and had no caller.
                    let mut summary = SessionSummary::from_record(record);
                    // Girder attaches an agent for the turn that is in flight. Upstream separates an idle
                    // attached agent from a running one; this build has the agent only while the turn runs.
                    if runtime
                        .as_ref()
                        .is_some_and(|runtime| runtime.is_running(&record.id))
                    {
                        summary.running = true;
                        summary.agent_available = true;
                    }
                    if let Some(header) = catalog.session_header(&record.id) {
                        summary.cwd = header.cwd.clone();
                        summary.parent_session_id = header.parent_session.clone();
                        summary.origin = header.origin.clone();
                    }
                    summary.cwd = summary.cwd.or_else(|| recorded_cwd(&places, &record.id));
                    summary
                })
                .collect();
            serde_json::to_value(SessionListValue { items })
                .map_err(|error| RemoteError::Failed(error.to_string()))
        })
    });
}

/// Registers `session.create`.
fn register_create(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    places: &Arc<std::sync::Mutex<crate::session::workspace::Index>>,
    workspaces: Option<&Arc<crate::api::config::WorkspaceRegistry>>,
) {
    let shared: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    let places = Arc::clone(places);
    let workspaces = workspaces.cloned();
    registry.register("session", "create", move |call| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&shared);
        let places = Arc::clone(&places);
        let workspaces = workspaces.clone();
        Box::pin(async move {
            let request: SessionCreateRequest = parse_args("session", "create", call.args)?;
            let place = create_place(&request, workspaces.as_ref())?;
            // An id is either supplied or minted. A minted id carries the millisecond so two
            // creates in the same second do not collide.
            let id = request
                .session_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(mint_session_id);

            let header = SessionHeaderMeta {
                created_at: crate::now_millis(),
                cwd: place.clone(),
                agent_preset: request.agent_preset.clone(),
                ..SessionHeaderMeta::default()
            };
            catalog.create_with_header(&id, header).await?;
            if let Some(cwd) = place {
                remember_place(&places, &id, &cwd);
                if let Some(workspace_id) =
                    request.workspace_id.as_deref().filter(|id| !id.is_empty())
                {
                    if let Some(workspaces) = &workspaces {
                        let _ = workspaces.attach_session(workspace_id, &id, crate::now_millis());
                    }
                }
            }
            serde_json::to_value(SessionCreateValue {
                session_id: id,
                agent_preset: request.agent_preset,
            })
            .map_err(|error| RemoteError::Failed(error.to_string()))
        })
    });
}

/// Rebinds `session.list`, `session.create`, and `session.follow` to one place index.
///
/// The first registration keeps its own index. A server that also has a workspace registry calls
/// this so a created session, its list row, and its follow header name the same directory.
pub fn bind_session_places(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    places: &Arc<std::sync::Mutex<crate::session::workspace::Index>>,
    workspaces: Option<&Arc<crate::api::config::WorkspaceRegistry>>,
    runtime: Option<&Arc<crate::core::runtime::AgentRuntime>>,
) {
    register_list(registry, catalog, runtime, places);
    register_create(registry, catalog, places, workspaces);
    register_follow(registry, catalog, places);
}

fn create_invalid(message: impl Into<String>) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: "session".to_owned(),
        method: "create".to_owned(),
        message: message.into(),
    }
}

fn create_place(
    request: &SessionCreateRequest,
    workspaces: Option<&Arc<crate::api::config::WorkspaceRegistry>>,
) -> Result<Option<String>, RemoteError> {
    let workspace_id = request
        .workspace_id
        .as_deref()
        .filter(|id| !id.trim().is_empty());
    let cwd = request.cwd.as_deref().filter(|cwd| !cwd.trim().is_empty());
    if workspace_id.is_some() && cwd.is_some() {
        return Err(create_invalid(
            "session.create accepts workspaceId or cwd, not both",
        ));
    }
    if let Some(id) = workspace_id {
        let Some(workspaces) = workspaces else {
            return Err(RemoteError::Failed(format!("workspace \"{id}\" not found")));
        };
        let Some(workspace) = workspaces.get(id) else {
            return Err(RemoteError::Failed(format!("workspace \"{id}\" not found")));
        };
        let path = workspace.path.to_string_lossy().into_owned();
        crate::session::workspace::WorkspaceId::from_path(&path)
            .map_err(|error| create_invalid(error.to_string()))?;
        return Ok(Some(path));
    }
    if let Some(cwd) = cwd {
        crate::session::workspace::WorkspaceId::from_path(cwd)
            .map_err(|error| create_invalid(error.to_string()))?;
        return Ok(Some(cwd.to_owned()));
    }
    Ok(None)
}

fn remember_place(
    places: &std::sync::Mutex<crate::session::workspace::Index>,
    session_id: &str,
    cwd: &str,
) {
    let Ok(workspace) = crate::session::workspace::WorkspaceId::from_path(cwd) else {
        return;
    };
    places
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .associate(crate::session::workspace::Association {
            session: session_id.to_owned(),
            workspace,
            at_ms: crate::now_millis(),
        });
}

fn recorded_cwd(
    places: &std::sync::Mutex<crate::session::workspace::Index>,
    session_id: &str,
) -> Option<String> {
    places
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workspace_of(session_id)
        .map(|workspace| workspace.as_str().to_owned())
}

/// Mints a session id from the current time.
fn mint_session_id() -> String {
    // A millisecond is not a unit of uniqueness: two conversations opened in the same one collided, and `create` is
    // deliberately idempotent per id, so the second was silently merged into the first -- two chats in one log, no error
    // anywhere. The counter is what makes two creates in one millisecond two sessions.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The timestamp first so ids sort by creation time, the counter after so they stay distinct. Zero-padded to a fixed
    // width because a sortable id has to be lexically sortable too.
    format!("session-{now}-{seq:06}")
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

/// Registers `session.follow` as the live stream the Web client opens.
///
/// The first frame is the opening snapshot. Later frames are durable events that land after that
/// cursor. The stream stays open until the client cancels it.
/// Registers `session.stream`, which answers with frames rather than one value.
///
/// The transport has no stream mode, so this is a bounded batch of frames the client can ask for again from the last seq
/// it saw. That is the honest shape: a method that claimed to push and instead returned one array would be a different
/// wire contract with the same name, which is what this build refused to do while it was unimplemented.
///
/// The difference from `follow` is the unit. `follow` returns events; this returns frames tagged with the kind a client
/// dispatches on, and it reports why the stream ended so a client knows whether to ask again.
fn register_stream(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    let catalog: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    registry.register("session", "stream", move |call| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&catalog);
        Box::pin(async move {
            let session_id = call
                .args
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "stream".to_owned(),
                    message: "`sessionId` is required".to_owned(),
                })?
                .to_owned();
            // Zero means "from the beginning", matching `follow` and `page`: a client that omits it and one that sends
            // zero mean the same thing.
            let from = call.args.get("fromSeq").and_then(Value::as_u64).unwrap_or(0);
            let limit = usize::try_from(
                call.args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(FOLLOW_LIMIT as u64),
            )
            .unwrap_or(FOLLOW_LIMIT)
            .max(1);

            let Some(record) = catalog.read(&session_id).await else {
                return Err(RemoteError::Failed(format!(
                    "no session `{session_id}` is known to this deployment"
                )));
            };

            let mut stream = crate::session::stream::SessionStream::new(record.events, from);
            let mut frames = Vec::new();
            while frames.len() < limit {
                let Some(frame) = stream.next_frame() else {
                    break;
                };
                frames.push(serde_json::json!({
                    "seq": frame.seq,
                    "kind": frame.kind,
                    "payload": frame.payload,
                }));
            }
            let next = stream.emitted().saturating_add(from);
            Ok(serde_json::json!({
                "frames": frames,
                "nextSeq": next,
                "done": matches!(stream.end_reason(), Some(crate::session::stream::EndReason::Exhausted)),
            }))
        })
    });
}

fn register_follow(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    places: &Arc<std::sync::Mutex<crate::session::workspace::Index>>,
) {
    let catalog: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    let places = Arc::clone(places);
    let projections = crate::session::projection_setup::assemble()
        .expect("built-in session projections have unique keys");
    registry.register_stream("session", "follow", move |open| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&catalog);
        let places = Arc::clone(&places);
        let projections = Arc::clone(&projections);
        Box::pin(async move {
            let Some(session_id) = follow_session_id(&open.call.args) else {
                let _ = open.fail(RemoteError::InvalidArguments {
                    namespace: "session".to_owned(),
                    method: "follow".to_owned(),
                    message: "`address.sessionId` is required".to_owned(),
                });
                return;
            };
            let max_messages = open
                .call
                .args
                .get("maxMessages")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .filter(|value| *value > 0);
            let mut cursor = 0_u64;
            let mut opened = false;
            loop {
                if open.cancel.is_cancelled() {
                    return;
                }
                if let Some(record) = catalog.read(&session_id).await {
                    if !opened {
                        let page = page_of(
                            &record.events,
                            &SessionPageRequest {
                                address: SessionAddress {
                                    kind: "session".to_owned(),
                                    session_id: session_id.clone(),
                                },
                                max_messages,
                                ..SessionPageRequest::default()
                            },
                        );
                        // `session.follow` uses the upstream SessionSeq space: the first event
                        // is seq 0 and an empty log is cursor -1. `page.cursor` is the local
                        // history-page count, so keep the next physical offset separately.
                        cursor = u64::try_from(record.events.len()).unwrap_or(u64::MAX);
                        let wire_cursor = record
                            .events
                            .len()
                            .checked_sub(1)
                            .and_then(|index| u64::try_from(index).ok())
                            .map_or(-1_i64, |seq| i64::try_from(seq).unwrap_or(i64::MAX));
                        let records = page
                            .records
                            .iter()
                            .enumerate()
                            .filter_map(|(index, record)| {
                                let seq = page
                                    .cursor
                                    .saturating_sub(page.records.len() as u64)
                                    .saturating_add(index as u64);
                                session_wire_record(&record.event, seq)
                            })
                            .collect::<Vec<_>>();
                        let projected = projections.project(&record.events);
                        let stored = catalog.session_header(&session_id);
                        let mut header = serde_json::json!({
                            "version": 4,
                            "id": session_id,
                            "createdAt": stored
                                .as_ref()
                                .map(|header| header.created_at)
                                .unwrap_or(record.updated_at),
                            "isSeeded": stored.as_ref().is_some_and(|header| header.is_seeded),
                            "delegationDepth": stored
                                .as_ref()
                                .map(|header| header.delegation_depth)
                                .unwrap_or(0),
                        });
                        if let Some(cwd) = stored
                            .as_ref()
                            .and_then(|header| header.cwd.clone())
                            .or_else(|| recorded_cwd(&places, &session_id))
                        {
                            header["cwd"] = serde_json::Value::String(cwd);
                        }
                        if let Some(parent) = stored
                            .as_ref()
                            .and_then(|header| header.parent_session.clone())
                        {
                            header["parentSession"] = serde_json::Value::String(parent);
                        }
                        if let Some(origin) =
                            stored.as_ref().and_then(|header| header.origin.clone())
                        {
                            header["origin"] = serde_json::Value::String(origin);
                        }
                        if let Some(preset) = stored
                            .as_ref()
                            .and_then(|header| header.agent_preset.clone())
                        {
                            header["agentPreset"] = serde_json::Value::String(preset);
                        }
                        if !open.push(serde_json::json!({
                            "type": "snapshot",
                            "header": header,
                            "cursor": wire_cursor,
                            "records": records,
                            "hasMore": page.has_more,
                            "projections": {
                                "asOfSeq": projected.as_of,
                                "values": projected.values,
                            },
                        })) {
                            return;
                        }
                        opened = true;
                    } else {
                        let len = u64::try_from(record.events.len()).unwrap_or(u64::MAX);
                        let start = usize::try_from(cursor).unwrap_or(usize::MAX);
                        for (offset, event) in record.events.iter().enumerate().skip(start) {
                            let seq = u64::try_from(offset).unwrap_or(u64::MAX);
                            let Some(frame) = session_wire_record(event, seq) else {
                                continue;
                            };
                            if !open.push(frame) {
                                return;
                            }
                        }
                        cursor = len;
                    }
                } else if !opened {
                    if !open.push(serde_json::json!({
                        "type": "snapshot",
                        "header": {
                            "version": 4,
                            "id": session_id,
                            "createdAt": 0,
                            "isSeeded": false,
                            "delegationDepth": 0,
                        },
                        "cursor": -1,
                        "records": [],
                        "hasMore": false,
                        "projections": {},
                    })) {
                        return;
                    }
                    opened = true;
                }
                tokio::select! {
                    () = open.cancel.cancelled() => return,
                    incoming = open.uplink.recv() => {
                        if !matches!(incoming, Ok(Some(_))) {
                            return;
                        }
                    }
                    () = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
                }
            }
        })
    });
}

fn follow_session_id(args: &Value) -> Option<String> {
    args.get("address")
        .and_then(|address| address.get("sessionId"))
        .and_then(Value::as_str)
        .or_else(|| args.get("sessionId").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// One `{ type: "event", event }` record whose event is a `SessionWireEvent`.
///
/// Surface events carry `surfaceOp: "append"`. `user/message` puts the message itself in `data`;
/// every other type keeps its typed fields there. The sequence is the upstream zero-based
/// `SessionSeq`; event time is currently unavailable in Girder's compact event enum and remains
/// the stable placeholder `0`. Returns `None` when the event cannot be serialized.
pub fn session_wire_record(event: &SessionEvent, seq: u64) -> Option<Value> {
    let mut value = serde_json::to_value(event).ok()?;
    let object = value.as_object_mut()?;
    let event_type = object.remove("type")?.as_str()?.to_owned();
    // `user/message` carries the message itself as `data`. The other surface events keep their
    // typed fields, including `message`, inside `data`.
    let data = if event_type == "user/message" {
        object
            .remove("message")
            .unwrap_or_else(|| Value::Object(std::mem::take(object)))
    } else {
        Value::Object(std::mem::take(object))
    };
    let mut wire = serde_json::Map::new();
    wire.insert("type".to_owned(), Value::String(event_type.clone()));
    wire.insert("seq".to_owned(), Value::from(seq));
    wire.insert("time".to_owned(), Value::from(0));
    wire.insert("data".to_owned(), data);
    if matches!(
        event_type.as_str(),
        "system/message"
            | "developer/message"
            | "user/message"
            | "assistant/message"
            | "tool/result"
    ) {
        wire.insert("surfaceOp".to_owned(), Value::String("append".to_owned()));
    }
    Some(serde_json::json!({
        "type": "event",
        "event": Value::Object(wire),
    }))
}

/// How many events one `session.follow` poll returns.
///
/// 64: a poll that returned thousands would make one frame enormous and the next poll empty, which reads as a stall.
pub const FOLLOW_LIMIT: usize = 64;

/// Registers `session.prompt`.
fn register_prompt(registry: &mut RemoteRegistry) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::<
        String,
        std::sync::Arc<PromptReceipt>,
    >::new()));
    registry.register("session", "prompt", move |call| {
        let seen = std::sync::Arc::clone(&seen);
        Box::pin(async move {
            let request = admit_prompt(&call.args)?;
            let Some((key, receipt, owner)) = begin_prompt(&seen, &request) else {
                return prompt_value(1);
            };
            if !owner {
                return prompt_result_value(wait_prompt(receipt).await);
            }
            // This build has no agent registry to attach to, so the prompt is accepted and
            // acknowledged rather than run. Returning an error would be worse: a client would retry
            // a call the harness has genuinely received.
            finish_prompt(&seen, &key, &receipt, Ok(1), true);
            prompt_result_value(Ok(1))
        })
    });
}

/// One prompt after the browser `content` parts, or the local `text` field, have been read.
struct AdmittedPrompt {
    session_id: String,
    text: String,
    request_id: Option<String>,
    delivery: PromptDelivery,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptDelivery {
    /// `{ sessionId, text }` with no `mode`. A busy session refuses this.
    Legacy,
    /// Wait for the running turn, then answer.
    Queue,
    /// Join the running turn at the next model step.
    Steer,
}

fn prompt_invalid(message: impl Into<String>) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: "session".to_owned(),
        method: "prompt".to_owned(),
        message: message.into(),
    }
}

fn prompt_value(turn: u64) -> Result<Value, RemoteError> {
    serde_json::to_value(SessionPromptValue {
        accepted: true,
        turn,
    })
    .map_err(|error| RemoteError::Failed(error.to_string()))
}

fn prompt_result_value(result: Result<u64, String>) -> Result<Value, RemoteError> {
    result.map_or_else(|error| Err(RemoteError::Failed(error)), prompt_value)
}

/// Reads a `session.prompt` payload.
///
/// The browser sends `{ requestId, sessionId, mode, content, clientTimeZone }`. `content` is a list
/// of text, image, and file parts. This build runs a single text prompt, so text parts are joined
/// and an image or file is refused instead of being dropped. `mode` is `queue` or `steer`. An idle
/// session starts a turn for either. A busy session queues `queue` for the next turn and inserts
/// `steer` at the next model step. A prompt without `mode` still refuses a busy session.
/// `clientTimeZone` is accepted and not stored: the message source has no time-zone field yet.
/// Callers that still send `{ sessionId, text }` keep working.
fn admit_prompt(args: &Value) -> Result<AdmittedPrompt, RemoteError> {
    let delivery = match args.get("mode") {
        None => PromptDelivery::Legacy,
        Some(Value::String(mode)) if mode == "queue" => PromptDelivery::Queue,
        Some(Value::String(mode)) if mode == "steer" => PromptDelivery::Steer,
        Some(Value::String(mode)) => {
            return Err(prompt_invalid(format!(
                "`mode` must be `queue` or `steer`, got `{mode}`"
            )));
        }
        Some(_) => return Err(prompt_invalid("`mode` must be `queue` or `steer`")),
    };
    let session_id = args
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| prompt_invalid("`sessionId` is required"))?
        .to_owned();
    if let Some(content) = args.get("content") {
        let parts = content
            .as_array()
            .ok_or_else(|| prompt_invalid("`content` must be an array of parts"))?;
        let mut texts = Vec::new();
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = part
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| prompt_invalid("a text part needs `text`"))?;
                    texts.push(text);
                }
                Some(other) => {
                    return Err(prompt_invalid(format!(
                        "part type `{other}` needs an attachment store this build does not have"
                    )));
                }
                None => return Err(prompt_invalid("a content part needs `type`")),
            }
        }
        let text = texts.join("\n");
        if text.trim().is_empty() {
            return Err(prompt_invalid("`text` must not be empty"));
        }
        let request_id = args
            .get("requestId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| prompt_invalid("`requestId` is required"))?;
        return Ok(AdmittedPrompt {
            session_id,
            text,
            request_id: Some(request_id),
            delivery,
        });
    }
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() {
        return Err(prompt_invalid("`text` must not be empty"));
    }
    let request_id = args
        .get("requestId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    Ok(AdmittedPrompt {
        session_id,
        text: text.to_owned(),
        request_id,
        delivery,
    })
}

fn prompt_key(request: &AdmittedPrompt) -> Option<String> {
    request
        .request_id
        .as_ref()
        .map(|id| format!("{}\u{0}{id}", request.session_id))
}

struct PromptReceipt {
    result: std::sync::Mutex<Option<Result<u64, String>>>,
    notify: tokio::sync::Notify,
}

fn begin_prompt(
    seen: &std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Arc<PromptReceipt>>>,
    request: &AdmittedPrompt,
) -> Option<(String, std::sync::Arc<PromptReceipt>, bool)> {
    let key = prompt_key(request)?;
    let mut seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(receipt) = seen.get(&key) {
        return Some((key, std::sync::Arc::clone(receipt), false));
    }
    let receipt = new_prompt_receipt();
    seen.insert(key.clone(), std::sync::Arc::clone(&receipt));
    Some((key, receipt, true))
}

fn new_prompt_receipt() -> std::sync::Arc<PromptReceipt> {
    std::sync::Arc::new(PromptReceipt {
        result: std::sync::Mutex::new(None),
        notify: tokio::sync::Notify::new(),
    })
}

async fn wait_prompt(receipt: std::sync::Arc<PromptReceipt>) -> Result<u64, String> {
    loop {
        let notified = receipt.notify.notified();
        if let Some(result) = receipt
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return result;
        }
        notified.await;
    }
}

fn finish_prompt(
    seen: &std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Arc<PromptReceipt>>>,
    key: &str,
    receipt: &std::sync::Arc<PromptReceipt>,
    result: Result<u64, String>,
    retain: bool,
) {
    let is_error = result.is_err();
    *receipt
        .result
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
    receipt.notify.notify_waiters();
    if is_error && !retain {
        let mut entries = seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries
            .get(key)
            .is_some_and(|current| std::sync::Arc::ptr_eq(current, receipt))
        {
            entries.remove(key);
        }
    }
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
    runtime: Arc<crate::core::runtime::AgentRuntime>,
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
    runtime: &Arc<crate::core::runtime::AgentRuntime>,
    routes: &Arc<crate::llm::providers::RouteRegistry>,
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
fn register_cancel(
    registry: &mut RemoteRegistry,
    runtime: &Arc<crate::core::runtime::AgentRuntime>,
) {
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
    runtime: &Arc<crate::core::runtime::AgentRuntime>,
) {
    let catalog: Arc<dyn SessionCatalog> = Arc::clone(catalog);
    let runtime = Arc::clone(runtime);
    let seen = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::<
        String,
        std::sync::Arc<PromptReceipt>,
    >::new()));
    registry.register("session", "prompt", move |call| {
        let catalog: Arc<dyn SessionCatalog> = Arc::clone(&catalog);
        let runtime = Arc::clone(&runtime);
        let seen = std::sync::Arc::clone(&seen);
        Box::pin(async move {
            let request = admit_prompt(&call.args)?;
            let (key, receipt, owner) = begin_prompt(&seen, &request)
                .unwrap_or_else(|| (String::new(), new_prompt_receipt(), true));
            if !owner {
                return prompt_result_value(wait_prompt(receipt).await);
            }

            // The session must exist: prompting an unknown id should fail rather than mint a log.
            let Some(session) = catalog.open(&request.session_id) else {
                let error = format!("no session named `{}`", request.session_id);
                finish_prompt(&seen, &key, &receipt, Err(error.clone()), false);
                return Err(RemoteError::Failed(error));
            };

            let result = match crate::core::runtime::AgentRuntime::prompt(
                &runtime,
                &request.session_id,
                &request.text,
                std::sync::Arc::clone(&session),
            )
            .await
            {
                Ok(turn) => Ok(turn),
                Err(crate::core::runtime::RuntimeError::Busy { .. })
                    if request.delivery != PromptDelivery::Legacy =>
                {
                    runtime
                        .enqueue(
                            &request.session_id,
                            &request.text,
                            request.delivery == PromptDelivery::Steer,
                        )
                        .ok_or_else(|| {
                            format!("session `{}` is already running a turn", request.session_id)
                        })
                }
                Err(error) => Err(error.to_string()),
            };
            finish_prompt(&seen, &key, &receipt, result.clone(), false);
            prompt_result_value(result)
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
    projections: &std::sync::Arc<crate::session::projection::ProjectionRegistry>,
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
        move |call: RemoteCall| {
            let jobs = std::sync::Arc::clone(&jobs);
            Box::pin(async move { Ok(job_list_value(&jobs, session_of(&call.args))) })
        }
    });
    let stream_jobs = std::sync::Arc::clone(&jobs);
    registry.register_stream("job", "list", move |open| {
        let jobs = std::sync::Arc::clone(&stream_jobs);
        let session_id = session_of(&open.call.args).map(str::to_owned);
        Box::pin(async move {
            let mut previous = serde_json::Value::Null;
            loop {
                let notified = jobs.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let rows = job_list_value(&jobs, session_id.as_deref());
                if rows != previous {
                    if !open.push(serde_json::json!({
                        "type": "rows",
                        "jobs": rows["jobs"],
                    })) {
                        return;
                    }
                    previous = rows;
                }
                tokio::select! {
                    () = open.cancel.cancelled() => return,
                    incoming = open.uplink.recv() => {
                        if !matches!(incoming, Ok(Some(_))) {
                            return;
                        }
                    }
                    () = notified => {}
                }
            }
        })
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
                let outcome = crate::api::job::kill_outcome(job.status);
                if job.status.is_live() {
                    let _ = jobs.request_stop(&job_id);
                }
                Ok(serde_json::json!({ "outcome": outcome }))
            })
        }
    });
}

fn session_of(args: &serde_json::Value) -> Option<&str> {
    args.get("sessionId")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
}

fn job_list_value(
    jobs: &std::sync::Arc<crate::jobs::JobRegistry>,
    session_id: Option<&str>,
) -> serde_json::Value {
    // The whole visible set, not a delta: a reconnecting client's first frame is already correct.
    // Unowned jobs are visible to every session. An owned job is visible only to its owner.
    let mut views = Vec::new();
    for id in jobs.ids() {
        if let Ok(job) = jobs.get(&id) {
            if !job.visible_to(session_id) {
                continue;
            }
            let view = crate::api::job::project(&job);
            views.push(serde_json::json!({
                "id": view.id,
                "kind": view.kind,
                "label": view.label,
                "status": view.status,
                "code": view.code,
            }));
        }
    }
    serde_json::json!({ "jobs": views })
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
        &Arc::new(crate::session::projection::ProjectionRegistry::new()),
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
    for method in crate::api::job::job_methods() {
        names.push(method.qualified());
    }
    for method in crate::api::session::session_methods() {
        names.push(method.qualified());
    }
    for method in crate::api::config::workspace_methods() {
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
    workspaces: &std::sync::Arc<crate::api::config::WorkspaceRegistry>,
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
            // An omitted title means "name it after what it says", which is what a client showing a list of untitled
            // sessions wants and what a person would do by hand. An explicit title always wins: the derived one is a
            // convenience, not an opinion about what the conversation is about.
            let title = if let Some(title) = call.args.get("title").and_then(Value::as_str) {
                title.to_owned()
            } else {
                {
                    let Some(record) = catalog.read(&session_id).await else {
                        return Err(RemoteError::InvalidArguments {
                            namespace: "session".to_owned(),
                            method: "rename".to_owned(),
                            message: format!("no session `{session_id}`"),
                        });
                    };
                    // Whether a model title is even appropriate is decided before deriving one.
                    // `schedule` is upstream's rule: a seeded session (one that started from an
                    // existing transcript) is never auto-titled, a subagent's session is not either,
                    // and a title the user set is never overwritten. Deriving a lexical title in those
                    // cases would put a machine's leading-words guess where a person's label belongs.
                    let count = record
                        .events
                        .iter()
                        .filter(|event| {
                            matches!(event, girder_core::SessionEvent::UserMessage { .. })
                        })
                        .count();
                    let verdict = girder_core::session::title_llm::schedule(
                        girder_core::session::title_llm::Automatic::FirstPrompt,
                        girder_core::session::title_llm::Envelope {
                            is_seeded: false,
                            is_subagent: false,
                        },
                        girder_core::session::title_llm::TitleState { count, owner: None },
                        // No provider is wired to title generation yet, so the answer is `false` and
                        // `schedule` falls through to `FallbackOnly` -- the lexical title this call
                        // produces. The decision is still asked rather than assumed: when a provider
                        // exists, this is the one place that changes.
                        false,
                    );
                    let Some(derived) = (verdict
                        != girder_core::session::title_llm::Schedule::Ignore)
                        .then(|| {
                            girder_core::derive_title(
                                &record.events,
                                girder_core::TitleSource::FirstPrompt,
                                // A list row shows a few dozen characters; a longer title would be cut by whatever renders it,
                                // and cutting here means the cut is the same everywhere.
                                60,
                            )
                        })
                        .flatten()
                    else {
                        return Err(RemoteError::InvalidArguments {
                            namespace: "session".to_owned(),
                            method: "rename".to_owned(),
                            message:
                                "no title given and the session has no message to name it after"
                                    .to_owned(),
                        });
                    };
                    derived
                }
            };

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
    routes: &Arc<crate::llm::providers::RouteRegistry>,
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
                            "maxOutput": model.max_tokens,
                            // What this model can do, so a client can disable an option instead of offering one
                            // that will fail. A selector that lists a tool-capable and a non-tool-capable model
                            // identically makes the user discover which is which by losing a turn.
                            "capabilities": {
                                "tools": model.supports_tools,
                                "vision": model.input_modalities.iter().any(|modality| {
                                    matches!(modality, crate::llm::providers::Modality::Image)
                                }),
                                "reasoning": model.supports_reasoning,
                            },
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
    routes: &Arc<crate::llm::providers::RouteRegistry>,
) {
    let routes = Arc::clone(routes);
    // `session.initializeDefaultModel`: choose a default when the deployment has none. Idempotent
    // against a configured deployment, and an error rather than a no-op when nothing can serve a
    // model -- both of which are upstream's behaviour and both of which a client depends on.
    let init_routes = Arc::clone(&routes);
    registry.register(
        "session",
        "initializeDefaultModel",
        move |_call: RemoteCall| {
            let routes = Arc::clone(&init_routes);
            Box::pin(async move {
                if crate::llm::default_model::is_configured(&routes) {
                    // Already configured: return without touching the operator's choice.
                    return Ok(serde_json::json!({ "initialized": false }));
                }
                let selection = crate::llm::default_model::choose_selection(
                    &routes,
                    crate::llm::default_model::PREFERRED_PROVIDER,
                )
                .map_err(|error| RemoteError::Failed(error.to_string()))?;
                Ok(serde_json::json!({
                    "initialized": true,
                    "selection": selection.to_wire(),
                }))
            })
        },
    );

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

            let selection = crate::core::agent::ModelSelection::new(provider, model);
            // Validated before it is recorded: a selection naming a route this deployment does not
            // mount would otherwise be accepted here and fail at the next request, far from the client
            // that sent it.
            crate::core::agent::validate(&selection, &routes).map_err(|error| {
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
pub fn register_attachment_method<S: crate::attachment::ImageShelf + 'static>(
    registry: &mut RemoteRegistry,
    store: &Arc<S>,
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

            let Some(image) = store.fetch(id) else {
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
    commands: &Arc<crate::interaction::commands::Registry>,
) {
    let catalog = Arc::clone(commands);
    registry.register("commands", "list", move |_call: RemoteCall| {
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
                                crate::interaction::commands::Input::None => "none",
                                crate::interaction::commands::Input::Text { .. } => "text",
                                crate::interaction::commands::Input::Files { .. } => "files",
                                crate::interaction::commands::Input::Choice { .. } => "choice",
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
    registry.register("commands", "execute", move |call: RemoteCall| {
        let run = Arc::clone(&run);
        Box::pin(async move {
            let name = call
                .args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: "commands".to_owned(),
                    method: "run".to_owned(),
                    message: "`name` is required".to_owned(),
                })?
                .to_owned();

            // The argument kind is derived from what the client sent rather than from the declaration: a client that
            // sent `text` to a command asking for a choice made a mistake the registry will name.
            let arguments = match call.args.get("arguments") {
                None | Some(serde_json::Value::Null) => {
                    crate::interaction::commands::Arguments::None
                }
                Some(serde_json::Value::String(text)) => {
                    crate::interaction::commands::Arguments::Text(text.clone())
                }
                Some(serde_json::Value::Array(values)) => {
                    crate::interaction::commands::Arguments::Files(
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                    )
                }
                Some(other) => {
                    return Err(RemoteError::InvalidArguments {
                        namespace: "commands".to_owned(),
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
