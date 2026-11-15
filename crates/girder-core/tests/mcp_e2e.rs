//! End-to-end tests for the MCP client against a **real server process**.
//!
//! Every other MCP test in this crate feeds values to a type: [`FrameBuffer`] is given byte slices,
//! `parse_response` is given `serde_json::Value`s, and the lifecycle record is driven by hand. None of
//! them proves that the bytes a client writes are the bytes a server reads, or that a server's reply
//! comes back through framing, JSON decoding, and protocol validation intact. Those are the seams where
//! a real server breaks a client: a frame split by the pipe, an id that does not match, a process that
//! dies between two calls.
//!
//! # Why a shell script instead of an existing MCP server
//!
//! A test that downloads `npx @modelcontextprotocol/server-filesystem` needs a network, a package
//! manager, and a specific upstream version, and it can only produce the happy path — it will never
//! send a non-JSON line or exit halfway through a frame. A fixture script is the opposite: hermetic,
//! instant, and able to emit exactly the malformed sequence a test names. The script is still a real
//! process on a real pipe, which is the property these tests are about.
//!
//! # Why the client half lives in this file
//!
//! `mcp_client` builds the requests and parses the replies, and `mcp_stdio` frames them, but nothing in
//! the tree yet implements [`McpTransport`] over a spawned child (the only implementation,
//! `ssh::Unavailable`, refuses every call). [`Session`] below is therefore the smallest honest version
//! of that transport: production functions for every step, a real child process for the peer. Where a
//! step has no production function at all — reading a `tools/call` result — the gap is called out at
//! the function that fills it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// `McpRegistry` is imported by its crate-root alias: the bare `Registry` at that path is the service
// registry, and importing it here would compile to a type with none of the methods these tests need.
use girder_core::{
    BoxFut, FrameBuffer, IdAllocator, McpCallResult, McpCatalogue, McpError as RegistryError,
    McpProtocolError, McpProtocolTool, McpRegistry, McpResponse, McpTool as RegistryTool,
    McpTransport, PROTOCOL_VERSION, call_tool_request, encode_frame, initialize_request,
    list_tools_request, parse_response, parse_tools,
};
use serde_json::{Value, json};

/// How long a test waits for the server before declaring that the client hung.
///
/// Bounded on purpose: the failure these tests guard against is a client that waits forever on a pipe
/// nobody will write to, so a wait without a deadline would turn exactly that bug into a hanging test
/// suite instead of a failing assertion.
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(10);

// -- The fixture server's behaviours -----------------------------------------------------------------
//
// Named constants rather than bare strings at the call sites: a typo would fall through the script's
// `case` to the happy path, and a failure test that quietly exercises the happy path is worse than no
// test at all.

/// Answers every request correctly.
const NORMAL: &str = "normal";
/// Answers the handshake, then exits when asked for the tool list.
const EXIT_MID_SESSION: &str = "exit-mid-session";
/// Answers the handshake, then exits in the middle of a frame for the tool list.
const DIE_MID_FRAME: &str = "die-mid-frame";
/// Answers the tool list with a line that is not JSON.
const GARBAGE: &str = "garbage";
/// Answers the tool list with JSON that is not an object.
const SCALAR: &str = "scalar";
/// Answers `tools/call` with a JSON-RPC `error` object.
const JSON_RPC_ERROR: &str = "json-rpc-error";
/// Answers `tools/call` with a result flagged `isError`.
const TOOL_ERROR: &str = "tool-error";

/// A stand-in MCP server: one JSON-RPC request per line in, one reply per line out.
///
/// The id is echoed from the request rather than hard-coded, so a client that changed its id scheme
/// would still be answered correctly and the assertions about ids would still mean something. The
/// `text` argument of a `tools/call` is echoed too, which is what proves the arguments crossed the pipe
/// instead of being replaced by a constant on this side.
const SERVER_SCRIPT: &str = r#"#!/bin/sh
set -u

mode="${1:-normal}"

request_id() {
    printf '%s' "$1" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'
}

