//! Tests for the terminal tools: `terminal_start`, `terminal_send`, and `terminal_read`.
//!
//! The tools are a thin surface over [`girder_core::TerminalRegistry`], and the properties worth
//! pinning are the ones a plausible implementation gets wrong:
//!
//! - `terminal_start` must return an id that is already registered, or the model's next call fails
//!   on a race it cannot see
//! - `terminal_send` must refuse an unknown id instead of dropping the input
//! - `terminal_read` must accumulate into the transcript, and must distinguish "nothing yet" from
//!   "the terminal is gone" — otherwise a slow command reads as a dead shell
//! - a deployment with no shell must say so, rather than hand out a session that answers nothing
//!
//! The shells here are fakes: what is under test is the surface, and a real pty would add a platform
//! dependency and a timing race to properties that have nothing to do with either.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use girder_core::{
    AgentId, BoxFut, ContentBlock, PermissionCatalogue, PermissionState, Shell, TerminalError,
    TerminalLaunch, TerminalLauncher, TerminalRegistry, ToolCallId, ToolDefinition, ToolExecution,
    ToolRegistry, ToolResult, ToolRunContext, UnavailableLauncher, register_terminal_tools,
    terminal_read_tool, terminal_send_tool, terminal_start_tool,
};
use tokio_util::sync::CancellationToken;

// ---- fixtures ---------------------------------------------------------------------------------

/// Names a scratch directory uniquely enough for tests that may run in one process at once.
fn scratch(name: &str) -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-tt-{name}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// A shell that records what it was told and replays a fixed queue of output.
///
/// Deterministic on purpose: a fake that produced output on a timer would make every read test a
/// race, and the properties under test are about what the tools do with a chunk, not when it arrives.
struct FakeShell {
    /// Inputs received, in order.
    written: std::sync::Mutex<Vec<String>>,
    /// Output chunks queued for the next reads.
    pending: std::sync::Mutex<std::collections::VecDeque<String>>,
    /// Whether `close` was called.
    closed: std::sync::atomic::AtomicBool,
}

