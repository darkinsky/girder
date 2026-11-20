//! The `host.*` methods: asking the machine the server runs on to act on a path.
//!
//! Upstream keeps this ability in `packages/host/` and `packages/util/native-command`: a client that
//! holds a workspace path can ask the host desktop to open it in the default application
//! (`openWorkspacePath`, `open-in-app`) or to reveal it in the file manager. Without it a client can
//! only *show* a path as text, which is what this build did before this module existed.
//!
//! # Why the boundary is a workspace root
//!
//! These methods end in a process launch. A path argument that reached the opener unchecked would let
//! a model — which is the party that writes most path arguments — start the default handler for any
//! file on the machine, including a freshly written executable or a document outside the project the
//! user believes it is confined to. So the root is captured at registration and every path is
//! confined to it before the opener is called. The check is a refusal, not a warning: a single
//! successful escape makes every later claim about the confinement untrustworthy.
//!
//! # Why two checks and not one
//!
//! The lexical check on the normalized path refuses `../../etc/passwd` before anything touches the
//! disk, which is the cheap and common case. It cannot see a symlink: a link named `notes.txt` inside
//! the workspace whose target is `~/.ssh/id_rsa` passes it, and the opener would then open the real
//! file. The canonicalized target is therefore checked against the canonicalized root as well. Both
//! paths are canonicalized for that comparison, because comparing a canonical target against a
//! root spelled with a symlink in it would refuse every legitimate path under that root.
//!
//! # Why the opener is a trait
//!
//! Guaranteeing "the workspace boundary holds" must be testable without opening a window, and a test
//! that spawned `xdg-open` would take over the machine running it. [`Opener`] is the seam: the
//! production implementation ([`SystemOpener`]) starts a platform command, the tests use a recorder,
//! and a deployment with its own idea of "open" (a sandbox, an SSH desktop, a mock) can supply
//! another one without touching the method surface. [`UnavailableOpener`] is the honest answer for a
//! host that has no desktop at all — it reports why rather than pretending the call worked.
//!
//! # Why the process runs off the reactor
//!
//! The seam is synchronous, and starting a process then waiting for its exit is a blocking call. A
//! file manager that takes a second to hand off would otherwise stall every other call sharing that
//! runtime thread. The handler therefore hops to a blocking thread for exactly the one call.
//!
//! # Why the paths go through [`girder_core`]
//!
//! The crate aliases itself as `girder_core`, and the integration test includes this file by path
//! because wiring the module into `lib.rs` belongs to a different change. Spelling the paths through
//! the crate root keeps one copy of them working from inside the crate and from the test crate.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde_json::{Value, json};

use girder_core::{
    Declaration, RemoteError, RemoteRegistry, normalize_path, resolve_in_workspace,
    workspace_contains,
};

/// The namespace both methods live under.
///
/// `host` rather than `workspace`: what these methods act on is the serving machine's desktop, and a
/// deployment may well serve a workspace it cannot open windows on. The path is the argument, not the
/// subject.
pub const NAMESPACE: &str = "host";

/// Opens one workspace path with the operating system's default application.
pub const OPEN_PATH_METHOD: &str = "openPath";

/// Shows one workspace path in the platform file manager.
pub const REVEAL_IN_FILE_MANAGER_METHOD: &str = "revealInFileManager";

/// The resource kind these methods declare to the access policy.
///
/// The launch is an effect on the host rather than a change to the workspace, so a read-only policy
/// mounted on the registry refuses it. Without a declaration the policy would treat the call as a
/// read of `host` and let a mirror peer pop windows on the serving desktop.
const RESOURCE_KIND: &str = "host";

/// Which platform's conventions a [`LaunchPlan`] follows.
///
/// An explicit value rather than a `cfg!` inside the planner so the argv for all three platforms can
/// be asserted on one machine: the mapping is the part most likely to be wrong, and it is the part no
/// test can observe by actually launching something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// macOS: `open`, with `-R` to reveal.
    MacOs,
    /// Windows: `explorer`, with `/select,` to reveal.
    Windows,
    /// Linux and other XDG desktops: `xdg-open`, revealing the containing directory.
    Linux,
}