argument_text() {
    printf '%s' "$1" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p'
}

while IFS= read -r line; do
    id=$(request_id "$line")
    case "$line" in
    *'"method":"initialize"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"fixture","version":"0.1.0"}}}\n' "$id"
        ;;
    *'"method":"tools/list"'*)
        case "$mode" in
        exit-mid-session) exit 0 ;;
        die-mid-frame) printf '{"jsonrpc":"2.0","id":%s,"res' "$id"; exit 0 ;;
        garbage) printf 'this line is not json at all\n' ;;
        scalar) printf '"just a string"\n' ;;
        *) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo the text back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}}\n' "$id" ;;
        esac
        ;;
    *'"method":"tools/call"'*)
        case "$mode" in
        json-rpc-error) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"invalid arguments","data":{"field":"text"}}}\n' "$id" ;;
        tool-error) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"the echo tool broke"}],"isError":true}}\n' "$id" ;;
        *) text=$(argument_text "$line"); printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}],"isError":false}}\n' "$id" "$text" ;;
        esac
        ;;
    esac
done
"#;

/// What the reader thread observed.
enum Event {
    /// One complete frame, in the MCP stdio framing.
    Frame(Vec<u8>),
    /// stdout reached EOF, with this many bytes of an unfinished frame still buffered.
    Closed {
        /// Bytes that arrived but never completed a frame.
        leftover: usize,
    },
    /// Reading stdout failed.
    Broken {
        /// Why.
        detail: String,
    },
}

