//! The JSON-RPC 2.0 wire layer for talking to an external MCP server.
//!
//! [`crate::mcp`] owns the client-side vocabulary — what a server is, what it offers, how a call is addressed
//! and how a failure is reported. This module turns that vocabulary into the bytes a server on the other end of
//! a pipe actually reads, and turns its answers back into values: requests for `initialize`, `tools/list`, and
//! `tools/call`, and the parsing of whatever comes back.
//!
//! # Why this layer owns no process
//!
//! Spawning a stdio child, framing messages on its stdin and stdout, noticing that it died, and restarting it
//! are transport concerns that differ per deployment (a pipe, a socket, an SSH channel). Keeping them out means
//! every request and every reply in this module can be tested as a value, including the malformed replies a
//! live server only produces once in a while — which is exactly the case that breaks a client in production.
//!
//! # Why ids come from an allocator rather than a counter at each call site
//!
//! JSON-RPC has no ordering guarantee: two calls in flight can be answered in either order, and the only thing
//! that ties a reply to a request is its `id`. A duplicate id therefore does not fail loudly — it delivers one
//! request's result to another request's caller, or leaves a future waiting forever because its reply was
//! consumed by the wrong one. [`IdAllocator`] hands out a strictly increasing id and refuses to wrap, so
//! uniqueness is a property of the allocator while the request builders stay pure functions over an id.
//!
//! # Why a tool name carries its server
//!
//! Two servers commonly offer a tool called `read`. A model-facing registry keyed on the bare name would have
//! one shadow the other, leaving the model calling a tool whose behaviour differs from the description it read.
//! [`qualified_name`] makes the server part of the identity: `server_a__read` and `server_b__read` are distinct
//! tools, and the doubled underscore is the one separator the rest of this crate splits on.
//!
//! # Why a protocol error is a value but a malformed reply is an error
//!
//! A server answering `{"error": {"code": -32601, "message": "method not found"}}` is speaking the protocol:
//! the deployment can report "this server has no `tools/call`" without treating the connection as broken.
//! A reply that is not JSON-RPC at all, or that carries both `result` and `error`, is a protocol violation, and
//! nothing downstream can be trusted after it — so it becomes [`McpError`] instead of a response value.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

/// The JSON-RPC version every message in this module declares.
pub const JSONRPC_VERSION: &str = "2.0";

/// The MCP revision this client asks for during `initialize`.
///
/// Pinned rather than floated: a client that asked for "latest" would change what it accepts without a code
/// change, and a server's answer is negotiated — it may reply with an older revision, which the caller records
/// rather than rejects.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The first id [`IdAllocator::new`] hands out.
///
/// Zero is a legal JSON-RPC id, but it is also what logs, `Option`-shaped structs, and several servers use as
/// the placeholder for "no id". Starting at one means an id that appears in a reply is either one this client
/// issued or obviously foreign, instead of possibly a placeholder.
const FIRST_REQUEST_ID: u64 = 1;

/// One MCP server a deployment may connect to.
///
/// Only the fields the protocol layer needs: the name, which becomes the tool namespace, and the stdio command
/// that a transport will spawn. Nothing here starts a process.
///
/// [`ServerConfig::name`] is public so a caller can build the struct directly from parsed configuration, but a
/// name that is empty or contains [`NAMESPACE_SEPARATOR`] would make qualified tool names ambiguous; build the
/// struct with [`ServerConfig::new`] or run [`validate_server_name`] over a directly constructed one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    /// Its name, used as the tool namespace.
    pub name: String,
    /// The program to run for the stdio transport.
    pub command: String,
    /// The arguments passed to that program.
    pub args: Vec<String>,
}

impl ServerConfig {
    /// Builds a configuration for a stdio server.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::InvalidServerName`] when the name cannot be a namespace, and
    /// [`McpError::EmptyCommand`] when there is no program to run. Both are configuration mistakes that would
    /// otherwise surface as "this server has no tools" much later, when the server is first started.
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Result<Self, McpError> {
        let name = name.into();
        validate_server_name(&name)?;
        let command = command.into();
        if command.trim().is_empty() {
            return Err(McpError::EmptyCommand { name });
        }
        Ok(Self {
            name,
            command,
            args: Vec::new(),
        })
    }

