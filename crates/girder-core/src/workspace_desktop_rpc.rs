//! `session.workspaceDesktop`, `session.canOpenWorkspacePath` and `session.workspacePathApplications`.
//!
//! # What upstream answers
//!
//! Upstream keeps these on the session controller (`packages/api/session-controller/src/index.ts`):
//!
//! - `canOpenWorkspacePath()` — a boolean: can this deployment hand a workspace path to a native
//!   desktop at all.
//! - `workspaceDesktop()` — `{ name, available, fileManager }`, where `name` is the machine's host
//!   name, `fileManager` is `finder` on macOS, `explorer` on Windows and on WSL, `directory` on a
//!   plain XDG desktop, and `null` where no file manager exists; `available` is
//!   `fileManager !== null && canOpenPath()`. Upstream serves this object over the HTTP route
//!   `/api/present.host` rather than over the Remote namespace; this build has one channel, so it is
//!   registered here under the same namespace as the rest of the session surface.
//! - `workspacePathApplications({ path })` — the applications currently registered to open that
//!   path, in OS preference order, each `{ id, name, default, icon }`. Upstream answers `[]` before
//!   it even validates the path when opening is unavailable, verifies the path through the composed
//!   filesystem, then asks the desktop.
//!
//! # Where the answer comes from, and how far this build goes
//!
//! Listing handlers is a per-platform probe, and upstream has three separate ones:
//! `gio` plus XDG `.desktop` parsing on Linux, a JXA/AppKit script against `LaunchServices` on macOS,
//! and a generated C# helper over the Shell COM interfaces on Windows. This module implements the
//! Linux probe (the platform this build is developed and deployed on) and answers with an empty list
//! everywhere else.
//!
//! That split is deliberate. An empty list is a true statement about this build — "I do not know
//! which applications are registered here" — and its only consumer is a menu that offers choices.
//! A guessed list would be a false statement about the user's machine, and the first wrong entry
//! would fail at launch, after the user picked it. Inventing a MIME database, or shelling out to
//! tools whose output was never verified against a real desktop, would be the same mistake with
//! extra steps.
//!
//! The Linux probe follows upstream exactly, including the parts that look redundant:
//!
//! - the content type comes from `gio info -a standard::content-type`, never from a file extension;
//!   upstream treats a missing content type as an error rather than guessing one, and so does this
//! - the registered handlers come from `gio mime <type>`, which is GIO's own association database;
//!   the `MimeType=` field of a `.desktop` file is a claim, not the registration, and reading it
//!   would list applications the desktop does not actually use
//! - XDG precedence is honoured, nested application directories produce dashed ids, and localized
//!   `Name[...]` fields are preferred in the order the locale implies
//!
//! One field is deliberately poorer than upstream's: `icon` is always `null`. Upstream reads PNG or
//! SVG artwork from the icon theme and base64-encodes it into a data URL per application. That is a
//! lot of file reads per listing for a decorative field the client already accepts as `null`, and a
//! half-implemented theme lookup would show the wrong artwork for exactly the applications whose
//! entries are interesting.
//!
//! # Why availability is an environment probe and not a question for the opener
//!
//! [`Opener`] exposes `open` and `reveal` and nothing else, so the only way to ask it whether it
//! works would be to launch something — which is precisely what this method must not do. Upstream is
//! in the same position and answers the same way: `canOpenNativePath()` reads the platform and the
//! display variables, not the opener. [`DesktopEnvironment`] mirrors that rule, so a host wired with
//! [`girder_core::UnavailableOpener`] (which `SystemOpener::detect` produces exactly when no display
//! is announced) also reports `available: false`.
//!
//! The consequence has to be stated: a deployment that injects its own opener on a graphical host
//! cannot be detected here. Such a deployment should pass a matching [`DesktopEnvironment`] through
//! [`register_workspace_desktop_methods_with`] instead of relying on [`DesktopEnvironment::detect`].
//!
//! # Why the path is confined
//!
//! The query names a path, and the answer reveals what the *machine* does with that path. A path
//! outside the workspace would let a model enumerate associations for files the tools could not
//! read, so the same two-layer containment `host.openPath` uses is applied first: a lexical check on
//! the normalised path, then a canonical check that catches a symlink leading out.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use girder_core::{
    Declaration, Opener, RemoteCall, RemoteError, RemoteRegistry, normalize_path,
    resolve_in_workspace, workspace_contains,
};