impl FakeShell {
    fn new(chunks: &[&str]) -> Self {
        Self {
            written: std::sync::Mutex::new(Vec::new()),
            pending: std::sync::Mutex::new(
                chunks.iter().map(|chunk| (*chunk).to_owned()).collect(),
            ),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn written(&self) -> Vec<String> {
        self.written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn was_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

impl Shell for FakeShell {
    fn write(&self, input: &str) -> BoxFut<Result<(), TerminalError>> {
        let input = input.to_owned();
        self.written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(input);
        Box::pin(async move { Ok(()) })
    }

    fn read(&self, _wait: Duration) -> BoxFut<Result<Option<String>, TerminalError>> {
        // No sleep: the queue is the answer, and a real shell at a prompt returns as soon as it knows
        // nothing is there, which is the same shape.
        let chunk = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        Box::pin(async move { Ok(chunk) })
    }

    fn close(&self) -> BoxFut<Result<(), TerminalError>> {
        self.closed.store(true, Ordering::Relaxed);
        Box::pin(async move { Ok(()) })
    }
}

/// A launcher that hands out one shell and remembers the specs it was asked for.
struct FakeLauncher {
    shell: Arc<FakeShell>,
    specs: std::sync::Mutex<Vec<TerminalLaunch>>,
}

impl FakeLauncher {
    fn new(shell: Arc<FakeShell>) -> Self {
        Self {
            shell,
            specs: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn specs(&self) -> Vec<TerminalLaunch> {
        self.specs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl TerminalLauncher for FakeLauncher {
    fn launch(&self, spec: TerminalLaunch) -> BoxFut<Result<Arc<dyn Shell>, String>> {
        self.specs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(spec);
        let shell: Arc<dyn Shell> = self.shell.clone();
        Box::pin(async move { Ok(shell) })
    }
}

/// One mounted surface plus the pieces a test inspects afterwards.
struct Tools {
    registry: Arc<TerminalRegistry>,
    launcher: Arc<FakeLauncher>,
    start: ToolDefinition,
    send: ToolDefinition,
    read: ToolDefinition,
}

impl Tools {
    fn new(shell: &Arc<FakeShell>) -> Self {
        let registry = Arc::new(TerminalRegistry::new());
        let launcher = Arc::new(FakeLauncher::new(Arc::clone(shell)));
        Self {
            start: terminal_start_tool(Arc::clone(&registry), launcher.clone()),
            send: terminal_send_tool(Arc::clone(&registry)),
            read: terminal_read_tool(Arc::clone(&registry)),
            registry,
            launcher,
        }
    }

    /// Starts the default terminal and returns the id it printed.
    async fn started(&self) -> String {
        let result = run(&self.start, serde_json::json!({})).await;
        assert!(!result.is_error, "{}", text_of(&result));
        let ids = self.registry.ids();
        assert_eq!(ids.len(), 1, "`terminal_start` must register exactly one");
        ids.into_iter().next().expect("one id")
    }
}

/// Runs a tool with the default context.
async fn run(tool: &ToolDefinition, args: serde_json::Value) -> ToolResult {
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("the tool answered")
}

/// The text a result renders to, whichever variant carried it.
fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// ---- terminal_start ---------------------------------------------------------------------------

#[tokio::test]
async fn starting_a_terminal_returns_a_registered_id() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);

    let text = text_of(&run(&tools.start, serde_json::json!({})).await);
    let ids = tools.registry.ids();
    assert_eq!(ids.len(), 1, "{text}");
    // A receipt naming an id the registry does not hold is the model's next call failing for no
    // visible reason, so the printed id and the registered id must be the same one.
    assert!(
        text.contains(&ids[0]),
        "the receipt must name the id: {text}"
    );
}

#[tokio::test]
async fn a_chosen_id_is_used_and_its_options_reach_the_launcher() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);

    let result = run(
        &tools.start,
        serde_json::json!({
            "id": "build",
            "shell": "bash",
            "cwd": "sub",
            "columns": 120,
            "rows": 40,
            "max_bytes": 1024
        }),
    )
    .await;
    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(tools.registry.ids(), vec!["build".to_owned()]);

    let specs = tools.launcher.specs();
    assert_eq!(specs.len(), 1);
    let spec = &specs[0];
    assert_eq!(spec.id, "build");
    assert_eq!(spec.shell.as_deref(), Some("bash"));
    assert_eq!(spec.size.columns, 120);
    assert_eq!(spec.size.rows, 40);
    assert_eq!(spec.max_bytes, 1024);
    // `sub` is relative and no workspace was given, so it stays relative rather than being guessed
    // against a directory the tool did not choose.
    assert_eq!(spec.cwd.as_deref(), Some(std::path::Path::new("sub")));
}

#[tokio::test]
async fn a_relative_directory_resolves_against_the_workspace() {
    let workspace = scratch("workspace");
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);

    let result = (tools.start.execute)(
        serde_json::json!({ "cwd": "inner" }),
        ToolRunContext::default().in_workspace(workspace.clone()),
    )
    .await
    .expect("answered");
    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(
        tools.launcher.specs()[0].cwd.as_deref(),
        Some(workspace.join("inner").as_path())
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn a_taken_id_is_refused_without_starting_a_second_shell() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);

    let _ = run(&tools.start, serde_json::json!({ "id": "one" })).await;
    let result = run(&tools.start, serde_json::json!({ "id": "one" })).await;

    assert!(result.is_error);
    assert!(
        text_of(&result).contains("already exists"),
        "{}",
        text_of(&result)
    );
    // The launch must not have happened: a second shell started for a refused call is a process
    // nothing holds a handle to.
    assert_eq!(tools.launcher.specs().len(), 1);
}

#[tokio::test]
async fn a_read_only_session_may_not_start_a_terminal() {
    let catalogue = PermissionCatalogue::builtin();
    let mut state = PermissionState::default();
    state.apply(&catalogue, "read-only").expect("applied");

    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);
    let result = (tools.start.execute)(
        serde_json::json!({}),
        ToolRunContext::default().with_permissions(Arc::new(state)),
    )
    .await
    .expect("answered");

    assert!(result.is_error);
    assert!(
        text_of(&result).contains("read-only"),
        "{}",
        text_of(&result)
    );
    assert!(tools.registry.ids().is_empty());
    assert!(tools.launcher.specs().is_empty());
}

#[tokio::test]
async fn an_unavailable_launcher_refuses_in_terms_of_what_is_missing() {
    let registry = Arc::new(TerminalRegistry::new());
    let tool = terminal_start_tool(
        Arc::clone(&registry),
        Arc::new(UnavailableLauncher::no_shell()),
    );

    let result = run(&tool, serde_json::json!({})).await;

    assert!(result.is_error);
    let text = text_of(&result);
    // Both halves matter: which terminal failed, and that a wiring step is what is absent. "unknown
    // error" would leave whoever reads the transcript with nothing to act on.
    assert!(text.contains("cannot start terminal `"), "{text}");
    assert!(text.contains("TerminalLauncher"), "{text}");
    assert!(registry.ids().is_empty());
}

#[tokio::test]
async fn an_invalid_size_is_refused_before_the_shell_starts() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);