impl Platform {
    /// The platform this build runs on, when it has a desktop to hand a path to.
    ///
    /// A Linux host answers `Err` when no display server is announced. That is not a guess: `xdg-open`
    /// on a headless host either fails or, worse, starts a terminal browser nobody asked for, so the
    /// deployment needs to know before it offers a button that cannot work.
    ///
    /// # Errors
    ///
    /// Returns a sentence naming the missing facility when no opener is available here.
    pub fn current() -> Result<Self, String> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self::MacOs)
        }
        #[cfg(target_os = "windows")]
        {
            Ok(Self::Windows)
        }
        #[cfg(target_os = "linux")]
        {
            let announced = ["DISPLAY", "WAYLAND_DISPLAY", "WSL_DISTRO_NAME"]
                .iter()
                .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
            if announced {
                Ok(Self::Linux)
            } else {
                Err(
                    "this host announces no display server (DISPLAY and WAYLAND_DISPLAY are unset), \
                     so there is no desktop to open a path on"
                        .to_owned(),
                )
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            Err(format!(
                "no native path opener is defined for `{}`",
                std::env::consts::OS
            ))
        }
    }
}

/// What the caller wants the desktop to do with a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenAction {
    /// Hand the path to the default application for its type.
    Open,
    /// Show the path — selected, where the platform supports it — in a file manager.
    Reveal,
}

impl OpenAction {
    /// The wire spelling, as it appears in a response.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Reveal => "reveal",
        }
    }
}

/// One process invocation: a program, its argv, and how to read its exit.
///
/// Split from running it so the platform mapping can be asserted without spawning anything. The
/// arguments are a list and never a shell string: the path is caller-supplied, and a string that was
/// handed to a shell would turn a filename containing `;` into a second command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchPlan {
    /// The program to run.
    pub program: String,
    /// Arguments, one argv element each.
    pub args: Vec<String>,
    /// Whether an exit code of 1 still means the request was accepted.
    ///
    /// Windows Explorer exits 1 after handing the request to the desktop process that is already
    /// running, which is success in every sense the caller cares about. Treating it as a failure
    /// would report an error for the normal case of an open desktop session.
    tolerate_exit_one: bool,
}

impl LaunchPlan {
    /// A plan that treats any non-zero exit as a failure.
    #[must_use]
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            tolerate_exit_one: false,
        }
    }

    /// The same plan, accepting Explorer's delegated handoff exit code.
    #[must_use]
    pub fn tolerating_exit_one(mut self) -> Self {
        self.tolerate_exit_one = true;
        self
    }

    /// Runs the plan and reports whether the desktop accepted the request.
    ///
    /// The child inherits no descriptors: a file manager that keeps running would otherwise hold this
    /// process's pipes open and, in a client that reads them, look like a command that never finished.
    ///
    /// # Errors
    ///
    /// Returns a sentence naming the program when it is missing (`ENOENT`), when it could not be
    /// started, or when it exited non-zero. A missing program is reported as its own case because
    /// that is the failure a container or a minimal image produces, and "no such file or directory"
    /// on its own sends the reader looking for the *path* that was passed instead of the tool.
    pub fn run(&self) -> Result<(), String> {
        let status = std::process::Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| match error.kind() {
                ErrorKind::NotFound => format!(
                    "`{}` is not installed or not on PATH, so the path cannot be handed to the desktop",
                    self.program
                ),
                _ => format!("failed to run `{}`: {error}", self.program),
            })?;

        if status.success() || (self.tolerate_exit_one && status.code() == Some(1)) {
            return Ok(());
        }

        Err(format!(
            "`{}` exited with {}",
            self.program,
            match status.code() {
                Some(code) => code.to_string(),
                // No code means a signal ended it, which is worth saying explicitly: "exited with
                // None" would read as a missing value rather than as a crash.
                None => "a signal".to_owned(),
            }
        ))
    }
}

/// The platform command for one action on one path.
///
/// Reveal differs by platform in more than the flag. macOS selects the item in Finder, Windows selects
/// it in Explorer, and an XDG desktop has no "select" verb at all — so the containing directory is
/// opened instead, which is what upstream's `directory` file manager does. Opening the parent of a
/// directory would be wrong there, so a directory is revealed as itself.
#[must_use]
pub fn launch_plan(platform: Platform, action: OpenAction, path: &Path) -> LaunchPlan {
    let target = path.to_string_lossy().into_owned();
    match (platform, action) {
        (Platform::MacOs, OpenAction::Open) => LaunchPlan::new("open", vec![target]),
        (Platform::MacOs, OpenAction::Reveal) => {
            LaunchPlan::new("open", vec!["-R".to_owned(), target])
        }
        (Platform::Windows, OpenAction::Open) => {
            LaunchPlan::new("explorer", vec![explorer_target(&target)]).tolerating_exit_one()
        }
        (Platform::Windows, OpenAction::Reveal) => LaunchPlan::new(
            "explorer",
            vec![format!("/select,{}", explorer_target(&target))],
        )
        .tolerating_exit_one(),
        (Platform::Linux, OpenAction::Open) => LaunchPlan::new("xdg-open", vec![target]),
        (Platform::Linux, OpenAction::Reveal) => {
            let containing = if path.is_dir() {
                path
            } else {
                path.parent().unwrap_or(path)
            };
            LaunchPlan::new("xdg-open", vec![containing.to_string_lossy().into_owned()])
        }
    }
}