    /// Adds the program's arguments.
    ///
    /// Chained onto [`ServerConfig::new`] because most configurations are a name, a command, and a list of
    /// arguments in one expression; splitting the arguments into a second constructor would give two ways to
    /// describe the same server.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Checks a configuration built field by field.
    ///
    /// # Errors
    ///
    /// Same conditions as [`ServerConfig::new`], for callers that deserialise into the struct directly and
    /// therefore bypassed that constructor.
    pub fn validate(&self) -> Result<(), McpError> {
        validate_server_name(&self.name)?;
        if self.command.trim().is_empty() {
            return Err(McpError::EmptyCommand {
                name: self.name.clone(),
            });
        }
        Ok(())
    }
}

/// One tool a server published, in the protocol's shape.
///
/// Deliberately not [`crate::mcp::Tool`]: that type is the registry's view, and a protocol layer that reused it
/// would have to change whenever the registry did. The conversion at the boundary is [`McpTool::from_value`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpTool {
    /// Its name as the server knows it, unqualified.
    pub name: String,
    /// What it does, as the server described it.
    pub description: String,
    /// The JSON Schema of its arguments.
    pub input_schema: Value,
}

impl McpTool {
    /// Builds a tool from one entry of a `tools/list` result.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::ToolNotAnObject`] or [`McpError::InvalidTool`] for an entry this client cannot address
    /// or validate: no name, a description that is not a string, or a schema that is not an object. Such an entry
    /// is rejected rather than repaired, because a tool whose schema the model cannot read is a tool whose calls
    /// will fail at the server with a message the model cannot act on.
    pub fn from_value(value: &Value) -> Result<Self, McpError> {
        let object = value.as_object().ok_or(McpError::ToolNotAnObject)?;
        let name = match object.get("name").and_then(Value::as_str) {
            Some(name) if !name.is_empty() => name.to_owned(),
            Some(_) => {
                return Err(McpError::InvalidTool {
                    reason: "a tool name is empty",
                });
            }
            None => {
                return Err(McpError::InvalidTool {
                    reason: "a tool has no string `name`",
                });
            }
        };
        let description = match object.get("description") {
            // A tool without a description is still callable; the protocol makes the field optional, and dropping
            // the entry would hide a working tool because its author was terse.
            None => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(_) => {
                return Err(McpError::InvalidTool {
                    reason: "a tool's `description` is not a string",
                });
            }
        };
        let input_schema = match object.get("inputSchema") {
            Some(schema @ Value::Object(_)) => schema.clone(),
            Some(_) => {
                return Err(McpError::InvalidTool {
                    reason: "a tool's `inputSchema` is not an object",
                });
            }
            None => {
                return Err(McpError::InvalidTool {
                    reason: "a tool has no `inputSchema`",
                });
            }
        };
        Ok(Self {
            name,
            description,
            input_schema,
        })
    }

    /// This tool's registry name.
    #[must_use]
    pub fn qualified(&self, server: &str) -> String {
        qualified_name(server, &self.name)
    }
}

/// A JSON-RPC error object as the server sent it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcError {
    /// Its numeric code.
    pub code: i64,
    /// Its human-readable message.
    pub message: String,
    /// Its optional structured detail.
    pub data: Option<Value>,
}

impl RpcError {
    /// The code JSON-RPC reserves for "this server does not implement that method".
    ///
    /// Named as a method rather than exposed as a constant because the only decision a caller makes from it is
    /// whether to report a missing capability instead of a failure.
    const METHOD_NOT_FOUND: i64 = -32601;

    /// Whether the server reports that it does not implement the method.
    ///
    /// This is how a deployment learns that a server speaks an older MCP revision without `tools/list` or
    /// `tools/call`: the connection is healthy and the capability is absent, which are different reports.
    #[must_use]
    pub fn is_method_not_found(&self) -> bool {
        self.code == Self::METHOD_NOT_FOUND
    }
}