    for args in [
        serde_json::json!({ "columns": 0 }),
        serde_json::json!({ "rows": 0 }),
        serde_json::json!({ "columns": "wide" }),
    ] {
        let result = run(&tools.start, args).await;
        assert!(result.is_error, "{}", text_of(&result));
        assert!(tools.registry.ids().is_empty());
        assert!(tools.launcher.specs().is_empty());
    }
}

// ---- terminal_send ----------------------------------------------------------------------------

#[tokio::test]
async fn sending_input_reaches_the_shell_with_a_newline() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);
    let id = tools.started().await;

    let result = run(
        &tools.send,
        serde_json::json!({ "id": id, "input": "echo hi" }),
    )
    .await;
    assert!(!result.is_error, "{}", text_of(&result));

    // Enter is the default: a command that was typed but never run is the failure this prevents.
    assert_eq!(shell.written(), vec!["echo hi\n".to_owned()]);
}

#[tokio::test]
async fn submit_false_sends_the_text_without_a_newline() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);
    let id = tools.started().await;

    let result = run(
        &tools.send,
        serde_json::json!({ "id": id, "input": "echo hi", "submit": false }),
    )
    .await;
    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(shell.written(), vec!["echo hi".to_owned()]);
}

#[tokio::test]
async fn sending_to_an_unknown_id_is_an_error_naming_the_tool_to_use() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);
    let _ = tools.started().await;

    let result = run(
        &tools.send,
        serde_json::json!({ "id": "nope", "input": "ls" }),
    )
    .await;

    assert!(result.is_error);
    let text = text_of(&result);
    assert!(text.contains("no terminal `nope`"), "{text}");
    // Nothing was written: the input went nowhere, and dropping it silently is what the message
    // exists to prevent the model from assuming.
    assert!(shell.written().is_empty());
}

#[tokio::test]
async fn sending_with_nothing_to_send_is_refused() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);
    let id = tools.started().await;

    let result = run(&tools.send, serde_json::json!({ "id": id })).await;
    assert!(result.is_error);
    assert!(
        text_of(&result).contains("`input` is required"),
        "{}",
        text_of(&result)
    );
    // The session must still exist: a refused send must not close what it could not address.
    assert_eq!(tools.registry.ids(), vec![id]);
}

#[tokio::test]
async fn closing_ends_the_shell_and_releases_the_id() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);
    let id = tools.started().await;

    let result = run(&tools.send, serde_json::json!({ "id": id, "close": true })).await;
    assert!(!result.is_error, "{}", text_of(&result));
    assert!(text_of(&result).contains("closed"));
    assert!(shell.was_closed(), "the shell must be closed, not dropped");
    // A closed session left in the registry would hold an id the model cannot reuse.
    assert!(tools.registry.ids().is_empty());

    let after = run(&tools.read, serde_json::json!({ "id": id })).await;
    assert!(after.is_error, "a closed terminal must not read as alive");
    assert!(
        text_of(&after).contains("no terminal"),
        "{}",
        text_of(&after)
    );
}

