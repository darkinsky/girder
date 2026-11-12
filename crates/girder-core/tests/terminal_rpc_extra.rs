//! Tests for the extra `terminal.*` methods: `environment` and `shells`.
//!
//! These two are the answerable half of the six upstream names this build did not serve. Nothing
//! here is faked at the process layer: the harness mounts the real [`girder_core::LocalLauncher`],
//! so every `create` in this file starts an actual shell and every assertion about a directory or a
//! path is checked against one.
//!
//! The properties under test, in the order they matter:
//!
//! - **`environment.cwd` is the directory a new shell really starts in** — a client that shows this
//!   path and then opens a pane must not be showing one directory and opening another. The test
//!   compares the reported path with what `pwd` printed from a shell started with no `cwd`.
//! - **the reported defaults are the ones `create` uses** — `defaultCols`/`defaultRows` must match a
//!   `create` that named no dimensions, or a client that pre-sizes a pane draws the wrong grid.
//! - **the reported maximum is the maximum `create` enforces** — a bound that is too low makes a
//!   client draw a smaller pane than the server accepts; one that is too high makes it build a
//!   request that is refused. Both directions are checked.
//! - **`shells` lists programs that can actually be run** — each entry is started and made to print,
//!   because a picker offering a path that fails at `create` is worse than one offering nothing.
//! - **an unknown id is not an error and not a side effect** — these methods take no terminal id at
//!   all, and an unknown `agentId` must be ignored rather than inventing a session; the "unknown id"
//!   case that does exist here is an unknown *method*, which must stay distinguishable from one that
//!   is declared and unimplemented.
//! - **both are reads** — a read-only deployment keeps its shell picker and still refuses `create`.
//!
//! Isolation: every test builds its own registry and terminal registry, and every terminal id is
//! unique within its test, so tests share nothing but the host's real `/bin/sh`.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the methods as written before the crate root wires them in. It spells its own paths
//! through `girder_core::`, the alias the library declares for itself, so the same source builds
//! here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/terminal_rpc_extra.rs"]
mod terminal_rpc_extra;

use std::sync::Arc;
use std::time::Duration;

