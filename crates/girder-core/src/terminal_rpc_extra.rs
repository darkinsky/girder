//! The rest of the `terminal.*` Remote surface: `environment` and `shells`.
//!
//! [`girder_core::register_terminal_rpc_methods`] serves five names — `list`, `create`, `send`,
//! `read`, and `close` — over one [`girder_core::TerminalRegistry`]. Upstream's terminal controller
//! declares nine `@Remote` methods: `environment`, `shells`, `list`, `create`, `watch`, `retain`,
//! `attach`, `rename`, and `close`. Two of the names this build serves are not upstream names at
//! all: `send` and `read` are its non-streaming substitute for the streaming `attach`. So the
//! *upstream* names still missing here are six, and this module serves the two that are answerable
//! from facts the process already holds:
//!
//! - `environment` answers the defaults a client needs before it opens a pane: the directory a shell
//!   will start in, the dimensions `terminal.create` falls back to, the largest dimensions it
//!   accepts, and how much transcript it retains.
//! - `shells` answers which programs on this host can actually be run as a shell, so a picker offers
//!   a path that `terminal.create` will accept instead of a name the deployment guesses.
//!
//! # What is still missing, and why
//!
//! | upstream method | why it is not here |
//! |---|---|
//! | `attach` | duplex stream: output frames out, input commands in |
//! | `retain` | stream: holds one terminal open for one window |
//! | `watch` | stream: pushes the whole roster after every change |
//! | `rename` | the title has nowhere observable to go (see below) |
//!
//! The first three need a *stream mode*, and [`girder_core::RemoteRegistry`] has none: `register`
//! takes a handler that resolves one value and returns, so there is no way to push frames or to
//! accept input frames on one connection. Serving `attach` as a request/response call would not be a
//! simplification, it would be a different wire contract under the same name — the client opens a
//! stream and waits for frames that can never arrive. That is a protocol change (a registered stream
//! method kind on [`girder_core::RemoteRegistry`], plus a transport that keeps the call open), and
//! it is declared as a gap rather than faked. `terminal.send` and `terminal.read` remain the
//! polling substitute for `attach`, which is the same reasoning stated in `terminal_rpc`'s docs.
//!
//! `rename` is a different kind of gap, and the reason is worth stating precisely because it looks
//! implementable. It takes an id and a title, and the only place a title could ever be *seen* is
//! `terminal.list`, whose projection is written in the module that owns the registry. This module
//! may not replace `list` (re-registering it would silently drop the fields it already reports), and
//! [`girder_core::TerminalSession`] holds no title. A title recorded in a side table here would
//! therefore be visible to no method at all: `rename` would answer success while every client kept
//! displaying the id. The missing piece is a `title` on the session (or on the listing), which
//! belongs to `terminal.rs` and to the module that projects it — not to a module that only adds
//! method names. Until then the name stays a declared gap.
//!
//! # Why arguments are ignored
//!
//! Upstream's `environment` and `shells` take an `agent`, which identifies one Session's execution
//! environment. This build has one process and one deployment-wide set of defaults, and
//! [`girder_core::RemoteCall`] carries a peer string nothing here can authenticate against (the same
//! limitation `terminal_rpc` states for ids). So a request's arguments are accepted and not read: a
//! generated client that sends `agentId` gets the same answer as one that sends nothing, which is
//! the truth — the answer does not depend on which caller asked.
//!
//! # Why `environment` borrows upstream's field names but not all of its fields
//!
//! Upstream's `TerminalEnvironment` carries `cwd`, `maxInputBytes`, `maxBufferedBytes`, `maxCols`,
//! `maxRows`, and `scrollback`. Three of those describe limits whose equivalent this build does not
//! enforce, and inventing a number would be worse than omitting the key:
//!
//! - `maxInputBytes` / `maxBufferedBytes`: upstream refuses input by size and by unacknowledged
//!   buffered bytes. [`girder_core::TerminalSession`] writes whatever string it is handed, and this
//!   layer has no per-call bound, so any number reported here would be a promise no method keeps.
//!   They are omitted, and their absence is the honest signal that a client may paste freely.
//! - `scrollback`: upstream counts scrollback *lines*. The transcript here is bounded in *bytes*
//!   ([`girder_core::DEFAULT_TRANSCRIPT_BYTES`]) and is read back whole rather than paged by line,
//!   so a line count would be a unit conversion nothing here can perform. The byte bound is
//!   reported under its own name, `maxTranscriptBytes`.
//!
//! A field is omitted rather than sent as `null` for the reason `terminal_rpc` gives: a client that
//! formats a value renders a `null` as the string "null".

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use girder_core::{
    Access, DEFAULT_TRANSCRIPT_BYTES, Declaration, RemoteCall, RemoteError, RemoteRegistry,
    TerminalSize,
};

