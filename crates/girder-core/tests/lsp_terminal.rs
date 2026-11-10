//! Tests for the LSP seam and terminal sessions.
//!
//! Both convert between two conventions — UTF-16 and bytes for positions, a bounded transcript for a shell that
//! prints forever — and both get the conversion wrong if unguarded.

use std::sync::Arc;
use std::time::Duration;

use girder_core::{
    LspAnswer, LspError, LspLocation, LspOperation, LspPosition, LspRange, LspRegistry, LspRequest,
    StaticLsp, TerminalError, TerminalLaunch, TerminalRegistry, TerminalSession, TerminalSize,
    TerminalStatus, Transcript, utf16_column_to_byte,
};

// ---- LSP positions ----------------------------------------------------------------------------

#[test]
fn an_ascii_column_maps_to_its_byte() {
    assert_eq!(utf16_column_to_byte("hello", 2), Some(2));
    assert_eq!(utf16_column_to_byte("hello", 5), Some(5));
}

#[test]
fn a_multibyte_character_occupies_one_utf16_unit() {
    // `字` is three UTF-8 bytes and one UTF-16 unit, so column 1 is byte 3.
    assert_eq!(utf16_column_to_byte("字x", 1), Some(3));
    assert_eq!(utf16_column_to_byte("字x", 2), Some(4));
}

#[test]
fn an_out_of_range_column_has_no_byte() {
    assert_eq!(utf16_column_to_byte("hi", 5), None);
}

#[test]
fn an_empty_line_maps_only_column_zero() {
    assert_eq!(utf16_column_to_byte("", 0), Some(0));
    assert_eq!(utf16_column_to_byte("", 1), None);
}

// ---- LSP operations ---------------------------------------------------------------------------

#[test]
fn each_operation_names_its_protocol_method() {
    assert_eq!(LspOperation::Definition.method(), "textDocument/definition");
    assert_eq!(LspOperation::References.method(), "textDocument/references");
    assert_eq!(
        LspOperation::Implementation.method(),
        "textDocument/implementation"
    );
    assert_eq!(LspOperation::Hover.method(), "textDocument/hover");
}

#[test]
fn hover_is_the_one_that_returns_prose() {
    // A consumer that expected locations would render an empty list for an answer it actually received.
    assert!(!LspOperation::Hover.returns_locations());
    assert!(LspOperation::Definition.returns_locations());
    assert!(LspOperation::References.returns_locations());
}

#[test]
fn a_range_reports_whether_it_is_empty() {
    let position = LspPosition::new(1, 2);

    assert!(LspRange::new(position, position).is_empty());
    assert!(!LspRange::new(position, LspPosition::new(1, 3)).is_empty());
}

// ---- the registry -----------------------------------------------------------------------------

struct Server;

impl girder_core::LspProvider for Server {
    fn name(&self) -> &'static str {
        "static"
    }

    fn extensions(&self) -> &[String] {
        &[]
    }

    fn query(&self, _request: LspRequest) -> girder_core::BoxFut<Result<LspAnswer, LspError>> {
        Box::pin(async { Ok(LspAnswer::Nothing) })
    }
}

fn server(name: &str, extensions: Vec<&str>) -> Arc<StaticLsp> {
    Arc::new(StaticLsp::new(
        name,
        extensions.into_iter().map(str::to_owned).collect(),
    ))
}

#[tokio::test]
async fn a_query_reaches_the_server_that_serves_the_file() {
    let mut rust = StaticLsp::new("rust", vec!["rs".to_owned()]);
    rust.answer(
        1,
        2,
        LspAnswer::Locations(vec![LspLocation {
            path: "/src/a.rs".into(),
            range: LspRange::new(LspPosition::new(10, 0), LspPosition::new(10, 5)),
        }]),
    );
    let registry = LspRegistry::new();
    registry.register(Arc::new(rust)).expect("registered");
    registry
        .register(server("python", vec!["py"]))
        .expect("registered");

    let answer = registry
        .query(LspRequest::new(
            LspOperation::Definition,
            "/work",
            "/src/b.rs",
            LspPosition::new(1, 2),
        ))
        .await
        .expect("answered");

    assert_eq!(answer.locations().len(), 1);
    assert!(answer.locations()[0].path.ends_with("a.rs"));
}

