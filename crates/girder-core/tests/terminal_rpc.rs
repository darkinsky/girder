//! Tests for the `terminal.*` Remote methods.
//!
//! The methods are a client-facing face over [`girder_core::TerminalRegistry`], and the properties
//! worth pinning are the ones a page depends on and a plausible implementation gets wrong:
//!
//! - `create` must return a terminal that `list` then shows, or a client draws an empty pane it
//!   believes is a live shell
//! - `create` must be idempotent for an id that already exists, or a retried request starts a second
//!   process that nobody closes
//! - `close` must succeed for an id that is already gone, because that is the state the caller asked
//!   for, while every other close failure must still be reported
//! - a launcher that refuses must have its own words forwarded, or the person who wired the
//!   deployment cannot see what was refused
//!
//! The shells here are fakes. A real pty would add a platform dependency and a timing race to
//! properties that are about the RPC surface, not about process control.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use girder_core::{
    BoxFut, RemoteCall, RemoteError, RemoteRegistry, Shell, TerminalError, TerminalLaunch,
    TerminalLauncher, TerminalRegistry,
};
use girder_core::{
    TERMINAL_DEFAULT_READ_WAIT_MS as DEFAULT_READ_WAIT_MS, TERMINAL_MAX_WAIT_MS as MAX_WAIT_MS,
    register_terminal_rpc_methods as register_terminal_methods,
};
use serde_json::{Value, json};

// ---- fixtures ---------------------------------------------------------------------------------

/// Names a scratch directory uniquely enough for tests that may run in one process at once.
///
/// The registry is built rather than shared, but the directory is where a `cwd` argument points, and
/// two tests writing into one would disagree about what exists.
fn scratch(name: &str) -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-trpc-{name}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// A shell that records input and leaves closing under the test's control.
struct FakeShell {
    /// Inputs received, in order.
    written: std::sync::Mutex<Vec<String>>,
    /// Output waiting for the next read.
    pending: std::sync::Mutex<Option<String>>,
    /// Whether `close` was called.
    closed: std::sync::atomic::AtomicBool,
    /// Whether `close` reports a failure, for the test that needs one.
    close_fails: bool,
}

impl FakeShell {
    fn new(chunk: Option<&str>) -> Self {
        Self {
            written: std::sync::Mutex::new(Vec::new()),
            pending: std::sync::Mutex::new(chunk.map(ToOwned::to_owned)),
            closed: std::sync::atomic::AtomicBool::new(false),
            close_fails: false,
        }
    }

    fn refusing_close() -> Self {
        Self {
            close_fails: true,
            ..Self::new(None)
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
        Box::pin(async { Ok(()) })
    }

    fn read(&self, _wait: Duration) -> BoxFut<Result<Option<String>, TerminalError>> {
        // The queue is the answer, with no sleep: a fake that produced output on a timer would make
        // every read a race, and what is under test is what the method does with a chunk.
        let chunk = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        Box::pin(async move { Ok(chunk) })
    }

    fn close(&self) -> BoxFut<Result<(), TerminalError>> {
        self.closed.store(true, Ordering::Relaxed);
        let fails = self.close_fails;
        Box::pin(async move {
            if fails {
                Err(TerminalError::Spawn {
                    id: "fake".to_owned(),
                    reason: "the shell refused to stop".to_owned(),
                })
            } else {
                Ok(())
            }
        })
    }
}

/// A launcher that hands out one shell, or refuses with a fixed reason.
struct FakeLauncher {
    shell: Arc<FakeShell>,
    refusal: Option<String>,
    specs: std::sync::Mutex<Vec<TerminalLaunch>>,
}

impl FakeLauncher {
    fn serving(shell: Arc<FakeShell>) -> Self {
        Self {
            shell,
            refusal: None,
            specs: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn refusing(reason: &str) -> Self {
        Self {
            shell: Arc::new(FakeShell::new(None)),
            refusal: Some(reason.to_owned()),
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
        let refusal = self.refusal.clone();
        let shell: Arc<dyn Shell> = Arc::clone(&self.shell) as Arc<dyn Shell>;
        Box::pin(async move { refusal.map_or(Ok(shell), Err) })
    }
}

/// One registered method set over its own registry and launcher.
struct Harness {
    registry: RemoteRegistry,
    terminals: Arc<TerminalRegistry>,
    launcher: Arc<FakeLauncher>,
}

impl Harness {
    fn serving(shell: Arc<FakeShell>) -> Self {
        Self::over(Arc::new(FakeLauncher::serving(shell)))
    }

    fn refusing(reason: &str) -> Self {
        Self::over(Arc::new(FakeLauncher::refusing(reason)))
    }

    fn over(launcher: Arc<FakeLauncher>) -> Self {
        let terminals = Arc::new(TerminalRegistry::new());
        let mut registry = RemoteRegistry::new();
        let dynamic: Arc<dyn TerminalLauncher> = Arc::clone(&launcher) as Arc<dyn TerminalLauncher>;
        register_terminal_methods(&mut registry, &terminals, &dynamic);
        Self {
            registry,
            terminals,
            launcher,
        }
    }

    async fn call(&self, method: &str, args: Value) -> Result<Value, RemoteError> {
        self.registry
            .dispatch(
                "terminal",
                method,
                RemoteCall {
                    args,
                    peer: "test".to_owned(),
                },
            )
            .await
    }

    async fn ok(&self, method: &str, args: Value) -> Value {
        self.call(method, args)
            .await
            .unwrap_or_else(|error| panic!("`terminal.{method}` failed: {error}"))
    }
}

/// The ids a listing reports, in order.
fn ids(value: &Value) -> Vec<String> {
    value["terminals"]
        .as_array()
        .expect("an array of terminals")
        .iter()
        .map(|terminal| terminal["id"].as_str().expect("an id").to_owned())
        .collect()
}

// ---- list -------------------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_registry_lists_no_terminals() {
    let dir = scratch("empty");
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));