/// The namespace these methods register under.
pub const NAMESPACE: &str = "terminal";

/// The method name of the deployment-defaults query.
pub const ENVIRONMENT_METHOD: &str = "environment";

/// The method name of the shell discovery query.
pub const SHELLS_METHOD: &str = "shells";

/// The upstream method this module declares as a gap.
///
/// `watch` is declared here and not in `terminal_rpc` because that module's list predates it: the
/// three streaming names it already declares (`attach`, `retain`) and `rename` are left alone so a
/// declaration is not recorded twice, and `watch` — which upstream also marks `mode: 'stream'` — is
/// added so a client is told "not implemented by this build" rather than "unknown method", which
/// would read as its own typo.
pub const WATCH_METHOD: &str = "watch";

/// The programs `shells` probes, in the order a picker should prefer them.
///
/// Upstream's default candidate list. `sh` is deliberately absent: it is the fallback the launcher
/// runs when the deployment names no shell, and it reaches the list through [`default_program`]
/// instead, so the entry that is actually the default appears once and first instead of twice.
const CANDIDATES: &[&str] = &["zsh", "bash", "fish", "pwsh", "powershell", "cmd"];

/// The resource kind these methods declare to an access policy.
const RESOURCE_KIND: &str = "terminal";

/// What both methods here declare to an access policy.
///
/// Neither starts, writes, or ends a shell: one reads deployment constants, the other reads `PATH`.
/// Declaring them as reads is what lets a read-only policy keep a shell picker working while it
/// still refuses `terminal.create`.
pub const DECLARED_ACCESS: Access = Access::Read;

/// The largest width or height `terminal.create` accepts.
///
/// The `u16` that [`girder_core::TerminalSize`] stores, which is the real bound: `create` refuses a
/// larger value rather than clamping it (`terminal_rpc::dimension`), so the `u16` ceiling is the
/// truthful answer. Upstream reports 500 columns and 200 rows because *its* validation refuses more;
/// borrowing those numbers would describe a limit this build does not enforce, and a client that
/// clamped its layout to them would draw a smaller pane than the server would have accepted.
pub const MAX_DIMENSION: u16 = u16::MAX;

/// Registers `terminal.environment` and `terminal.shells`.
///
/// A free function taking only the registry, like the other `register_*` functions: the host decides
/// which capabilities exist, and a module that registered itself would make that decision for it.
///
/// No [`girder_core::TerminalRegistry`] is taken, deliberately. Neither method reads session state:
/// one reports deployment constants, the other probes the host's `PATH`. Taking a registry they do
/// not read would suggest a coupling that does not exist, and would invite a later reader to look
/// for the session lookup that was never there. The function that registers a method that *does*
/// need sessions is `terminal_rpc::register_terminal_methods`, which owns that argument.
pub fn register_terminal_extra_methods(registry: &mut RemoteRegistry) {
    registry.register(NAMESPACE, ENVIRONMENT_METHOD, |_call: RemoteCall| {
        Box::pin(async { environment() })
    });
    registry.register(NAMESPACE, SHELLS_METHOD, |_call: RemoteCall| {
        Box::pin(async { Ok(shells()) })
    });

    registry.declare(NAMESPACE, ENVIRONMENT_METHOD, declaration());
    registry.declare(NAMESPACE, SHELLS_METHOD, declaration());

    // Declared, not omitted: a client is told "this build cannot" rather than "no such method", and
    // the two call for different handling on its side.
    registry.declare_unimplemented(NAMESPACE, &[WATCH_METHOD]);
}

