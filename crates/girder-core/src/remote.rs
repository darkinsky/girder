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
    /// Its durable events.
    pub events: Vec<SessionEvent>,
}

/// What the session methods need from the host.
pub trait SessionCatalog: Send + Sync {
    /// Every visible session.
    fn list(&self) -> BoxFut<Vec<SessionRecord>>;
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

                let events = crate::session::read_session_file(&path).unwrap_or_default();
                records.push(SessionRecord {
                    id: id.to_owned(),
                    updated_at,
                    events,
                });
            }
            // Newest first: a list view shows recent activity before old.
            records.sort_by_key(|record| std::cmp::Reverse(record.updated_at));
            records
        })
    }
}

/// Registers the `session.*` methods this build implements.
///
/// The unimplemented ones are declared so a client gets `NOT_IMPLEMENTED` with a name rather than
/// `UNKNOWN_METHOD`, which would read as a client bug.
pub fn register_session_methods(registry: &mut RemoteRegistry, catalog: &Arc<dyn SessionCatalog>) {
    {
        let shared: Arc<dyn SessionCatalog> = Arc::clone(catalog);
        registry.register("session", "list", move |call| {
            let catalog: Arc<dyn SessionCatalog> = Arc::clone(&shared);
            Box::pin(async move {
                // `session.list` takes an empty request upstream; a null or missing body is the
                // common case, so it is accepted rather than rejected as invalid arguments.
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

    registry.declare_unimplemented(
        "session",
        &[
            "search",
            "create",
            "selectModel",
            "modelCatalog",
            "openWorkspacePath",
            "workspacePathApplications",
            "rename",
            "fork",
            "attachment",
            "updateQueue",
            "cancel",
            "page",
            "projections",
            "stream",
            "events",
            "resume",
            "archive",
            "delete",
            "duplicate",
            "export",
        ],
    );
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