/// One reply from a server, once it is known to be well-formed.
#[derive(Clone, Debug, PartialEq)]
pub enum McpResponse {
    /// The server answered with `result`.
    Success {
        /// The id of the request this answers.
        id: u64,
        /// The result, as the server sent it.
        result: Value,
    },
    /// The server answered with `error`.
    Failure {
        /// The id of the request this answers, when the server could name one.
        ///
        /// `None` is legal: JSON-RPC allows a null id for a reply to a message whose id could not be read, and
        /// refusing those would turn "the server rejected our framing" into a client-side parse failure.
        id: Option<u64>,
        /// What it said.
        error: RpcError,
    },
}

impl McpResponse {
    /// The id this reply carries, if any.
    #[must_use]
    pub fn id(&self) -> Option<u64> {
        match self {
            Self::Success { id, .. } => Some(*id),
            Self::Failure { id, .. } => *id,
        }
    }

    /// The result, for a success.
    #[must_use]
    pub fn result(&self) -> Option<&Value> {
        match self {
            Self::Success { result, .. } => Some(result),
            Self::Failure { .. } => None,
        }
    }

    /// The error, for a failure.
    #[must_use]
    pub fn error(&self) -> Option<&RpcError> {
        match self {
            Self::Failure { error, .. } => Some(error),
            Self::Success { .. } => None,
        }
    }
}

/// Why a message could not be built, parsed, or accepted.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum McpError {
    /// The reply was not a JSON object.
    #[error("an MCP reply must be a JSON object")]
    ResponseNotAnObject,

    /// The reply did not declare a JSON-RPC version.
    #[error("an MCP reply has no `jsonrpc` field")]
    MissingVersion,

    /// The reply declared a version this client does not speak.
    #[error("an MCP reply declared JSON-RPC `{found}`, not `2.0`")]
    UnsupportedVersion {
        /// What it declared.
        found: String,
    },

    /// A success reply had no id, so it cannot be matched to a request.
    #[error("a successful MCP reply has no `id`, so it cannot be matched to a request")]
    MissingId,

    /// The reply's id was not an unsigned integer or null.
    #[error("an MCP reply carries `id` = {found}, which is not an integer id")]
    InvalidId {
        /// What it carried.
        found: String,
    },

    /// The reply carried both `result` and `error`.
    #[error("an MCP reply carries both `result` and `error`")]
    ResultAndError,

    /// The reply carried neither `result` nor `error`.
    #[error("an MCP reply carries neither `result` nor `error`")]
    NeitherResultNorError,

    /// The reply's `error` was not a JSON object.
    #[error("an MCP reply's `error` is not a JSON object")]
    ErrorNotAnObject,

    /// The reply's `error` had no integer code.
    #[error("an MCP reply's `error` has no integer `code`")]
    ErrorMissingCode,

    /// The reply's `error` had no string message.
    #[error("an MCP reply's `error` has no string `message`")]
    ErrorMissingMessage,

    /// A `tools/list` result had no `tools` array.
    #[error("a `tools/list` result has no `tools` array")]
    ToolsNotListed,

    /// One entry of a tool list could not be used.
    #[error("an MCP tool cannot be used: {reason}")]
    InvalidTool {
        /// Why, in one clause.
        reason: &'static str,
    },

    /// One entry of a tool list was not a JSON object.
    #[error("an MCP tool entry is not a JSON object")]
    ToolNotAnObject,

    /// A server name cannot serve as a tool namespace.
    #[error("`{name}` cannot be an MCP server name: {reason}")]
    InvalidServerName {
        /// The name as configured.
        name: String,
        /// Why, in one clause.
        reason: &'static str,
    },

    /// A server configuration has no program to run.
    #[error("MCP server `{name}` has an empty command")]
    EmptyCommand {
        /// The server's name.
        name: String,
    },

    /// The id allocator reached the end of its range.
    #[error("MCP request ids are exhausted")]
    IdExhausted,

    /// A name has no server namespace to split off.
    #[error("`{name}` is not a server-qualified MCP tool name")]
    UnqualifiedToolName {
        /// The name as given.
        name: String,
    },
}

/// The separator between the server namespace and the tool name.
///
/// Doubled underscore because a single one is common inside both halves (`read_file`, `server_a`), and a
/// separator that appeared inside a half would make the split point a guess.
pub const NAMESPACE_SEPARATOR: &str = "__";