/// What both registrations declare.
///
/// Built from [`DECLARED_ACCESS`] rather than spelling [`Declaration::read`] twice, so the constant
/// and what the registry was told cannot say different things.
fn declaration() -> Declaration {
    match DECLARED_ACCESS {
        Access::Write => Declaration::write(RESOURCE_KIND),
        Access::Destructive => Declaration::destructive(RESOURCE_KIND),
        Access::Read => Declaration::read(RESOURCE_KIND),
    }
}

/// Answers the defaults a client needs before it opens a pane.
///
/// The working directory is read from the process rather than from a request, because that is where
/// a `terminal.create` with no `cwd` actually starts: the launcher passes no `current_dir` to the
/// child, so the child inherits this process's directory. Reporting anything else — a workspace
/// root, a remembered path — would name a directory the next shell would not be in.
fn environment() -> Result<Value, RemoteError> {
    let cwd = std::env::current_dir().map_err(|error| {
        // Reachable on a host whose working directory was deleted underneath it. Reporting an empty
        // or invented path would send the client to a directory nobody can open, so the reason is
        // forwarded instead.
        RemoteError::Failed(format!(
            "cannot read the directory a shell would start in: {error}"
        ))
    })?;
    let headless = TerminalSize::headless();

    Ok(json!({
        // Lossy, because `cwd` goes back to `terminal.create` as a JSON string and a path that is
        // not UTF-8 cannot make that round trip. The lossy form is the closest thing a client can
        // send back, and it is what a shell's own `pwd` would print for the same directory on most
        // hosts.
        "cwd": cwd.to_string_lossy(),
        "defaultCols": headless.columns,
        "defaultRows": headless.rows,
        "maxCols": MAX_DIMENSION,
        "maxRows": MAX_DIMENSION,
        "maxTranscriptBytes": DEFAULT_TRANSCRIPT_BYTES,
    }))
}

/// Answers which programs on this host can be run as a shell.
///
/// The response wraps the array — `{"default": …, "shells": […]}` — rather than returning a bare
/// array as upstream does, because `terminal.list` already answers `{"terminals": […]}` in this
/// build and a client reading two listings should not have to remember which one is unwrapped.
///
/// The first entry is the program the launcher runs when a request names no shell, whenever that
/// program resolves. When it does not resolve it is absent from the array while `default` still
/// names it, and that difference is the honest signal that `terminal.create` without a `shell` will
/// fail on this host — as opposed to upstream, which refuses the whole call when its declared
/// default is missing.
fn shells() -> Value {
    let default = default_program();
    let discovered = discover_shells(&default, CANDIDATES);
    let entries: Vec<Value> = discovered
        .iter()
        .map(|shell| {
            json!({
                "path": shell.path.to_string_lossy(),
                "name": &shell.name,
                // Empty on purpose. Upstream's profiles carry launch arguments (`-i`,
                // `-NoLogo`); `terminal.create` here takes a program and nothing else, so a
                // non-empty list would advertise flags no request could send. A client that needs
                // them is better served by an empty, true array than by a promise the launcher
                // does not keep.
                "args": [],
            })
        })
        .collect();

    json!({ "default": default, "shells": entries })
}

/// One shell this host can run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalShell {
    /// The program to pass as `terminal.create`'s `shell`.
    pub path: PathBuf,
    /// Its display name: the file name, as the file itself spells it.
    pub name: String,
}

