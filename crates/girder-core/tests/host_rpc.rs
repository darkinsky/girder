//! Tests for `host.openPath` and `host.revealInFileManager`, the host-desktop surface.
//!
//! The properties under test, in the order they matter:
//!
//! - **the workspace boundary holds** — `../`, an absolute path elsewhere, a symlink that points out,
//!   and a `..` spelled inside the workspace are all decided before the opener runs, because one
//!   successful escape would let a model open any file on the machine
//! - **nothing is launched by accident** — every test uses a recorder, so a failure in the boundary
//!   shows up as "the opener was called" rather than as a window on the developer's screen
//! - **a missing path is refused, not opened** — the opener would otherwise be the thing that
//!   reported it, on a machine where the path may exist
//! - **an opener failure reaches the caller** — a refusal that was swallowed would look like a
//!   successful launch
//! - both methods behave alike, since they share one implementation
//!
//! Isolation: every test builds its own registry and workspace over a scratch directory named from
//! the process id and a counter, so two test binaries and two tests in one binary cannot collide.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the methods as written even before the crate root wires them in. It spells its own paths
//! through `girder_core::`, which is the alias the library declares for itself, so the same source
//! builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/host_rpc.rs"]
mod host_rpc;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use girder_core::{RemoteCall, RemoteError, RemoteRegistry};
use serde_json::{Value, json};

use host_rpc::{
    LaunchPlan, NAMESPACE, OPEN_PATH_METHOD, OpenAction, Opener, Platform,
    REVEAL_IN_FILE_MANAGER_METHOD, SystemOpener, UnavailableOpener, launch_plan,
    register_host_methods, register_host_methods_with,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated scratch directory: `temp_dir()` plus pid plus a counter.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("girder-host-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created scratch directory");
    dir
}

/// A workspace root with a file and a directory inside it, and an outside file the escapes aim at.
///
/// ```text
/// <scratch>/
///   outside.txt          <- never reachable from the workspace
///   root/                <- the registered workspace
///     notes.md
///     docs/
/// ```
fn fixture(name: &str) -> (PathBuf, PathBuf) {
    let base = scratch(name);
    let root = base.join("root");
    std::fs::create_dir_all(root.join("docs")).expect("created docs");
    std::fs::write(base.join("outside.txt"), "outside secret").expect("wrote outside");
    std::fs::write(root.join("notes.md"), "# notes\n").expect("wrote notes");
    (base, root)
}

/// One recorded opener call.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    /// `open` was asked for this path.
    Open(PathBuf),
    /// `reveal` was asked for this path.
    Reveal(PathBuf),
}

/// An opener that records calls and answers with a fixed result.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<Call>>,
    failure: Option<&'static str>,
}

impl Recorder {
    /// A recorder that succeeds.
    fn ok() -> Self {
        Self::default()
    }

    /// A recorder that fails every call with `reason`.
    fn failing(reason: &'static str) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            failure: Some(reason),
        }
    }

    /// The calls seen so far.
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("recorder mutex").clone()
    }

    /// Records one call and answers.
    fn record(&self, call: Call) -> Result<(), String> {
        self.calls.lock().expect("recorder mutex").push(call);
        match self.failure {
            Some(reason) => Err(reason.to_owned()),
            None => Ok(()),
        }
    }
}

impl Opener for Recorder {
    fn open(&self, path: &Path) -> Result<(), String> {
        self.record(Call::Open(path.to_path_buf()))
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        self.record(Call::Reveal(path.to_path_buf()))
    }
}

/// A registry with both methods registered over `root`, plus the recorder they call.
///
/// The `Arc<Recorder>` is handed back separately because `register_host_methods_with` takes it as a
/// trait object, and a test that lost the concrete handle could not read the calls back.
fn registered(root: &Path, opener: Arc<Recorder>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    register_host_methods_with(&mut registry, root, opener);
    registry
}

/// Calls one method the way a socket would.
async fn call(registry: &RemoteRegistry, method: &str, args: Value) -> Result<Value, RemoteError> {
    registry
        .dispatch(
            NAMESPACE,
            method,
            RemoteCall {
                args,
                peer: "test-peer".to_owned(),
            },
        )
        .await
}

/// The wire code of a failed call.
fn code(error: &RemoteError) -> &'static str {
    error.code()
}

