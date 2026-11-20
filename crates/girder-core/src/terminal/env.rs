//! Which shells this host can offer, and the defaults a client needs before it draws a pane.
//!
//! `terminal.environment` and `terminal.shells` are answerable from facts a process already holds,
//! yet no module owned the question "what can be run on this host". This is that module, and it
//! owns nothing else: no session, no process, no transport. A caller reads a value and returns.
//!
//! # Why probing never launches a shell
//!
//! The obvious way to answer "does this shell work" is to run it. That is the wrong question twice
//! over. It is expensive — a picker that probes seven candidates pays seven spawns on a path a
//! client is waiting on. And it is unreliable: a shell started without a terminal may sit waiting
//! for input, so a probe either hangs the caller or needs a timeout that turns a slow host into a
//! host with no shells at all. What a launch would detect is a *broken installation*, and
//! [`girder_core::LocalLauncher`] reports that when the shell is actually
//! started. So this module checks two facts per candidate — the path is a regular file, and it
//! carries an executable bit — and stops there.
//!
//! The consequence is stated rather than hidden: an installed-but-broken shell is offered, and the
//! failure surfaces at `terminal.create`. That is why the flag on [`Shell`] is `is_default` and not
//! "verified working".
//!
//! # Why `/etc/shells` comes first, and why it is not enough
//!
//! `/etc/shells` is the host's own answer to this question, and it is the only place a shell
//! installed somewhere unusual appears. It is also advisory: a package manager removes a shell
//! without editing the file, and a minimal container image ships no file at all. So its entries
//! lead the list, and each one is still checked for existence and the executable bit — the file's
//! opinion is a claim, not a fact. When the file cannot be read, that is not an error: falling back
//! to the common paths answers a more useful question than failing the whole probe.
//!
//! The common paths are the supplement, not the source: `/bin/sh` is first among them because POSIX
//! guarantees it, and a host whose `/etc/shells` is empty or missing should still yield something a
//! picker can offer. `$SHELL` is inserted between the two, so the shell that is about to be marked
//! the default is present in the list it is the default of.
//!
//! # Why duplicates are collapsed by canonical path
//!
//! On a host with a merged `/usr`, `/bin` is a symlink to `/usr/bin`, so `/bin/bash` and
//! `/usr/bin/bash` are one executable reachable by two spellings — and `/etc/shells` routinely
//! lists both. A picker showing the same shell twice invites the user to choose between two
//! identical things. The comparison is therefore on the resolved path, which is also what makes
//! `$SHELL` recognised when it is spelled differently from the entry that reached the list first.
//!
//! # Why exactly one shell is marked the default
//!
//! A client renders "default" next to one row. Two marked rows would make that label meaningless,
//! and zero would leave a pane with a shell it cannot explain. [`detect_shells`] therefore marks
//! the shell `$SHELL` names when it resolves, and otherwise the first shell it found, so a
//! non-empty list always has exactly one default. [`Environment::default_shell`] is read back from
//! that same flag rather than recomputed, so the field and the flag cannot disagree.
//!
//! # Why colour is decided by `TERM` alone
//!
//! `TERM` names the terminal type the session presents, and `dumb` is by convention the terminal
//! with no capabilities: `tmux` under `dumb`, a pipe, and a daemon started by an init system all
//! set it (or set nothing), and a client that drew escape sequences anyway would fill a transcript
//! with control characters. An empty value is treated the same as an absent one, because that is
//! what a non-terminal has. `NO_COLOR` is deliberately not consulted: it records a person's
//! preference about decoration, not the renderer's ability, and conflating the two here would
//! answer a question this module was not asked.
//!
//! # Why the platform is a compile-time constant
//!
//! [`Environment::platform`] is `std::env::consts::OS`, the target this binary was built for, not a
//! runtime probe. That is the truth a client needs: the shells listed beside it were found by this
//! binary's own rules, and a runtime guess would describe a different machine — the case that
//! matters is a host that is not the one the shell list came from.

