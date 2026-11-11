//! Tests for terminal session bookkeeping.
//!
//! The property worth pinning here is the one a session cannot learn from `close`: a shell that exited on
//! its own never calls it. A session that only trusted `close` would keep answering `running`, and a reader
//! would be told a terminal is available while the process behind it is gone.
//!
//! The shells are fakes because the question is about what the session does with the answer, not about how
//! a real pty observes its child. Each fake decides its own liveness, so the tests are deterministic.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use girder_core::{
    BoxFut, Shell, TerminalError, TerminalRegistry, TerminalSession, TerminalSize, TerminalStatus,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// A shell whose liveness, exit code, and remaining output a test controls.
struct FakeShell {
    /// Whether the process is reported as running.
    alive: AtomicBool,
    /// The code reported once it is not, if the shell can see one.
    code: Option<i32>,
    /// Output chunks queued for the next reads.
    pending: std::sync::Mutex<std::collections::VecDeque<String>>,
    /// Inputs received, in order.
    written: std::sync::Mutex<Vec<String>>,
    /// Whether `close` was called.
    closed: AtomicBool,
    /// Whether `close` should report a failure.
    close_fails: bool,
}

impl FakeShell {
    /// A shell in the given state.
    fn new(alive: bool, code: Option<i32>, close_fails: bool) -> Arc<Self> {
        Arc::new(Self {
            alive: AtomicBool::new(alive),
            code,
            pending: std::sync::Mutex::new(std::collections::VecDeque::new()),
            written: std::sync::Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            close_fails,
        })
    }

    /// A live shell with nothing queued and no exit code to report.
    fn live() -> Arc<Self> {
        Self::new(true, None, false)
    }

    /// A shell that exited on its own before anybody asked, reporting `code`.
    fn exited(code: Option<i32>) -> Arc<Self> {
        Self::new(false, code, false)
    }

    /// A shell whose `close` reports a failure.
    fn failing_close() -> Arc<Self> {
        Self::new(true, None, true)
    }

    /// Queues one chunk for the next read.
    fn queue(&self, chunk: &str) -> &Self {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(chunk.to_owned());
        self
    }

    /// The inputs it received.
    fn written(&self) -> Vec<String> {
        self.written
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
        self.closed.store(true, Ordering::SeqCst);
        self.alive.store(false, Ordering::SeqCst);
        let fails = self.close_fails;
        Box::pin(async move {
            if fails {
                Err(TerminalError::NotRunning {
                    id: "t1".to_owned(),
                    status: "closed",
                })
            } else {
                Ok(())
            }
        })
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn exit_code(&self) -> Option<i32> {
        // `None` while alive, so a shell cannot claim an exit it has not had.
        if self.alive.load(Ordering::SeqCst) {
            None
        } else {
            self.code
        }
    }
}

/// A shell written against the trait as it was before liveness was observable.
///
/// It overrides neither `is_alive` nor `exit_code`, which is the compatibility case: an implementation
/// someone else compiled against the old trait must still build, and must keep answering `running` rather
/// than being failed by a default that guessed.
struct LegacyShell;

impl Shell for LegacyShell {
    fn write(&self, _input: &str) -> BoxFut<Result<(), TerminalError>> {
        Box::pin(async { Ok(()) })
    }

    fn read(&self, _wait: Duration) -> BoxFut<Result<Option<String>, TerminalError>> {
        Box::pin(async { Ok(None) })
    }