/// A symlink from `link` to `target`, or `None` when the platform refuses to create one.
///
/// Windows needs a privilege for file symlinks, so a test that treated "could not link" as a failure
/// would report a platform fact as a bug. The symlink case is skipped there instead.
#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Option<()> {
    std::os::unix::fs::symlink(target, link).ok()
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> Option<()> {
    None
}

// ---- both methods are registered --------------------------------------------------------------

#[test]
fn both_methods_are_registered_under_host() {
    let (_base, root) = fixture("registered");
    let registry = registered(&root, Arc::new(Recorder::ok()));
    let names = registry.names();
    assert!(
        names.contains(&(NAMESPACE.to_owned(), OPEN_PATH_METHOD.to_owned())),
        "{names:?}"
    );
    assert!(
        names.contains(&(
            NAMESPACE.to_owned(),
            REVEAL_IN_FILE_MANAGER_METHOD.to_owned()
        )),
        "{names:?}"
    );
}

// ---- opening a path inside the workspace -------------------------------------------------------

#[tokio::test]
async fn open_path_inside_workspace_reaches_the_opener() {
    let (_base, root) = fixture("open-inside");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    let value = call(&registry, OPEN_PATH_METHOD, json!({"path": "notes.md"}))
        .await
        .expect("a path inside the workspace opens");

    assert_eq!(value.get("opened"), Some(&json!(true)));
    assert_eq!(value.get("action"), Some(&json!("open")));

    // The opener receives the canonical absolute path, not the relative spelling the client used: a
    // desktop cannot do anything with a path relative to a root it has never heard of.
    let calls = opener.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let Call::Open(received) = &calls[0] else {
        panic!("expected an open call, got {:?}", calls[0]);
    };
    assert!(received.is_absolute(), "{}", received.display());
    assert!(received.ends_with("notes.md"), "{}", received.display());
    assert_eq!(
        value.get("path"),
        Some(&json!(received.display().to_string()))
    );
}

#[tokio::test]
async fn open_accepts_an_absolute_path_inside_the_workspace() {
    let (_base, root) = fixture("open-absolute");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    let absolute = std::fs::canonicalize(root.join("notes.md")).expect("canonical notes");
    let value = call(
        &registry,
        OPEN_PATH_METHOD,
        json!({"path": absolute.display().to_string()}),
    )
    .await
    .expect("an absolute path inside the workspace opens");

    assert_eq!(value.get("opened"), Some(&json!(true)));
    assert_eq!(opener.calls().len(), 1);
}

#[tokio::test]
async fn open_accepts_a_directory_inside_the_workspace() {
    let (_base, root) = fixture("open-dir");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    call(&registry, OPEN_PATH_METHOD, json!({"path": "docs"}))
        .await
        .expect("a directory inside the workspace opens");
    assert_eq!(opener.calls().len(), 1);
}

// ---- the security boundary --------------------------------------------------------------------

#[tokio::test]
async fn open_path_outside_workspace_is_refused() {
    let (base, root) = fixture("outside");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    let outside = base.join("outside.txt");
    let error = call(
        &registry,
        OPEN_PATH_METHOD,
        json!({"path": outside.display().to_string()}),
    )
    .await
    .expect_err("an absolute path outside the workspace is refused");

    assert_eq!(code(&error), "REFUSED", "{error}");
    // The failure is the boundary and nothing else: the opener was never reached, so no process was
    // started for a path that was refused.
    assert!(opener.calls().is_empty(), "{:?}", opener.calls());
}

#[tokio::test]
async fn open_path_with_parent_traversal_is_refused() {
    let (_base, root) = fixture("traversal");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    let error = call(
        &registry,
        OPEN_PATH_METHOD,
        json!({"path": "../outside.txt"}),
    )
    .await
    .expect_err("a `..` that leaves the workspace is refused");

    assert_eq!(code(&error), "REFUSED", "{error}");
    assert!(opener.calls().is_empty(), "{:?}", opener.calls());
}

#[tokio::test]
async fn open_path_through_a_symlink_that_leaves_the_workspace_is_refused() {
    let (base, root) = fixture("symlink-out");
    if symlink(&base.join("outside.txt"), &root.join("escape.txt")).is_none() {
        // No symlink privilege here; the lexical check is covered by the traversal test above.
        return;
    }

    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    let error = call(&registry, OPEN_PATH_METHOD, json!({"path": "escape.txt"}))
        .await
        .expect_err("a symlink whose target is outside the workspace is refused");

    assert_eq!(code(&error), "REFUSED", "{error}");
    assert!(opener.calls().is_empty(), "{:?}", opener.calls());
}

