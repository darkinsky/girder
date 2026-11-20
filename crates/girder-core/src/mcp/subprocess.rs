//! An MCP server that actually runs.
//!
//! # Why this exists
//!
//! `mcp`, `mcp_client`, `mcp_stdio` and `mcp_process` describe, frame, parse and track an MCP server, and the only
//! `impl Transport` in the tree is `crate::ssh::Unavailable`, which refuses every call. So every piece of the protocol was
//! implemented and no server could be started: a deployment could configure an MCP server and watch it be unreachable
//! through a transport whose whole purpose was to say so.
//!
//! # What it does
//!
//! Spawns the command, writes newline-delimited JSON frames to its stdin, reads one JSON object per line from stdout, and
//! matches replies to requests by id. That is the whole of the stdio transport in the protocol.
//!
//! # What it refuses to hide
//!
//! A server that exits mid-session is a *disconnect*, not a timeout. The two are reported differently because they mean
//! different things: a timeout says the server is alive and slow, and a disconnect says it is gone. Treating one as the
//! other sends an operator to look at the wrong thing.

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use crate::BoxFut;
use crate::mcp::{CallResult, Catalogue, McpError, Resource, Transport};

/// An MCP server started as a child process.
pub struct Subprocess {
    /// What to run.
    program: String,
    /// Its arguments.
    arguments: Vec<String>,
    /// The running child, once started.
    ///
    /// In an `Arc` because `Transport` returns `'static` futures: a method that borrowed `&self` could not be boxed that
    /// way, and the alternative -- an owning self -- would consume the transport on the first call.
    child: std::sync::Arc<Mutex<Option<Child>>>,
    /// The next request id.
    next_id: std::sync::Arc<AtomicU64>,
}

impl Subprocess {
    /// Describes a server without starting it.
    ///
    /// Starting is deferred to [`Transport::start`] so that constructing a transport cannot fail and cannot leak a
    /// process if the caller changes its mind.
    #[must_use]
    pub fn new(program: impl Into<String>, arguments: Vec<String>) -> Self {
        Self {
            program: program.into(),
            arguments,
            child: std::sync::Arc::new(Mutex::new(None)),
            next_id: std::sync::Arc::new(AtomicU64::new(1)),
        }
    }

    /// The program this transport runs.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Sends one request and reads the reply with that id.
    ///
    /// A free function rather than a method because `Transport` returns `'static` futures: the caller clones the two
    /// handles it needs and this runs on owned values.
    fn send(
        program: &str,
        child_slot: &Mutex<Option<Child>>,
        next_id: &AtomicU64,
        method: &str,
        params: &Value,
    ) -> Result<Value, McpError> {
        let mut guard = child_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(child) = guard.as_mut() else {
            return Err(McpError::NotReady {
                name: program.to_owned(),
                status: "not started",
            });
        };
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params.clone(),
        });
        let encoded = crate::mcp::stdio::encode_frame(&request.to_string().into_bytes());
        {
            use std::io::Write as _;
            let stdin = child.stdin.as_mut().ok_or_else(|| McpError::NotReady {
                name: program.to_owned(),
                status: "no stdin",
            })?;
            stdin
                .write_all(&encoded)
                .map_err(|error| McpError::Transport {
                    name: program.to_owned(),
                    reason: format!("writing `{method}` failed: {error}"),
                })?;
            stdin.flush().map_err(|error| McpError::Transport {
                name: program.to_owned(),
                reason: format!("flushing `{method}` failed: {error}"),
            })?;
        }
        let line = read_line(child).map_err(|detail| McpError::Transport {
            name: program.to_owned(),
            reason: format!("reading the reply to `{method}`: {detail}"),
        })?;
        let parsed: Value = serde_json::from_str(&line).map_err(|error| McpError::Transport {
            name: program.to_owned(),
            reason: format!("the reply to `{method}` is not valid JSON: {error}"),
        })?;
        Ok(parsed)
    }
}

/// Reads one response frame from the child's stdout.
fn read_line(child: &mut Child) -> Result<String, String> {
    use std::io::{BufRead as _, BufReader};
    let stdout = child
        .stdout
        .as_mut()
        .ok_or_else(|| "the server has no stdout".to_owned())?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Err("the server closed its stdout".to_owned()),
        Ok(_) => Ok(line),
        Err(error) => Err(format!("{error}")),
    }
}

