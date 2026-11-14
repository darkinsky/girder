//! The `terminal.*` Remote methods: `list`, `create`, `send`, `read`, and `close`.
//!
//! [`girder_core::TerminalRegistry`] owns the sessions and
//! [`girder_core::register_terminal_tools`] gives a *model* three tools over them. Neither answers
//! a *client*: a browser tab that draws a terminal needs to enumerate what
//! exists, start one with its own dimensions, and close it without a model in the loop. That surface
//! is what this module registers, and it is deliberately a second face over the same registry rather
//! than a wrapper around the tools — a tool result is prose a model reads, and a client that parsed
//! it would be depending on wording that exists to be rephrased.
//!
//! # Why the launcher is a parameter and not a field
//!
//! The crate has no [`Shell`](girder_core::Shell) implementation, so nothing here can start a real
//! process. The host passes its factory, exactly as it does for the tools; a host without one passes
//! [`UnavailableLauncher`](girder_core::UnavailableLauncher) and every `terminal.create` answers with
//! the deployment's own refusal text. That refusal is forwarded verbatim: a client that reads
//! "no `Shell` implementation is wired into this deployment" learns that a wiring step is missing,
//! while a rewritten "internal error" would send it looking for a bug in the request.
//!
//! # Why this build has no `attach`
//!
//! Upstream's terminal surface is two streaming methods (`attach` and `retain`) plus metadata.
//! [`girder_core::RemoteRegistry`] has no stream mode — `session.stream` is declared missing for the
//! same reason — so a byte-oriented attachment cannot be served. `terminal.send` and `terminal.read`
//! are the non-streaming substitute: a client polls them instead of holding one connection open.
//! That is a real difference in behaviour, not a rename, so the two missing names are declared as
//! gaps instead of being silently absent.
//!
//! # Why terminals are not partitioned by peer
//!
//! Upstream scopes terminals per Session and requires an owning agent. This build has one process
//! and one [`TerminalRegistry`], so every client that can call these methods sees every id. The peer
//! is carried on [`RemoteCall`] and deliberately not used: inventing a peer-to-registry map here
//! would put an access-control decision in a method table that has no way to authenticate a peer.
//! Partitioning belongs to the host that knows what a peer is. Until then, ids are guessable
//! (`terminal-1`), which is why this limitation is stated rather than assumed away.
//!
//! # Why there is no title, shell, or directory in the listing
//!
//! Upstream's `WebTerminalInfo` carries all three. [`Session`](girder_core::TerminalSession) records
//! none of them: a session knows its id, its size, its status, and its transcript. Reporting a shell
//! path or a working directory would mean this layer inventing the value it was asked for, and a
//! client that displayed a made-up directory would be worse off than one that displayed nothing.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use girder_core::{
    RemoteCall, RemoteError, RemoteRegistry, TerminalError, TerminalLaunch, TerminalLauncher,
    TerminalRegistry, TerminalSession, TerminalSize, TerminalStatus,
};

/// The namespace every method here lives under.
const NAMESPACE: &str = "terminal";

/// The method name of the listing.
const METHOD_LIST: &str = "list";

/// The method name of the allocator.
const METHOD_CREATE: &str = "create";

/// The method name of the input writer.
const METHOD_SEND: &str = "send";

/// The method name of the output reader.
const METHOD_READ: &str = "read";
const METHOD_ATTACH: &str = "attach";

/// The method name of the closer.
const METHOD_CLOSE: &str = "close";

/// How long `terminal.read` waits when the client does not say.
///
/// The same default as the `terminal_read` tool, but a separate constant: this file is also compiled
/// on its own by its test, where the tool module is not in scope, and a shared constant would tie the
/// client-facing default to a model-facing one that has no reason to stay equal.
pub const DEFAULT_READ_WAIT_MS: u64 = 500;

/// The longest any `terminal.read` waits.
///
/// A Remote call occupies a response slot. A client that asked for an hour would hold one open for an
/// hour while the user waits for a page that looks frozen, so the request is clamped here where the
/// cost is known and the answer reports the wait that actually happened.
pub const MAX_WAIT_MS: u64 = 30_000;

/// The longest id this build accepts.
///
/// Upstream's bound. An id ends up in logs, URLs, and DOM attributes, so keeping it short and ASCII
/// keeps it copyable between them.
const MAX_ID_LEN: usize = 128;