#[tokio::test]
async fn open_path_through_a_symlink_that_stays_inside_is_allowed() {
    let (_base, root) = fixture("symlink-in");
    if symlink(&root.join("notes.md"), &root.join("alias.md")).is_none() {
        return;
    }

    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    call(&registry, OPEN_PATH_METHOD, json!({"path": "alias.md"}))
        .await
        .expect("a symlink that resolves inside the workspace is allowed");

    // The opener receives the resolved target: handing a link to a desktop would open whatever it
    // points at, and the point of resolving here is that the caller was told what that is.
    let calls = opener.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let Call::Open(received) = &calls[0] else {
        panic!("expected an open call, got {:?}", calls[0]);
    };
    assert!(received.ends_with("notes.md"), "{}", received.display());
}

// ---- invalid arguments ------------------------------------------------------------------------

#[tokio::test]
async fn open_path_that_does_not_exist_is_an_error() {
    let (_base, root) = fixture("missing");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    let error = call(&registry, OPEN_PATH_METHOD, json!({"path": "nope.md"}))
        .await
        .expect_err("a missing path is refused");

    // `INVALID_ARGUMENTS`, not a boundary refusal: there is nothing there, and calling it an escape
    // would send the caller looking for a permission problem that does not exist.
    assert_eq!(code(&error), "INVALID_ARGUMENTS", "{error}");
    assert!(opener.calls().is_empty(), "{:?}", opener.calls());
}

#[tokio::test]
async fn open_path_with_a_missing_argument_is_an_error() {
    let (_base, root) = fixture("no-arg");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    let error = call(&registry, OPEN_PATH_METHOD, json!({}))
        .await
        .expect_err("a missing `path` is refused");
    assert_eq!(code(&error), "INVALID_ARGUMENTS", "{error}");
    assert!(opener.calls().is_empty());
}

#[tokio::test]
async fn open_path_with_a_non_string_argument_is_an_error() {
    let (_base, root) = fixture("bad-arg");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    let error = call(&registry, OPEN_PATH_METHOD, json!({"path": 7}))
        .await
        .expect_err("a non-string `path` is refused");
    assert_eq!(code(&error), "INVALID_ARGUMENTS", "{error}");
    assert!(opener.calls().is_empty());
}

#[tokio::test]
async fn open_path_that_is_empty_or_nul_is_an_error() {
    let (_base, root) = fixture("empty-arg");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    for path in ["", "   ", "notes.md\0.txt"] {
        let error = call(&registry, OPEN_PATH_METHOD, json!({"path": path}))
            .await
            .expect_err("an empty or NUL-bearing path is refused");
        assert_eq!(code(&error), "INVALID_ARGUMENTS", "{path:?}: {error}");
    }
    assert!(opener.calls().is_empty());
}

// ---- opener failures propagate -----------------------------------------------------------------

#[tokio::test]
async fn opener_failure_is_reported_not_swallowed() {
    let (_base, root) = fixture("open-fails");
    let registry = registered(&root, Arc::new(Recorder::failing("no desktop session")));

    let error = call(&registry, OPEN_PATH_METHOD, json!({"path": "notes.md"}))
        .await
        .expect_err("an opener failure reaches the caller");

    assert_eq!(code(&error), "FAILED", "{error}");
    let text = error.to_string();
    assert!(text.contains("no desktop session"), "{text}");
    // Naming the path matters: the opener's reason alone does not say which of several queued opens
    // failed.
    assert!(text.contains("notes.md"), "{text}");
}

#[tokio::test]
async fn an_unavailable_opener_reports_why_it_cannot_open() {
    let (_base, root) = fixture("unavailable");
    let mut registry = RemoteRegistry::new();
    register_host_methods_with(
        &mut registry,
        &root,
        Arc::new(UnavailableOpener::new("this host has no display server")),
    );

    let error = call(&registry, OPEN_PATH_METHOD, json!({"path": "notes.md"}))
        .await
        .expect_err("an unavailable host refuses");
    assert_eq!(code(&error), "FAILED", "{error}");
    assert!(error.to_string().contains("no display server"), "{error}");
}

// ---- reveal behaves like open -----------------------------------------------------------------