/// The namespace every method here lives under.
///
/// The same one the rest of the session surface uses: upstream registers these on the session
/// controller, and a client reaching them under `host.*` would be reaching a second, divergent name.
pub const NAMESPACE: &str = "session";

/// Describes the serving desktop: host name, availability, and file-manager conventions.
pub const DESKTOP_METHOD: &str = "workspaceDesktop";

/// Reports whether this deployment can hand a path to a native desktop.
pub const CAN_OPEN_METHOD: &str = "canOpenWorkspacePath";

/// Lists the applications registered to open one workspace path.
pub const PATH_APPLICATIONS_METHOD: &str = "workspacePathApplications";

/// The resource kind a desktop description reads.
const RESOURCE_KIND_HOST: &str = "host";

/// The resource kind a path-association query reads.
///
/// `workspace` rather than `host`: the query is about one file inside the workspace, and a policy
/// that hides workspace contents must refuse it.
const RESOURCE_KIND_WORKSPACE: &str = "workspace";

/// The host name reported when the machine will not say what it is called.
const UNKNOWN_HOST_NAME: &str = "unknown-host";

/// The XDG default when `XDG_DATA_DIRS` is unset, per the base directory specification.
const DEFAULT_DATA_DIRECTORIES: &str = "/usr/local/share:/usr/share";

/// How deep an `applications` directory tree is followed.
///
/// Nested directories exist (a vendor shipping `applications/vendor-*.desktop`), but a tree this
/// deep is not a desktop environment, it is a mistake, and following it without a bound would let a
/// symlinked loop cost a stack.
const MAX_APPLICATION_DIRECTORY_DEPTH: usize = 4;

/// The operating system a desktop answer is about.
///
/// An explicit value rather than `cfg!` at each use, so the mapping from platform to file manager and
/// to availability can be asserted for all four cases on one machine. The mapping is the part most
/// likely to be wrong and the part no test can observe by actually opening a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostPlatform {
    /// macOS: Finder, `open`.
    MacOs,
    /// Windows: Explorer.
    Windows,
    /// A Linux or other XDG desktop.
    Linux,
    /// Any other platform: no file manager, no opener.
    Other,
}

impl HostPlatform {
    /// The platform this build runs on.
    #[must_use]
    pub fn detect() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }

    /// The wire spelling, as `fileManager` values are written in the response.
    #[must_use]
    pub const fn file_manager(self) -> Option<FileManager> {
        match self {
            Self::MacOs => Some(FileManager::Finder),
            Self::Windows => Some(FileManager::Explorer),
            Self::Linux => Some(FileManager::Directory),
            Self::Other => None,
        }
    }
}

/// How a path is shown in this platform's file manager.
///
/// The three values upstream uses, with upstream's spellings: a client switches on them to decide
/// whether "reveal" selects the file (Finder, Explorer) or only opens its directory (an XDG desktop
/// has no "select this item" verb).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum FileManager {
    /// macOS Finder, which can select one item.
    #[serde(rename = "finder")]
    Finder,
    /// Windows Explorer, which can select one item.
    #[serde(rename = "explorer")]
    Explorer,
    /// An XDG desktop, whose file manager is handed the containing directory.
    #[serde(rename = "directory")]
    Directory,
}

impl FileManager {
    /// The wire spelling, for a caller that logs the value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Finder => "finder",
            Self::Explorer => "explorer",
            Self::Directory => "directory",
        }
    }
}

