//! Tests for `session.workspaceDesktop`, `session.canOpenWorkspacePath` and `session.workspacePathApplications`.
//!
//! The properties under test, in the order they matter:
//!
//! - **the desktop answer follows the platform** — every platform and display combination is asserted
//!   on whatever machine runs the tests, because the mapping is the part that cannot be observed by
//!   actually opening a window
//! - **a headless host answers an empty list without touching the path** — the short-circuit runs
//!   before validation, and the prober is never reached, so a client drawing a menu on a server gets
//!   "nothing" instead of an argument error
//! - **the workspace boundary holds** — an escape and a missing path are decided before the prober
//!   runs, and the confinement is the same one `host.openPath` applies
//! - **a probe failure is reported, not swallowed** — an empty list would look like "no applications
//!   are registered", which is a different and false statement
//! - **the Linux entry parser reads what GIO and the desktop actually write** — nested ids, XDG
//!   precedence, localized names, `Hidden=true`, and the main group only
//!
//! No test starts a GUI program. The only subprocess that could run is `gio`, and every test either
//! injects a prober or answers a case that returns before the prober is reached. The parser tests run
//! against files in a scratch directory rather than against the machine's installed applications, so
//! the results do not depend on what happens to be installed.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the methods as written even before the crate root wires them in. It spells its paths
//! through `girder_core::`, which is the alias the library declares for itself, so the same source
//! builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/workspace_desktop_rpc.rs"]
mod workspace_desktop_rpc;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use girder_core::Opener;
use girder_core::{RemoteCall, RemoteError, RemoteRegistry};
use serde_json::{Value, json};

use workspace_desktop_rpc::{
    CAN_OPEN_METHOD, DESKTOP_METHOD, DesktopEnvironment, FileApplication, FileApplicationProber,
    FileManager, GioFileApplicationProber, HostPlatform, NAMESPACE, PATH_APPLICATIONS_METHOD,
    applications_for, desktop_entry_fields, localized_name, parse_content_type,
    parse_mime_applications, register_workspace_desktop_methods,
    register_workspace_desktop_methods_with,
};

// ---- fixtures ---------------------------------------------------------------------------------

/// An isolated scratch directory: `temp_dir()` plus pid plus a counter.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-desktop-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created scratch directory");
    dir
}

/// A workspace root with one file inside it.
fn fixture(name: &str) -> PathBuf {
    let root = scratch(name);
    std::fs::write(root.join("notes.md"), "# notes\n").expect("wrote notes");
    root
}

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// An opener that records nothing, for the registrations that never consult it.
struct SilentOpener;