use std::path::{Path, PathBuf};

/// The file a Unix host registers its login shells in.
///
/// Read on every platform rather than behind a `cfg`: on a host without it the read fails and the
/// list simply has no entries, which is the same state as an empty one.
const SHELLS_FILE: &str = "/etc/shells";

/// Paths checked after `/etc/shells`, on a Unix host.
///
/// `/bin/sh` leads because POSIX guarantees it, and a picker should open something even on a host
/// whose `/etc/shells` is empty. The rest cover the package-manager and Homebrew prefixes that the
/// file sometimes omits; a path that does not exist costs one failed `stat`.
#[cfg(unix)]
const COMMON_SHELLS: &[&str] = &[
    "/bin/sh",
    "/bin/bash",
    "/bin/zsh",
    "/bin/dash",
    "/bin/ksh",
    "/bin/tcsh",
    "/bin/csh",
    "/usr/bin/fish",
    "/usr/local/bin/fish",
    "/usr/local/bin/bash",
    "/usr/local/bin/zsh",
    "/opt/homebrew/bin/bash",
    "/opt/homebrew/bin/zsh",
    "/opt/homebrew/bin/fish",
];

/// Programs checked after `/etc/shells`, on a Windows host.
///
/// Bare names rather than absolute paths, because the PowerShell install directory is versioned and
/// a literal path would go stale; [`resolve`] searches `PATH` for those. `cmd.exe` is last because
/// it is the least capable of the three, not because it is less likely to exist.
#[cfg(windows)]
const COMMON_SHELLS: &[&str] = &["pwsh.exe", "powershell.exe", "bash.exe", "cmd.exe"];

/// Programs checked after `/etc/shells`, on a target that is neither Unix nor Windows.
///
/// Compiled only for a target this crate does not currently build for; kept so the module states a
/// fallback instead of failing to compile there.
#[cfg(not(any(unix, windows)))]
const COMMON_SHELLS: &[&str] = &["sh"];

/// One shell that can be offered to a client.
///
/// The fields are plain owned values rather than borrowed paths because the value crosses into JSON
/// and, from there, to a client: nothing outlives that trip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shell {
    /// The path to launch, for example `/bin/bash`.
    ///
    /// This is the string a client sends back as `terminal.create`'s `shell`, so it is the path
    /// that was checked, not a canonicalised one. A canonical path would turn `/bin/bash` into
    /// `/usr/bin/bash` on a merged-`/usr` host — still correct, but not what the user's
    /// `/etc/shells` says.
    pub path: String,
    /// The display name: the path's final component, for example `bash`.
    ///
    /// The file name rather than a friendly label ("Bourne Again Shell") because a shell this
    /// module has never heard of — everything `/etc/shells` lists that is not in the common list —
    /// has no label to show, and inventing one for some entries and not others would make the
    /// picker inconsistent. The name a program chose for itself is always available and always
    /// true.
    pub name: String,
    /// Whether this is the shell a launch that names none would use.
    ///
    /// At most one shell in a list carries this flag; see the module docs for how the one is
    /// chosen.
    pub is_default: bool,
}

/// What a client needs in order to render a terminal.
///
/// Complete as a value: every field is decided here, so a client renders from one response instead
/// of from several queries that could disagree with each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    /// The [`Shell::path`] of the shell flagged as default, or `None` when nothing was found.
    ///
    /// `None` is not a failure: it says this host offers no shell this module could verify, and a
    /// client that shows it must fall back to asking the user for a path. An invented fallback here
    /// would be a path that `terminal.create` is about to reject.
    pub default_shell: Option<String>,
    /// Every shell this host can offer, in the order it should be presented.
    ///
    /// In `/etc/shells` order, then `$SHELL`, then the common paths — which is the host's own
    /// preference first and this module's guesses last. The default is flagged rather than moved to
    /// the front, so a client that sorts the list itself does not lose the flag.
    pub shells: Vec<Shell>,
    /// The target this binary was built for, for example `linux`.
    ///
    /// One of the `std::env::consts::OS` values. A client uses it to choose key bindings and the
    /// shape of a path, so a value it does not recognise must degrade gracefully rather than crash.
    pub platform: String,
    /// Whether the session's terminal can show colour.
    ///
    /// Decided from `TERM`; see the module docs for why that variable alone, and why an empty or
    /// absent value counts as "no".
    pub supports_color: bool,
}