/// Why a request did not produce a reply.
///
/// Split by *where* the failure happened, because the layers answer different questions and a test
/// that collapsed them could not tell "the server is gone" from "the server is talking nonsense".
#[derive(Debug, PartialEq, Eq)]
enum ClientError {
    /// The server closed its end of the pipe.
    Closed {
        /// Bytes of a frame that never completed; zero means a clean end of stream.
        leftover: usize,
    },
    /// Reading from or writing to the pipe failed.
    Broken {
        /// Why.
        detail: String,
    },
    /// No reply arrived before [`RECEIVE_TIMEOUT`].
    TimedOut {
        /// How long the client waited.
        waited: Duration,
    },
    /// The reply was not JSON, so nothing could be parsed out of it.
    MalformedJson {
        /// The parser's complaint.
        detail: String,
    },
    /// The reply was JSON but violated JSON-RPC.
    Protocol {
        /// What `parse_response` refused.
        error: McpProtocolError,
    },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed { leftover } => write!(
                formatter,
                "the server closed its stdout with {leftover} bytes of an incomplete frame"
            ),
            Self::Broken { detail } => write!(formatter, "the MCP pipe broke: {detail}"),
            Self::TimedOut { waited } => {
                write!(formatter, "the server sent nothing within {waited:?}")
            }
            Self::MalformedJson { detail } => write!(formatter, "{detail}"),
            Self::Protocol { error } => write!(formatter, "the reply violated JSON-RPC: {error}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// One fixture server process and the channel its stdout is read on.
///
/// The reader lives on its own thread because `ChildStdout::read` blocks: a client that owns the pipe
/// cannot distinguish "the server is thinking" from "the server is gone" without a deadline, and the
/// deadline is only expressible with a thread (or an async runtime) doing the read.
struct Server {
    /// Removed on drop, so a run leaves no directories behind.
    directory: PathBuf,
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<Event>,
}

impl Server {
    /// Writes the script into a fresh temporary directory and starts it in `mode`.
    fn start(mode: &'static str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "girder-mcp-e2e-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).expect("a temporary directory is writable");
        let script = directory.join("server.sh");
        std::fs::write(&script, SERVER_SCRIPT).expect("the fixture script is writable");

        let mut child = Command::new("sh")
            .arg(&script)
            .arg(mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited so a mistake in the script is visible in the test output instead of being
            // swallowed while the test waits ten seconds for a reply that will never come.
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the fixture server starts");

        let stdin = child.stdin.take().expect("the fixture's stdin is piped");
        let stdout = child.stdout.take().expect("the fixture's stdout is piped");
        Self {
            directory,
            child,
            stdin,
            frames: spawn_reader(stdout),
        }
    }

    /// Writes one request, framed the way the MCP specification frames stdio.
    fn send(&mut self, request: &Value) -> Result<(), ClientError> {
        let payload = serde_json::to_vec(request).expect("a JSON value is encodable");
        self.stdin
            .write_all(&encode_frame(&payload))
            .map_err(|error| ClientError::Broken {
                detail: format!("writing the request failed: {error}"),
            })?;
        self.stdin.flush().map_err(|error| ClientError::Broken {
            detail: format!("flushing the request failed: {error}"),
        })
    }

    /// Waits for the next complete frame.
    fn receive_frame(&mut self) -> Result<Vec<u8>, ClientError> {
        match self.frames.recv_timeout(RECEIVE_TIMEOUT) {
            Ok(Event::Frame(frame)) => Ok(frame),
            Ok(Event::Closed { leftover }) => Err(ClientError::Closed { leftover }),
            Ok(Event::Broken { detail }) => Err(ClientError::Broken { detail }),
            Err(RecvTimeoutError::Timeout) => Err(ClientError::TimedOut {
                waited: RECEIVE_TIMEOUT,
            }),
            Err(RecvTimeoutError::Disconnected) => Err(ClientError::Broken {
                detail: "the thread reading the server's stdout stopped".to_owned(),
            }),
        }
    }

    /// Sends a request and returns its reply, already decoded from JSON.
    fn reply_to(&mut self, request: &Value) -> Result<Value, ClientError> {
        self.send(request)?;
        let frame = self.receive_frame()?;
        serde_json::from_slice(&frame).map_err(|error| ClientError::MalformedJson {
            detail: format!("the reply is not valid JSON: {error}"),
        })
    }

    /// Sends a request and waits for the next frame, without decoding it.
    ///
    /// Used by the tests whose expected answer is *not* a reply: a stream that ends, or ends mid-frame,
    /// has no value to parse, and going through [`Server::reply_to`] would report the disconnect as a
    /// parse failure instead of the transport failure it is.
    fn receive_frame_after(&mut self, request: &Value) -> Result<Vec<u8>, ClientError> {
        self.send(request)?;
        self.receive_frame()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Kill before reaping: a test that failed mid-handshake must not leave a `sh` holding this
        // binary's inherited descriptors, and `wait` reaps the zombie the kill creates.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Reads frames off the child's stdout until EOF, handing them to the test thread.
///
/// The buffer lives here rather than in the test so that framing is the real `FrameBuffer` over a real
/// pipe: a reply the kernel splits across two reads is reassembled by the same code a deployment uses,
/// not by the test.
fn spawn_reader(mut stdout: ChildStdout) -> Receiver<Event> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buffer = FrameBuffer::new();
        let mut chunk = [0_u8; 4096];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    let _ = sender.send(Event::Closed {
                        leftover: buffer.pending(),
                    });
                    return;
                }
                Ok(read) => {
                    for frame in buffer.feed(&chunk[..read]) {
                        if sender.send(Event::Frame(frame)).is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = sender.send(Event::Broken {
                        detail: error.to_string(),
                    });
                    return;
                }
            }
        }
    });
    receiver
}

// -- The transport the crate does not have yet --------------------------------------------------------

/// A [`McpTransport`] over one fixture process.
///
/// `Mutex<Option<..>>` because the trait's methods take `&self`: the connection is mutable state that
/// the registry holds by shared reference, which is exactly what a real transport would have to do too.
struct Session {
    server: Server,
    ids: IdAllocator,
}

impl Session {
    /// Starts a server in `mode` with its own id space.
    fn start(mode: &'static str) -> Self {
        Self {
            server: Server::start(mode),
            ids: IdAllocator::new(),
        }
    }