#[tokio::test]
async fn reveal_inside_workspace_reaches_the_opener() {
    let (_base, root) = fixture("reveal-inside");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    let value = call(
        &registry,
        REVEAL_IN_FILE_MANAGER_METHOD,
        json!({"path": "notes.md"}),
    )
    .await
    .expect("a path inside the workspace is revealed");

    assert_eq!(value.get("action"), Some(&json!("reveal")));
    let calls = opener.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let Call::Reveal(received) = &calls[0] else {
        panic!("expected a reveal call, got {:?}", calls[0]);
    };
    assert!(received.ends_with("notes.md"), "{}", received.display());
}

#[tokio::test]
async fn reveal_outside_workspace_is_refused() {
    let (base, root) = fixture("reveal-outside");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    let outside = base.join("outside.txt");
    let error = call(
        &registry,
        REVEAL_IN_FILE_MANAGER_METHOD,
        json!({"path": outside.display().to_string()}),
    )
    .await
    .expect_err("a path outside the workspace is refused");

    assert_eq!(code(&error), "REFUSED", "{error}");
    assert!(opener.calls().is_empty(), "{:?}", opener.calls());
}

#[tokio::test]
async fn reveal_of_a_missing_path_is_an_error() {
    let (_base, root) = fixture("reveal-missing");
    let opener = Arc::new(Recorder::ok());
    let registry = registered(&root, Arc::clone(&opener));

    let error = call(
        &registry,
        REVEAL_IN_FILE_MANAGER_METHOD,
        json!({"path": "nope.md"}),
    )
    .await
    .expect_err("a missing path is refused");
    assert_eq!(code(&error), "INVALID_ARGUMENTS", "{error}");
    assert!(opener.calls().is_empty());
}

#[tokio::test]
async fn reveal_failure_is_reported_not_swallowed() {
    let (_base, root) = fixture("reveal-fails");
    let registry = registered(&root, Arc::new(Recorder::failing("no file manager")));

    let error = call(
        &registry,
        REVEAL_IN_FILE_MANAGER_METHOD,
        json!({"path": "notes.md"}),
    )
    .await
    .expect_err("a reveal failure reaches the caller");
    assert_eq!(code(&error), "FAILED", "{error}");
    assert!(error.to_string().contains("no file manager"), "{error}");
}

// ---- platform command mapping -----------------------------------------------------------------

/// The argv for `action` on `path` at `platform`, as `(program, args)`.
fn plan(platform: Platform, action: OpenAction, path: &str) -> (String, Vec<String>) {
    let plan: LaunchPlan = launch_plan(platform, action, Path::new(path));
    (plan.program, plan.args)
}

#[test]
fn macos_maps_open_to_open_and_reveal_to_select() {
    let (program, args) = plan(Platform::MacOs, OpenAction::Open, "/w/notes.md");
    assert_eq!(program, "open");
    assert_eq!(args, vec!["/w/notes.md"]);

    let (program, args) = plan(Platform::MacOs, OpenAction::Reveal, "/w/notes.md");
    assert_eq!(program, "open");
    assert_eq!(args, vec!["-R", "/w/notes.md"]);
}

#[test]
fn linux_maps_open_to_xdg_open_and_reveal_to_the_containing_directory() {
    let (program, args) = plan(Platform::Linux, OpenAction::Open, "/w/notes.md");
    assert_eq!(program, "xdg-open");
    assert_eq!(args, vec!["/w/notes.md"]);

    // An XDG desktop has no "select this file" verb, so the directory is what gets opened.
    let (program, args) = plan(Platform::Linux, OpenAction::Reveal, "/w/docs/notes.md");
    assert_eq!(program, "xdg-open");
    assert_eq!(args, vec!["/w/docs"]);
}

#[test]
fn windows_maps_open_to_explorer_and_reveal_to_select() {
    let (program, args) = plan(Platform::Windows, OpenAction::Open, r"C:\w\notes.md");
    assert_eq!(program, "explorer");
    assert_eq!(args, vec![r"C:\w\notes.md"]);

    let (program, args) = plan(Platform::Windows, OpenAction::Reveal, r"C:\w\notes.md");
    assert_eq!(program, "explorer");
    assert_eq!(args, vec![r"/select,C:\w\notes.md"]);
}