/// The answer to `session.workspaceDesktop`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopInfo {
    /// The host's own idea of its name, which is what a person recognises on a shared deployment.
    pub name: String,
    /// Whether a path can actually be handed to a desktop here.
    ///
    /// Read from the environment, not from the injected opener — see the module documentation for why
    /// that distinction matters to a deployment that supplies its own opener.
    pub available: bool,
    /// The platform's file-manager convention, or `null` where there is none.
    pub file_manager: Option<FileManager>,
}

/// The machine facts a desktop answer is made of, injectable for tests.
///
/// Everything here is either an environment variable, a kernel-reported host name, or a platform
/// constant. Gathering it into one value is what lets the whole surface — including the decision not
/// to probe headless hosts — be tested on any machine, without a display and without a desktop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopEnvironment {
    platform: HostPlatform,
    name: String,
    wsl: bool,
    display: bool,
    data_directories: Vec<PathBuf>,
    locale: Option<String>,
}

impl DesktopEnvironment {
    /// Reads this machine's facts.
    ///
    /// The WSL and display probes are only meaningful on Linux, and are recorded regardless so a
    /// test can describe a Linux host on any machine.
    #[must_use]
    pub fn detect() -> Self {
        let platform = HostPlatform::detect();
        Self {
            platform,
            name: host_name(),
            wsl: platform == HostPlatform::Linux && wsl_announced(),
            display: display_announced(),
            data_directories: xdg_data_directories(),
            locale: locale_from_environment(),
        }
    }

    /// Describes an arbitrary host, for a test or for a deployment with its own opener.
    ///
    /// Nothing is probed: a value built here answers exactly what it was told, which is the point.
    /// `display` starts `false`, so a Linux host built this way is headless until
    /// [`DesktopEnvironment::with_display`] says otherwise.
    #[must_use]
    pub fn for_host(platform: HostPlatform, name: impl Into<String>) -> Self {
        Self {
            platform,
            name: name.into(),
            wsl: false,
            display: false,
            data_directories: Vec::new(),
            locale: None,
        }
    }

    /// Records whether this host is WSL, where paths go to the Windows desktop.
    #[must_use]
    pub fn with_wsl(mut self, wsl: bool) -> Self {
        self.wsl = wsl;
        self
    }

    /// Records whether a display server is announced.
    #[must_use]
    pub fn with_display(mut self, announced: bool) -> Self {
        self.display = announced;
        self
    }

    /// Sets the XDG data roots, in precedence order, that application entries are read from.
    #[must_use]
    pub fn with_data_directories(mut self, directories: Vec<PathBuf>) -> Self {
        self.data_directories = directories;
        self
    }

    /// Sets the locale used to choose a localized `Name[...]` field.
    #[must_use]
    pub fn with_locale(mut self, locale: Option<String>) -> Self {
        self.locale = locale;
        self
    }

    /// The platform this environment describes.
    #[must_use]
    pub const fn platform(&self) -> HostPlatform {
        self.platform
    }

    /// The host name reported in the desktop answer.
    #[must_use]
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    /// Whether this host is WSL, where a Linux path is handed to the Windows desktop.
    #[must_use]
    pub const fn is_wsl(&self) -> bool {
        self.wsl
    }

    /// Whether a display server was announced.
    #[must_use]
    pub const fn has_display(&self) -> bool {
        self.display
    }

    /// The XDG data roots, in precedence order.
    #[must_use]
    pub fn data_directories(&self) -> &[PathBuf] {
        self.data_directories.as_slice()
    }

    /// The locale an application name is localized for, if one was announced.
    #[must_use]
    pub fn locale(&self) -> Option<&str> {
        self.locale.as_deref()
    }

    /// The file-manager convention for this host.
    ///
    /// WSL answers `explorer`: the path is translated to a Windows path and handed to Explorer, which
    /// is what upstream does and what a user of a WSL workspace sees.
    #[must_use]
    pub const fn file_manager(&self) -> Option<FileManager> {
        match self.platform {
            HostPlatform::Linux if self.wsl => Some(FileManager::Explorer),
            other => other.file_manager(),
        }
    }