/// Probes the shells this host can offer.
///
/// **Checks presence and the executable bit only; nothing is started.** Starting each candidate is
/// too expensive for a call a client makes before it draws a pane, and some shells block waiting
/// for input when they are not attached to a terminal, so a launch-based probe would hang instead
/// of answering. See the module docs for the full reasoning.
///
/// The result is ordered by [`Environment::shells`] and never contains the same executable twice.
/// An empty result is possible — a host with no readable `/etc/shells` and none of the common paths
/// — and it means "offer the user a text field", not "this call failed".
#[must_use]
pub fn detect_shells() -> Vec<Shell> {
    shells_from(Path::new(SHELLS_FILE), process_shell().as_deref())
}

/// Probes the environment a client renders a terminal in.
///
/// Composed from [`detect_shells`] plus two process facts — the `TERM` variable and the build
/// target — so the shell list is discovered once and the default is read back from the flag rather
/// than recomputed. Calling this twice on an unchanged host returns equal values.
#[must_use]
pub fn detect() -> Environment {
    let shells = detect_shells();
    // Read back from the flag instead of repeating the `$SHELL`-or-first rule: a second copy of the
    // rule is a second thing that can drift from the one the caller renders.
    let default_shell = shells
        .iter()
        .find(|shell| shell.is_default)
        .map(|shell| shell.path.clone());

    Environment {
        default_shell,
        shells,
        platform: std::env::consts::OS.to_owned(),
        supports_color: supports_color(std::env::var("TERM").ok().as_deref()),
    }
}

/// Discovers the shells a `/etc/shells`-format file and the common paths yield.
///
/// The seam [`detect_shells`] is built on: the file is a parameter so the parsing, the
/// deduplication, and the "only offer what exists" rule can be tested against a fixture instead of
/// the host's real file, and so a host can point the probe at its own list. `preferred` is `$SHELL`
/// in production; it is appended after the file's entries and before the common paths, and it is
/// what gets flagged as the default when it resolves.
///
/// Every program — from the file, from `preferred`, from the common list — goes through
/// [`resolve`], so a listed path that has been uninstalled is skipped rather than offered.
#[must_use]
pub(crate) fn shells_from(listing: &Path, preferred: Option<&str>) -> Vec<Shell> {
    let mut programs = listed_programs(listing);
    if let Some(preferred) = preferred {
        programs.push(preferred.to_owned());
    }
    programs.extend(COMMON_SHELLS.iter().map(|program| (*program).to_owned()));

    // Resolved once and compared against every entry, so `$SHELL` is recognised even when the file
    // reached the same executable through a different spelling. Bound to a variable first so the
    // resolved path outlives the borrow taken from it.
    let spelled = preferred.and_then(resolve);
    let wanted = spelled.as_deref().map(identity_of);

    let mut identities: Vec<PathBuf> = Vec::new();
    let mut found: Vec<Shell> = Vec::new();
    for program in &programs {
        let Some(path) = resolve(program) else {
            continue;
        };
        let identity = identity_of(&path);
        if identities.contains(&identity) {
            continue;
        }
        found.push(Shell {
            name: file_name(&path),
            is_default: false,
            path: path.to_string_lossy().into_owned(),
        });
        identities.push(identity);
    }

    // The named shell when it resolved, otherwise the first one found. `.or_else` on the index
    // keeps an empty list without a default instead of marking a row that does not exist.
    let chosen = wanted
        .as_ref()
        .and_then(|wanted| identities.iter().position(|identity| identity == wanted))
        .or_else(|| (!found.is_empty()).then_some(0));
    if let Some(shell) = chosen.and_then(|index| found.get_mut(index)) {
        shell.is_default = true;
    }
    found
}