impl Opener for SilentOpener {
    fn open(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
    fn reveal(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
}

/// A prober that returns a fixed answer and records what it was asked.
struct StubProber {
    asked: Mutex<Vec<PathBuf>>,
    answer: Result<Vec<FileApplication>, String>,
}

impl Default for StubProber {
    fn default() -> Self {
        Self::answering(Vec::new())
    }
}

impl StubProber {
    fn answering(applications: Vec<FileApplication>) -> Self {
        Self {
            asked: Mutex::new(Vec::new()),
            answer: Ok(applications),
        }
    }

    fn failing(reason: &str) -> Self {
        Self {
            asked: Mutex::new(Vec::new()),
            answer: Err(reason.to_owned()),
        }
    }

    fn calls(&self) -> usize {
        self.asked.lock().expect("unpoisoned").len()
    }
}

impl FileApplicationProber for StubProber {
    fn applications(
        &self,
        path: &Path,
        _environment: &DesktopEnvironment,
    ) -> Result<Vec<FileApplication>, String> {
        self.asked
            .lock()
            .expect("unpoisoned")
            .push(path.to_path_buf());
        self.answer.clone()
    }
}

/// An environment with everything supplied, so no test asserts on the machine it runs on.
fn environment(platform: HostPlatform) -> DesktopEnvironment {
    DesktopEnvironment::for_host(platform, "test-host")
}

/// A graphical Linux host, which is the only environment that reaches the probe at all.
fn graphical_linux() -> DesktopEnvironment {
    environment(HostPlatform::Linux).with_display(true)
}

fn application(id: &str, name: &str, default: bool) -> FileApplication {
    FileApplication {
        id: id.to_owned(),
        name: name.to_owned(),
        default,
        icon: None,
    }
}

/// Builds a registry over `workspace` with the given prober and machine facts.
fn registry(
    workspace: &Path,
    environment: DesktopEnvironment,
    prober: Arc<dyn FileApplicationProber>,
) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    register_workspace_desktop_methods_with(
        &mut registry,
        workspace,
        &(Arc::new(SilentOpener) as Arc<dyn Opener>),
        prober,
        Arc::new(environment),
    );
    registry
}

/// Dispatches `session.workspacePathApplications` for `path`.
async fn applications_for_path(
    registry: &RemoteRegistry,
    path: &str,
) -> Result<Value, RemoteError> {
    registry
        .dispatch(
            NAMESPACE,
            PATH_APPLICATIONS_METHOD,
            call(json!({ "path": path })),
        )
        .await
}

// ---- the desktop answer -----------------------------------------------------------------------

#[tokio::test]
async fn the_desktop_answer_names_the_host_and_the_platform_convention() {
    // `None` is the expected `fileManager` for a platform with no convention at all.
    for (platform, expected_manager, expected_available) in [
        (HostPlatform::MacOs, Some("finder"), true),
        (HostPlatform::Windows, Some("explorer"), true),
        (HostPlatform::Linux, Some("directory"), false),
        (HostPlatform::Other, None, false),
    ] {
        let workspace = fixture("desktop");
        let registry = registry(
            &workspace,
            environment(platform),
            Arc::new(StubProber::default()),
        );
        let value = registry
            .dispatch(NAMESPACE, DESKTOP_METHOD, call(Value::Null))
            .await
            .expect("desktop answered");

        assert_eq!(value["name"], "test-host");
        assert_eq!(value["available"], expected_available);
        match expected_manager {
            Some(name) => assert_eq!(value["fileManager"], name),
            None => assert_eq!(value["fileManager"], Value::Null),
        }
        let _ = std::fs::remove_dir_all(&workspace);
    }
}

#[tokio::test]
async fn a_headless_linux_host_keeps_its_convention_but_reports_itself_unavailable() {
    // `fileManager` describes the platform, `available` describes this deployment. A headless server
    // still has a "directory" convention; it simply cannot launch anything, and a client that
    // conflated the two would offer a button that opens nothing.
    let workspace = fixture("headless");
    let registry = registry(
        &workspace,
        environment(HostPlatform::Linux),
        Arc::new(StubProber::default()),
    );
    let value = registry
        .dispatch(NAMESPACE, DESKTOP_METHOD, call(Value::Null))
        .await
        .expect("desktop answered");

    assert_eq!(value["fileManager"], "directory");
    assert_eq!(value["available"], false);
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn wsl_reports_the_windows_file_manager_and_is_available() {
    // The path goes to the Windows desktop, so the reveal convention is Explorer's, not a
    // directory-opening one.
    let workspace = fixture("wsl");
    let registry = registry(
        &workspace,
        environment(HostPlatform::Linux).with_wsl(true),
        Arc::new(StubProber::default()),
    );
    let value = registry
        .dispatch(NAMESPACE, DESKTOP_METHOD, call(Value::Null))
        .await
        .expect("desktop answered");

    assert_eq!(value["fileManager"], "explorer");
    assert_eq!(value["available"], true);
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn can_open_agrees_with_the_desktop_answer() {
    // Two methods answering the same question must not disagree; a client reads the boolean for a
    // cheap check and the object for a menu, and a mismatch shows one of them as broken.
    for environment in [
        environment(HostPlatform::MacOs),
        environment(HostPlatform::Windows),
        environment(HostPlatform::Linux),
        graphical_linux(),
        environment(HostPlatform::Other),
    ] {
        let workspace = fixture("can-open");
        let registry = registry(
            &workspace,
            environment.clone(),
            Arc::new(StubProber::default()),
        );
        let desktop = registry
            .dispatch(NAMESPACE, DESKTOP_METHOD, call(Value::Null))
            .await
            .expect("desktop answered");
        let can_open = registry
            .dispatch(NAMESPACE, CAN_OPEN_METHOD, call(Value::Null))
            .await
            .expect("canOpen answered");

        assert_eq!(can_open, desktop["available"]);
        let _ = std::fs::remove_dir_all(&workspace);
    }
}

// ---- workspacePathApplications: the unavailable short-circuit ----------------------------------

#[tokio::test]
async fn an_unavailable_desktop_answers_an_empty_list_without_validating_the_path() {
    // Upstream returns [] before it verifies the path, and the order matters: a menu drawn on a
    // headless server should be empty, not an error about a path nobody can open.
    let workspace = fixture("headless-list");
    let prober = Arc::new(StubProber::answering(vec![application("a", "A", true)]));
    let registry = registry(
        &workspace,
        environment(HostPlatform::Linux),
        Arc::clone(&prober) as Arc<dyn FileApplicationProber>,
    );

    for path in ["notes.md", "", "../../etc/passwd", "missing.txt"] {
        let value = applications_for_path(&registry, path)
            .await
            .expect("an unavailable desktop answers an empty list");
        assert_eq!(value, json!([]), "path {path:?}");
    }
    assert_eq!(
        prober.calls(),
        0,
        "the prober must not run on a headless host"
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

// ---- workspacePathApplications: the answered path ---------------------------------------------

#[tokio::test]
async fn a_registered_path_answers_the_prober_list_with_its_fields() {
    let workspace = fixture("list");
    let prober = Arc::new(StubProber::answering(vec![
        application("/usr/share/applications/editor.desktop", "Editor", true),
        application("/usr/share/applications/viewer.desktop", "Viewer", false),
    ]));
    let registry = registry(
        &workspace,
        graphical_linux(),
        Arc::clone(&prober) as Arc<dyn FileApplicationProber>,
    );

    let value = applications_for_path(&registry, "notes.md")
        .await
        .expect("listed");

    assert_eq!(
        value,
        json!([
            {
                "id": "/usr/share/applications/editor.desktop",
                "name": "Editor",
                "default": true,
                "icon": null,
            },
            {
                "id": "/usr/share/applications/viewer.desktop",
                "name": "Viewer",
                "default": false,
                "icon": null,
            },
        ])
    );

    // The prober is handed the canonical absolute path, not the caller's spelling: the desktop looks
    // associations up by the real file, and a relative path would be looked up against the server's
    // own working directory.
    let asked = prober.asked.lock().expect("unpoisoned").clone();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].is_absolute(), "{:?}", asked[0]);
    assert!(asked[0].ends_with("notes.md"), "{:?}", asked[0]);
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn no_registered_application_answers_an_empty_list() {
    // The honest "nothing is registered here", which is what a file type with no handler produces.
    let workspace = fixture("no-apps");
    let registry = registry(
        &workspace,
        graphical_linux(),
        Arc::new(StubProber::answering(Vec::new())),
    );

    let value = applications_for_path(&registry, "notes.md")
        .await
        .expect("listed");
    assert_eq!(value, json!([]));
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn a_probe_failure_is_reported_rather_than_answered_as_an_empty_list() {
    // "I could not ask" and "nothing is registered" call for different reactions from an operator,
    // and an empty list would hide a missing `gio` behind a plausible-looking menu.
    let workspace = fixture("probe-failure");
    let registry = registry(
        &workspace,
        graphical_linux(),
        Arc::new(StubProber::failing("`gio` is not installed")),
    );

    let error = applications_for_path(&registry, "notes.md")
        .await
        .expect_err("a failed probe is an error");
    match error {
        RemoteError::Failed(message) => {
            assert!(message.contains("`gio` is not installed"), "{message}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&workspace);
}

// ---- workspacePathApplications: arguments and the boundary ------------------------------------

#[tokio::test]
async fn a_missing_path_argument_is_refused() {
    let workspace = fixture("no-argument");
    let registry = registry(
        &workspace,
        graphical_linux(),
        Arc::new(StubProber::default()),
    );

    for args in [json!({}), json!({ "path": 7 }), json!({ "path": null })] {
        let error = registry
            .dispatch(NAMESPACE, PATH_APPLICATIONS_METHOD, call(args))
            .await
            .expect_err("an argument refusal");
        assert!(
            matches!(error, RemoteError::InvalidArguments { .. }),
            "{error:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn an_unknown_path_is_an_argument_error_and_never_reaches_the_prober() {
    let workspace = fixture("unknown");
    let prober = Arc::new(StubProber::answering(vec![application("a", "A", true)]));
    let registry = registry(
        &workspace,
        graphical_linux(),
        Arc::clone(&prober) as Arc<dyn FileApplicationProber>,
    );

    let error = applications_for_path(&registry, "not-here.txt")
        .await
        .expect_err("a missing file is refused");
    match error {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(message.contains("does not exist"), "{message}");
        }
        other => panic!("expected InvalidArguments, got {other:?}"),
    }
    assert_eq!(prober.calls(), 0);
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn a_path_outside_the_workspace_is_refused_before_the_prober_runs() {
    // The boundary is the point: the answer describes what the machine does with a path, and a path
    // the tools could not read must not be described either.
    let workspace = fixture("escape");
    let prober = Arc::new(StubProber::answering(vec![application("a", "A", true)]));
    let registry = registry(
        &workspace,
        graphical_linux(),
        Arc::clone(&prober) as Arc<dyn FileApplicationProber>,
    );

    for path in ["../../etc/passwd", "/etc/passwd", "../"] {
        let error = applications_for_path(&registry, path)
            .await
            .expect_err("an escape is refused");
        assert!(
            matches!(error, RemoteError::Refused { .. }),
            "path {path:?} gave {error:?}"
        );
    }
    assert_eq!(prober.calls(), 0);
    let _ = std::fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn an_empty_path_is_an_argument_error() {
    let workspace = fixture("empty-path");
    let registry = registry(
        &workspace,
        graphical_linux(),
        Arc::new(StubProber::default()),
    );

    for path in ["", "   "] {
        let error = applications_for_path(&registry, path)
            .await
            .expect_err("an empty path is refused");
        assert!(
            matches!(error, RemoteError::InvalidArguments { .. }),
            "path {path:?} gave {error:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&workspace);
}

// ---- the Linux entry parser -------------------------------------------------------------------

#[test]
fn the_content_type_is_read_from_gio_info_output() {
    let stdout = "display name: notes.md\n\
                  standard::content-type: text/markdown\n\
                  standard::icon: text-x-generic\n";
    assert_eq!(parse_content_type(stdout).as_deref(), Some("text/markdown"));
    // A missing attribute is `None`: guessing one from the file name is what upstream refuses to do.
    assert_eq!(parse_content_type("display name: notes.md\n"), None);
    assert_eq!(parse_content_type("standard::content-type:\n"), None);
}

#[test]
fn gio_mime_output_yields_the_default_first_and_the_rest_after_it() {
    let stdout = "Default application for text/markdown: editor.desktop\n\
                  Registered applications:\n\
                  \teditor.desktop\n\
                  \tviewer.desktop\n\
                  Recommended applications:\n\
                  \tviewer.desktop\n";
    let (default, ordered) = parse_mime_applications(stdout);
    assert_eq!(default.as_deref(), Some("editor.desktop"));
    assert_eq!(
        ordered,
        vec!["editor.desktop".to_owned(), "viewer.desktop".to_owned()]
    );
}

#[test]
fn a_registration_without_a_default_is_still_listed() {
    let stdout = "Registered applications:\n\tviewer.desktop\n";
    let (default, ordered) = parse_mime_applications(stdout);
    assert_eq!(default, None);
    assert_eq!(ordered, vec!["viewer.desktop".to_owned()]);
}

#[test]
fn prose_that_is_not_an_application_id_is_ignored() {
    // The parser must not turn a line of help text into a menu entry; only a bare id is a
    // registration, and only a `.desktop` name is an id.
    let stdout = "No default application\n\
                  Registered applications:\n\
                  \there/is/a/path.desktop\n\
                  \tnot-an-application\n\
                  \treal.desktop\n";
    let (_, ordered) = parse_mime_applications(stdout);
    assert_eq!(ordered, vec!["real.desktop".to_owned()]);
}

#[test]
fn a_desktop_entry_s_actions_do_not_override_its_name() {
    let text = "[Desktop Entry]\n\
                Name=Player\n\
                Name[zh_CN]=播放器\n\
                MimeType=audio/mpeg\n\
                Hidden=false\n\
                \n\
                [Desktop Action NewWindow]\n\
                Name=Wrong Name\n";
    let fields = desktop_entry_fields(text);
    assert_eq!(fields.get("Name").map(String::as_str), Some("Player"));
    assert_eq!(
        fields.get("Name[zh_CN]").map(String::as_str),
        Some("播放器")
    );
    // A key that only exists in an action group must not appear.
    assert_eq!(fields.get("Name[Desktop Action NewWindow]"), None);
}

#[test]
fn a_localized_name_is_preferred_in_locale_order() {
    let text = "[Desktop Entry]\nName=Player\nName[zh]=玩家\nName[zh_CN]=播放器\n";
    let fields = desktop_entry_fields(text);

    assert_eq!(
        localized_name(&fields, Some("zh_CN.UTF-8")).as_deref(),
        Some("播放器")
    );
    // The language alone is the next fallback, which covers a locale the entry does not translate.
    assert_eq!(
        localized_name(&fields, Some("zh_TW.UTF-8")).as_deref(),
        Some("玩家")
    );
    // A locale with no translation at all falls back to the untranslated name.
    assert_eq!(
        localized_name(&fields, Some("en_US.UTF-8")).as_deref(),
        Some("Player")
    );
    assert_eq!(localized_name(&fields, None).as_deref(), Some("Player"));
}

#[test]
fn the_field_parser_ignores_comments_and_blank_keys() {
    let text = "[Desktop Entry]\n# Name=Commented\n=no key\nName=Real\n";
    let fields = desktop_entry_fields(text);
    assert_eq!(fields.len(), 1);
    assert_eq!(fields.get("Name").map(String::as_str), Some("Real"));
}

#[test]
fn applications_are_resolved_from_the_xdg_roots_with_nested_ids_and_hidden_entries_skipped() {
    let base = scratch("entries");
    let root = base.join("share");
    let applications = root.join("applications");
    std::fs::create_dir_all(applications.join("vendor")).expect("created vendor directory");
    std::fs::write(
        applications.join("editor.desktop"),
        "[Desktop Entry]\nName=Editor\n",
    )
    .expect("wrote editor");
    std::fs::write(
        applications.join("vendor").join("viewer.desktop"),
        "[Desktop Entry]\nName=Viewer\n",
    )
    .expect("wrote viewer");
    std::fs::write(
        applications.join("hidden.desktop"),
        "[Desktop Entry]\nName=Hidden\nHidden=true\n",
    )
    .expect("wrote hidden");
    std::fs::write(
        applications.join("nameless.desktop"),
        "[Desktop Entry]\nExec=nameless\n",
    )
    .expect("wrote nameless");

    let environment = graphical_linux()
        .with_data_directories(vec![root])
        .with_locale(Some("en_US.UTF-8".to_owned()));
    let ids = vec![
        "editor.desktop".to_owned(),
        "vendor-viewer.desktop".to_owned(),
        "hidden.desktop".to_owned(),
        "nameless.desktop".to_owned(),
        // An id GIO knows but no data root ships: the application was removed after the database was
        // written, and skipping it beats inventing a name for it.
        "uninstalled.desktop".to_owned(),
    ];

    let applications = applications_for(&ids, Some("editor.desktop"), &environment);

    // Two entries survive: the hidden one and the nameless one are not menu items, and the
    // uninstalled id has no file to read.
    assert_eq!(applications.len(), 2);
    assert!(applications[0].id.ends_with("editor.desktop"));
    assert_eq!(applications[0].name, "Editor");
    assert!(applications[0].default);
    // A nested entry is addressed by its id (a dashed relative path) but reports the file it lives in,
    // which is what an open call needs.
    let nested = Path::new(&applications[1].id);
    assert!(nested.ends_with("vendor/viewer.desktop"), "{nested:?}");
    assert_eq!(applications[1].name, "Viewer");
    assert!(!applications[1].default);
    assert!(applications.iter().all(|app| app.icon.is_none()));

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn an_earlier_data_root_wins_over_a_later_one() {
    // XDG precedence, applied where it matters: a user's own entry replaces the system one instead of
    // appearing twice, once with each name.
    let base = scratch("precedence");
    let user = base.join("user");
    let system = base.join("system");
    std::fs::create_dir_all(user.join("applications")).expect("created user root");
    std::fs::create_dir_all(system.join("applications")).expect("created system root");
    std::fs::write(
        user.join("applications").join("editor.desktop"),
        "[Desktop Entry]\nName=My Editor\n",
    )
    .expect("wrote user entry");
    std::fs::write(
        system.join("applications").join("editor.desktop"),
        "[Desktop Entry]\nName=System Editor\n",
    )
    .expect("wrote system entry");

    let environment = graphical_linux().with_data_directories(vec![user.clone(), system]);
    let applications = applications_for(
        &["editor.desktop".to_owned()],
        Some("editor.desktop"),
        &environment,
    );

    assert_eq!(applications.len(), 1);
    assert_eq!(applications[0].name, "My Editor");
    assert_eq!(
        applications[0].id,
        user.join("applications")
            .join("editor.desktop")
            .display()
            .to_string()
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn the_prober_answers_an_empty_list_where_launch_services_or_shell_enumeration_would_be_needed() {
    // The frontier of this build, asserted as a decision rather than left to look like a bug: macOS
    // needs LaunchServices and Windows needs Shell COM, so both answer "nothing known here" instead
    // of a guess.
    let path = Path::new("/tmp/report.txt");
    for platform in [
        HostPlatform::MacOs,
        HostPlatform::Windows,
        HostPlatform::Other,
    ] {
        let environment = environment(platform);
        let applications = GioFileApplicationProber
            .applications(path, &environment)
            .expect("an unsupported platform answers an empty list, not an error");
        assert!(applications.is_empty(), "{platform:?}");
    }
}

#[test]
fn the_environment_detection_names_this_machine_and_reports_a_usable_platform() {
    // The one test that reads the real environment. It asserts only properties true of any host, so
    // it does not depend on the machine: a name is always produced, and the platform is one of the
    // four values rather than something unrecognised.
    let detected = DesktopEnvironment::detect();
    assert!(!detected.name().is_empty());
    assert_ne!(detected.name(), "");
    assert!(matches!(
        detected.platform(),
        HostPlatform::MacOs | HostPlatform::Windows | HostPlatform::Linux | HostPlatform::Other
    ));
    // The desktop answer agrees with its own parts, whichever platform this is.
    let info = detected.desktop_info();
    assert_eq!(info.name, detected.name());
    assert_eq!(info.file_manager, detected.file_manager());
    assert_eq!(
        info.available,
        detected.can_open() && info.file_manager.is_some()
    );
    // A file manager spells itself the way the wire expects.
    if let Some(manager) = detected.file_manager() {
        assert_eq!(manager.as_str(), serde_json::to_value(manager).unwrap());
    }
}

#[test]
fn a_host_built_for_a_test_probes_nothing() {
    // The constructor is a seam, so it must describe exactly the host it was given: no display
    // inherited from the machine running the tests, and no XDG roots.
    let built = DesktopEnvironment::for_host(HostPlatform::Linux, "described");
    assert_eq!(built.name(), "described");
    assert!(!built.has_display());
    assert!(!built.is_wsl());
    assert!(built.data_directories().is_empty());
    assert_eq!(built.locale(), None);
    assert!(!built.can_open());
    assert_eq!(built.file_manager(), Some(FileManager::Directory));
}

#[test]
fn the_file_manager_spellings_match_a_client_s_expectations() {
    assert_eq!(FileManager::Finder.as_str(), "finder");
    assert_eq!(FileManager::Explorer.as_str(), "explorer");
    assert_eq!(FileManager::Directory.as_str(), "directory");
}

// ---- the convenience registration -------------------------------------------------------------

#[tokio::test]
async fn the_default_registration_answers_the_desktop_methods_on_this_machine() {
    // The entry point the wiring uses, exercised against the real environment. Only the two methods
    // that never start a subprocess are dispatched: `workspacePathApplications` would run `gio`, and
    // no test in this file is allowed to depend on what is installed here.
    let workspace = fixture("default");
    let mut registry = RemoteRegistry::new();
    register_workspace_desktop_methods(
        &mut registry,
        &workspace,
        &(Arc::new(SilentOpener) as Arc<dyn Opener>),
    );

    let desktop = registry
        .dispatch(NAMESPACE, DESKTOP_METHOD, call(Value::Null))
        .await
        .expect("desktop answered");
    assert!(desktop["name"].is_string());
    assert!(desktop["available"].is_boolean());
    assert!(desktop["fileManager"].is_string() || desktop["fileManager"].is_null());

    let can_open = registry
        .dispatch(NAMESPACE, CAN_OPEN_METHOD, call(Value::Null))
        .await
        .expect("canOpen answered");
    assert_eq!(can_open, desktop["available"]);

    let _ = std::fs::remove_dir_all(&workspace);
}