    // A directory is created so the fixture is exercised even though `list` takes no arguments; the
    // id of the scratch path is what keeps this test's name unique among parallel runs.
    assert!(dir.is_dir());
    let value = harness.ok("list", json!({})).await;

    assert_eq!(value, json!({ "terminals": [] }));
}

#[tokio::test]
async fn creating_a_terminal_puts_it_in_the_listing() {
    let _dir = scratch("create-list");
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));

    let created = harness.ok("create", json!({"id": "t1"})).await;
    assert_eq!(created["id"], "t1");
    assert_eq!(created["state"], "running");

    let listed = harness.ok("list", json!({})).await;
    assert_eq!(ids(&listed), vec!["t1".to_owned()]);
    assert_eq!(listed["terminals"][0]["state"], "running");
}

// ---- create -----------------------------------------------------------------------------------

#[tokio::test]
async fn create_uses_the_headless_size_when_none_is_given() {
    let _dir = scratch("defaults");
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));

    let created = harness.ok("create", json!({"id": "t1"})).await;

    assert_eq!(created["cols"], 80);
    assert_eq!(created["rows"], 24);
    // The defaults reached the launcher, not just the response: a shell sized differently from the
    // record would answer width questions with numbers no client asked for.
    let specs = harness.launcher.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].size.columns, 80);
    assert_eq!(specs[0].size.rows, 24);
}

#[tokio::test]
async fn create_forwards_the_shell_directory_and_size_to_the_launcher() {
    let dir = scratch("forward");
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).expect("created");

    let created = harness
        .ok(
            "create",
            json!({"id": "t1", "shell": "/bin/sh", "cwd": cwd.to_str().expect("utf8"),
                   "columns": 120, "rows": 40}),
        )
        .await;

    assert_eq!(created["cols"], 120);
    assert_eq!(created["rows"], 40);
    let specs = harness.launcher.specs();
    assert_eq!(specs[0].shell.as_deref(), Some("/bin/sh"));
    assert_eq!(specs[0].cwd.as_deref(), Some(cwd.as_path()));
}

#[tokio::test]
async fn create_generates_an_id_when_the_client_omits_one() {
    let _dir = scratch("generated");
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));

    let first = harness.ok("create", json!({})).await;
    let second = harness.ok("create", Value::Null).await;

    let first = first["id"].as_str().expect("an id").to_owned();
    let second = second["id"].as_str().expect("an id").to_owned();
    // Distinct, or the second create would have been answered by the first terminal and the client
    // would have two panes over one shell.
    assert_ne!(first, second);
    assert_eq!(ids(&harness.ok("list", json!({})).await).len(), 2);
}

#[tokio::test]
async fn create_for_an_existing_id_returns_the_same_terminal() {
    let _dir = scratch("idempotent");
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));

    let first = harness.ok("create", json!({"id": "t1"})).await;
    let second = harness
        .ok("create", json!({"id": "t1", "columns": 200}))
        .await;

    // The size of the first call wins: returning a second record for one shell would describe a
    // terminal that does not exist.
    assert_eq!(first, second);
    assert_eq!(harness.launcher.specs().len(), 1);
}