    /// Whether handing a path to the desktop can work at all here.
    ///
    /// macOS and Windows always can; a Linux host can when it is WSL (the Windows desktop takes the
    /// path) or when a display server is announced. A headless or containerised Linux host cannot,
    /// and saying so is what stops a client from offering a button that would spawn `xdg-open` into
    /// nothing.
    #[must_use]
    pub const fn can_open(&self) -> bool {
        match self.platform {
            HostPlatform::MacOs | HostPlatform::Windows => true,
            HostPlatform::Linux => self.wsl || self.display,
            HostPlatform::Other => false,
        }
    }

    /// The desktop answer this environment implies.
    ///
    /// `available` is `fileManager !== null && canOpen()`, exactly as upstream composes it: a Windows
    /// host with a display still cannot offer anything if some future platform reports no file
    /// manager, and a headless Linux host keeps `directory` because the convention exists even when
    /// nothing can be launched.
    #[must_use]
    pub fn desktop_info(&self) -> DesktopInfo {
        let file_manager = self.file_manager();
        DesktopInfo {
            name: self.name.clone(),
            available: file_manager.is_some() && self.can_open(),
            file_manager,
        }
    }

    /// Whether the desktop answer reports itself available.
    #[must_use]
    pub fn available(&self) -> bool {
        self.desktop_info().available
    }
}

/// One application registered to open a path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileApplication {
    /// The application's identifier: the absolute path of its `.desktop` entry on Linux.
    ///
    /// This is what `gio launch` takes, and what the client hands back to an open call.
    pub id: String,
    /// The display name, localized when the entry carries a translation for this locale.
    pub name: String,
    /// Whether the desktop would use this application by default for the path's content type.
    pub default: bool,
    /// Artwork as a data URL, or `null`.
    ///
    /// Always `null` in this build: the icon theme lookup upstream performs is not implemented here,
    /// and a guessed icon would be worse than none.
    pub icon: Option<String>,
}

/// Where `session.workspacePathApplications` gets its answer.
///
/// A trait because the probe is the platform-specific, subprocess-running half, and the method around
/// it — argument checking, confinement, the headless short-circuit — is what most of the tests are
/// about. Splitting them means a test of the method never has to have a desktop.
pub trait FileApplicationProber: Send + Sync {
    /// Lists the applications registered to open `path`, most preferred first.
    ///
    /// # Errors
    ///
    /// Returns the caller-facing reason the associations could not be determined. "I could not ask"
    /// is deliberately not answered with an empty list: a deployment missing the probe tool has a
    /// problem its operator should see.
    fn applications(
        &self,
        path: &Path,
        environment: &DesktopEnvironment,
    ) -> Result<Vec<FileApplication>, String>;
}

/// The real prober: GIO's association database plus XDG desktop entries on Linux, nothing elsewhere.
#[derive(Clone, Copy, Debug, Default)]
pub struct GioFileApplicationProber;

impl FileApplicationProber for GioFileApplicationProber {
    fn applications(
        &self,
        path: &Path,
        environment: &DesktopEnvironment,
    ) -> Result<Vec<FileApplication>, String> {
        // macOS needs LaunchServices and Windows needs Shell COM enumeration; neither is implemented
        // here, and an empty list is the honest answer. Upstream returns [] for an unknown platform
        // for the same reason.
        if environment.platform() != HostPlatform::Linux {
            return Ok(Vec::new());
        }
        let content_type = gio_content_type(path)?;
        let (default, registered) = gio_registered_applications(&content_type)?;
        Ok(applications_for(
            &registered,
            default.as_deref(),
            environment,
        ))
    }
}