/// A character set a server name may use.
///
/// Restricted to what a tool name may contain under the provider APIs this crate targets, so a qualified name
/// built from a validated server name is itself always a usable tool name.
fn is_namespace_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '-' || character == '_'
}

/// Checks that a name can serve as a tool namespace.
///
/// # Errors
///
/// Returns [`McpError::InvalidServerName`] when the name is empty, contains whitespace or anything outside
/// `[A-Za-z0-9_-]`, or contains [`NAMESPACE_SEPARATOR`]. The last rule is the one that matters for correctness:
/// [`qualified_name`] is only reversible, and two servers only stay in separate namespaces, while the server
/// half contains no separator.
pub fn validate_server_name(name: &str) -> Result<(), McpError> {
    let reason = if name.trim().is_empty() {
        Some("a namespace cannot be empty")
    } else if name.contains(NAMESPACE_SEPARATOR) {
        Some("a namespace cannot contain `__`, the tool-name separator")
    } else if !name.chars().all(is_namespace_char) {
        Some("a namespace may only use letters, digits, `-`, and `_`")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(McpError::InvalidServerName {
            name: name.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

/// The name a tool is registered under, given its server.
///
/// `server_a` + `read` is `server_a__read`. The server half is expected to have passed [`validate_server_name`]:
/// that is what makes the first separator the split point rather than one candidate among several, and what
/// stops a server named `a__b` from publishing tools into server `a`'s namespace.
#[must_use]
pub fn qualified_name(server: &str, tool: &str) -> String {
    format!("{server}{NAMESPACE_SEPARATOR}{tool}")
}

/// Splits a qualified name back into its server and tool halves.
///
/// Split at the *first* separator so a tool name may itself contain one; only the server half is forbidden from
/// doing so, which is exactly the constraint [`validate_server_name`] enforces.
///
/// # Errors
///
/// Returns [`McpError::UnqualifiedToolName`] when there is no separator, because a bare name would otherwise be
/// routed to whichever server happened to be looked up first.
pub fn split_qualified_name(qualified: &str) -> Result<(&str, &str), McpError> {
    qualified
        .split_once(NAMESPACE_SEPARATOR)
        .ok_or_else(|| McpError::UnqualifiedToolName {
            name: qualified.to_owned(),
        })
}

/// Builds the `initialize` request.
///
/// It is always the first message on a connection: every server rejects `tools/list` before the handshake, so a
/// client that skipped it would see a healthy-looking socket answer nothing.
#[must_use]
pub fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            // The client declares no capabilities: it reads tool lists and calls tools, and claiming more would
            // invite a server to send messages this client never handles.
            "capabilities": {},
            "clientInfo": {
                "name": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
            },
        },
    })
}

/// Builds the `tools/list` request.
///
/// The empty `params` object is sent explicitly rather than omitted: several servers validate `params` presence,
/// and an omitted field is reported as a malformed request rather than as "no cursor".
#[must_use]
pub fn list_tools_request(id: u64) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "method": "tools/list",
        "params": {},
    })
}

/// Builds the `tools/call` request.
///
/// `arguments` is passed through unmodified. A value that is not an object is left for the server to reject,
/// because rewriting it into one would hide a caller that built its arguments from the wrong schema, and the
/// server's `-32602` names the parameter that was wrong.
#[must_use]
pub fn call_tool_request(id: u64, name: &str, arguments: Value) -> Value {
    // Built by insertion rather than with `json!` so `arguments` is moved into the request instead of cloned:
    // a tool's arguments can be a large document, and the caller has no use for it afterwards.
    let mut params = serde_json::Map::new();
    params.insert("name".to_owned(), Value::String(name.to_owned()));
    params.insert("arguments".to_owned(), arguments);
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "method": "tools/call",
        "params": Value::Object(params),
    })
}