#[tokio::test]
async fn a_file_no_server_serves_is_refused() {
    // Named in the error, so a caller can tell which file had no server.
    let registry = LspRegistry::new();
    registry
        .register(server("rust", vec!["rs"]))
        .expect("registered");

    let error = registry
        .query(LspRequest::new(
            LspOperation::Definition,
            "/work",
            "/src/a.xyz",
            LspPosition::new(0, 0),
        ))
        .await
        .expect_err("no provider");

    let LspError::NoProvider { path } = error else {
        panic!("expected NoProvider");
    };
    assert!(path.ends_with("a.xyz"));
}

#[test]
fn an_extension_match_is_case_insensitive() {
    use girder_core::LspProvider as _;
    let provider = StaticLsp::new("rust", vec!["rs".to_owned()]);

    assert!(provider.serves(std::path::Path::new("a.RS")));
    assert!(!provider.serves(std::path::Path::new("a.py")));
}

#[test]
fn the_registry_reports_its_extensions() {
    let registry = LspRegistry::new();
    registry
        .register(server("rust", vec!["rs"]))
        .expect("registered");
    registry
        .register(server("python", vec!["py"]))
        .expect("registered");

    assert_eq!(registry.extensions(), vec!["py", "rs"]);
}

#[test]
fn a_duplicate_server_name_is_refused() {
    let registry = LspRegistry::new();
    registry
        .register(server("a", vec!["rs"]))
        .expect("registered");

    let error = registry
        .register(server("a", vec!["py"]))
        .expect_err("duplicate");
    assert_eq!(error, "a");
    let _ = Server;
}

#[tokio::test]
async fn nothing_is_distinct_from_an_empty_location_list() {
    // "No definition exists" and "I could not determine one" are different facts.
    let registry = LspRegistry::new();
    registry
        .register(server("a", vec!["rs"]))
        .expect("registered");

    let answer = registry
        .query(LspRequest::new(
            LspOperation::Hover,
            "/work",
            "/a.rs",
            LspPosition::new(0, 0),
        ))
        .await
        .expect("answered");

    assert_eq!(answer, LspAnswer::Nothing);
    assert!(answer.locations().is_empty());
}

// ---- terminal ---------------------------------------------------------------------------------

#[test]
fn a_headless_size_is_fixed() {
    // A session that inherited the launching process's size would produce output a test cannot reproduce.
    assert_eq!(
        TerminalSize::headless(),
        TerminalSize {
            columns: 80,
            rows: 24
        }
    );
}

#[test]
fn a_zero_dimension_is_refused() {
    // A program that divided by it would fail in its own way rather than in one place.
    assert!(TerminalSize::new(0, 24).is_none());
    assert!(TerminalSize::new(80, 0).is_none());
    assert!(TerminalSize::new(80, 24).is_some());
}

#[test]
fn a_transcript_bounds_itself_as_it_grows() {
    // A terminal open for an hour holds everything ever printed.
    let mut transcript = Transcript::new(10);
    transcript.push("aaaaaaaaaa");
    transcript.push("bbbbbbbbbb");

    assert!(transcript.retained_bytes() <= 10);
    assert_eq!(transcript.text(), "bbbbbbbbbb");
    assert_eq!(
        transcript.end_offset(),
        20,
        "the offset counts everything appended"
    );
}

#[test]
fn a_bounded_transcript_reports_what_it_dropped() {
    let mut transcript = Transcript::new(5);
    transcript.push("aaaa");
    transcript.push("bbbb");

    assert_eq!(
        transcript.omitted(),
        girder_core::Omitted::Exact { count: 4 }
    );
}

#[test]
fn an_unbounded_transcript_reports_no_omission() {
    let mut transcript = Transcript::new(1000);
    transcript.push("hello");

    assert_eq!(transcript.omitted(), girder_core::Omitted::None);
}

#[test]
fn a_zero_bound_is_not_unbounded() {
    let mut transcript = Transcript::new(0);
    transcript.push("a");
    transcript.push("b");

    assert_eq!(transcript.retained_bytes(), 1, "one chunk stays");
}

/// A shell that records what it was asked to write and answers reads from a queue.
struct FakeShell {
    output: std::sync::Mutex<std::collections::VecDeque<String>>,
    written: std::sync::Mutex<Vec<String>>,
    closed: std::sync::atomic::AtomicBool,
}