/// Registers the desktop surface over `workspace`.
///
/// The opener is accepted so this registration has the same shape as
/// [`girder_core::register_open_workspace_path`] at the wiring site, where one opener serves both.
/// It is not consulted — see the module documentation — and the real prober and this machine's facts
/// are used.
pub fn register_workspace_desktop_methods(
    registry: &mut RemoteRegistry,
    workspace: &Path,
    opener: &Arc<dyn Opener>,
) {
    register_workspace_desktop_methods_with(
        registry,
        workspace,
        opener,
        Arc::new(GioFileApplicationProber),
        Arc::new(DesktopEnvironment::detect()),
    );
}

/// Registers the desktop surface with the prober and the machine facts supplied by the caller.
///
/// This is the seam for tests and for a deployment whose opener does not agree with what
/// [`DesktopEnvironment::detect`] observes: it passes a matching environment instead of editing this
/// module.
pub fn register_workspace_desktop_methods_with(
    registry: &mut RemoteRegistry,
    workspace: &Path,
    opener: &Arc<dyn Opener>,
    prober: Arc<dyn FileApplicationProber>,
    environment: Arc<DesktopEnvironment>,
) {
    // Deliberately unused: `Opener` exposes only `open` and `reveal`, so the sole way to ask it
    // whether it works would be to launch something. Availability is read from the environment, the
    // way upstream reads it, and this argument is here for the wiring's shape.
    let _ = opener;

    registry.declare(
        NAMESPACE,
        DESKTOP_METHOD,
        Declaration::read(RESOURCE_KIND_HOST),
    );
    registry.register(NAMESPACE, DESKTOP_METHOD, {
        let environment = Arc::clone(&environment);
        move |_call: RemoteCall| {
            let environment = Arc::clone(&environment);
            Box::pin(async move { desktop_value(&environment) })
        }
    });

    registry.declare(
        NAMESPACE,
        CAN_OPEN_METHOD,
        Declaration::read(RESOURCE_KIND_HOST),
    );
    registry.register(NAMESPACE, CAN_OPEN_METHOD, {
        let environment = Arc::clone(&environment);
        move |_call: RemoteCall| {
            let environment = Arc::clone(&environment);
            Box::pin(async move { Ok(Value::Bool(environment.available())) })
        }
    });

    // Declared as a read: the query only lists. It is a read of the workspace, because what it
    // reveals is derived from a file inside it, and a policy that hid workspace contents would
    // otherwise answer with the machine's association database.
    registry.declare(
        NAMESPACE,
        PATH_APPLICATIONS_METHOD,
        Declaration::read(RESOURCE_KIND_WORKSPACE),
    );
    register_path_applications(
        registry,
        Arc::new(workspace.to_path_buf()),
        environment,
        prober,
    );
}

/// Registers `session.workspacePathApplications`.
fn register_path_applications(
    registry: &mut RemoteRegistry,
    root: Arc<PathBuf>,
    environment: Arc<DesktopEnvironment>,
    prober: Arc<dyn FileApplicationProber>,
) {
    registry.register(NAMESPACE, PATH_APPLICATIONS_METHOD, move |call: RemoteCall| {
        let root = Arc::clone(&root);
        let environment = Arc::clone(&environment);
        let prober = Arc::clone(&prober);
        Box::pin(async move {
            let requested = required_str(&call.args, "path", PATH_APPLICATIONS_METHOD)?;

            // Checked before the path is: upstream answers an empty list when the desktop cannot open
            // anything at all, and validating a path nobody can act on would turn "no desktop here"
            // into an argument error for a client that is drawing a menu.
            if !environment.available() {
                return Ok(Value::Array(Vec::new()));
            }

            let target = confine(&root, requested, PATH_APPLICATIONS_METHOD).await?;
            // Off the reactor: the probe reads the XDG database and starts `gio`. A file manager query
            // that takes a moment must not stall unrelated calls.
            let applications = tokio::task::spawn_blocking(move || {
                prober.applications(&target, &environment)
            })
            .await
            .map_err(|error| {
                failed(format!("the application probe task did not finish: {error}"))
            })?;
            let applications = applications.map_err(|reason| {
                failed(format!(
                    "`{PATH_APPLICATIONS_METHOD}` could not list applications for `{requested}`: {reason}"
                ))
            })?;
            serde_json::to_value(applications).map_err(|error| {
                failed(format!(
                    "the application list could not be encoded: {error}"
                ))
            })
        })
    });
}