/// Encodes a path so Explorer's own command-line parser does not split it.
///
/// Explorer separates fields at commas and equals signs and silently opens a different target when a
/// path contains one. Only those two characters are escaped: Explorer rejects percent-encoded
/// non-ASCII and falls back to the user's Documents folder, so escaping more would break paths this
/// fixes nothing for.
fn explorer_target(path: &str) -> String {
    path.replace('%', "%25")
        .replace(',', "%2C")
        .replace('=', "%3D")
}

/// How a path is handed to a desktop.
///
/// The seam exists so the workspace boundary can be tested without a display: the methods under test
/// decide *whether* a path may be opened, and this decides *how*. Keeping them apart means a test for
/// the first never needs the second.
pub trait Opener: Send + Sync {
    /// Opens `path` with the default application for its type.
    ///
    /// # Errors
    ///
    /// Returns the caller-facing reason the path could not be opened.
    fn open(&self, path: &Path) -> Result<(), String>;

    /// Shows `path` in the platform file manager.
    ///
    /// # Errors
    ///
    /// Returns the caller-facing reason the path could not be revealed.
    fn reveal(&self, path: &Path) -> Result<(), String>;
}

/// The real opener: one platform command per action.
#[derive(Clone, Copy, Debug)]
pub struct SystemOpener {
    platform: Platform,
}

impl SystemOpener {
    /// Builds the opener for this host.
    ///
    /// # Errors
    ///
    /// Returns the reason this host has no desktop opener, which is what a headless Linux server
    /// answers.
    pub fn detect() -> Result<Self, String> {
        Platform::current().map(|platform| Self { platform })
    }

    /// The platform conventions this opener follows.
    #[must_use]
    pub const fn platform(self) -> Platform {
        self.platform
    }
}

impl Opener for SystemOpener {
    fn open(&self, path: &Path) -> Result<(), String> {
        launch_plan(self.platform, OpenAction::Open, path).run()
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        launch_plan(self.platform, OpenAction::Reveal, path).run()
    }
}

/// An opener for a host that cannot act on a path.
///
/// The alternative is a silent success: a client that was told "opened" would show a path as handled
/// while nothing happened, and the user would conclude the button is broken rather than the host is
/// headless. Every call therefore fails with the recorded reason.
#[derive(Clone, Debug)]
pub struct UnavailableOpener {
    reason: String,
}

impl UnavailableOpener {
    /// Records why no path can be opened here.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The recorded reason, for a caller that wants to report it before it tries.
    #[must_use]
    pub fn reason(&self) -> &str {
        self.reason.as_str()
    }
}

impl Opener for UnavailableOpener {
    fn open(&self, path: &Path) -> Result<(), String> {
        Err(format!(
            "`{}` cannot be opened: {}",
            path.display(),
            self.reason
        ))
    }

    fn reveal(&self, path: &Path) -> Result<(), String> {
        Err(format!(
            "`{}` cannot be revealed: {}",
            path.display(),
            self.reason
        ))
    }
}

/// Registers `host.openPath` and `host.revealInFileManager` over `workspace`.
///
/// The opener is chosen here rather than supplied by the caller because this is the convenient entry
/// point: a deployment with a desktop gets the real one, and a deployment without gets a refusal that
/// says so instead of a call that appears to work.
pub fn register_host_methods(registry: &mut RemoteRegistry, workspace: &Path) {
    let opener: Arc<dyn Opener> = match SystemOpener::detect() {
        Ok(opener) => Arc::new(opener),
        Err(reason) => Arc::new(UnavailableOpener::new(reason)),
    };
    register_host_methods_with(registry, workspace, opener);
}

/// Registers both methods over `workspace`, with `opener` deciding how a path reaches the desktop.
///
/// `workspace` is captured as the confinement root. It is not re-read per call: a client cannot name
/// a root at all, which is the half of upstream's guarantee that matters — a caller names a
/// registration, not a directory.
pub fn register_host_methods_with(
    registry: &mut RemoteRegistry,
    workspace: &Path,
    opener: Arc<dyn Opener>,
) {
    register_method(
        registry,
        workspace,
        opener.clone(),
        OPEN_PATH_METHOD,
        OpenAction::Open,
    );
    register_method(
        registry,
        workspace,
        opener,
        REVEAL_IN_FILE_MANAGER_METHOD,
        OpenAction::Reveal,
    );
}