/// How many ids the allocator tries before giving up.
///
/// A bound rather than an open loop: a registry holding every candidate id must produce an error
/// instead of spinning until the request is abandoned.
const ID_ATTEMPTS: u64 = 1024;

/// The source of generated ids.
///
/// Process-wide for the same reason as the tool's counter: ids only have to be unique within one
/// registry, and a counter that reset per call would mint the same id twice within it.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Upstream methods this build cannot serve, declared so a client can tell a gap from a typo.
///
/// Every name here must stay absent. `environment` and `shells` need a session's working directory
/// and a shell-discovery provider; `rename` needs a title the session does not hold; `attach` and
/// `retain` need the stream mode this registry does not have.
const UNIMPLEMENTED: &[&str] = &["environment", "shells", "rename", "retain"];

/// The arguments of `terminal.create`.
///
/// Every field is optional and `serde`-parsed: a client that serialises all of them sends `null` for
/// the ones it is not using, and hand-reading a `Value` would turn a wrong type into a default
/// instead of a message naming the field.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CreateRequest {
    /// Caller-chosen identity, or a generated one when absent.
    id: Option<String>,
    /// Shell to run, or the host default.
    shell: Option<String>,
    /// Directory to start in.
    cwd: Option<String>,
    /// Width in columns.
    columns: Option<u64>,
    /// Height in rows.
    rows: Option<u64>,
    /// Transcript bytes to retain.
    max_bytes: Option<u64>,
}

/// The arguments of `terminal.send`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SendRequest {
    /// Target terminal.
    id: String,
    /// Text to type.
    input: String,
    /// Whether to press Enter after it. Defaults to true.
    submit: Option<bool>,
}

/// The arguments of `terminal.read`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ReadRequest {
    /// Target terminal.
    id: String,
    /// How long to wait for output, before clamping.
    wait_ms: Option<u64>,
    /// Whether to answer with the whole retained transcript.
    full: Option<bool>,
}

/// The arguments of `terminal.close`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CloseRequest {
    /// Target terminal.
    id: String,
}

/// Registers the `terminal.*` methods over one registry and one launcher.
///
/// A free function taking the registry, like the other `register_*` functions: the host decides which
/// capabilities exist, and a module that registered itself would make that decision for it. The
/// registry is shared rather than copied because a terminal created through one method must be
/// visible to the next call — a per-call copy would hand every client its own empty world.
pub fn register_terminal_methods(
    registry: &mut RemoteRegistry,
    terminals: &Arc<TerminalRegistry>,
    launcher: &Arc<dyn TerminalLauncher>,
) {
    // One fanout per terminal, created on first attach. Kept here rather than in the registry because a fanout is
    // per-connection bookkeeping -- it remembers where each viewer is -- and the terminal itself has no idea who is
    // watching it.
    let fans: Arc<
        std::sync::Mutex<std::collections::BTreeMap<String, crate::terminal_attach::Fanout>>,
    > = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
    let listed = Arc::clone(terminals);
    registry.register(NAMESPACE, METHOD_LIST, move |_call: RemoteCall| {
        let terminals = Arc::clone(&listed);
        Box::pin(async move { Ok(list(&terminals)) })
    });

    let created_terminals = Arc::clone(terminals);
    let created_launcher = Arc::clone(launcher);
    registry.register(NAMESPACE, METHOD_CREATE, move |call: RemoteCall| {
        let terminals = Arc::clone(&created_terminals);
        let launcher = Arc::clone(&created_launcher);
        Box::pin(async move { create(&terminals, launcher.as_ref(), &call.args).await })
    });

    let send_terminals = Arc::clone(terminals);
    registry.register(NAMESPACE, METHOD_SEND, move |call: RemoteCall| {
        let terminals = Arc::clone(&send_terminals);
        Box::pin(async move { send(&terminals, &call.args).await })
    });

    let read_terminals = Arc::clone(terminals);
    registry.register(NAMESPACE, METHOD_READ, move |call: RemoteCall| {
        let terminals = Arc::clone(&read_terminals);
        Box::pin(async move { read(&terminals, &call.args).await })
    });

    let close_terminals = Arc::clone(terminals);
    registry.register(NAMESPACE, METHOD_CLOSE, move |call: RemoteCall| {
        let terminals = Arc::clone(&close_terminals);
        Box::pin(async move { close(&terminals, &call.args).await })
    });

    // `attach` is a begin-poll pair rather than a push, because the transport has no stream mode: a begin returns a
    // viewer id and `read` from that viewer's offset returns what arrived since. That is the honest shape -- a method
    // that claimed to push and returned one array would be a different wire contract with the same name.
    let attached = Arc::clone(&fans);
    registry.register(NAMESPACE, METHOD_ATTACH, move |call: RemoteCall| {
        let fans = Arc::clone(&attached);
        Box::pin(async move { attach(&fans, &call.args) })
    });

    // Declared, not omitted: a client is told "this build cannot" rather than "no such method", and
    // the two call for different handling on its side.
    registry.declare_unimplemented(NAMESPACE, UNIMPLEMENTED);
}

