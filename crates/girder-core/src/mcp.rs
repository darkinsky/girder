//! MCP: connecting to external servers that offer tools.
//!
//! The Model Context Protocol is a wire contract: a server exposes tools, resources, and prompts, and a client
//! calls them. This module owns the *client-side vocabulary* — what a server is, what it offers, and how a
//! call is addressed — and leaves the transport to whoever implements it.
//!
//! # Why the transport is a trait
//!
//! Servers reach a client over stdio, over HTTP, or over a socket. The selection logic and the tool surface are
//! identical across all three, and putting the transport behind a trait means a new one does not change how a
//! tool is named or how a failure is reported.
//!
//! # Why a tool name is qualified
//!
//! Two servers can offer `search`. A tool registry keyed on the bare name would have one shadow the other, and
//! the model would call a tool that does not do what the description said. [`qualified_name`] makes the server
//! part of the identity.
//!
//! # Why a server's state is explicit
//!
//! A server that failed to start and one that is connected but idle look the same to a caller that only asks
//! "do you have this tool". [`Status`] distinguishes them, because the first is a configuration problem and the
//! second is not.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::scope::BoxFut;

/// Where a server is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Configured, not started.
    Stopped,
    /// Connected and answering.
    Ready,
    /// Failed to start or dropped.
    Failed {
        /// Why.
        reason: String,
    },
}

impl Status {
    /// Whether the server can answer a call.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// The wire name.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Ready => "ready",
            Self::Failed { .. } => "failed",
        }
    }
}

/// One tool a server offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    /// Its name within the server.
    pub name: String,
    /// What it does, for the model.
    pub description: String,
    /// Its JSON Schema parameters.
    pub input_schema: serde_json::Value,
}

impl Tool {
    /// The name the tool registry uses.
    ///
    /// Server-qualified because two servers can offer `search`, and a bare name would let one shadow the other
    /// — leaving the model calling a tool that does not do what its description said.
    #[must_use]
    pub fn qualified_name(server: &str, tool: &str) -> String {
        format!("{server}__{tool}")
    }

    /// This tool's qualified name.
    #[must_use]
    pub fn qualified(&self, server: &str) -> String {
        Self::qualified_name(server, &self.name)
    }
}

/// What a server offers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalogue {
    /// Its tools, by name.
    pub tools: BTreeMap<String, Tool>,
    /// Its resource URIs, when it exposes any.
    ///
    /// A resource is a readable document; a tool is callable. Keeping them apart matters because a model asks
    /// for one by URI and the other by name, and a consumer that conflated them would build a tool for a
    /// document.
    pub resources: Vec<String>,
}

impl Catalogue {
    /// Finds a tool by its bare name.
    #[must_use]
    pub fn tool(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }

    /// The tools, sorted by name.
    #[must_use]
    pub fn tools(&self) -> Vec<&Tool> {
        self.tools.values().collect()
    }

    /// The resource URIs a catalogue holds.
    ///
    /// Bare URIs rather than qualified names: a URI is already an address, and prefixing it would produce something
    /// that is no longer a URI and that a server would not recognise.
    #[must_use]
    pub fn resources(&self) -> &[String] {
        &self.resources
    }
}

/// Why an MCP operation failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum McpError {
    /// No server has that name.
    #[error("no MCP server named `{name}`")]
    UnknownServer {
        /// The name.
        name: String,
    },

    /// The server is not ready.
    #[error("MCP server `{name}` is {status}")]
    NotReady {
        /// The name.
        name: String,
        /// Its state.
        status: &'static str,
    },

    /// No server offers that tool.
    #[error("no MCP tool `{name}`")]
    UnknownTool {
        /// The qualified name.
        name: String,
    },

    /// The transport failed.
    #[error("MCP server `{name}` failed: {reason}")]
    Transport {
        /// The name.
        name: String,
        /// Why.
        reason: String,
    },

    /// The server reported a tool failure.
    #[error("MCP tool `{name}` failed: {message}")]
    ToolFailed {
        /// The qualified name.
        name: String,
        /// What the server said.
        message: String,
    },
}

/// The result of one tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallResult {
    /// The content blocks the server returned, as opaque JSON.
    ///
    /// Opaque because MCP's content union grows: a client that modelled every variant would break when a server
    /// added one, and the text a model needs is inside a shape the consumer can read generically.
    pub content: Vec<serde_json::Value>,
    /// Whether the server reported a failure.
    pub is_error: bool,
}

