//! Subagent providers: registering capability-bounded backends and selecting one by name.
//!
//! A subagent runs a *separate* agent to do one job and reports back. The interesting part is not the
//! delegation — [`crate::subagent::delegate`] already does that — but the seam a backend plugs into.
//!
//! # Why capabilities are declared rather than assumed
//!
//! A backend that spawns a process can set a working directory; one that talks to a remote service may
//! not. A caller that assumed the capability would send a request the backend silently ignores, and a
//! child would run somewhere the caller did not intend. So a backend declares what it supports, and a
//! request asking for something else is refused **before** a child is established.
//!
//! The refusal is the important part: once a child exists it has a session, a log, and possibly a
//! process, and unwinding that because one field was unsupported is far worse than refusing up front.
//!
//! # Why publication is the ownership boundary
//!
//! A provider establishes the child and returns its handle. Until it returns, the provider owns
//! cleanup; after, the caller does. There is no moment where both think they own it or neither does.
//! That is why the API is "start and hand back a handle" rather than "start" then "get the handle":
//! the second form has a window in which a failed start leaks.
//!
//! # Why multiple providers coexist
//!
//! An in-process fork, a spawned process, and a remote agent all satisfy the same need with different
//! tradeoffs. Registering each under a name lets a caller choose — and lets one deployment use a
//! different backend than another without a code change.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::brand::SessionId;
use crate::scope::BoxFut;

/// What a backend can do.
///
/// Declared rather than probed: a probe costs a round trip and still cannot promise the next request
/// will be honoured. A declaration is a contract the backend owns.
///
/// A set of independent flags is exactly what this is: a backend either can or cannot honour each
/// request field, and folding them into an enum would produce a combinatorial mess of named
/// combinations that no backend would ever use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent capability flags, not a state machine"
)]
pub struct Capabilities {
    /// Can accept agent options (provider, model, reasoning effort) for the child.
    pub agent_options: bool,
    /// Can run the child under a preset that differs from the parent's.
    pub agent_preset: bool,
    /// Can constrain the child's output to a JSON schema.
    pub output_schema: bool,
    /// Enforces a delegation-depth ceiling.
    ///
    /// A backend that cannot enforce it must not claim it: the ceiling is what stops unbounded
    /// recursion, and a claim without enforcement turns a bounded tree into an unbounded one.
    pub depth_limit: bool,
    /// Can be steered with a message while running.
    pub steering: bool,
}

impl Capabilities {
    /// Everything a fully capable in-process backend supports.
    #[must_use]
    pub fn full() -> Self {
        Self {
            agent_options: true,
            agent_preset: true,
            output_schema: true,
            depth_limit: true,
            steering: true,
        }
    }

    /// What a backend that establishes a child and then reports a single result supports.
    ///
    /// No steering: an external execution has no message channel back into the child.
    #[must_use]
    pub fn one_shot() -> Self {
        Self {
            steering: false,
            ..Self::full()
        }
    }
}

/// A request to start a subagent.
#[derive(Clone, Debug)]
pub struct StartRequest {
    /// The task the child should perform.
    pub prompt: String,
    /// Its working directory, when the caller wants one different from the parent's.
    pub cwd: Option<std::path::PathBuf>,
    /// A preset to run under, when the caller wants one different from the parent's.
    pub preset: Option<String>,
    /// A JSON schema the child's output must satisfy.
    pub output_schema: Option<serde_json::Value>,
    /// How deep the child may delegate.
    pub max_depth: Option<u32>,
}

impl StartRequest {
    /// Builds a request for a plain task.
    #[must_use]
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            cwd: None,
            preset: None,
            output_schema: None,
            max_depth: None,
        }
    }

    /// Sets the working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<std::path::PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets the preset.
    #[must_use]
    pub fn with_preset(mut self, preset: impl Into<String>) -> Self {
        self.preset = Some(preset.into());
        self
    }

    /// Sets the output schema.
    #[must_use]
    pub fn with_output_schema(mut self, schema: serde_json::Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Sets the delegation ceiling for the child.
    #[must_use]
    pub fn with_max_depth(mut self, depth: u32) -> Self {
        self.max_depth = Some(depth);
        self
    }

    /// Whether the request asks for anything beyond a plain prompt.
    ///
    /// A plain prompt needs no capability, which is why a backend can be useful with none of the four
    /// declared: it can still run a task.
    #[must_use]
    pub fn needs(&self) -> Vec<&'static str> {
        let mut needs = Vec::new();
        if self.preset.is_some() {
            needs.push("agent_preset");
        }
        if self.output_schema.is_some() {
            needs.push("output_schema");
        }
        if self.max_depth.is_some() {
            needs.push("depth_limit");
        }
        needs
    }
}