/// Answers `session.workspaceDesktop`.
fn desktop_value(environment: &DesktopEnvironment) -> Result<Value, RemoteError> {
    serde_json::to_value(environment.desktop_info()).map_err(|error| {
        failed(format!(
            "the desktop description could not be encoded: {error}"
        ))
    })
}

/// Resolves `requested` inside `root`, refusing anything that leaves it.
///
/// The order is the same one `host.openPath` uses, and for the same reason: the lexical check runs
/// before the disk is touched, so an escape is refused even when the target does not exist, and the
/// canonical check runs after, so a symlink leading out is refused too. A missing path is its own
/// error rather than a containment failure, because "there is nothing there" and "that is not yours"
/// call for different reactions.
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
        // The root was supplied at registration, so failing to resolve it is a deployment problem
        // rather than a caller mistake.
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
/// method exists — the path is simply not one this deployment will act on.
fn refused(method: &str, reason: impl Into<String>) -> RemoteError {
    RemoteError::Refused {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        reason: reason.into(),
    }
}

/// Builds a failure, for something that went wrong after the arguments were accepted.
fn failed(message: String) -> RemoteError {
    RemoteError::Failed(message)
}

/// The machine's host name, as `/proc` or the environment reports it.
///
/// No libc call: `unsafe` is denied at the workspace level, and the kernel already publishes the
/// answer as a file. The order is Windows first (where the others do not exist), then the two files
/// a Linux or BSD host writes, then `$HOSTNAME`, which is set by most shells but is not authoritative.
fn host_name() -> String {
    if let Some(name) = std::env::var_os("COMPUTERNAME").filter(|value| !value.is_empty()) {
        return name.to_string_lossy().into_owned();
    }
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            let name = text.trim();
            if !name.is_empty() {
                return name.to_owned();
            }
        }
    }
    match std::env::var_os("HOSTNAME") {
        Some(name) if !name.is_empty() => name.to_string_lossy().into_owned(),
        _ => UNKNOWN_HOST_NAME.to_owned(),
    }
}

/// Whether this Linux host is WSL, where paths belong to the Windows desktop.
///
/// The environment variables are the cheap answer; the kernel release string is the one that survives
/// a login shell that scrubbed the environment.
fn wsl_announced() -> bool {
    let announced = ["WSL_DISTRO_NAME", "WSL_INTEROP"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    if announced {
        return true;
    }
    std::fs::read_to_string("/proc/sys/kernel/osrelease").is_ok_and(|release| {
        let release = release.to_ascii_lowercase();
        release.contains("microsoft") || release.contains("wsl")
    })
}

/// Whether a display server was announced.
///
/// The same two variables `SystemOpener::detect` gates on, so this answer and the opener a deployment
/// wired up agree about a headless host.
fn display_announced() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// The XDG data roots, most specific first, as the base directory specification defines them.
fn xdg_data_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        Some(home) => directories.push(PathBuf::from(home)),
        None => {
            if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
                directories.push(PathBuf::from(home).join(".local").join("share"));
            }
        }
    }
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_DATA_DIRECTORIES.to_owned());
    directories.extend(
        system
            .split(':')
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from),
    );
    directories
}

/// The locale an application name is preferred in, from the first variable that sets one.
fn locale_from_environment() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty())
}