#[tokio::test]
async fn create_refuses_an_id_that_could_not_be_typed_back() {
    let _dir = scratch("bad-id");
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));

    for id in ["", "with space", "a/b", "x".repeat(129).as_str()] {
        let error = harness
            .call("create", json!({"id": id}))
            .await
            .expect_err("an unacceptable id is refused");
        assert_eq!(error.code(), "INVALID_ARGUMENTS", "{id:?} gave {error}");
        // Nothing was started for a request that was refused, or a bad id would still cost a process.
        assert!(harness.launcher.specs().is_empty());
    }
}

#[tokio::test]
async fn create_refuses_a_zero_or_oversized_dimension() {
    let _dir = scratch("bad-size");
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));

    for args in [
        json!({"id": "t1", "columns": 0}),
        json!({"id": "t1", "rows": 0}),
        json!({"id": "t1", "columns": 65_536}),
    ] {
        let error = harness
            .call("create", args.clone())
            .await
            .expect_err("an impossible size is refused");
        assert_eq!(error.code(), "INVALID_ARGUMENTS", "{args} gave {error}");
    }
    assert!(harness.launcher.specs().is_empty());
}

#[tokio::test]
async fn a_refusing_launcher_reaches_the_client_in_its_own_words() {
    let _dir = scratch("refused");
    let harness = Harness::refusing("no `Shell` implementation is wired into this deployment");

    let error = harness
        .call("create", json!({"id": "t1"}))
        .await
        .expect_err("a refusal is an error");

    assert_eq!(error.code(), "FAILED");
    let message = error.to_string();
    // The deployment's reason survives, and the id is named once by this layer: a client that sees
    // only "internal error" cannot tell a missing wiring step from a bug in its own request.
    assert!(
        message.contains("no `Shell` implementation is wired"),
        "{message}"
    );
    assert!(message.contains("t1"), "{message}");
    assert!(harness.terminals.ids().is_empty());
}

#[tokio::test]
async fn create_with_wrong_argument_types_is_invalid_arguments() {
    let _dir = scratch("bad-types");
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));

    let error = harness
        .call("create", json!({"id": "t1", "columns": "wide"}))
        .await
        .expect_err("a string width is not a width");

    // A refusal the caller can act on, not a terminal created from a default width it never asked
    // for. The message is serde's, so it names the type it expected; the method is named by this
    // layer, which is what tells a client which call to fix.
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(error.to_string().contains("terminal.create"), "{error}");
    assert!(error.to_string().contains("u64"), "{error}");
    assert!(harness.launcher.specs().is_empty());
}

// ---- send -------------------------------------------------------------------------------------

#[tokio::test]
async fn send_submits_by_default_and_can_withhold_enter() {
    let _dir = scratch("send");
    let shell = Arc::new(FakeShell::new(None));
    let harness = Harness::serving(Arc::clone(&shell));
    harness.ok("create", json!({"id": "t1"})).await;

    harness.ok("send", json!({"id": "t1", "input": "ls"})).await;
    harness
        .ok(
            "send",
            json!({"id": "t1", "input": "cd /", "submit": false}),
        )
        .await;

    // One newline appended, none doubled: a caller that already ended the line must not have a blank
    // command run after it.
    assert_eq!(shell.written(), vec!["ls\n".to_owned(), "cd /".to_owned()]);
}

#[tokio::test]
async fn send_does_not_double_an_existing_newline() {
    let _dir = scratch("send-newline");
    let shell = Arc::new(FakeShell::new(None));
    let harness = Harness::serving(Arc::clone(&shell));
    harness.ok("create", json!({"id": "t1"})).await;

    harness
        .ok("send", json!({"id": "t1", "input": "ls\n"}))
        .await;

    assert_eq!(shell.written(), vec!["ls\n".to_owned()]);
}

#[tokio::test]
async fn send_to_an_unknown_id_names_the_id() {
    let _dir = scratch("send-unknown");
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));

    let error = harness
        .call("send", json!({"id": "ghost", "input": "ls"}))
        .await
        .expect_err("an unknown terminal cannot take input");

    assert!(error.to_string().contains("ghost"), "{error}");
}

// ---- read -------------------------------------------------------------------------------------