/// Whether `term` names a terminal that can show colour.
///
/// Total over the cases a process can be in: `None` and an empty or whitespace-only value mean
/// there is no terminal to ask, and `dumb` — case-insensitively, since the value is a name rather
/// than a token — is the terminal that explicitly has no capabilities. Everything else is taken at
/// its word, including a name this module has never seen: a terminal type it cannot check is more
/// likely to be a capable one than an incapable one, and refusing colour on an unknown name would
/// break every emulator released after this list was written.
#[must_use]
pub(crate) fn supports_color(term: Option<&str>) -> bool {
    term.is_some_and(|term| {
        let term = term.trim();
        !term.is_empty() && !term.eq_ignore_ascii_case("dumb")
    })
}

/// The paths a `/etc/shells`-format file lists, in file order, without its comments.
///
/// A read failure is an empty list rather than an error: the caller's job is to answer "what is
/// available", and a missing or unreadable file is information about the file, not a reason to
/// withhold the shells that can be found another way.
///
/// Only whole-line comments are stripped. `#` is a legal character in a file name, so treating it
/// as the start of a trailing comment would silently truncate a path that happens to contain one.
fn listed_programs(listing: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(listing) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned)
        .collect()
}

/// The shell the process was started with, if it named one.
///
/// `$SHELL` is the shell the person running the harness chose, so it is the right answer for
/// "default" on an interactive host, and it agrees with
/// [`girder_core::LocalLauncher`], which captures the same variable. A value
/// that is empty or only whitespace names nothing and is ignored rather than probed.
fn process_shell() -> Option<String> {
    std::env::var("SHELL")
        .ok()
        .map(|shell| shell.trim().to_owned())
        .filter(|shell| !shell.is_empty())
}

/// Resolves one program name or path to a file this process can execute.
///
/// A program containing a separator is a path the caller already chose — the `/etc/shells` case —
/// and is used as written, so the path a client is offered is the one the host's file lists.
/// Anything else is looked up on `PATH`, which is what the process layer will do when it spawns the
/// program, so a shell that is not on `PATH` is not offered under a name that would fail to spawn.
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

/// The file names a bare program could be stored under.
///
/// Windows resolves `pwsh` to `pwsh.exe`, so a `PATH` search for the bare name would find nothing
/// and the shell would be missing from a picker that should show it. The suffix is tried only there
/// and only when the name does not already carry it; a full `PATHEXT` walk would be more faithful
/// and would also be a lookup rule to keep in step with the process layer's, so it is documented as
/// not attempted rather than assumed complete.
fn spellings(program: &str) -> Vec<String> {
    if cfg!(windows) && !program.to_ascii_lowercase().ends_with(".exe") {
        vec![program.to_owned(), format!("{program}.exe")]
    } else {
        vec![program.to_owned()]
    }
}

/// Whether `path` is a regular file this process could execute.
///
/// `metadata` follows symlinks, which is what is wanted: a shell is routinely a link (`/bin/sh` to
/// `dash`, a `python3`-style versioned target), and refusing one because the link itself carries no
/// permission bits would hide the shell it points at.
///
/// On a platform with POSIX permission bits, a file without any execute bit is refused — that is
/// the same failure a spawn would produce, caught at the probe instead. On a platform without them
/// there is no portable bit to test, so existence is all that can be checked and the spawn reports
/// the rest.
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

/// The path an executable is reachable by, for comparison.
///
/// Canonical when the filesystem can answer, and the path as written when it cannot — a broken link
/// or a race with an uninstall. Falling back keeps two spellings distinguishable rather than
/// collapsing them into one error, which is the safer direction for a deduplication key.
fn identity_of(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The final component of `path`, or the whole path when it has none.
///
/// Total rather than fallible: `/` cannot be a shell, and returning an `Option` here would push a
/// branch nobody can act on into every caller.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}