impl FakeShell {
    fn new(outputs: Vec<&str>) -> Arc<Self> {
        Arc::new(Self {
            output: std::sync::Mutex::new(outputs.into_iter().map(str::to_owned).collect()),
            written: std::sync::Mutex::new(Vec::new()),
            closed: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

impl girder_core::Shell for FakeShell {
    fn write(&self, input: &str) -> girder_core::BoxFut<Result<(), TerminalError>> {
        self.written.lock().unwrap().push(input.to_owned());
        Box::pin(async { Ok(()) })
    }

    fn read(&self, _wait: Duration) -> girder_core::BoxFut<Result<Option<String>, TerminalError>> {
        let next = self.output.lock().unwrap().pop_front();
        Box::pin(async move { Ok(next) })
    }

    fn close(&self) -> girder_core::BoxFut<Result<(), TerminalError>> {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn a_session_records_what_it_reads() {
    let shell = FakeShell::new(vec!["$ ", "output\n"]);
    let session = TerminalSession::new(
        "t1",
        Arc::clone(&shell) as Arc<dyn girder_core::Shell>,
        TerminalSize::headless(),
        1024,
    );

    session.read(Duration::from_millis(1)).await.expect("read");
    session.read(Duration::from_millis(1)).await.expect("read");

    assert_eq!(session.transcript(), "$ output\n");
    assert_eq!(session.status(), TerminalStatus::Running);
}

#[tokio::test]
async fn a_read_with_nothing_available_returns_none() {
    // The normal answer for a shell sitting at a prompt.
    let shell = FakeShell::new(Vec::new());
    let session = TerminalSession::new(
        "t1",
        Arc::clone(&shell) as Arc<dyn girder_core::Shell>,
        TerminalSize::headless(),
        1024,
    );

    assert!(
        session
            .read(Duration::from_millis(1))
            .await
            .expect("read")
            .is_none()
    );
}

#[tokio::test]
async fn a_closed_session_refuses_input() {
    let shell = FakeShell::new(Vec::new());
    let session = TerminalSession::new(
        "t1",
        Arc::clone(&shell) as Arc<dyn girder_core::Shell>,
        TerminalSize::headless(),
        1024,
    );

    session.close().await.expect("closed");
    let error = session.send("ls\n").await.expect_err("not running");

    assert!(matches!(error, TerminalError::NotRunning { .. }));
}

#[tokio::test]
async fn the_registry_tracks_running_sessions() {
    let registry = TerminalRegistry::new();
    let shell = FakeShell::new(Vec::new());
    let session = Arc::new(TerminalSession::new(
        "t1",
        Arc::clone(&shell) as Arc<dyn girder_core::Shell>,
        TerminalSize::headless(),
        1024,
    ));
    registry.add(Arc::clone(&session)).expect("added");

    assert_eq!(registry.ids(), vec!["t1"]);
    assert_eq!(registry.running(), vec!["t1"]);

    session.close().await.expect("closed");
    assert!(registry.running().is_empty());
}

#[tokio::test]
async fn removing_a_session_closes_it_first() {
    // A session removed without being closed leaves a process with nothing that could stop it.
    let registry = TerminalRegistry::new();
    let shell = FakeShell::new(Vec::new());
    let session = Arc::new(TerminalSession::new(
        "t1",
        Arc::clone(&shell) as Arc<dyn girder_core::Shell>,
        TerminalSize::headless(),
        1024,
    ));
    registry.add(session).expect("added");

    registry.remove("t1").await.expect("removed");

    assert!(shell.closed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(registry.ids().is_empty());
}

#[test]
fn a_launch_needs_an_id() {
    assert!(TerminalLaunch::new("").is_none());
    assert!(TerminalLaunch::new("t1").is_some());
}

#[test]
fn a_launch_defaults_to_a_headless_size_and_a_bounded_transcript() {
    let launch = TerminalLaunch::new("t1").expect("valid");

    assert_eq!(launch.size, TerminalSize::headless());
    assert_eq!(launch.max_bytes, girder_core::DEFAULT_TRANSCRIPT_BYTES);
}

#[test]
fn the_status_names_are_stable() {
    assert_eq!(TerminalStatus::Stopped.as_str(), "stopped");
    assert_eq!(TerminalStatus::Running.as_str(), "running");
    assert_eq!(TerminalStatus::Exited { code: Some(0) }.as_str(), "exited");
    assert_eq!(
        TerminalStatus::Failed {
            reason: "x".to_owned()
        }
        .as_str(),
        "failed"
    );
}