/// Reads the content type `gio` reports for `path`.
///
/// A missing content type is an error rather than a guess from the file name: guessing would invent
/// associations for a file whose type the desktop itself does not know.
fn gio_content_type(path: &Path) -> Result<String, String> {
    let stdout = run_gio(&[
        OsStr::new("info"),
        OsStr::new("-a"),
        OsStr::new("standard::content-type"),
        path.as_os_str(),
    ])?;
    parse_content_type(&stdout).ok_or_else(|| {
        format!(
            "`gio` did not report a content type for `{}`, so no association list can be built without guessing",
            path.display()
        )
    })
}

/// The content type in `gio info` output, when the attribute is present.
pub fn parse_content_type(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let trimmed = line.trim();
        let value = trimmed.strip_prefix("standard::content-type:")?.trim();
        (!value.is_empty() && !value.contains(char::is_whitespace)).then(|| value.to_owned())
    })
}

/// Asks `gio` which applications are registered for `content_type`.
fn gio_registered_applications(
    content_type: &str,
) -> Result<(Option<String>, Vec<String>), String> {
    let stdout = run_gio(&[OsStr::new("mime"), OsStr::new(content_type)])?;
    let (default, registered) = parse_mime_applications(&stdout);
    if default.is_none() && registered.is_empty() {
        // GIO answered, but with nothing this parser recognises. Reporting that as "no applications"
        // would hide a format change behind a plausible-looking empty menu.
        return Err(format!(
            "`gio mime` reported no applications for `{content_type}` in a form this build understands"
        ));
    }
    Ok((default, registered))
}

/// Splits `gio mime` output into its default application and the ordered registered ids.
///
/// The default leads the list and is not repeated, which is the order upstream produces: the
/// desktop's own preference is the first entry of the menu.
pub fn parse_mime_applications(stdout: &str) -> (Option<String>, Vec<String>) {
    let mut default = None;
    let mut ordered: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains(':') {
            // Section headers end with ':', and the "Default application ...: x.desktop" line carries
            // one. A bare id, which is the only thing a registration looks like, has neither.
            if let Some(value) = default_from_line(trimmed) {
                default = Some(value);
            }
            continue;
        }
        if is_desktop_id(trimmed) {
            ordered.push(trimmed.to_owned());
        }
    }
    if let Some(preferred) = default.clone() {
        ordered.retain(|id| id != &preferred);
        ordered.insert(0, preferred);
    }
    ordered.dedup();
    (default, ordered)
}

/// The application id on a `Default application ...:` line.
fn default_from_line(line: &str) -> Option<String> {
    let value = line.rsplit(':').next()?.trim();
    let value = value
        .trim_matches(|character: char| matches!(character, '"' | '\'' | '“' | '”' | '‘' | '’'));
    is_desktop_id(value).then(|| value.to_owned())
}

/// Whether a value looks like a desktop-entry id.
///
/// Ids are file names, so anything with a path separator is not one; requiring the suffix keeps a
/// stray line of prose from becoming an application.
fn is_desktop_id(value: &str) -> bool {
    value.ends_with(".desktop") && !value.contains('/') && !value.contains('\\')
}

/// Builds the application list from resolved ids.
pub fn applications_for(
    ids: &[String],
    default: Option<&str>,
    environment: &DesktopEnvironment,
) -> Vec<FileApplication> {
    let index = desktop_index(environment.data_directories());
    let mut applications = Vec::new();
    for id in ids {
        let Some(file) = index.get(id) else {
            // An id GIO knows but no data root ships: the application was removed after the database
            // was written. Skipping it is right; inventing a name would not be.
            continue;
        };
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let fields = desktop_entry_fields(&text);
        if fields
            .get("Hidden")
            .is_some_and(|hidden| hidden.eq_ignore_ascii_case("true"))
        {
            // Hidden=true is how an entry says "do not offer me"; the desktop honours it, so a list
            // that did not would offer an application the desktop refuses to launch.
            continue;
        }
        let Some(name) = localized_name(&fields, environment.locale()) else {
            // An entry with no Name cannot be shown as a menu item.
            continue;
        };
        applications.push(FileApplication {
            id: file.display().to_string(),
            name,
            default: default.is_some_and(|preferred| preferred == id),
            icon: None,
        });
    }
    applications
}