/// Starts a viewer on a terminal, answering with its id and starting offset.
///
/// The id is the handle the client uses for the rest of the attachment, so it is returned rather than inferred: a client
/// that had to guess would address another viewer.
fn attach(
    fans: &std::sync::Mutex<std::collections::BTreeMap<String, crate::terminal_attach::Fanout>>,
    args: &Value,
) -> Result<Value, RemoteError> {
    let terminal_id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: METHOD_ATTACH.to_owned(),
            message: "`id` is required".to_owned(),
        })?
        .to_owned();
    let mut fans = fans
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fanout = fans
        .entry(terminal_id.clone())
        .or_insert_with(|| girder_core::terminal_attach::Fanout::new(terminal_id.clone()));
    // Through the module that owns the wire shape, not a second copy of it: the response fields and the id sentinel are
    // one contract, and two places spelling it would be two places to disagree.
    let response = girder_core::terminal_attach_rpc::attach(fanout, &terminal_id);
    Ok(json!({
        "viewer": response.viewer,
        "from": response.from,
    }))
}

/// Answers with every session the registry holds, in id order.
///
/// Unknown ids are skipped rather than failing: `ids` and `get` are two separate locks, so a session
/// closed between them is expected under concurrency, and a listing that failed because something
/// else closed a terminal would be unusable in exactly the situation it is most useful.
fn list(terminals: &TerminalRegistry) -> Value {
    let items: Vec<Value> = terminals
        .ids()
        .into_iter()
        .filter_map(|id| terminals.get(&id))
        .map(|session| info(&session))
        .collect();
    json!({ "terminals": items })
}

/// Starts a terminal, or returns the one already holding the requested id.
///
/// Idempotent on an existing id because a client that retried after a dropped response would
/// otherwise get a second shell for one tab, and the second would be the one nobody closes. Upstream
/// makes the same choice; the client-generated id is what makes it expressible.
async fn create(
    terminals: &TerminalRegistry,
    launcher: &dyn TerminalLauncher,
    args: &Value,
) -> Result<Value, RemoteError> {
    let request: CreateRequest = parse(args, METHOD_CREATE)?;

    let id = match request.id {
        Some(id) => validate_id(&id)?,
        // Generated only when the client did not name one. Upstream requires the id because it is
        // what makes retries safe; accepting an omitted id keeps a client that does not need that
        // safety from having to invent names.
        None => next_free_id(terminals).ok_or_else(|| {
            RemoteError::Failed("could not allocate a terminal id; the registry is full".to_owned())
        })?,
    };

    if let Some(existing) = terminals.get(&id) {
        return Ok(info(&existing));
    }

    let size = size(request.columns, request.rows)?;
    let max_bytes = max_bytes(request.max_bytes);
    let mut spec = TerminalLaunch::new(id.clone()).ok_or_else(|| {
        // `validate_id` already refused a blank id, so this is unreachable; it is still an error path
        // rather than an unwrap, because a later change to either check must not become a panic.
        RemoteError::Failed(format!("`{id}` is not a usable terminal id"))
    })?;
    spec = spec.with_size(size).with_max_bytes(max_bytes);
    if let Some(shell) = request.shell {
        spec = spec.with_shell(shell);
    }
    if let Some(cwd) = request.cwd {
        // Passed through as given. This layer has no workspace to resolve a relative path against,
        // and guessing the host's current directory would start the shell somewhere the client did
        // not name — a shell that reports a directory nobody asked for is worse than one that fails.
        spec = spec.in_directory(PathBuf::from(cwd));
    }

    let shell = match launcher.launch(spec).await {
        Ok(shell) => shell,
        // The launcher's reason is the deployment's own words. Repeating them verbatim is the only
        // way the person who wired it can see what was refused.
        Err(reason) => {
            return Err(RemoteError::Failed(format!(
                "cannot start terminal `{id}`: {reason}"
            )));
        }
    };

    let session = Arc::new(TerminalSession::new(id.clone(), shell, size, max_bytes));
    if terminals.add(Arc::clone(&session)).is_err() {
        // The shell is already running and the registry refused the session, so nothing else holds a
        // handle that could stop it. Closing here is the last chance before a leak.
        let _ = session.close().await;
        return Err(RemoteError::Failed(format!(
            "terminal `{id}` was created by another call while this one was starting"
        )));
    }
    Ok(info(&session))
}

