//! Tests for `session.openWorkspacePath`.
//!
//! The method is the session-named spelling of `host.openPath`, so its properties are the same: the workspace is the
//! boundary, and a refusal is REFUSED rather than a retryable failure.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use girder_core::Opener;
use girder_core::{RemoteCall, RemoteError, RemoteRegistry};
use serde_json::json;

fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-open-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// Records what it was asked to open, so a test can assert the call reached the opener.
#[derive(Default)]
struct Recorder {
    opened: std::sync::Mutex<Vec<PathBuf>>,
    revealed: std::sync::Mutex<Vec<PathBuf>>,
}

impl Opener for Recorder {
    fn open(&self, path: &Path) -> Result<(), String> {
        self.opened
            .lock()
            .expect("unpoisoned")
            .push(path.to_path_buf());
        Ok(())
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        self.revealed
            .lock()
            .expect("unpoisoned")
            .push(path.to_path_buf());
        Ok(())
    }
}

/// A recorder that always refuses, for propagation.
struct Refusing;

impl Opener for Refusing {
    fn open(&self, _path: &Path) -> Result<(), String> {
        Err("no desktop".to_owned())
    }
    fn reveal(&self, _path: &Path) -> Result<(), String> {
        Err("no desktop".to_owned())
    }
}

fn registry(workspace: &Path, recorder: &Arc<dyn Opener>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    girder_core::register_open_workspace_path(&mut registry, workspace, recorder);
    registry
}

#[tokio::test]
async fn an_in_workspace_file_is_opened() {
    let dir = scratch("open");
    std::fs::write(dir.join("a.txt"), "x").expect("written");
    let recorder = Arc::new(Recorder::default());

    let value = registry(&dir, &(Arc::clone(&recorder) as Arc<dyn Opener>))
        .dispatch(
            "session",
            "openWorkspacePath",
            call(json!({"path": "a.txt"})),
        )
        .await
        .expect("opened");

    assert_eq!(value["opened"], true);
    assert_eq!(value["action"], "open");
    assert_eq!(recorder.opened.lock().expect("unpoisoned").len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn reveal_goes_through_the_reveal_path() {
    let dir = scratch("reveal");
    std::fs::write(dir.join("a.txt"), "x").expect("written");
    let recorder = Arc::new(Recorder::default());

    let value = registry(&dir, &(Arc::clone(&recorder) as Arc<dyn Opener>))
        .dispatch(
            "session",
            "openWorkspacePath",
            call(json!({"path": "a.txt", "action": "reveal"})),
        )
        .await
        .expect("revealed");

    assert_eq!(value["action"], "reveal");
    assert_eq!(recorder.opened.lock().expect("unpoisoned").len(), 0);
    assert_eq!(recorder.revealed.lock().expect("unpoisoned").len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_path_outside_the_workspace_is_refused_without_reaching_the_opener() {
    // The boundary is the point of the method: a client must not be able to open something the tools could not read.
    let dir = scratch("escape");
    let recorder = Arc::new(Recorder::default());

    let error = registry(&dir, &(Arc::clone(&recorder) as Arc<dyn Opener>))
        .dispatch(
            "session",
            "openWorkspacePath",
            call(json!({"path": "../../etc/passwd"})),
        )
        .await
        .expect_err("refused");

    assert!(matches!(error, RemoteError::Refused { .. }));
    assert_eq!(
        recorder.opened.lock().expect("unpoisoned").len(),
        0,
        "a refused path must not reach the opener"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_absolute_path_outside_the_workspace_is_refused() {
    let dir = scratch("absolute");
    let recorder = Arc::new(Recorder::default());

    let error = registry(&dir, &(Arc::clone(&recorder) as Arc<dyn Opener>))
        .dispatch(
            "session",
            "openWorkspacePath",
            call(json!({"path": "/etc/passwd"})),
        )
        .await
        .expect_err("refused");

    assert!(matches!(error, RemoteError::Refused { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_path_is_an_argument_error_not_a_refusal() {
    // A typo and a permission problem call for different reactions: one to fix, one to stop.
    let dir = scratch("missing");
    let recorder = Arc::new(Recorder::default());

    let error = registry(&dir, &(recorder as Arc<dyn Opener>))
        .dispatch(
            "session",
            "openWorkspacePath",
            call(json!({"path": "absent.txt"})),
        )
        .await
        .expect_err("missing");

    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unknown_action_is_refused_rather_than_defaulted() {
    // A client that asked for something specific and silently got `open` would report the wrong thing.
    let dir = scratch("action");
    std::fs::write(dir.join("a.txt"), "x").expect("written");
    let recorder = Arc::new(Recorder::default());

    let error = registry(&dir, &(recorder as Arc<dyn Opener>))
        .dispatch(
            "session",
            "openWorkspacePath",
            call(json!({"path": "a.txt", "action": "print"})),
        )
        .await
        .expect_err("unknown action");

    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_missing_path_argument_is_refused() {
    let dir = scratch("nopath");
    let recorder = Arc::new(Recorder::default());

    let error = registry(&dir, &(recorder as Arc<dyn Opener>))
        .dispatch("session", "openWorkspacePath", call(json!({})))
        .await
        .expect_err("missing argument");

    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_opener_failure_propagates() {
    let dir = scratch("failing");
    std::fs::write(dir.join("a.txt"), "x").expect("written");

    let refusing: Arc<dyn Opener> = Arc::new(Refusing);
    let error = registry(&dir, &refusing)
        .dispatch(
            "session",
            "openWorkspacePath",
            call(json!({"path": "a.txt"})),
        )
        .await
        .expect_err("opener failed");

    let RemoteError::Failed(reason) = error else {
        panic!("expected Failed");
    };
    assert!(reason.contains("no desktop"), "{reason}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_response_echoes_the_canonical_path() {
    // Echoing the caller's spelling would let two different inputs claim the same outcome.
    let dir = scratch("echo");
    std::fs::write(dir.join("a.txt"), "x").expect("written");
    let recorder = Arc::new(Recorder::default());

    let value = registry(&dir, &(recorder as Arc<dyn Opener>))
        .dispatch(
            "session",
            "openWorkspacePath",
            call(json!({"path": "./a.txt"})),
        )
        .await
        .expect("opened");

    let shown = value["path"].as_str().expect("a path");
    assert!(shown.ends_with("a.txt"), "{shown}");
    assert!(!shown.contains("./"), "the canonical form: {shown}");
    let _ = std::fs::remove_dir_all(&dir);
}