/// The transport a deployment implements.
///
/// Separated from the registry so a new transport does not change how a tool is named or how a failure is
/// reported: the naming rule and the error shape are the same over stdio, HTTP, and a socket.
pub trait Transport: Send + Sync {
    /// Starts a server and returns what it offers.
    fn start(&self) -> BoxFut<Result<Catalogue, McpError>>;

    /// Calls one tool.
    fn call(
        &self,
        tool: &str,
        arguments: serde_json::Value,
    ) -> BoxFut<Result<CallResult, McpError>>;

    /// Stops the server.
    fn stop(&self) -> BoxFut<Result<(), McpError>>;

    /// Lists the resources the server offers.
    ///
    /// Defaulted to an empty list rather than required: tools and resources are separate capabilities in the protocol,
    /// and a server that offers only tools is not incomplete. Forcing every transport to implement this would make a
    /// working tool-only server fail to compile.
    fn resources(&self) -> BoxFut<Result<Vec<Resource>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Reads one resource.
    ///
    /// Defaulted to a refusal for the same reason. A caller that asks a server without resources is told so rather than
    /// receiving an empty string it would render as empty content.
    fn read_resource(&self, uri: &str) -> BoxFut<Result<ResourceContent, McpError>> {
        let uri = uri.to_owned();
        Box::pin(async move {
            Err(McpError::ToolFailed {
                name: uri,
                message: "this server offers no resources".to_owned(),
            })
        })
    }
}

/// One resource a server publishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    /// Its address, which is what a caller names to read it.
    pub uri: String,
    /// A human-readable name.
    pub name: String,
    /// What it is, for a model or a person choosing one.
    pub description: Option<String>,
    /// Its media type when the server states one.
    pub mime_type: Option<String>,
}

/// What reading a resource returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceContent {
    /// The resource's address.
    pub uri: String,
    /// Its media type when the server states one.
    pub mime_type: Option<String>,
    /// The content, as the server sent it.
    ///
    /// Kept as a string rather than bytes because the protocol's text resources are text; a binary resource arrives
    /// base64-encoded inside this field and the caller decides how to decode it.
    pub text: String,
}

/// One configured server.
struct Server {
    name: String,
    transport: Arc<dyn Transport>,
    status: Status,
    catalogue: Catalogue,
}

/// The servers this deployment knows about.
#[derive(Default)]
pub struct Registry {
    servers: std::sync::Mutex<BTreeMap<String, Server>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("servers", &self.names())
            .finish()
    }
}