/// Parses one reply into a success or a protocol error.
///
/// # Errors
///
/// Returns [`McpError`] for anything that is not a JSON-RPC 2.0 reply: not an object, a missing or foreign
/// version, an id that is neither an unsigned integer nor null, both `result` and `error`, neither, or an
/// `error` that is not `{code, message, data?}`. The distinction from [`McpResponse::Failure`] is deliberate: a
/// server that reports an error is talking, and a server that sends this is not.
pub fn parse_response(value: &Value) -> Result<McpResponse, McpError> {
    let object = value.as_object().ok_or(McpError::ResponseNotAnObject)?;

    match object.get("jsonrpc").and_then(Value::as_str) {
        Some(JSONRPC_VERSION) => {}
        Some(found) => {
            return Err(McpError::UnsupportedVersion {
                found: found.to_owned(),
            });
        }
        None => return Err(McpError::MissingVersion),
    }

    let id = match object.get("id") {
        // Absent entirely is different from an explicit null: a reply has to name the request it answers.
        None => return Err(McpError::MissingId),
        Some(Value::Null) => None,
        Some(Value::Number(number)) => {
            let id = number.as_u64().ok_or_else(|| McpError::InvalidId {
                found: number.to_string(),
            })?;
            Some(id)
        }
        Some(other) => {
            return Err(McpError::InvalidId {
                found: other.to_string(),
            });
        }
    };

    match (object.get("result"), object.get("error")) {
        (Some(_), Some(_)) => Err(McpError::ResultAndError),
        (None, None) => Err(McpError::NeitherResultNorError),
        (Some(result), None) => {
            // A success cannot answer "some request": without an id it could only be delivered by guesswork.
            let id = id.ok_or(McpError::MissingId)?;
            Ok(McpResponse::Success {
                id,
                result: result.clone(),
            })
        }
        (None, Some(error)) => Ok(McpResponse::Failure {
            id,
            error: parse_rpc_error(error)?,
        }),
    }
}

/// Parses the `error` member of a reply.
fn parse_rpc_error(value: &Value) -> Result<RpcError, McpError> {
    let object = value.as_object().ok_or(McpError::ErrorNotAnObject)?;
    let code = object
        .get("code")
        .and_then(Value::as_i64)
        .ok_or(McpError::ErrorMissingCode)?;
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .ok_or(McpError::ErrorMissingMessage)?;
    Ok(RpcError {
        code,
        message: message.to_owned(),
        data: object.get("data").cloned(),
    })
}

/// Parses the result of a `tools/list` request into tools.
///
/// # Errors
///
/// Returns [`McpError::ToolsNotListed`] when the result is not `{"tools": [...]}`, and the [`McpTool::from_value`]
/// errors for one unusable entry. Failing the listing rather than skipping the entry is the safer default here:
/// a model that cannot see a tool will not call it, and a tool list that silently loses an entry looks like a
/// server that never offered it.
pub fn parse_tools(result: &Value) -> Result<Vec<McpTool>, McpError> {
    let entries = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or(McpError::ToolsNotListed)?;
    entries
        .iter()
        .map(McpTool::from_value)
        .collect::<Result<Vec<_>, _>>()
}

/// Hands out request ids that are unique for the life of a connection.
///
/// One allocator per connection: ids only have to be unique among outstanding requests in one JSON-RPC session,
/// but a monotonic sequence is cheaper to reason about than reuse, and a reused id that arrives after its first
/// request timed out is indistinguishable from a late answer.
#[derive(Debug)]
pub struct IdAllocator {
    next: AtomicU64,
}

impl IdAllocator {
    /// Creates an allocator whose first id is one.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(FIRST_REQUEST_ID),
        }
    }

    /// Creates an allocator whose first id is `start`.
    ///
    /// For a client that did not open the connection itself — a restarted process replaying a session — so it
    /// does not hand out ids the peer has already seen.
    #[must_use]
    pub const fn starting_at(start: u64) -> Self {
        Self {
            next: AtomicU64::new(start),
        }
    }

    /// Reserves the next id.
    ///
    /// Ids run up to `u64::MAX - 1`: the allocator's stored cursor is always "the id to hand out next", and the
    /// increment that would move it past `u64::MAX` is what it refuses.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::IdExhausted`] at the end of the `u64` range. Refusing is the whole point: wrapping
    /// would reissue ids and break the only link between a reply and its request, so the allocator stops instead
    /// and a caller that sees the error reconnects (or returns a fresh id space) rather than corrupting the
    /// session. `fetch_update` makes the check and the increment one atomic step, so concurrent callers cannot
    /// both pass it.
    pub fn next_id(&self) -> Result<u64, McpError> {
        self.next
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| McpError::IdExhausted)
    }
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}