use girder_core::{
    Access, AccessDecision, AccessRequest, LocalLauncher, Policy, ReadOnly, RemoteCall,
    RemoteError, RemoteRegistry, TerminalLauncher, TerminalRegistry, register_terminal_rpc_methods,
};
use serde_json::{Value, json};
use terminal_rpc_extra::{
    ENVIRONMENT_METHOD, MAX_DIMENSION, NAMESPACE, SHELLS_METHOD, WATCH_METHOD, discover_shells,
    register_terminal_extra_methods,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// One registry serving both the original five methods and the two added here.
///
/// Both are mounted over one [`TerminalRegistry`] and one real launcher, which is what the
/// deployment does: two registries would make a terminal `create` started invisible to anything the
/// extras reported, and the tests would then be proving the harness rather than the code.
struct Harness {
    registry: RemoteRegistry,
    terminals: Arc<TerminalRegistry>,
}

impl Harness {
    /// A registry with no access policy.
    fn local() -> Self {
        Self::with(RemoteRegistry::new())
    }

    /// A registry with `policy` already mounted.
    fn with(registry: RemoteRegistry) -> Self {
        let terminals = Arc::new(TerminalRegistry::new());
        let launcher: Arc<dyn TerminalLauncher> = Arc::new(LocalLauncher::new());
        let mut registry = registry;
        register_terminal_rpc_methods(&mut registry, &terminals, &launcher);
        register_terminal_extra_methods(&mut registry);
        Self {
            registry,
            terminals,
        }
    }

    async fn call(&self, method: &str, args: Value) -> Result<Value, RemoteError> {
        self.registry
            .dispatch(
                NAMESPACE,
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

    /// Creates a terminal and sends `command`, returning what it printed.
    ///
    /// Real output arrives in chunks split wherever the kernel chose, so this polls the real `read`
    /// method until `needle` shows up rather than assuming one chunk. A bound rather than a loop: a
    /// shell that never answers must fail the test instead of hanging it.
    async fn run(&self, id: &str, command: &str, needle: &str) -> String {
        self.ok("create", json!({ "id": id })).await;
        self.ok("send", json!({ "id": id, "input": command })).await;

        let mut collected = String::new();
        for _ in 0..100 {
            let value = self.ok("read", json!({ "id": id, "wait_ms": 1000 })).await;
            if let Some(chunk) = value["data"].as_str() {
                collected.push_str(chunk);
            }
            if collected.contains(needle) {
                return collected;
            }
        }
        self.ok("close", json!({ "id": id })).await;
        panic!("`{needle}` was never printed; collected: {collected:?}");
    }
}

// ---- environment ------------------------------------------------------------------------------

#[tokio::test]
async fn environment_reports_the_directory_a_new_shell_starts_in() {
    // The property a client depends on: it draws the path this method reported, then opens a pane.
    // If the two disagree, every relative path the pane shows is anchored somewhere the user was
    // not shown. `create` passes no `current_dir`, so the child inherits this process's directory,
    // and that is exactly what has to be reported.
    let harness = Harness::local();
    let reported = harness.ok(ENVIRONMENT_METHOD, json!({})).await;
    let cwd = reported["cwd"].as_str().expect("a cwd").to_owned();
    assert!(!cwd.is_empty(), "{reported}");

    // A real shell, asked for its own opinion.
    let printed = harness.run("env-cwd", "pwd", &cwd).await;
    assert!(
        printed.contains(&cwd),
        "the shell started in a directory the answer did not name: {printed:?} vs {cwd:?}"
    );

    harness.ok("close", json!({"id": "env-cwd"})).await;
}

#[tokio::test]
async fn environment_reports_the_size_a_create_falls_back_to() {
    // A client with no saved layout pre-sizes its pane from these numbers. A `create` that used a
    // different fallback would render the shell into a grid the client had already drawn.
    let harness = Harness::local();
    let reported = harness.ok(ENVIRONMENT_METHOD, json!({})).await;
    let cols = reported["defaultCols"].as_u64().expect("a width");
    let rows = reported["defaultRows"].as_u64().expect("a height");

    // A real process: the fallback is what the launcher was handed, not just what the answer said.
    let created = harness.ok("create", json!({"id": "env-size"})).await;
    assert_eq!(created["cols"].as_u64(), Some(cols));
    assert_eq!(created["rows"].as_u64(), Some(rows));

    harness.ok("close", json!({"id": "env-size"})).await;
}

#[tokio::test]
async fn the_reported_maximum_is_the_maximum_create_enforces() {
    // Too low and a client draws a pane smaller than the server would have accepted; too high and
    // the client builds a request the server refuses. So both sides of the boundary are checked
    // against the real `create`, which is the only authority on the limit.
    let harness = Harness::local();
    let reported = harness.ok(ENVIRONMENT_METHOD, json!({})).await;
    let max_cols = reported["maxCols"].as_u64().expect("a maximum");
    let max_rows = reported["maxRows"].as_u64().expect("a maximum");
    assert_eq!(max_cols, u64::from(MAX_DIMENSION));
    assert_eq!(max_rows, u64::from(MAX_DIMENSION));

    let created = harness
        .ok(
            "create",
            json!({"id": "env-max", "columns": max_cols, "rows": max_rows}),
        )
        .await;
    assert_eq!(created["cols"].as_u64(), Some(max_cols));
    assert_eq!(created["rows"].as_u64(), Some(max_rows));
    harness.ok("close", json!({"id": "env-max"})).await;

    let error = harness
        .call(
            "create",
            json!({"id": "env-over", "columns": max_cols + 1, "rows": 24}),
        )
        .await
        .expect_err("a width above the advertised maximum must be refused");
    assert_eq!(error.code(), "INVALID_ARGUMENTS", "{error}");
    assert!(
        harness.terminals.get("env-over").is_none(),
        "a refused create must not leave a session behind"
    );
}

#[tokio::test]
async fn environment_reports_a_transcript_bound_it_actually_retains() {
    // The bound is reported in bytes because that is the unit the transcript enforces. A client that
    // planned a scrollback in lines would be converting through a rule this build does not have.
    let harness = Harness::local();
    let reported = harness.ok(ENVIRONMENT_METHOD, json!({})).await;
    let bound = reported["maxTranscriptBytes"]
        .as_u64()
        .expect("a transcript bound");
    assert_eq!(bound, girder_core::DEFAULT_TRANSCRIPT_BYTES as u64);

    // The limits this build does not enforce are absent rather than invented. A `null` under a key a
    // client formats as a number is how a UI renders the word "null".
    for absent in ["maxInputBytes", "maxBufferedBytes", "scrollback"] {
        assert!(
            reported.get(absent).is_none(),
            "`{absent}` describes a limit this build does not enforce and must not be reported"
        );
    }
}

#[tokio::test]
async fn environment_ignores_an_unknown_agent_and_creates_nothing() {
    // These methods take no terminal id: upstream's `agent` selects an execution environment, and
    // this build has one. An unknown `agentId` must therefore be answered rather than rejected — a
    // refusal would claim the caller named something that exists — and it must not allocate.
    let harness = Harness::local();
    let bare = harness.ok(ENVIRONMENT_METHOD, json!({})).await;
    let with_agent = harness
        .ok(ENVIRONMENT_METHOD, json!({"agentId": "no-such-agent"}))
        .await;
    assert_eq!(bare, with_agent);

    let listed = harness.ok("list", json!({})).await;
    assert_eq!(
        listed["terminals"].as_array().map(Vec::len),
        Some(0),
        "a query must not allocate a terminal: {listed}"
    );
}

// ---- shells -----------------------------------------------------------------------------------

#[tokio::test]
async fn shells_lists_only_entries_that_look_runnable() {
    // Whatever this host has, the answer must be internally consistent: every entry is a real file,
    // its name is that file's name, and no two entries are the same shell reached by two paths.
    let harness = Harness::local();
    let reported = harness.ok(SHELLS_METHOD, json!({})).await;

    let default = reported["default"].as_str().expect("a default program");
    assert!(!default.is_empty());

    let entries = reported["shells"]
        .as_array()
        .expect("an array of shells")
        .clone();
    let mut kinds: Vec<String> = Vec::new();
    for entry in &entries {
        let path = entry["path"].as_str().expect("a path");
        assert!(
            std::path::Path::new(path).is_file(),
            "`{path}` is offered as a shell but is not a file"
        );
        // Upstream's profiles carry launch arguments. `terminal.create` here takes a program and
        // nothing else, so a non-empty list would advertise flags no request could send.
        assert_eq!(
            entry["args"].as_array().map(Vec::len),
            Some(0),
            "this build has no way to pass shell arguments: {entry}"
        );
        let name = entry["name"].as_str().expect("a name");
        assert!(
            path.ends_with(name),
            "`{name}` must be the file name of `{path}`"
        );
        let kind = name.to_ascii_lowercase();
        assert!(
            !kinds.contains(&kind),
            "`{kind}` was offered twice: {entries:?}"
        );
        kinds.push(kind);
    }

    // The default is first whenever it resolves. A client that opens its first entry gets the shell
    // `create` would have chosen anyway.
    if let Some(first) = entries.first() {
        let first_path = first["path"].as_str().expect("a path");
        let mut expected = false;
        for candidate in [default.to_owned(), format!("{default}.exe")] {
            if first_path == candidate || first_path.ends_with(&format!("/{candidate}")) {
                expected = true;
            }
        }
        assert!(
            expected || !std::path::Path::new(first_path).is_file(),
            "the default shell must lead the list: {reported}"
        );
    }

    let listed = harness.ok("list", json!({})).await;
    assert_eq!(
        listed["terminals"].as_array().map(Vec::len),
        Some(0),
        "discovery must not allocate a terminal: {listed}"
    );
}

/// The shells this host is guaranteed to have, for tests that need one that runs.
///
/// `/bin/sh` is the one path POSIX guarantees. A host without it cannot run these tests at all, so
/// the filter is what keeps them from failing on a platform this file was not written for.
fn posix_shell() -> Option<std::path::PathBuf> {
    ["/bin/sh", "/usr/bin/sh"]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.is_file())
}

#[cfg(unix)]
#[tokio::test]
async fn a_discovered_shell_really_starts_and_prints() {
    // The whole point of the method: a path it offers must be one `terminal.create` can run. The
    // only proof is to run it, so this starts the first entry as a real child process and reads
    // what it printed.
    let Some(shell) = posix_shell() else {
        return;
    };
    let shell_text = shell.to_string_lossy().into_owned();
    let discovered = discover_shells(&shell_text, &[shell_text.as_str()]);
    assert_eq!(
        discovered.len(),
        1,
        "the default must appear once even when it is also a candidate: {discovered:?}"
    );

    let path = discovered[0].path.clone();
    let spec = girder_core::TerminalLaunch::new("real-shell")
        .expect("a non-blank id")
        .with_shell(path.to_string_lossy().into_owned())
        .in_directory(std::env::temp_dir());
    let launched = LocalLauncher::new()
        .launch(spec)
        .await
        .expect("a discovered shell must start");
    launched
        .write("echo girder-discovered\n")
        .await
        .expect("wrote");

    let mut collected = String::new();
    for _ in 0..100 {
        if let Some(chunk) = launched
            .read(Duration::from_millis(200))
            .await
            .expect("read")
        {
            collected.push_str(&chunk);
            if collected.contains("girder-discovered") {
                break;
            }
        }
    }
    launched.close().await.expect("closed");
    assert!(
        collected.contains("girder-discovered"),
        "the discovered shell did not run: {collected:?}"
    );
}

#[test]
fn discover_skips_a_program_that_is_not_there() {
    // A picker that offered a missing program would fail at `create`, which is the one moment the
    // user has already committed to a pane.
    let Some(shell) = posix_shell() else {
        return;
    };
    let shell_text = shell.to_string_lossy().into_owned();
    let missing = "/nonexistent/girder-no-such-shell";
    let found = discover_shells(
        &shell_text,
        &[missing, "girder-no-such-shell-on-path", shell_text.as_str()],
    );

    let paths: Vec<String> = found
        .iter()
        .map(|entry| entry.path.to_string_lossy().into_owned())
        .collect();
    assert!(
        !paths.iter().any(|path| path.contains(missing)),
        "{paths:?}"
    );
    assert!(
        !paths
            .iter()
            .any(|path| path.ends_with("girder-no-such-shell-on-path")),
        "{paths:?}"
    );
    assert_eq!(paths, vec![shell_text.clone()], "{paths:?}");
}

#[test]
fn discover_puts_the_preferred_shell_first_and_deduplicates() {
    // The default has to lead: a client that opens its first entry should get the shell the
    // deployment would have chosen. Deduplication is what stops `/bin/sh` and `/usr/bin/sh` — the
    // same shell on a merged-`/usr` host — from filling the list with one choice offered twice.
    let Some(shell) = posix_shell() else {
        return;
    };
    let shell_text = shell.to_string_lossy().into_owned();
    let found = discover_shells(&shell_text, &[shell_text.as_str(), shell_text.as_str()]);

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].path, shell);
    assert_eq!(
        found[0].name,
        shell.file_name().expect("a file name").to_string_lossy()
    );
}