impl Registry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a server without starting it.
    ///
    /// # Errors
    ///
    /// Returns the name when it is taken: two servers under one name would make a qualified tool name ambiguous.
    pub fn add(
        &self,
        name: impl Into<String>,
        transport: Arc<dyn Transport>,
    ) -> Result<(), String> {
        let name = name.into();
        let mut servers = self.lock();
        if servers.contains_key(&name) {
            return Err(name);
        }
        servers.insert(
            name.clone(),
            Server {
                name,
                transport,
                status: Status::Stopped,
                catalogue: Catalogue::default(),
            },
        );
        Ok(())
    }

    /// The configured server names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// One server's state.
    #[must_use]
    pub fn status(&self, name: &str) -> Option<Status> {
        self.lock().get(name).map(|server| server.status.clone())
    }

    /// Starts one server, recording what it offers.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::UnknownServer`] or whatever the transport reports. A failure is recorded as
    /// [`Status::Failed`] rather than dropped: a server that did not start must be distinguishable from one that
    /// was never configured.
    pub async fn start(&self, name: &str) -> Result<Catalogue, McpError> {
        let transport = {
            let servers = self.lock();
            let server = servers.get(name).ok_or_else(|| McpError::UnknownServer {
                name: name.to_owned(),
            })?;
            Arc::clone(&server.transport)
        };

        match transport.start().await {
            Ok(catalogue) => {
                if let Some(server) = self.lock().get_mut(name) {
                    server.status = Status::Ready;
                    server.catalogue = catalogue.clone();
                }
                Ok(catalogue)
            }
            Err(error) => {
                if let Some(server) = self.lock().get_mut(name) {
                    server.status = Status::Failed {
                        reason: error.to_string(),
                    };
                }
                Err(error)
            }
        }
    }

    /// Every tool across every ready server, qualified.
    ///
    /// Only ready servers contribute: a stopped one has no catalogue, and claiming its tools would offer the
    /// model something that cannot answer.
    #[must_use]
    pub fn tools(&self) -> Vec<(String, Tool)> {
        let servers = self.lock();
        let mut tools = Vec::new();
        for server in servers.values() {
            if !server.status.is_ready() {
                continue;
            }
            for tool in server.catalogue.tools.values() {
                tools.push((tool.qualified(&server.name), tool.clone()));
            }
        }
        tools.sort_by(|a, b| a.0.cmp(&b.0));
        tools
    }

    /// Calls a qualified tool.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::UnknownTool`] for a name no ready server offers, [`McpError::NotReady`] for one a
    /// non-ready server would offer, and whatever the transport reports.
    /// Lists every resource every ready server publishes, as `(server, uri)`.
    ///
    /// Grouped by server rather than flattened: a URI is meaningful only to the server that published it, so a caller
    /// reading one has to know which server to ask.
    pub async fn resources(&self) -> Vec<(String, String)> {
        let servers: Vec<(String, Arc<dyn Transport>, bool)> = {
            let guard = self.lock();
            guard
                .iter()
                .map(|(name, server)| {
                    (
                        name.clone(),
                        Arc::clone(&server.transport),
                        server.status.is_ready(),
                    )
                })
                .collect()
        };
        let mut out = Vec::new();
        for (name, transport, ready) in servers {
            if !ready {
                continue;
            }
            if let Ok(list) = transport.resources().await {
                for resource in list {
                    out.push((name.clone(), resource.uri));
                }
            }
        }
        out
    }

    /// Reads one resource from the server that published it.
    pub async fn read_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<ResourceContent, McpError> {
        let transport = {
            let guard = self.lock();
            let known = guard.get(server).ok_or_else(|| McpError::UnknownServer {
                name: server.to_owned(),
            })?;
            if !known.status.is_ready() {
                return Err(McpError::NotReady {
                    name: server.to_owned(),
                    status: known.status.as_str(),
                });
            }
            Arc::clone(&known.transport)
        };
        transport.read_resource(uri).await
    }

    pub async fn call(
        &self,
        qualified: &str,
        arguments: serde_json::Value,
    ) -> Result<CallResult, McpError> {
        let (server_name, tool_name) =
            qualified
                .split_once("__")
                .ok_or_else(|| McpError::UnknownTool {
                    name: qualified.to_owned(),
                })?;

        let (transport, status) = {
            let servers = self.lock();
            let server = servers
                .get(server_name)
                .ok_or_else(|| McpError::UnknownTool {
                    name: qualified.to_owned(),
                })?;
            // A server that knows the tool but is not ready is a different problem from one that never had it,
            // and a caller reports them differently.
            if !server.catalogue.tools.contains_key(tool_name) && !server.status.is_ready() {
                return Err(McpError::NotReady {
                    name: server_name.to_owned(),
                    status: server.status.as_str(),
                });
            }
            (Arc::clone(&server.transport), server.status.clone())
        };

        if !status.is_ready() {
            return Err(McpError::NotReady {
                name: server_name.to_owned(),
                status: status.as_str(),
            });
        }

        let result = transport.call(tool_name, arguments).await?;
        if result.is_error {
            return Err(McpError::ToolFailed {
                name: qualified.to_owned(),
                message: result
                    .content
                    .first()
                    .and_then(|value| value.get("text"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("the server reported a failure")
                    .to_owned(),
            });
        }
        Ok(result)
    }

    /// Stops one server.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::UnknownServer`] or whatever the transport reports.
    pub async fn stop(&self, name: &str) -> Result<(), McpError> {
        let transport = {
            let servers = self.lock();
            let server = servers.get(name).ok_or_else(|| McpError::UnknownServer {
                name: name.to_owned(),
            })?;
            Arc::clone(&server.transport)
        };
        let result = transport.stop().await;
        if let Some(server) = self.lock().get_mut(name) {
            // A stopped server keeps no catalogue: its tools are no longer callable, and offering them would
            // invite a call that cannot answer.
            server.catalogue = Catalogue::default();
            server.status = if result.is_ok() {
                Status::Stopped
            } else {
                Status::Failed {
                    reason: result
                        .as_ref()
                        .err()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                }
            };
        }
        result
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Server>> {
        self.servers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