    /// Builds a request with a fresh id, sends it, and parses the reply.
    ///
    /// Both halves of the round trip are reported as [`ClientError`] so a caller can tell a dead pipe
    /// from a live one that lied; [`Session::request`] flattens that distinction into the registry's
    /// single transport error, which is what a deployment reports.
    fn round_trip(&mut self, build: impl FnOnce(u64) -> Value) -> Result<McpResponse, ClientError> {
        let id = self.ids.next_id().map_err(|error| ClientError::Broken {
            detail: error.to_string(),
        })?;
        let reply = self.server.reply_to(&build(id))?;
        parse_response(&reply).map_err(|error| ClientError::Protocol { error })
    }

    /// Sends a request and returns the protocol-level reply.
    fn request(&mut self, build: impl FnOnce(u64) -> Value) -> Result<McpResponse, RegistryError> {
        self.round_trip(build)
            .map_err(|error| transport_error(&error.to_string()))
    }
}

/// A [`McpTransport`] over one fixture process, shaped the way a deployment's would be.
struct FixtureTransport {
    /// `None` before the handshake and after `stop`.
    session: Arc<Mutex<Option<Session>>>,
}

impl FixtureTransport {
    /// Wraps a server that has not been handshaken yet.
    fn new(mode: &'static str) -> Self {
        Self {
            session: Arc::new(Mutex::new(Some(Session::start(mode)))),
        }
    }

    /// Runs `work` against the live session, mapping a lock or a missing session to a transport error.
    fn run<T>(
        session: &Arc<Mutex<Option<Session>>>,
        work: impl FnOnce(&mut Session) -> Result<T, RegistryError>,
    ) -> Result<T, RegistryError> {
        let mut guard = session
            .lock()
            .map_err(|_| transport_error("the fixture's lock was poisoned"))?;
        let session = guard
            .as_mut()
            .ok_or_else(|| transport_error("the fixture server is not running"))?;
        work(session)
    }
}

impl McpTransport for FixtureTransport {
    fn start(&self) -> BoxFut<Result<McpCatalogue, RegistryError>> {
        let session = Arc::clone(&self.session);
        Box::pin(async move {
            Self::run(&session, |session| {
                let handshake = session.request(initialize_request)?;
                handshake
                    .result()
                    .ok_or_else(|| transport_error("the fixture refused the handshake"))?;
                let listed = session.request(list_tools_request)?;
                let result = listed
                    .result()
                    .ok_or_else(|| transport_error("`tools/list` returned no result"))?;
                let tools =
                    parse_tools(result).map_err(|error| transport_error(&error.to_string()))?;
                Ok(catalogue_from(tools))
            })
        })
    }

    fn call(&self, tool: &str, arguments: Value) -> BoxFut<Result<McpCallResult, RegistryError>> {
        let session = Arc::clone(&self.session);
        let tool = tool.to_owned();
        Box::pin(async move {
            Self::run(&session, |session| {
                let reply = session.request(|id| call_tool_request(id, &tool, arguments))?;
                match reply {
                    McpResponse::Success { result, .. } => Ok(call_result(&result)),
                    // A JSON-RPC error on `tools/call` is the tool failing, which the model can read
                    // and work around, so it is reported as a tool failure rather than a dead server.
                    McpResponse::Failure { error, .. } => Err(RegistryError::ToolFailed {
                        name: tool.clone(),
                        message: error.message,
                    }),
                }
            })
        })
    }

    fn stop(&self) -> BoxFut<Result<(), RegistryError>> {
        let session = Arc::clone(&self.session);
        Box::pin(async move {
            let mut guard = session
                .lock()
                .map_err(|_| transport_error("the fixture's lock was poisoned"))?;
            // Dropping the `Session` drops the `Server`, which kills and reaps the child.
            *guard = None;
            Ok(())
        })
    }
}

/// Turns a `tools/list` result into the registry's catalogue.
fn catalogue_from(tools: Vec<McpProtocolTool>) -> McpCatalogue {
    let mut catalogue = McpCatalogue::default();
    for tool in tools {
        catalogue.tools.insert(
            tool.name.clone(),
            RegistryTool {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
            },
        );
    }
    catalogue
}