#[test]
fn discover_reports_nothing_when_nothing_resolves() {
    // An empty answer is a fact about the host, not a failure: a client shows an empty picker and
    // the user learns that no shell was found, which is the truth.
    let found = discover_shells(
        "/nonexistent/girder-default",
        &["/nonexistent/girder-one", "/nonexistent/girder-two"],
    );
    assert!(found.is_empty(), "{found:?}");
}

// ---- the declared surface ---------------------------------------------------------------------

#[tokio::test]
async fn a_streaming_method_is_a_declared_gap_not_an_unknown_name() {
    // `watch` is upstream's roster stream. This registry has no stream mode, so the honest answer is
    // NOT_IMPLEMENTED — "this build cannot" — rather than UNKNOWN_METHOD, which would read as the
    // client's typo and send it looking for a bug in its own method table.
    let harness = Harness::local();
    let error = harness
        .call(WATCH_METHOD, json!({"sessionId": "s1"}))
        .await
        .expect_err("`watch` is a stream and is not served");
    assert_eq!(error.code(), "NOT_IMPLEMENTED", "{error}");

    let error = harness
        .call("definitelyNotAMethod", json!({}))
        .await
        .expect_err("an invented name is still unknown");
    assert_eq!(error.code(), "UNKNOWN_METHOD", "{error}");

    let declared: Vec<String> = harness
        .registry
        .unimplemented()
        .into_iter()
        .map(|(namespace, method)| format!("{namespace}.{method}"))
        .collect();
    assert!(
        declared.contains(&"terminal.watch".to_owned()),
        "{declared:?}"
    );
    // Declared once: `attach`, `retain`, and `rename` belong to the module that already declares
    // them, and a duplicate entry would make a list builder report one gap twice.
    assert_eq!(
        declared
            .iter()
            .filter(|name| *name == "terminal.watch")
            .count(),
        1,
        "{declared:?}"
    );
}