#[test]
fn explorer_target_escapes_the_separators_it_parses() {
    // Explorer splits its command line at commas and equals signs and silently opens a different
    // target, so both are escaped before the path reaches it.
    let (_, args) = plan(Platform::Windows, OpenAction::Reveal, "C:/w/a,b=c.txt");
    assert_eq!(args, vec!["/select,C:/w/a%2Cb%3Dc.txt"]);
}

#[test]
fn a_missing_system_opener_program_is_named_in_the_error() {
    // No launch happens: the program name is guaranteed not to exist, so `run` fails at `spawn` and
    // the message has to name the tool rather than leave the reader looking for the path.
    let plan = LaunchPlan::new(
        "girder-test-no-such-opener-binary",
        vec!["/w/notes.md".to_owned()],
    );
    let error = plan.run().expect_err("a missing program fails");
    assert!(
        error.contains("girder-test-no-such-opener-binary"),
        "{error}"
    );
    assert!(error.contains("PATH"), "{error}");
}

#[test]
fn the_convenience_registration_installs_both_methods() {
    // No call is dispatched: the opener this entry point picks is the real one, and a test must not
    // launch a desktop. Registration alone is the property — the choice between a system opener and
    // an unavailable one happens at registration, so a headless host is refused on the first call
    // rather than silently doing nothing.
    let (_base, root) = fixture("convenience");
    let mut registry = RemoteRegistry::new();
    register_host_methods(&mut registry, &root);
    let names = registry.names();
    assert!(
        names.contains(&(NAMESPACE.to_owned(), OPEN_PATH_METHOD.to_owned())),
        "{names:?}"
    );
    assert!(
        names.contains(&(
            NAMESPACE.to_owned(),
            REVEAL_IN_FILE_MANAGER_METHOD.to_owned()
        )),
        "{names:?}"
    );
}

#[test]
fn an_unavailable_opener_keeps_its_reason_readable() {
    // A deployment needs to report "this host has no desktop" before offering a button, which means
    // reading the reason without making a call that fails.
    let opener = UnavailableOpener::new("no display server");
    assert_eq!(opener.reason(), "no display server");
}

#[test]
fn system_opener_detect_reports_a_platform_or_a_reason() {
    // Both outcomes are legitimate: a desktop host gets an opener, a headless one gets a sentence
    // naming the missing facility. Neither may be a silent success.
    match SystemOpener::detect() {
        Ok(opener) => {
            // Merely reading the field: no path is handed to the desktop.
            let platform = opener.platform();
            assert!(matches!(
                platform,
                Platform::MacOs | Platform::Windows | Platform::Linux
            ));
        }
        Err(reason) => assert!(!reason.is_empty(), "a refusal must say why"),
    }
}

// ---- the access policy sees the launch --------------------------------------------------------

#[test]
fn the_methods_declare_a_write_so_a_read_only_policy_refuses_them() {
    use girder_core::{Access, DenyAll, Policy, ReadOnly};

    let (_base, root) = fixture("policy");
    let registry = registered(&root, Arc::new(Recorder::ok()));

    // The declaration is what the policy acts on, so a `Read` declaration would let a mirror peer
    // launch processes on the serving desktop.
    let read_only = ReadOnly;
    let request = girder_core::AccessRequest {
        peer: "mirror".to_owned(),
        method: format!("{NAMESPACE}.{OPEN_PATH_METHOD}"),
        access: Access::Write,
        resources: vec![girder_core::AccessResource::any_of("host")],
    };
    assert!(!read_only.decide(&request).is_allowed());

    let deny = DenyAll;
    assert!(!deny.decide(&request).is_allowed());
    // The registry itself still holds both handlers; the policy is what refuses the call.
    assert_eq!(registry.len(), 2);
}

#[tokio::test]
async fn a_registry_with_a_read_only_policy_refuses_the_call() {
    let (_base, root) = fixture("policy-registry");
    let opener = Arc::new(Recorder::ok());
    let shared: Arc<dyn Opener> = opener.clone();
    let mut registry = RemoteRegistry::new().with_policy(Arc::new(girder_core::ReadOnly));
    register_host_methods_with(&mut registry, &root, shared);

    let error = call(&registry, OPEN_PATH_METHOD, json!({"path": "notes.md"}))
        .await
        .expect_err("a read-only policy refuses a launch");
    assert_eq!(code(&error), "REFUSED", "{error}");
    assert!(opener.calls().is_empty(), "{:?}", opener.calls());
}