// ---- terminal_read ----------------------------------------------------------------------------

#[tokio::test]
async fn reading_returns_output_and_accumulates_it_in_the_transcript() {
    let shell = Arc::new(FakeShell::new(&["first\n", "second\n"]));
    let tools = Tools::new(&shell);
    let id = tools.started().await;

    let first = run(&tools.read, serde_json::json!({ "id": id })).await;
    assert!(!first.is_error, "{}", text_of(&first));
    assert_eq!(text_of(&first), "first\n");

    let second = run(&tools.read, serde_json::json!({ "id": id })).await;
    assert_eq!(text_of(&second), "second\n");

    // Neither read returns the scrollback, but both chunks are in the transcript: a caller that asked
    // for the whole thing must get everything that was printed, in order.
    let session = tools.registry.get(&id).expect("registered");
    assert_eq!(session.transcript(), "first\nsecond\n");
}

#[tokio::test]
async fn reading_an_idle_terminal_says_so_instead_of_returning_nothing() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);
    let id = tools.started().await;

    let result = run(&tools.read, serde_json::json!({ "id": id })).await;

    assert!(!result.is_error, "an idle terminal is not a failure");
    let text = text_of(&result);
    // An empty string would be indistinguishable from a command that printed an empty line, and from
    // a read that failed to attach.
    assert!(!text.is_empty());
    assert!(text.contains("no output"), "{text}");
    assert!(
        text.contains("running"),
        "the status must be reported: {text}"
    );
    assert!(
        text.contains("wait_ms"),
        "a second call needs to know how to wait longer: {text}"
    );
}

#[tokio::test]
async fn full_returns_the_whole_transcript() {
    let shell = Arc::new(FakeShell::new(&["alpha\n", "beta\n"]));
    let tools = Tools::new(&shell);
    let id = tools.started().await;

    let _ = run(&tools.read, serde_json::json!({ "id": id })).await;
    let result = run(&tools.read, serde_json::json!({ "id": id, "full": true })).await;

    let text = text_of(&result);
    assert!(text.contains("alpha"), "{text}");
    assert!(text.contains("beta"), "{text}");
}

#[tokio::test]
async fn reading_an_unknown_id_is_an_error() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);
    let _ = tools.started().await;

    let result = run(&tools.read, serde_json::json!({ "id": "nope" })).await;
    assert!(result.is_error);
    assert!(
        text_of(&result).contains("no terminal `nope`"),
        "{}",
        text_of(&result)
    );
}

#[tokio::test]
async fn a_cancelled_read_returns_instead_of_waiting() {
    let shell = Arc::new(FakeShell::new(&[]));
    let tools = Tools::new(&shell);
    let id = tools.started().await;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = (tools.read.execute)(
        serde_json::json!({ "id": id, "wait_ms": 30_000 }),
        ToolRunContext::new(
            ToolExecution {
                call_id: ToolCallId::new("call").expect("valid"),
                name: "terminal_read".to_owned(),
                arguments: "{}".to_owned(),
                agent_id: AgentId::new("agent").expect("valid"),
            },
            cancel,
        ),
    )
    .await
    .expect("answered");

    assert!(result.is_error);
    assert!(
        text_of(&result).contains("cancelled"),
        "{}",
        text_of(&result)
    );
}

// ---- mounting ---------------------------------------------------------------------------------

#[tokio::test]
async fn mounting_registers_all_three_tools() {
    let registry = Arc::new(TerminalRegistry::new());
    let tools = ToolRegistry::new();
    register_terminal_tools(
        &tools,
        Arc::clone(&registry),
        Arc::new(UnavailableLauncher::default()),
    );

    let names: Vec<String> = tools
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    // One call mounts all three because half a surface is worse than none: a model told a terminal
    // exists and unable to read it would report the command as silent.
    assert_eq!(
        names,
        vec![
            "terminal_read".to_owned(),
            "terminal_send".to_owned(),
            "terminal_start".to_owned()
        ]
    );
}