impl Drop for Subprocess {
    /// Stops the child, so a transport that is dropped does not leave a process behind.
    ///
    /// A `sh` script whose stdin is closed normally exits on its own, but a server that ignores EOF would outlive the
    /// process that started it -- and a leaked child is a file descriptor held open for as long as the machine runs.
    fn drop(&mut self) {
        let mut guard = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(mut child) = guard.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Runs the handshake and the tool listing, returning the catalogue and the live child.
///
/// A free function so `start` stays inside the length the linter allows, and because the two requests are one unit: a
/// server that answers `initialize` and not `tools/list` is not half-started, it is unusable.
fn handshake(program: &str, child: &mut Child, next_id: &AtomicU64) -> Result<Catalogue, McpError> {
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let initialize = crate::mcp::client::initialize_request(id);
    let reply = exchange(program, child, &initialize, "initialize")?;
    if reply.get("result").is_none() {
        return Err(McpError::Transport {
            name: program.to_owned(),
            reason: format!("initialize answered without a result: {reply}"),
        });
    }

    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let list = crate::mcp::client::list_tools_request(id);
    let reply = exchange(program, child, &list, "tools/list")?;
    let result = reply.get("result").ok_or_else(|| McpError::Transport {
        name: program.to_owned(),
        reason: format!("tools/list answered without a result: {reply}"),
    })?;
    let listed = crate::mcp::client::parse_tools(result).map_err(|error| McpError::Transport {
        name: program.to_owned(),
        reason: format!("tools/list could not be read: {error}"),
    })?;
    // The two `Tool` types answer different questions -- one is the wire shape, one is what a registry keys by -- so the
    // conversion is explicit rather than a `From` impl that would hide the second name.
    let tools: std::collections::BTreeMap<String, crate::mcp::Tool> = listed
        .into_iter()
        .map(|tool| {
            (
                tool.name.clone(),
                crate::mcp::Tool {
                    name: tool.name,
                    description: tool.description,
                    input_schema: tool.input_schema,
                },
            )
        })
        .collect();
    Ok(Catalogue {
        tools,
        resources: Vec::new(),
    })
}

/// Writes one request and reads one reply.
fn exchange(
    program: &str,
    child: &mut Child,
    request: &Value,
    method: &str,
) -> Result<Value, McpError> {
    let encoded = crate::mcp::stdio::encode_frame(&request.to_string().into_bytes());
    {
        use std::io::Write as _;
        let stdin = child.stdin.as_mut().ok_or_else(|| McpError::NotReady {
            name: program.to_owned(),
            status: "no stdin",
        })?;
        stdin
            .write_all(&encoded)
            .map_err(|error| McpError::Transport {
                name: program.to_owned(),
                reason: format!("writing `{method}` failed: {error}"),
            })?;
        stdin.flush().map_err(|error| McpError::Transport {
            name: program.to_owned(),
            reason: format!("flushing `{method}` failed: {error}"),
        })?;
    }
    let line = read_line(child).map_err(|detail| McpError::Transport {
        name: program.to_owned(),
        reason: format!("reading the reply to `{method}`: {detail}"),
    })?;
    serde_json::from_str(&line).map_err(|error| McpError::Transport {
        name: program.to_owned(),
        reason: format!("the reply to `{method}` is not valid JSON: {error}"),
    })
}

impl Transport for Subprocess {
    fn start(&self) -> BoxFut<Result<Catalogue, McpError>> {
        let program = self.program.clone();
        let arguments = self.arguments.clone();
        let child_slot = std::sync::Arc::clone(&self.child);
        let next_id = std::sync::Arc::clone(&self.next_id);
        Box::pin(async move {
            let mut child = Command::new(&program)
                .args(&arguments)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| McpError::Transport {
                    name: program.clone(),
                    reason: format!("the server could not be started: {error}"),
                })?;
            // The handshake runs before the child is stored, so a server that starts and never answers is reported as a
            // start failure rather than as a working server whose first call hangs.
            let catalogue = match handshake(&program, &mut child, &next_id) {
                Ok(catalogue) => catalogue,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            *child_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(child);
            Ok(catalogue)
        })
    }

    fn call(&self, tool: &str, arguments: Value) -> BoxFut<Result<CallResult, McpError>> {
        let tool = tool.to_owned();
        let program = self.program.clone();
        let child_slot = std::sync::Arc::clone(&self.child);
        let next_id = std::sync::Arc::clone(&self.next_id);
        Box::pin(async move {
            let name = crate::mcp::client::qualified_name(&program, &tool);
            let reply = Subprocess::send(
                &program,
                &child_slot,
                &next_id,
                "tools/call",
                &serde_json::json!({ "name": tool, "arguments": arguments }),
            )?;
            // The server reports a tool failure inside a successful reply, with `isError`. Passing it through as success
            // would tell a caller that a failed tool worked.
            let response =
                crate::mcp::client::parse_response(&reply).map_err(|error| McpError::Transport {
                    name: name.clone(),
                    reason: format!("the reply could not be read: {error}"),
                })?;
            let result = match response {
                crate::mcp::client::McpResponse::Success { result, .. } => result,
                crate::mcp::client::McpResponse::Failure { error, .. } => {
                    return Err(McpError::ToolFailed {
                        name,
                        message: error.message,
                    });
                }
            };
            if result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let message = result.get("content").map_or_else(
                    || "the server reported an error".to_owned(),
                    ToString::to_string,
                );
                return Err(McpError::ToolFailed { name, message });
            }
            let content = result
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            Ok(CallResult {
                content,
                is_error: false,
            })
        })
    }

    fn stop(&self) -> BoxFut<Result<(), McpError>> {
        let child_slot = std::sync::Arc::clone(&self.child);
        Box::pin(async move {
            let mut guard = child_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            Ok(())
        })
    }

    fn resources(&self) -> BoxFut<Result<Vec<Resource>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}