    fn close(&self) -> BoxFut<Result<(), TerminalError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Wraps a shell in a session with the headless size and a small transcript.
fn session(shell: Arc<dyn Shell>) -> TerminalSession {
    TerminalSession::new("t1", shell, TerminalSize::headless(), 1024)
}

// ---- a shell that exited on its own -----------------------------------------------------------

#[tokio::test]
async fn an_exited_shell_is_not_reported_as_running() {
    // The defect: nobody called `close`, so nothing ever moved the status off `running`.
    let session = session(FakeShell::exited(Some(0)));

    assert_ne!(
        session.status(),
        TerminalStatus::Running,
        "a dead shell must not read as a terminal a caller can use"
    );
    assert_eq!(session.status(), TerminalStatus::Exited { code: Some(0) });
}

#[tokio::test]
async fn an_exited_shell_reports_the_code_it_gave() {
    let session = session(FakeShell::exited(Some(3)));

    assert_eq!(session.status(), TerminalStatus::Exited { code: Some(3) });
}

#[tokio::test]
async fn a_shell_that_cannot_report_a_code_does_not_get_a_zero() {
    // A signalled process, or a transport that cannot see one: inventing `Some(0)` would report a crash as
    // a clean exit.
    let session = session(FakeShell::exited(None));

    assert_eq!(session.status(), TerminalStatus::Exited { code: None });
}

#[tokio::test]
async fn a_status_read_is_stable_once_the_shell_is_gone() {
    // The second observation must not re-decide anything, and must not lose the code the first one read.
    let session = session(FakeShell::exited(Some(7)));

    let first = session.status();
    let second = session.status();

    assert_eq!(first, second);
    assert_eq!(second, TerminalStatus::Exited { code: Some(7) });
}

#[tokio::test]
async fn a_shell_that_exits_while_open_is_noticed_on_the_next_read_of_status() {
    let shell = FakeShell::live();
    let session = session(Arc::clone(&shell) as Arc<dyn Shell>);
    assert_eq!(session.status(), TerminalStatus::Running);

    // The process exits between two calls, which is the case no `close` can ever report.
    shell.alive.store(false, Ordering::SeqCst);

    assert_eq!(session.status(), TerminalStatus::Exited { code: None });
}

#[tokio::test]
async fn an_exited_shell_refuses_input() {
    let shell = FakeShell::exited(Some(0));
    let session = session(Arc::clone(&shell) as Arc<dyn Shell>);

    let error = session.send("ls\n").await.expect_err("not running");

    assert!(matches!(error, TerminalError::NotRunning { .. }));
    assert!(
        shell.written().is_empty(),
        "a dead shell must not receive the input either"
    );
}

#[tokio::test]
async fn a_registry_stops_calling_an_exited_session_running() {
    // The status line a caller reads comes from the registry as often as from the session.
    let shell = FakeShell::exited(Some(0));
    let registry = TerminalRegistry::new();
    registry
        .add(Arc::new(session(Arc::clone(&shell) as Arc<dyn Shell>)))
        .expect("added");

    assert!(
        registry.running().is_empty(),
        "a terminal whose process is gone must not be listed as available"
    );
}

#[tokio::test]
async fn output_that_arrived_before_the_exit_is_still_readable() {
    // Noticing the exit must not throw away what the shell printed on its way out.
    let shell = FakeShell::exited(Some(0));
    shell.queue("bye\n");
    let session = session(Arc::clone(&shell) as Arc<dyn Shell>);

    let chunk = session.read(Duration::from_millis(1)).await.expect("read");

    assert_eq!(chunk.as_deref(), Some("bye\n"));
    assert_eq!(session.transcript(), "bye\n");
    assert_eq!(session.status(), TerminalStatus::Exited { code: Some(0) });
}

// ---- a shell that is still alive --------------------------------------------------------------

#[tokio::test]
async fn a_live_shell_stays_running() {
    let session = session(FakeShell::live());

    assert_eq!(session.status(), TerminalStatus::Running);
    session.send("ls\n").await.expect("sent");
    assert_eq!(session.status(), TerminalStatus::Running);
}

#[tokio::test]
async fn closing_a_live_shell_still_records_stopped() {
    // The caller ended it, so the exit is not the shell's own and `stopped` is the honest word.
    let session = session(FakeShell::live());

    session.close().await.expect("closed");

    assert_eq!(session.status(), TerminalStatus::Stopped);
}

#[tokio::test]
async fn a_failed_close_records_the_reason() {
    let session = session(FakeShell::failing_close());

    session.close().await.expect_err("close fails");

    let TerminalStatus::Failed { reason } = session.status() else {
        panic!("a shell that failed to close must read as failed");
    };
    assert!(!reason.is_empty(), "the reason must name what went wrong");
}

#[tokio::test]
async fn closing_an_already_exited_shell_keeps_how_it_ended() {
    // `close` on a session that was already seen to exit must not rewrite `exited` to `stopped`, which
    // would throw away the one fact a caller can use to tell a clean exit from a crash.
    let session = session(FakeShell::exited(Some(4)));
    assert_eq!(session.status(), TerminalStatus::Exited { code: Some(4) });

    session.close().await.expect("closed");

    assert_eq!(session.status(), TerminalStatus::Exited { code: Some(4) });
}

// ---- compatibility ----------------------------------------------------------------------------

#[tokio::test]
async fn a_shell_that_does_not_report_liveness_keeps_running() {
    // The default answer is `true`: adding liveness must not fail every implementation that predates it.
    let session = session(Arc::new(LegacyShell));

    assert_eq!(session.status(), TerminalStatus::Running);
    session.send("ls\n").await.expect("sent");
    assert_eq!(session.status(), TerminalStatus::Running);
}

#[test]
fn a_shell_that_does_not_report_liveness_has_no_exit_code() {
    let shell = LegacyShell;

    assert!(shell.is_alive());
    assert_eq!(shell.exit_code(), None);
}