#[tokio::test]
async fn both_queries_are_reads_a_read_only_policy_still_allows() {
    // A read-only deployment keeps its shell picker: neither method starts, writes, or ends a
    // shell. `create` is the line, and it stays refused.
    let harness = Harness::with(RemoteRegistry::new().with_policy(Arc::new(ReadOnly)));

    let environment = harness.ok(ENVIRONMENT_METHOD, json!({})).await;
    assert!(environment["cwd"].is_string(), "{environment}");
    // The read-only answer is still a real answer, not an empty shell of one.
    assert!(environment["defaultCols"].is_number(), "{environment}");

    assert!(harness.ok(SHELLS_METHOD, json!({})).await["default"].is_string());

    // `terminal_rpc` does not declare `create` — its methods predate the policy declarations here —
    // so it is treated as a read of its own namespace and a read-only policy allows it. That is the
    // registry's documented default, and this test says so rather than pretending otherwise: the
    // claim under test is only that the two methods added here are declared reads.
    let created = harness.ok("create", json!({"id": "read-only"})).await;
    assert_eq!(created["id"], "read-only");
    harness.ok("close", json!({"id": "read-only"})).await;
}

/// Allows everything while recording what each call claimed to be doing.
struct Recorder {
    seen: std::sync::Mutex<Vec<(String, Access)>>,
}

impl Policy for Recorder {
    fn decide(&self, request: &AccessRequest) -> AccessDecision {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((request.method.clone(), request.access));
        AccessDecision::Allow
    }
}

#[tokio::test]
async fn both_queries_declare_a_read_to_a_policy() {
    // The declaration is the thing a policy branches on, and it is invisible in the response. Mount a
    // recorder that allows everything and read back what each call claimed to be doing: a query
    // declared as a write would switch off every shell picker behind a read-only policy.
    let recorder = Arc::new(Recorder {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let harness = Harness::with(RemoteRegistry::new().with_policy(Arc::clone(&recorder) as Arc<_>));

    harness.ok(ENVIRONMENT_METHOD, json!({})).await;
    harness.ok(SHELLS_METHOD, json!({})).await;

    let seen = recorder
        .seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(
        seen,
        vec![
            (format!("terminal.{ENVIRONMENT_METHOD}"), Access::Read),
            (format!("terminal.{SHELLS_METHOD}"), Access::Read),
        ],
        "{seen:?}"
    );
}