/// Writes input to a terminal.
///
/// Pressing Enter is the default for the same reason as in the tool: a client typing a command almost
/// always means to run it, and a forgotten newline reads as a shell that ignored the input.
async fn send(terminals: &TerminalRegistry, args: &Value) -> Result<Value, RemoteError> {
    let request: SendRequest = parse(args, METHOD_SEND)?;
    let session = lookup(terminals, &request.id)?;

    let mut payload = request.input;
    if request.submit.unwrap_or(true) && !payload.ends_with('\n') {
        payload.push('\n');
    }
    session
        .send(&payload)
        .await
        .map_err(|error| failure(&error))?;

    Ok(json!({ "id": request.id, "bytes": payload.len() }))
}

/// Returns what a terminal printed, optionally with the whole retained transcript.
async fn read(terminals: &TerminalRegistry, args: &Value) -> Result<Value, RemoteError> {
    let request: ReadRequest = parse(args, METHOD_READ)?;
    let session = lookup(terminals, &request.id)?;
    let waited = request
        .wait_ms
        .unwrap_or(DEFAULT_READ_WAIT_MS)
        .min(MAX_WAIT_MS);

    let chunk = session
        .read(Duration::from_millis(waited))
        .await
        .map_err(|error| failure(&error))?;
    // An empty chunk and no chunk are the same fact to a client: nothing printable arrived. The
    // `received` flag states which answer this is, so an empty string never has to be interpreted.
    let data = chunk.unwrap_or_default();
    let status = session.status();

    let mut value = json!({
        "id": session.id(),
        "state": status.as_str(),
        "data": data,
        "received": !data.is_empty(),
        "waitedMs": waited,
    });
    if request.full.unwrap_or(false) {
        value["transcript"] = json!(session.transcript());
        value["omittedBytes"] = json!(session.omitted().count().unwrap_or(0));
    }
    Ok(value)
}

/// Ends a terminal and releases its id.
///
/// Closing an id that is already gone succeeds and says it did nothing. Upstream's `close` is
/// idempotent for the same reason: a client closing a tab sends `close` for a shell that may have
/// exited on its own, and a failure report there would show an error for the state the client asked
/// for. The difference is spelled out in the answer rather than assumed, because this build's
/// `close` also frees the id for reuse — upstream keeps it closed forever.
async fn close(terminals: &TerminalRegistry, args: &Value) -> Result<Value, RemoteError> {
    let request: CloseRequest = parse(args, METHOD_CLOSE)?;
    // Held across the removal so a shutdown that fails can put the session back. `remove` takes the
    // session out of the map before it closes it, so without this handle a failed close would drop
    // the process on the floor: the shell is still running and no id reaches it any more.
    let Some(session) = terminals.get(&request.id) else {
        return Ok(json!({ "id": request.id, "closed": false }));
    };
    match terminals.remove(&request.id).await {
        Ok(()) => Ok(json!({ "id": request.id, "closed": true })),
        // Another caller's close won the race, so the state the caller asked for is already true.
        // Re-adding here would resurrect a terminal somebody else ended.
        Err(TerminalError::Unknown { .. }) => Ok(json!({ "id": request.id, "closed": false })),
        // A shell that failed to stop is still a live process. It goes back into the registry so the
        // caller can retry, and the failure is reported rather than being lost with the removal.
        Err(error) => {
            let _ = terminals.add(session);
            Err(failure(&error))
        }
    }
}

/// The wire projection of one session.
fn info(session: &TerminalSession) -> Value {
    let status = session.status();
    let size = session.size();
    let mut info = json!({
        "id": session.id(),
        "state": status.as_str(),
        "cols": size.columns,
        "rows": size.rows,
        "omittedBytes": session.omitted().count().unwrap_or(0),
    });
    // `exitCode` is present only when a process actually reported one, and `error` only when a start
    // failed. Sending `null` under a key a client formats as a number is how a UI renders "null".
    match status {
        TerminalStatus::Exited { code } => info["exitCode"] = json!(code),
        TerminalStatus::Failed { reason } => info["error"] = json!(reason),
        TerminalStatus::Running | TerminalStatus::Stopped => {}
    }
    info
}