/// Maps every `.desktop` id under the data roots to its file, honouring XDG precedence.
///
/// The id of a nested entry is its path relative to the `applications` directory with separators
/// replaced by dashes, which is how the desktop menu names it and how GIO reports it.
fn desktop_index(directories: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    let mut index = BTreeMap::new();
    for directory in directories {
        let applications = directory.join("applications");
        collect_desktop_files(&applications, &applications, 0, &mut index);
    }
    index
}

/// Adds the entries under `directory` to `index`, without following directory symlinks.
///
/// `or_insert` is what gives the first data root precedence; a later root cannot replace an entry the
/// user's own `XDG_DATA_HOME` already defines.
fn collect_desktop_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    index: &mut BTreeMap<String, PathBuf>,
) {
    if depth > MAX_APPLICATION_DIRECTORY_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        // An absent or unreadable root contributes no applications, which is the same answer as an
        // empty one.
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // `file_type` rather than `is_dir` so a symlinked directory is not followed: a loop of links
        // would cost the depth budget and produce ids nobody meant.
        let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if is_directory {
            collect_desktop_files(root, &path, depth + 1, index);
            continue;
        }
        let is_entry = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("desktop"));
        if !is_entry {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let id = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("-");
        index.entry(id).or_insert(path);
    }
}

/// The fields of a desktop entry's main group.
///
/// Only `[Desktop Entry]` is read: action groups repeat keys like `Name`, and a lookup that took the
/// last one would name the application after its "New Window" action.
pub fn desktop_entry_fields(text: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut in_main_group = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_main_group = trimmed == "[Desktop Entry]";
            continue;
        }
        if !in_main_group || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let key = key.trim();
            if !key.is_empty() {
                fields.insert(key.to_owned(), value.trim().to_owned());
            }
        }
    }
    fields
}

/// The name to show for an entry, localized for `locale` when a translation exists.
///
/// The fallback chain is the one upstream uses: the full locale (`Name[zh_CN]`), then its language
/// (`Name[zh]`), then the untranslated `Name`.
pub fn localized_name(fields: &BTreeMap<String, String>, locale: Option<&str>) -> Option<String> {
    if let Some(locale) = locale {
        // `zh_CN.UTF-8@euro` is still `zh_CN` as far as desktop entries are concerned.
        let full = locale.split(['.', '@']).next().unwrap_or(locale);
        if let Some(name) = fields.get(&format!("Name[{full}]")) {
            return Some(name.clone());
        }
        let language = full.split('_').next().unwrap_or(full);
        if let Some(name) = fields.get(&format!("Name[{language}]")) {
            return Some(name.clone());
        }
    }
    fields.get("Name").cloned()
}

/// Runs `gio` and returns its standard output.
///
/// `LC_ALL=C` is set so the section headers this parser looks for are not translated; without it a
/// localized desktop would produce output no parser here recognises. The child inherits no stdin and
/// no descriptors beyond the pipes `output` owns, so a `gio` that lingers cannot hold this process
/// open.
fn run_gio(arguments: &[&OsStr]) -> Result<String, String> {
    let output = Command::new("gio")
        .args(arguments)
        .env("LC_ALL", "C")
        .output()
        .map_err(|error| match error.kind() {
            ErrorKind::NotFound => {
                "`gio` is not installed or not on PATH, so the applications registered for a path cannot be listed"
                    .to_owned()
            }
            _ => format!("`gio` could not be run: {error}"),
        })?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr);
        let reason = reason.lines().next().unwrap_or("").trim();
        return Err(format!(
            "`gio` exited with {}: {reason}",
            match output.status.code() {
                Some(code) => code.to_string(),
                // No code means a signal ended it, which is worth saying explicitly.
                None => "a signal".to_owned(),
            }
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| "`gio` printed output that is not valid UTF-8".to_owned())
}