/// Why a start was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StartError {
    /// No backend has that name.
    #[error("no subagent provider named `{name}`")]
    UnknownProvider {
        /// The name asked for.
        name: String,
    },

    /// The request asks for a capability the backend does not have.
    ///
    /// Refused before a child exists, so nothing has to be unwound.
    #[error("provider `{name}` cannot provide {capability}")]
    Unsupported {
        /// The provider.
        name: String,
        /// What it cannot do.
        capability: &'static str,
    },

    /// The backend failed to establish the child.
    #[error("provider `{name}` failed to start: {reason}")]
    Backend {
        /// The provider.
        name: String,
        /// Why.
        reason: String,
    },

    /// The depth ceiling would be exceeded.
    #[error("delegation depth {depth} would exceed the ceiling {ceiling}")]
    TooDeep {
        /// The depth asked for.
        depth: u32,
        /// The ceiling.
        ceiling: u32,
    },
}

/// A child that has been established and handed to the caller.
///
/// Holding this is what makes the caller responsible for the child: the provider released ownership
/// when it returned the handle.
pub struct ChildHandle {
    /// The child's session identity.
    pub session_id: SessionId,
    /// Which provider established it.
    pub provider: String,
    /// How deep it is.
    pub depth: crate::subagent::Depth,
    /// Stops it, whether or not it has finished.
    pub stop: Box<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for ChildHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildHandle")
            .field("session_id", &self.session_id)
            .field("provider", &self.provider)
            .field("depth", &self.depth)
            .finish_non_exhaustive()
    }
}

impl ChildHandle {
    /// Stops the child.
    pub fn stop(&self) {
        (self.stop)();
    }
}

/// A backend that can establish a child.
///
/// The return value carries ownership: a provider that fails must clean up before returning, because
/// the caller has no handle to clean up with.
pub trait SubagentProvider: Send + Sync {
    /// The name a caller selects this backend by.
    fn name(&self) -> &str;

    /// What this backend can do.
    fn capabilities(&self) -> Capabilities;

    /// Establishes a child and hands it over.
    fn start(
        &self,
        request: StartRequest,
        parent: &SessionId,
        depth: crate::subagent::Depth,
    ) -> BoxFut<Result<ChildHandle, StartError>>;
}

/// The registered backends.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn SubagentProvider>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ProviderRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a backend.
    ///
    /// # Errors
    ///
    /// Returns the name when it is taken. Replacing silently would send a caller's request to a
    /// backend it did not choose.
    pub fn register(&mut self, provider: Arc<dyn SubagentProvider>) -> Result<(), String> {
        let name = provider.name().to_owned();
        if self.providers.contains_key(&name) {
            return Err(name);
        }
        self.providers.insert(name, provider);
        Ok(())
    }

    /// The registered names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    /// Looks up a backend.
    ///
    /// # Errors
    ///
    /// Returns [`StartError::UnknownProvider`].
    pub fn get(&self, name: &str) -> Result<Arc<dyn SubagentProvider>, StartError> {
        self.providers
            .get(name)
            .cloned()
            .ok_or_else(|| StartError::UnknownProvider {
                name: name.to_owned(),
            })
    }

    /// Checks a request against a backend's declared capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`StartError::Unsupported`] for the first capability the request needs and the backend
    /// lacks. Checked here rather than inside each backend so every backend enforces it the same way:
    /// a backend that forgot would accept a request it cannot honour.
    pub fn check(
        name: &str,
        capabilities: Capabilities,
        request: &StartRequest,
    ) -> Result<(), StartError> {
        let mut needed = Vec::new();
        if request.preset.is_some() && !capabilities.agent_preset {
            needed.push("agent_preset");
        }
        if request.output_schema.is_some() && !capabilities.output_schema {
            needed.push("output_schema");
        }
        if request.max_depth.is_some() && !capabilities.depth_limit {
            needed.push("depth_limit");
        }
        if let Some(capability) = needed.first() {
            return Err(StartError::Unsupported {
                name: name.to_owned(),
                capability,
            });
        }
        Ok(())
    }

    /// Starts a child through a named backend.
    ///
    /// The order is capability check, then depth check, then start. Checking in that order means a
    /// refusal costs nothing: no child, no session, no process.
    ///
    /// # Errors
    ///
    /// Returns [`StartError::UnknownProvider`], [`StartError::Unsupported`], or
    /// [`StartError::TooDeep`].
    pub async fn start(
        &self,
        name: &str,
        request: StartRequest,
        parent: &SessionId,
        depth: crate::subagent::Depth,
        ceiling: u32,
    ) -> Result<ChildHandle, StartError> {
        let provider = self.get(name)?;
        Self::check(name, provider.capabilities(), &request)?;

        if depth.get() > ceiling {
            return Err(StartError::TooDeep {
                depth: depth.get(),
                ceiling,
            });
        }

        provider.start(request, parent, depth).await
    }
}

/// Shared registry handle.
pub type ProviderHandle = Arc<ProviderRegistry>;