#[tokio::test]
async fn read_reports_output_and_flags_an_empty_window() {
    let _dir = scratch("read");
    let harness = Harness::serving(Arc::new(FakeShell::new(Some("hello\n"))));
    harness.ok("create", json!({"id": "t1"})).await;

    let first = harness.ok("read", json!({"id": "t1"})).await;
    assert_eq!(first["data"], "hello\n");
    assert_eq!(first["received"], true);
    // The default wait is the one the constant names, so a client that saw a shorter one would have
    // been given a promise the method does not keep.
    assert_eq!(first["waitedMs"], DEFAULT_READ_WAIT_MS);

    let second = harness.ok("read", json!({"id": "t1"})).await;
    assert_eq!(second["data"], "");
    assert_eq!(second["received"], false);
}

#[tokio::test]
async fn read_clamps_a_long_wait_and_can_return_the_whole_transcript() {
    let _dir = scratch("read-clamp");
    let harness = Harness::serving(Arc::new(FakeShell::new(Some("one\ntwo\n"))));
    harness.ok("create", json!({"id": "t1"})).await;

    let value = harness
        .ok(
            "read",
            json!({"id": "t1", "wait_ms": 3_600_000, "full": true}),
        )
        .await;

    // Clamped rather than honoured: a call that waited an hour would hold a response slot for an
    // hour, and the answer says what it actually waited so the client can tell.
    assert_eq!(value["waitedMs"], MAX_WAIT_MS);
    assert_eq!(value["transcript"], "one\ntwo\n");
    assert_eq!(value["omittedBytes"], 0);
}

// ---- close ------------------------------------------------------------------------------------

#[tokio::test]
async fn close_ends_the_shell_and_removes_it_from_the_listing() {
    let _dir = scratch("close");
    let shell = Arc::new(FakeShell::new(None));
    let harness = Harness::serving(Arc::clone(&shell));
    harness.ok("create", json!({"id": "t1"})).await;

    let value = harness.ok("close", json!({"id": "t1"})).await;

    assert_eq!(value, json!({"id": "t1", "closed": true}));
    assert!(shell.was_closed());
    // Removed, not just stopped: a closed session left behind would hold an id a client cannot reuse
    // and its transcript for the life of the process.
    assert!(harness.terminals.ids().is_empty());
    assert_eq!(
        ids(&harness.ok("list", json!({})).await),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn close_of_an_unknown_id_succeeds_and_says_nothing_was_closed() {
    let _dir = scratch("close-unknown");
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));

    let value = harness.ok("close", json!({"id": "ghost"})).await;

    // The state the caller asked for is already true. Failing here would show an error in a UI for
    // the normal case of closing a pane whose shell exited on its own.
    assert_eq!(value, json!({"id": "ghost", "closed": false}));
}

#[tokio::test]
async fn a_close_that_the_shell_refuses_is_reported_and_keeps_the_id() {
    let _dir = scratch("close-fails");
    let shell = Arc::new(FakeShell::refusing_close());
    let harness = Harness::serving(Arc::clone(&shell));
    harness.ok("create", json!({"id": "t1"})).await;

    let error = harness
        .call("close", json!({"id": "t1"}))
        .await
        .expect_err("a shell that would not stop is a failure");

    assert_eq!(error.code(), "FAILED");
    assert!(error.to_string().contains("refused to stop"), "{error}");
    // The process is still alive, so its id stays addressable: dropping it would leave a shell no
    // caller could reach and therefore no caller could stop.
    assert_eq!(harness.terminals.ids(), vec!["t1".to_owned()]);
}

// ---- the declared surface ---------------------------------------------------------------------

#[test]
fn the_streaming_methods_are_declared_as_gaps() {
    let harness = Harness::serving(Arc::new(FakeShell::new(None)));

    let declared: Vec<String> = harness
        .registry
        .unimplemented()
        .into_iter()
        .map(|(namespace, method)| format!("{namespace}.{method}"))
        .collect();

    // `attach` and `retain` need a stream mode this registry does not have. Declaring them means a
    // client gets "not implemented" rather than "unknown method", which reads as its own typo.
    assert!(
        declared.contains(&"terminal.attach".to_owned()),
        "{declared:?}"
    );
    assert!(
        declared.contains(&"terminal.retain".to_owned()),
        "{declared:?}"
    );
    assert!(
        declared.contains(&"terminal.rename".to_owned()),
        "{declared:?}"
    );
    for name in ["list", "create", "send", "read", "close"] {
        assert!(
            harness.registry.get("terminal", name).is_some(),
            "`terminal.{name}` should be registered"
        );
    }
}