/// Reads a `tools/call` result into a [`McpCallResult`].
///
/// This is the one step of the client that exists nowhere in the crate: `parse_tools` reads a
/// `tools/list` result, but no function reads a call result, so every transport would invent its own
/// reading of `content` and `isError`. The shape is asserted end-to-end by
/// `a_tool_call_through_the_registry_returns_the_content_blocks`; if the crate grows a parser, that
/// test is what it has to satisfy.
fn call_result(result: &Value) -> McpCallResult {
    McpCallResult {
        content: result
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        // Absent means "no failure": the field is optional in the protocol, and a server that omits it
        // has reported nothing wrong.
        is_error: result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// Names the fixture in a transport failure, so a message says which server failed.
fn transport_error(reason: &str) -> RegistryError {
    RegistryError::Transport {
        name: "fixture".to_owned(),
        reason: reason.to_owned(),
    }
}

// -- 1. The handshake ---------------------------------------------------------------------------------

#[test]
fn the_initialize_handshake_reports_a_protocol_version_and_capabilities() {
    let mut server = Server::start(NORMAL);
    let ids = IdAllocator::new();
    let id = ids.next_id().expect("the allocator has ids");

    let reply = server
        .reply_to(&initialize_request(id))
        .expect("the fixture answers the handshake");

    let response = parse_response(&reply).expect("the reply is a legal JSON-RPC reply");
    // The id match is what makes every later assertion meaningful: a reply that answers a different
    // request would otherwise satisfy any check on its contents.
    assert_eq!(
        response.id(),
        Some(id),
        "the reply must answer this request"
    );

    let result = response
        .result()
        .expect("`initialize` answers with a result");
    assert_eq!(
        result["protocolVersion"], PROTOCOL_VERSION,
        "the server must state the protocol revision it negotiated"
    );
    assert!(
        result["capabilities"]["tools"].is_object(),
        "a server offering tools must advertise the capability: {result}"
    );
    assert_eq!(result["capabilities"]["tools"]["listChanged"], json!(false));
}

// -- 2. The tool list ---------------------------------------------------------------------------------

#[test]
fn the_tool_list_reports_names_descriptions_and_input_schemas() {
    let mut server = Server::start(NORMAL);
    let ids = IdAllocator::new();
    let handshake = ids.next_id().expect("the allocator has ids");
    server
        .reply_to(&initialize_request(handshake))
        .expect("the fixture answers the handshake");

    let listed = ids.next_id().expect("the allocator has ids");
    let reply = server
        .reply_to(&list_tools_request(listed))
        .expect("the fixture answers the tool list");

    let response = parse_response(&reply).expect("the reply is a legal JSON-RPC reply");
    assert_eq!(response.id(), Some(listed));
    let tools = parse_tools(
        response
            .result()
            .expect("`tools/list` answers with a result"),
    )
    .expect("the tool list is well formed");

    assert_eq!(tools.len(), 1, "the fixture publishes exactly one tool");
    let tool = &tools[0];
    assert_eq!(tool.name, "echo");
    assert_eq!(tool.description, "Echo the text back");
    // The schema is asserted field by field rather than as "an object": a client that dropped
    // `properties` or `required` would still hand the model a plausible-looking schema that cannot
    // describe a call.
    assert_eq!(tool.input_schema["type"], "object");
    assert_eq!(tool.input_schema["properties"]["text"]["type"], "string");
    assert_eq!(tool.input_schema["required"], json!(["text"]));
    assert_eq!(
        tool.qualified("fixture"),
        "fixture__echo",
        "the registry name must carry the server namespace"
    );
}

// -- 3. The tool call ---------------------------------------------------------------------------------

#[test]
fn a_tool_call_returns_the_servers_content_blocks() {
    let mut server = Server::start(NORMAL);
    let ids = IdAllocator::new();
    server
        .reply_to(&initialize_request(ids.next_id().expect("an id")))
        .expect("the fixture answers the handshake");

    // The argument is distinctive because the fixture echoes it: if it comes back, the arguments
    // travelled through the pipe instead of being replaced by a constant on either side.
    let called = ids.next_id().expect("an id");
    let reply = server
        .reply_to(&call_tool_request(
            called,
            "echo",
            json!({"text": "hello from the test"}),
        ))
        .expect("the fixture answers the call");

    let response = parse_response(&reply).expect("the reply is a legal JSON-RPC reply");
    assert_eq!(response.id(), Some(called));
    let result = response
        .result()
        .expect("`tools/call` answers with a result");

    let content = result["content"]
        .as_array()
        .expect("the result carries a content array");
    assert_eq!(content.len(), 1, "the fixture returns one block");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(
        content[0]["text"], "hello from the test",
        "the server saw the arguments the client sent"
    );
    assert_eq!(result["isError"], json!(false));
}

// -- 4. A server-reported error -----------------------------------------------------------------------

#[test]
fn a_json_rpc_error_reply_is_reported_as_a_failure_value() {
    let mut server = Server::start(JSON_RPC_ERROR);
    let ids = IdAllocator::new();
    server
        .reply_to(&initialize_request(ids.next_id().expect("an id")))
        .expect("the fixture answers the handshake");

    let called = ids.next_id().expect("an id");
    let reply = server
        .reply_to(&call_tool_request(called, "echo", json!({"text": "x"})))
        .expect("an error is still a reply, not a broken pipe");

    // Reaching this line without a panic is half the assertion: a server saying "no" must not take
    // the client down. The other half is that the refusal is a *value* the caller can branch on.
    let response = parse_response(&reply).expect("a JSON-RPC error is a legal reply");
    assert_eq!(response.id(), Some(called), "the error names its request");
    assert!(
        response.result().is_none(),
        "a failed call has no result to read"
    );

    let error = response.error().expect("the reply carries an error object");
    assert_eq!(error.code, -32602, "the server's own code must survive");
    assert_eq!(error.message, "invalid arguments");
    assert_eq!(
        error.data,
        Some(json!({"field": "text"})),
        "structured error detail must not be dropped"
    );
    assert!(
        !error.is_method_not_found(),
        "-32602 is an argument error, not a missing capability"
    );
}

// -- 5. The process exits mid-session -----------------------------------------------------------------

#[test]
fn a_server_that_exits_mid_session_reports_a_disconnect_instead_of_hanging() {
    let mut server = Server::start(EXIT_MID_SESSION);
    let ids = IdAllocator::new();
    let handshake = ids.next_id().expect("an id");
    server
        .reply_to(&initialize_request(handshake))
        .expect("the fixture answers the handshake before it dies");

    // The fixture exits on this request without answering it, which is the shape of a crash between
    // two calls: the connection was healthy a moment ago and now nothing will ever arrive.
    let listed = ids.next_id().expect("an id");
    let error = server
        .receive_frame_after(&list_tools_request(listed))
        .expect_err("a dead server cannot answer");

    // Specifically `Closed` with no leftover bytes: the client must report the disconnection as soon
    // as the pipe reaches EOF. A `TimedOut` here would mean the client sat on a dead pipe until its
    // deadline, which is the hang this test exists to catch.
    assert!(
        matches!(error, ClientError::Closed { leftover: 0 }),
        "expected a clean end of stream, got {error:?}"
    );
}

// -- 6. The process exits mid-frame -------------------------------------------------------------------

#[test]
fn a_server_that_dies_mid_frame_leaves_an_incomplete_frame() {
    let mut server = Server::start(DIE_MID_FRAME);
    let ids = IdAllocator::new();
    server
        .reply_to(&initialize_request(ids.next_id().expect("an id")))
        .expect("the fixture answers the handshake");

    // Half a message is the case a naive client either blocks on forever (waiting for the newline
    // that never comes) or feeds to the JSON parser (producing a parse error that blames the wrong
    // layer). The distinction the client has to keep is "stream ended" versus "stream ended mid-frame".
    let listed = ids.next_id().expect("an id");
    let error = server
        .receive_frame_after(&list_tools_request(listed))
        .expect_err("a truncated frame is not a reply");

    match error {
        ClientError::Closed { leftover } => assert!(
            leftover > 0,
            "the half-written frame must be visible as buffered bytes"
        ),
        other => panic!("expected the stream to end, got {other:?}"),
    }
}

// -- 7. A reply that is not JSON ----------------------------------------------------------------------

#[test]
fn a_non_json_reply_is_rejected_as_a_parse_failure() {
    let mut server = Server::start(GARBAGE);
    let ids = IdAllocator::new();
    server
        .reply_to(&initialize_request(ids.next_id().expect("an id")))
        .expect("the fixture answers the handshake");

    let listed = ids.next_id().expect("an id");
    let error = server
        .reply_to(&list_tools_request(listed))
        .expect_err("a non-JSON line is not a reply");

    // The failure has to be reported where it happened. A `Protocol` error here would mean the bytes
    // were parsed and then rejected, which would send an operator looking at the server's schema
    // instead of at whatever wrote a plain-text line onto the wire.
    match error {
        ClientError::MalformedJson { detail } => assert!(
            detail.contains("not valid JSON"),
            "the error must say the reply could not be parsed: {detail}"
        ),
        other => panic!("expected a parse failure, got {other:?}"),
    }
}

// -- 8. A reply that is JSON but not JSON-RPC ---------------------------------------------------------

#[test]
fn a_json_reply_that_is_not_an_object_is_rejected_as_a_protocol_violation() {
    let mut session = Session::start(SCALAR);
    session
        .round_trip(initialize_request)
        .expect("the fixture answers the handshake");

    let error = session
        .round_trip(list_tools_request)
        .expect_err("a bare string is valid JSON but not a JSON-RPC reply");

    // Parsing succeeded and validation refused it, so the two failure layers are distinguishable:
    // this is `Protocol`, while a non-JSON line is `MalformedJson`. Collapsing them would make a
    // server bug look like a broken pipe, and an operator would debug the wrong component.
    assert_eq!(
        error,
        ClientError::Protocol {
            error: McpProtocolError::ResponseNotAnObject,
        }
    );
}

// -- 9 and 10. The registry, end to end ----------------------------------------------------------------

#[tokio::test]
async fn a_tool_call_through_the_registry_returns_the_content_blocks() {
    let registry = McpRegistry::new();
    registry
        .add("fixture", Arc::new(FixtureTransport::new(NORMAL)))
        .expect("the server name is free");

    let catalogue = registry
        .start("fixture")
        .await
        .expect("the fixture handshakes");
    assert!(
        catalogue.tool("echo").is_some(),
        "the catalogue must carry the server's tools"
    );
    assert_eq!(
        registry.status("fixture"),
        Some(girder_core::McpStatus::Ready)
    );

    let result = registry
        .call("fixture__echo", json!({"text": "hello from the test"}))
        .await
        .expect("the call succeeds");
    assert!(!result.is_error);
    assert_eq!(result.content.len(), 1);
    assert_eq!(result.content[0]["type"], "text");
    assert_eq!(result.content[0]["text"], "hello from the test");
}

#[tokio::test]
async fn a_server_flagged_tool_failure_reaches_the_registry_as_a_tool_error() {
    let registry = McpRegistry::new();
    registry
        .add("fixture", Arc::new(FixtureTransport::new(TOOL_ERROR)))
        .expect("the server name is free");
    registry
        .start("fixture")
        .await
        .expect("the fixture handshakes");

    // The server answered — the connection is healthy — and said the *tool* failed. That distinction
    // is the whole reason `isError` is carried separately from a JSON-RPC `error`: the caller reports
    // a broken tool, not a broken server, and the model can try something else.
    let error = registry
        .call("fixture__echo", json!({"text": "hello"}))
        .await
        .expect_err("a flagged failure is not a result");

    assert_eq!(
        error,
        RegistryError::ToolFailed {
            name: "fixture__echo".to_owned(),
            message: "the echo tool broke".to_owned(),
        }
    );
}