/// Finds the runnable shells among `preferred` and `candidates`, default first.
///
/// Deduplicated by executable *kind* — the file name lowercased with any `.exe` removed — because
/// the same shell is routinely reachable by two paths (`/bin/bash` and `/usr/bin/bash` on merged
/// `/usr` systems) and a picker showing it twice invites the user to choose between two identical
/// things. The earliest entry wins, so the default keeps its first position.
///
/// An unresolvable program is skipped rather than fatal. Upstream rejects when the *declared* default
/// cannot resolve, which is right for a server that must pick one shell; this function answers a
/// question ("what is available"), and answering it with the shells that do exist is more useful than
/// a failure that hides them. The caller can still see that the default is missing by comparing
/// `default` with the array.
#[must_use]
pub fn discover_shells(preferred: &str, candidates: &[&str]) -> Vec<TerminalShell> {
    let mut programs: Vec<&str> = Vec::with_capacity(candidates.len() + 1);
    programs.push(preferred);
    programs.extend(candidates.iter().copied());

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut found: Vec<TerminalShell> = Vec::new();
    for program in programs {
        let Some(path) = resolve(program) else {
            continue;
        };
        let name = file_name(&path);
        if seen.insert(kind(&name)) {
            found.push(TerminalShell { path, name });
        }
    }
    found
}

/// The program `terminal.create` runs when a request names no shell.
///
/// `$SHELL` first, because that is the shell the person running this deployment chose, then `sh`,
/// which POSIX guarantees. The same rule as `terminal_local`'s private `default_shell`, repeated
/// because that one is not exported; the two must stay in step, or `environment` would report a
/// default the launcher does not use.
fn default_program() -> String {
    std::env::var("SHELL")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "sh".to_owned())
}

/// Resolves one program name or path to a file that can be executed.
///
/// A program containing a separator is a path the deployment already chose and is used as given;
/// anything else is looked up on `PATH`, because that is what the process layer will do when it
/// spawns the program. Resolution here is stricter than "the file exists": a non-executable file
/// named `bash` on `PATH` would otherwise be advertised and then fail at `terminal.create`.
fn resolve(program: &str) -> Option<PathBuf> {
    if program.contains('/') || program.contains('\\') {
        return spellings(program)
            .into_iter()
            .map(PathBuf::from)
            .find(|path| executable(path));
    }
    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search)
        .flat_map(|directory| {
            spellings(program)
                .into_iter()
                .map(move |name| directory.join(name))
        })
        .find(|candidate| executable(candidate))
}

/// The file names a program could be stored under.
///
/// Only `.exe` is tried, on Windows and only when the name does not already carry it. A full
/// `PATHEXT` walk would be more faithful to what `cmd` does, and it would also be a lookup rule this
/// module would then have to keep in step with the process layer's; the one suffix covers the
/// candidates that matter (`powershell`, `cmd`) and is documented rather than assumed complete.
fn spellings(program: &str) -> Vec<String> {
    if cfg!(windows) && !program.to_ascii_lowercase().ends_with(".exe") {
        vec![program.to_owned(), format!("{program}.exe")]
    } else {
        vec![program.to_owned()]
    }
}

/// Whether `path` is a regular file this process could execute.
///
/// Executability is the POSIX permission bits. On a platform without them there is no portable bit
/// to test, so the file's existence is all that can be checked and a spawn failure is left to
/// `terminal.create`, which reports the operating system's own words.
fn executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The file name of `path`, or the whole path when it has none.
///
/// Total rather than fallible: a path with no final component (`/`) cannot be a shell, but this is
/// called on every candidate and returning an `Option` would push a branch nobody can act on into
/// the caller.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The executable kind of a shell name: lowercased, without a trailing `.exe`.
///
/// The comparison key for "the same shell reached by two paths". Case is folded because Windows and
/// some shared filesystems are case-insensitive, and a picker offering `Bash` next to `bash` would be
/// offering one shell twice.
fn kind(name: &str) -> String {
    let lowered = name.to_ascii_lowercase();
    match lowered.strip_suffix(".exe") {
        Some(stem) => stem.to_owned(),
        None => lowered,
    }
}