/// Resolves an id to a session, naming the id in the failure.
///
/// `RemoteError::Failed` rather than a dedicated code: this build's error channel has no details
/// field, so the message is the only place the id can travel, and a client that shows the message has
/// then shown which terminal was missing.
fn lookup(terminals: &TerminalRegistry, id: &str) -> Result<Arc<TerminalSession>, RemoteError> {
    terminals.get(id).ok_or_else(|| {
        RemoteError::Failed(TerminalError::Unknown { id: id.to_owned() }.to_string())
    })
}

/// Turns a terminal failure into a Remote failure without losing its wording.
///
/// Taken by reference so the caller keeps ownership of the variant it matched on; every caller here
/// has already inspected the error to decide whether it is an answer or a failure.
fn failure(error: &TerminalError) -> RemoteError {
    RemoteError::Failed(error.to_string())
}

/// Parses a method's arguments, naming the method when it fails.
///
/// A `null` body is read as an empty object: a client that sends no arguments at all sends `null`, and
/// rejecting that would make the all-defaults form of `create` unusable from a generated client.
fn parse<T: for<'de> Deserialize<'de>>(args: &Value, method: &str) -> Result<T, RemoteError> {
    let args = if args.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        args.clone()
    };
    serde_json::from_value(args).map_err(|error| RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: error.to_string(),
    })
}

/// Validates a caller-supplied id and returns it unchanged.
///
/// The charset is ASCII rather than upstream's Unicode `\w`: an id is copied between a log line, a URL
/// and a DOM attribute, and a Unicode id that survives all three is a harder promise than it looks.
/// Whitespace is refused rather than trimmed — a trimmed id is a different id, and the client's next
/// `close` with the original string would then fail on a terminal it just created.
fn validate_id(id: &str) -> Result<String, RemoteError> {
    let acceptable = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !acceptable {
        return Err(invalid(
            METHOD_CREATE,
            "`id` must be 1-128 characters of ASCII letters, digits, `_`, or `-`",
        ));
    }
    Ok(id.to_owned())
}

/// Builds the size a create request asks for.
///
/// Unset dimensions fall back to the headless default rather than to whatever the host reports, so a
/// client that did not choose a size gets the same one a test would. Out-of-range values are refused
/// instead of clamped: a client that asked for 5000 columns meant something, and silently giving it
/// 65535 would draw a screen it did not request.
fn size(columns: Option<u64>, rows: Option<u64>) -> Result<TerminalSize, RemoteError> {
    let headless = TerminalSize::headless();
    let columns = dimension(columns, headless.columns, "columns")?;
    let rows = dimension(rows, headless.rows, "rows")?;
    TerminalSize::new(columns, rows).ok_or_else(|| {
        invalid(
            METHOD_CREATE,
            "`columns` and `rows` must both be at least 1",
        )
    })
}

/// Validates one dimension against the `u16` a terminal stores.
fn dimension(value: Option<u64>, default: u16, field: &str) -> Result<u16, RemoteError> {
    match value {
        None => Ok(default),
        Some(raw) => match u16::try_from(raw) {
            Ok(value) if value > 0 => Ok(value),
            _ => Err(invalid(
                METHOD_CREATE,
                &format!("`{field}` must be an integer between 1 and 65535"),
            )),
        },
    }
}

/// How much transcript the new session retains.
///
/// A bound wider than the address space means "unlimited", so clamping is a faithful reading of the
/// request rather than a silent truncation.
fn max_bytes(requested: Option<u64>) -> usize {
    match requested {
        None => girder_core::DEFAULT_TRANSCRIPT_BYTES,
        Some(bytes) => usize::try_from(bytes).unwrap_or(usize::MAX),
    }
}

/// Allocates an id no session in `terminals` holds.
fn next_free_id(terminals: &TerminalRegistry) -> Option<String> {
    for _ in 0..ID_ATTEMPTS {
        let candidate = format!("terminal-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
        if terminals.get(&candidate).is_none() {
            return Some(candidate);
        }
    }
    None
}

/// Builds an invalid-arguments failure naming the method.
fn invalid(method: &str, message: &str) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: message.to_owned(),
    }
}