/// Registers one of the two methods.
fn register_method(
    registry: &mut RemoteRegistry,
    workspace: &Path,
    opener: Arc<dyn Opener>,
    method: &'static str,
    action: OpenAction,
) {
    let root = Arc::new(workspace.to_path_buf());
    // Declared as a write: the policy is what stops a mirror peer from launching processes on the
    // serving desktop, and a read declaration would let one through.
    registry.declare(NAMESPACE, method, Declaration::write(RESOURCE_KIND));
    registry.register(NAMESPACE, method, move |call| {
        let root = Arc::clone(&root);
        let opener = Arc::clone(&opener);
        Box::pin(async move {
            let requested = required_str(&call.args, "path", method)?;
            let target = confine(&root, requested, method).await?;
            let handoff = target.clone();
            // One hop off the reactor for one blocking call: the opener waits for the child, and a
            // file manager that takes a second to hand off must not stall unrelated calls.
            tokio::task::spawn_blocking(move || match action {
                OpenAction::Open => opener.open(&handoff),
                OpenAction::Reveal => opener.reveal(&handoff),
            })
            .await
            .map_err(|error| {
                failed(format!(
                    "the `{method}` opener task did not finish: {error}"
                ))
            })?
            .map_err(|reason| {
                failed(format!(
                    "`{method}` could not handle `{}`: {reason}",
                    target.display()
                ))
            })?;

            Ok(json!({
                // The field upstream returns, so a client written against it reads the same answer.
                "opened": true,
                "action": action.as_str(),
                // The canonical absolute path that was actually handed to the desktop. Echoing the
                // request instead would hide a symlink leading somewhere the caller did not name.
                "path": target.display().to_string(),
            }))
        })
    });
}

/// Resolves `requested` inside `root`, refusing anything that leaves it.
///
/// The order is the point: the lexical check runs before the disk is touched, so an escape is refused
/// even when the target does not exist, and the canonical check runs after, so a symlink that leads
/// out is refused as well. A missing path is its own error rather than a containment failure, because
/// "there is nothing there" and "that is not yours" call for different reactions.
async fn confine(root: &Path, requested: &str, method: &str) -> Result<PathBuf, RemoteError> {
    if requested.trim().is_empty() {
        return Err(invalid(method, "`path` must be a non-empty string"));
    }
    if requested.contains('\0') {
        return Err(invalid(method, "`path` contains a NUL byte"));
    }

    let lexical = normalize_path(&resolve_in_workspace(Some(root), requested));
    if !workspace_contains(root, &lexical) {
        return Err(refused(
            method,
            format!("`{requested}` is outside the workspace root"),
        ));
    }

    let canonical_root = tokio::fs::canonicalize(root).await.map_err(|error| {
        // The root was supplied at registration, so failing to resolve it is a deployment problem,
        // not a caller mistake.
        failed(format!(
            "workspace root `{}` is unreadable: {error}",
            root.display()
        ))
    })?;

    let canonical_target = match tokio::fs::canonicalize(&lexical).await {
        Ok(target) => target,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(invalid(
                method,
                format!("`{requested}` does not exist in the workspace"),
            ));
        }
        Err(error) => {
            return Err(failed(format!(
                "`{}` could not be resolved: {error}",
                lexical.display()
            )));
        }
    };

    if !workspace_contains(&canonical_root, &canonical_target) {
        return Err(refused(
            method,
            format!("`{requested}` resolves outside the workspace root"),
        ));
    }

    Ok(canonical_target)
}

/// Reads a required string field, naming `method` in the refusal.
///
/// The method is passed in rather than fixed to one of the two: a shared helper that reported the
/// wrong name would send a client to the wrong declaration.
fn required_str<'a>(args: &'a Value, field: &str, method: &str) -> Result<&'a str, RemoteError> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(method, format!("`{field}` must be a string")))
}

/// Builds an argument refusal for `method`.
fn invalid(method: &str, message: impl Into<String>) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: message.into(),
    }
}

/// Builds a boundary refusal for `method`.
///
/// [`RemoteError::Refused`] rather than `InvalidArguments`: the arguments are well formed and the
/// method exists — the path is simply not one this deployment will act on. Its own wire code lets a
/// client stop retrying, which is the difference between "your request was wrong" and "this is not
/// permitted".
fn refused(method: &str, reason: impl Into<String>) -> RemoteError {
    RemoteError::Refused {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        reason: reason.into(),
    }
}

/// Builds a runtime failure for `method`.
fn failed(message: impl Into<String>) -> RemoteError {
    RemoteError::Failed(message.into())
}
