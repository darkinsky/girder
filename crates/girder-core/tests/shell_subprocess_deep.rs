//! Tests for terminal lifecycle semantics and the privileged-runner protocol.
//!
//! Corresponds to upstream `packages/subprocess/subprocess-local/tests/{shell-activity,
//! shell-activity-files,terminal,control}.spec.ts` — the parts of those files that exercise
//! decisions rather than a PTY. Every property pinned here is one upstream pins, plus the cases
//! upstream's looser JavaScript coercions accept and a Rust port must refuse.
//!
//! # Why the process table is scripted
//!
//! A real table cannot be made to recycle a pid, cannot hold a `Z` row on demand, and differs
//! between machines. [`Table`] is the injected observation boundary the inspector already has, so
//! the tracker, the fence, and the teardown ladder are driven deterministically. The tests that do
//! need a real world — the shell startup files, the launch locator — use a real directory, because
//! those are the parts whose correctness *is* about a filesystem path.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source files rather than
//! copies. Inside a `#[path]` module `crate::` names *this* test binary rather than `girder-core`,
//! so `subprocess_deep_gap`'s own `crate::shell::process_identity::…` imports would resolve to the sibling
//! `#[path]` module declared below. They are declared in that order for exactly that reason.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

// The core parser module is compiled whole and this binary drives only the part it needs; the rest
// of its surface is the sibling module's API, and a test binary is not a library.
#[allow(
    dead_code,
    reason = "the compiled-by-path parser exposes more than this binary exercises"
)]
#[path = "../src/shell/process_identity.rs"]
pub mod subprocess_gap;

// The included source names this module `crate::shell::process_identity`, which is where it lives
// in the library. The test binary has to offer the same path.
mod shell {
    pub use super::subprocess_gap as process_identity;
}

// The module under test is likewise a library surface compiled into a test binary: a public
// accessor no case here happens to call is still part of the seam the crate will export.
#[allow(
    dead_code,
    reason = "a public seam compiled into a test binary is not dead code"
)]
#[path = "../src/shell/subprocess_deep.rs"]
mod subprocess_deep_gap;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;
use subprocess_deep_gap::{
    ActivityFacts, ActivityState, ForegroundRefusal, LaunchFiles, ProtocolError,
    SerializedRunnerError, SessionTracker, ShellActivityRequest, ShellKind, Teardown,
    TeardownPhase, WindowsRunnerResult, decide_activity, foreground_target,
    is_windows_terminate_request, linux_launch_files_from_locator, parse_linux_launch_request,
    parse_state_record, parse_windows_runner_result, parse_windows_start_request,
    prepare_shell_activity, quote, survivors, union_members,
};
use subprocess_gap::{ProcessIdentity, Signal, Snapshot};

// ---- the scripted process table ----------------------------------------------------------------

/// A scripted `/proc`, with every delivery recorded.
///
/// Signals are recorded rather than delivered: a test that signalled a real pid it did not spawn
/// would be testing the kernel, and on a recycled id it would be testing its own luck.
#[derive(Default)]
struct Table {
    files: Mutex<BTreeMap<String, String>>,
    entries: Mutex<Option<Vec<u32>>>,
}

impl Table {
    fn new() -> Self {
        Self::default()
    }

    fn with(mut self, pid: u32, stat: String) -> Self {
        self.files
            .get_mut()
            .expect("fresh lock")
            .insert(format!("/proc/{pid}/stat"), stat);
        let entries = self.entries.get_mut().expect("fresh lock");
        let mut pids = entries.clone().unwrap_or_default();
        pids.push(pid);
        *entries = Some(pids);
        self
    }

    fn observe(&self) -> Snapshot {
        Snapshot::observe(self).expect("the scripted table is enumerable")
    }
}

impl subprocess_gap::Probe for Table {
    fn read(&self, path: &str) -> Option<String> {
        self.files.lock().expect("fresh lock").get(path).cloned()
    }

    fn entries(&self, path: &str) -> Option<Vec<u32>> {
        (path == "/proc")
            .then(|| self.entries.lock().expect("fresh lock").clone())
            .flatten()
    }

    fn signal(&self, _pid: i32, _signal: Signal) {
        unreachable!("this module never signals; it only decides what to signal")
    }
}

/// One `/proc/<pid>/stat` line in the kernel's field order.
///
/// After the closing `)` the fields are `state`(0) `ppid`(1) `pgrp`(2) `session`(3) `tty_nr`(4)
/// `tpgid`(5), then thirteen more before `starttime` at index 19 — which is why the padding below is
/// thirteen zeros.
fn stat_line(
    pid: u32,
    state: char,
    parent: u32,
    group: u32,
    session: u32,
    foreground: i32,
    started: &str,
) -> String {
    format!(
        "{pid} (cmd) {state} {parent} {group} {session} 0 {foreground} 0 0 0 0 0 0 0 0 0 0 0 0 0 {started} 0"
    )
}

/// The common row: no controlling-terminal foreground group.
fn row(pid: u32, state: char, parent: u32, group: u32, session: u32, started: &str) -> String {
    stat_line(pid, state, parent, group, session, -1, started)
}

fn identity(pid: u32, started: &str) -> ProcessIdentity {
    ProcessIdentity::new(pid, started)
}

// ---- shell state records ----------------------------------------------------------------------

/// Upstream's `/^(\d+):(\d+):(idle|busy)\n?$/u`, field by field.
#[test]
fn a_state_record_yields_the_shell_pid_the_sequence_and_the_state() {
    let record = parse_state_record("4242:7:idle\n").expect("a well-formed record");
    assert_eq!(record.pid, "4242");
    assert_eq!(record.sequence, "7");
    assert_eq!(record.state, ActivityState::Idle);

    let busy = parse_state_record("4242:7:busy").expect("the newline is optional");
    assert_eq!(busy.state, ActivityState::Busy);
}

/// The third field is `unknown` whenever a trap is installed, and that is *not* an idle claim.
///
/// Upstream's regex simply fails to match, and `inspect` turns that into `unknown`. A port that
/// defaulted an unparseable field to `idle` would reclaim a pane with a pending trap.
#[test]
fn an_unknown_third_field_is_refused_rather_than_defaulted() {
    assert!(parse_state_record("4242:7:unknown").is_none());
    assert!(parse_state_record("4242:7:").is_none());
    assert!(parse_state_record("4242:7:IDLE").is_none());
}

/// Every way the record can be malformed.
#[test]
fn malformed_state_records_are_refused() {
    for text in [
        "",
        ":",
        "4242",
        "4242:7",
        "4242:7:idle:extra",
        "abc:7:idle",
        "4242::idle",
        "-1:7:idle",
        "4242:7:idle\n\n",
        "4242\n:7:idle",
    ] {
        assert!(
            parse_state_record(text).is_none(),
            "{text:?} must not parse as a state record"
        );
    }
}

/// `\d` in JavaScript is ASCII-only, so a non-ASCII digit is refused here too.
///
/// Rust's own integer parser accepts a leading `+`, which `\d+` does not; the text check is what
/// keeps the two grammars identical.
#[test]
fn the_pid_and_sequence_are_ascii_digits_and_nothing_else() {
    assert!(parse_state_record("+1:0:busy").is_none());
    assert!(parse_state_record("١:0:busy").is_none());
    assert!(parse_state_record("0:0:busy").is_some());
}

// ---- shell activity ---------------------------------------------------------------------------

fn request<'a>(
    argv: &'a [String],
    env: &'a BTreeMap<String, String>,
    requested: bool,
) -> ShellActivityRequest<'a> {
    ShellActivityRequest {
        argv,
        env,
        requested,
        posix: true,
        directory: PathBuf::from("/tmp/dsh-shell-example"),
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

/// Every refusal upstream performs, each for a stated reason.
#[test]
fn a_launch_is_only_integrated_when_it_is_plain_interactive_bash_or_zsh() {
    let env = BTreeMap::new();
    let bash = argv(&["bash", "-i"]);

    assert!(prepare_shell_activity(&request(&bash, &env, true)).is_some());

    // Not requested: an unrequested hook is a surprise in a user's shell.
    assert!(prepare_shell_activity(&request(&bash, &env, false)).is_none());

    // An execution world with no startup-file mechanism.
    let mut windows = request(&bash, &env, true);
    windows.posix = false;
    assert!(prepare_shell_activity(&windows).is_none());

    // Arguments the integration did not write.
    let interactive_login = argv(&["bash", "-l", "-i"]);
    assert!(prepare_shell_activity(&request(&interactive_login, &env, true)).is_none());
    let login = argv(&["bash", "-li"]);
    assert!(prepare_shell_activity(&request(&login, &env, true)).is_none());
    let wrapped = argv(&["env", "bash", "-i"]);
    assert!(prepare_shell_activity(&request(&wrapped, &env, true)).is_none());

    // A shell with no hook.
    let fish = argv(&["fish", "-i"]);
    assert!(prepare_shell_activity(&request(&fish, &env, true)).is_none());
    // A basename that merely ends in the name.
    let mybash = argv(&["/usr/bin/mybash", "-i"]);
    assert!(prepare_shell_activity(&request(&mybash, &env, true)).is_none());
}

/// The shell is recognised by basename, so an absolute path is integrated exactly like `bash`.
#[test]
fn the_shell_is_recognised_by_its_basename() {
    assert_eq!(ShellKind::of_program("/bin/bash"), Some(ShellKind::Bash));
    assert_eq!(
        ShellKind::of_program("/usr/local/bin/zsh"),
        Some(ShellKind::Zsh)
    );
    assert_eq!(ShellKind::of_program("bash"), Some(ShellKind::Bash));
    assert_eq!(ShellKind::of_program("mybash"), None);
    assert_eq!(ShellKind::of_program(""), None);
}

/// Bash is redirected with `--rcfile`, and the private file sources the real `~/.bashrc` first.
#[test]
fn bash_is_launched_through_a_private_rcfile_that_keeps_the_users_environment() {
    let env = BTreeMap::new();
    let bash = argv(&["/bin/bash", "-i"]);
    let activity =
        prepare_shell_activity(&request(&bash, &env, true)).expect("a plain interactive bash");

    assert_eq!(activity.kind(), ShellKind::Bash);
    assert_eq!(activity.directory(), Path::new("/tmp/dsh-shell-example"));
    assert_eq!(
        activity.argv(),
        [
            "/bin/bash".to_owned(),
            "--rcfile".to_owned(),
            "/tmp/dsh-shell-example/bashrc".to_owned(),
            "-i".to_owned()
        ]
    );
    assert_eq!(
        activity.startup_file(),
        Path::new("/tmp/dsh-shell-example/bashrc")
    );
    // Zsh's redirect is an environment variable; bash's is an argument, so the environment is
    // passed through untouched.
    assert!(activity.env().is_empty());
    let content = activity.startup_content();
    assert!(
        content.contains("builtin source ~/.bashrc"),
        "the user's own rcfile must still be read"
    );
    assert!(content.contains("PROMPT_COMMAND"), "the idle hook");
    assert!(content.contains("PS0+="), "the busy hook that PS0 provides");
    assert!(content.contains("BASH_VERSINFO[0] > 4"), "the 4.4 floor");
}

/// The private state and guard file paths are quoted, so a directory with a space works.
#[test]
fn the_bash_integration_quotes_every_substituted_path() {
    let env = BTreeMap::new();
    let bash = argv(&["bash", "-i"]);
    let mut request = request(&bash, &env, true);
    request.directory = PathBuf::from("/tmp/a directory/it's here");
    let activity = prepare_shell_activity(&request).expect("a plain interactive bash");
    let content = activity.startup_content();
    assert!(
        content.contains(r"'/tmp/a directory/it'\''s here/state'"),
        "the state path must be single-quoted with embedded quotes escaped; got:\n{content}"
    );
}

/// Zsh is redirected with `ZDOTDIR`, keeps the inherited one, and restores it in the file.
#[test]
fn zsh_is_redirected_through_zdotdir_and_restores_the_inherited_value() {
    let mut env = BTreeMap::new();
    env.insert("ZDOTDIR".to_owned(), "/home/user/.config/zsh".to_owned());
    let zsh = argv(&["/usr/bin/zsh", "-i"]);
    let activity =
        prepare_shell_activity(&request(&zsh, &env, true)).expect("a plain interactive zsh");

    assert_eq!(activity.kind(), ShellKind::Zsh);
    // Zsh has no `--rcfile`, so the argv is exactly what was asked for.
    assert_eq!(
        activity.argv(),
        ["/usr/bin/zsh".to_owned(), "-i".to_owned()]
    );
    assert_eq!(
        activity.env().get("ZDOTDIR").map(String::as_str),
        Some("/tmp/dsh-shell-example")
    );
    assert_eq!(
        activity.startup_file(),
        Path::new("/tmp/dsh-shell-example/.zshenv")
    );
    let content = activity.startup_content();
    assert!(
        content.starts_with("ZDOTDIR='/home/user/.config/zsh'"),
        "the inherited ZDOTDIR must be restorable; got:\n{content}"
    );
    assert!(
        content.contains("builtin source"),
        "the real .zshenv is still read"
    );
    assert!(content.contains("add-zle-hook-widget"), "the prompt half");
    assert!(content.contains("add-zsh-hook preexec"), "the busy half");
    assert!(
        content.contains("ZSH_SUBSHELL == 0"),
        "a subshell writes nothing"
    );
}

/// With no inherited `ZDOTDIR` the private `.zshenv` must `unset` it, not set it to empty.
#[test]
fn zsh_without_an_inherited_zdotdir_unsets_it_before_sourcing() {
    let env = BTreeMap::new();
    let zsh = argv(&["zsh", "-i"]);
    let activity =
        prepare_shell_activity(&request(&zsh, &env, true)).expect("a plain interactive zsh");
    assert!(activity.startup_content().starts_with("unset ZDOTDIR\n"));
}

/// `quote` is the POSIX single-quote escape, and it survives a pathological value.
#[test]
fn quote_produces_a_posix_single_quoted_word() {
    assert_eq!(quote("plain"), "'plain'");
    assert_eq!(quote("it's"), r"'it'\''s'");
    assert_eq!(quote(""), "''");
    // The only character a single-quoted word cannot contain is the quote itself, and the escape
    // above is what reintroduces it.
    let hostile = "a'; rm -rf /; echo '";
    assert_eq!(quote(hostile), r"'a'\''; rm -rf /; echo '\'''");
}

/// The revision protocol: it bumps on a *changed* record and on invalidation, and nowhere else.
#[test]
fn the_activity_revision_moves_only_on_a_changed_record_or_an_invalidation() {
    let env = BTreeMap::new();
    let bash = argv(&["bash", "-i"]);
    let mut activity = prepare_shell_activity(&request(&bash, &env, true)).expect("bash");

    let first = activity.inspect(900, "900:1:idle\n");
    assert_eq!(first.state, ActivityState::Idle);
    let revision = first.revision;

    // The same record again is the same fact: asking twice must not manufacture a change.
    let repeated = activity.inspect(900, "900:1:idle\n");
    assert_eq!(repeated.revision, revision);

    // A new prompt is a new fact.
    let next = activity.inspect(900, "900:2:idle\n");
    assert_eq!(next.state, ActivityState::Idle);
    assert!(next.revision > revision);

    // Invalidation moves it whether or not the record changed.
    activity.invalidate("900:2:idle\n");
    let invalidated = activity.inspect(900, "900:2:idle\n");
    assert_eq!(invalidated.state, ActivityState::Unknown);
    assert!(invalidated.revision > next.revision);
}

/// A record written by a subshell is not the top-level shell's prompt.
///
/// The private directory is per-launch, but a `$(…)` in the user's prompt runs a subshell whose
/// `BASHPID` differs; its prompt is not evidence about the shell the caller is watching.
#[test]
fn a_record_from_a_different_pid_is_unknown() {
    let env = BTreeMap::new();
    let bash = argv(&["bash", "-i"]);
    let mut activity = prepare_shell_activity(&request(&bash, &env, true)).expect("bash");

    let observation = activity.inspect(900, "901:1:idle\n");
    assert_eq!(observation.state, ActivityState::Unknown);
    assert_eq!(activity.state(), ActivityState::Unknown);
}

/// Input delivered after a record makes that record stale until a new one arrives.
///
/// This is the window in which a caller must not reclaim the pane: the dispatch may have started a
/// command that has not yet reached `PS0`.
#[test]
fn input_invalidates_the_previous_prompt_evidence_until_a_new_one_arrives() {
    let env = BTreeMap::new();
    let bash = argv(&["bash", "-i"]);
    let mut activity = prepare_shell_activity(&request(&bash, &env, true)).expect("bash");

    assert_eq!(
        activity.inspect(900, "900:3:idle\n").state,
        ActivityState::Idle
    );
    activity.invalidate("900:3:idle\n");
    assert_eq!(
        activity.inspect(900, "900:3:idle\n").state,
        ActivityState::Unknown
    );
    // The shell ran something and reached its next prompt.
    assert_eq!(
        activity.inspect(900, "900:4:idle\n").state,
        ActivityState::Idle
    );
}

/// `unknown` on the record is `unknown` here, never `idle`.
#[test]
fn an_unknown_record_state_is_never_downgraded_to_idle() {
    let env = BTreeMap::new();
    let bash = argv(&["bash", "-i"]);
    let mut activity = prepare_shell_activity(&request(&bash, &env, true)).expect("bash");
    assert_eq!(
        activity.inspect(900, "900:1:unknown\n").state,
        ActivityState::Unknown
    );
    // An unreadable file is the same answer as an unparseable one.
    assert_eq!(activity.inspect(900, "").state, ActivityState::Unknown);
}

// ---- session tracking -------------------------------------------------------------------------

/// Adoption covers both the root's tree and its session, because job control moves jobs out of the
/// root's group.
#[test]
fn a_session_adopts_the_tree_and_the_session_but_never_the_root() {
    let table = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        // A job in its own process group, still in the shell's session.
        .with(901, row(901, 'S', 900, 901, 900, "200"))
        // A grandchild in the job's group.
        .with(902, row(902, 'R', 901, 901, 900, "300"))
        // Another session entirely.
        .with(950, row(950, 'S', 1, 950, 950, "400"));
    let observed = table.observe();

    let mut tracker = SessionTracker::new(900, SessionTracker::capture_root(900, &observed));
    assert_eq!(tracker.root_pid(), 900);
    let members = tracker.descendants(&observed);

    let pids: Vec<u32> = members.iter().map(|member| member.pid).collect();
    // Children before parents: a teardown in this order never kills a parent before the child that
    // would be re-parented to init by its death.
    assert_eq!(
        pids,
        vec![902, 901],
        "adopted, children first, root excluded"
    );
    assert_eq!(tracker.tracked().len(), 2);
}

/// Once the root pid no longer carries the spawned shell's start identity, nothing new is adopted.
///
/// This is the fence that stops a recycled pid from donating a stranger's children to this session.
#[test]
fn a_recycled_root_pid_stops_adoption() {
    let spawned = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        .with(901, row(901, 'S', 900, 901, 900, "200"));
    let observed = spawned.observe();
    let mut tracker = SessionTracker::new(900, SessionTracker::capture_root(900, &observed));
    assert_eq!(tracker.descendants(&observed).len(), 1);

    // The shell exited and an unrelated process took pid 900, with its own children.
    let recycled = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "999"))
        .with(910, row(910, 'S', 900, 910, 900, "1000"))
        .with(901, row(901, 'S', 1, 901, 900, "200"));
    let observed = recycled.observe();

    assert!(!tracker.root_verified(&observed));
    // The stranger's child is not adopted; the already-tracked member is still reported while it
    // is alive, which is what lets its own identity fence protect it.
    let members = tracker.descendants(&observed);
    assert_eq!(members, vec![identity(901, "200")]);
}

/// A tracker captured without an identity can never adopt anything.
///
/// The capture can fail when the shell exits between the spawn and the snapshot, and guessing is
/// worse than doing nothing: this build would otherwise signal a pid it cannot prove it started.
#[test]
fn a_tracker_without_a_captured_identity_adopts_nothing() {
    let table = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        .with(901, row(901, 'S', 900, 901, 900, "200"));
    let observed = table.observe();
    let mut tracker = SessionTracker::new(900, None);
    assert!(tracker.descendants(&observed).is_empty());
    assert!(tracker.tracked().is_empty());
}

/// A zombie is not a survivor: it holds no descriptors and cannot be signalled.
#[test]
fn a_zombie_member_is_filtered_out_of_the_survivor_list() {
    let table = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        .with(901, row(901, 'Z', 900, 901, 900, "200"));
    let observed = table.observe();
    let mut tracker = SessionTracker::new(900, SessionTracker::capture_root(900, &observed));
    assert!(tracker.descendants(&observed).is_empty());
}

/// Adoption is idempotent, and a member that disappears is dropped from the tracked set.
#[test]
fn tracking_keeps_a_stable_order_and_forgets_a_member_that_exits() {
    let table = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        .with(901, row(901, 'S', 900, 901, 900, "200"))
        .with(902, row(902, 'S', 900, 902, 900, "300"));
    let observed = table.observe();
    let mut tracker = SessionTracker::new(900, SessionTracker::capture_root(900, &observed));
    let first = tracker.descendants(&observed);
    // Asking again from the same observation must not duplicate anything or reorder it.
    assert_eq!(tracker.descendants(&observed), first);
    assert_eq!(first.len(), 2);

    let after_exit = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        .with(902, row(902, 'S', 900, 902, 900, "300"))
        .observe();
    assert_eq!(tracker.descendants(&after_exit), vec![identity(902, "300")]);
}

/// `union_members` deduplicates by the pid-and-start pair, first occurrence winning.
#[test]
fn union_members_keeps_the_first_occurrence_and_the_argument_order() {
    let first = vec![identity(1, "10"), identity(2, "20")];
    let second = vec![identity(2, "20"), identity(3, "30")];
    let union = union_members(&[first, second]);
    assert_eq!(
        union,
        vec![identity(1, "10"), identity(2, "20"), identity(3, "30")]
    );

    // The same pid with a different start identity is a different process and must not be merged.
    let recycled = vec![identity(1, "99")];
    assert_eq!(union_members(&[union, recycled]).len(), 4);
}

/// `survivors` filters against one observation, and reports an empty answer rather than assuming.
#[test]
fn survivors_reports_only_what_the_observation_showed_running() {
    let table = Table::new()
        .with(1, row(1, 'S', 1, 1, 1, "10"))
        .with(2, row(2, 'Z', 1, 1, 1, "20"));
    let observed = table.observe();
    let members = vec![identity(1, "10"), identity(2, "20"), identity(3, "30")];
    assert_eq!(survivors(&members, &observed), vec![identity(1, "10")]);
}

// ---- the teardown ladder ----------------------------------------------------------------------

/// `SIGTERM` to the adopted set, `SIGKILL` to what survived plus a fresh adoption, then verify.
#[test]
fn the_teardown_ladder_terminates_escalates_and_then_verifies() {
    let table = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        .with(901, row(901, 'R', 900, 901, 900, "200"));
    let observed = table.observe();
    let mut teardown = Teardown::new(
        900,
        SessionTracker::capture_root(900, &observed),
        Duration::from_millis(200),
    );

    let first = teardown.step(&observed);
    assert_eq!(first.phase, TeardownPhase::Terminate);
    assert_eq!(first.signal, Some(Signal::Terminate));
    assert_eq!(first.targets, vec![identity(901, "200")]);
    assert_eq!(first.grace, Duration::from_millis(200));

    // The job ignored TERM and started another child.
    let after_term = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        .with(901, row(901, 'R', 900, 901, 900, "200"))
        .with(903, row(903, 'R', 901, 901, 900, "400"))
        .observe();
    let second = teardown.step(&after_term);
    assert_eq!(second.phase, TeardownPhase::Kill);
    assert_eq!(second.signal, Some(Signal::Kill));
    // The round re-adopts rather than reusing the first round's list: the new child is included.
    assert_eq!(
        second.targets,
        vec![identity(901, "200"), identity(903, "400")]
    );
    assert_eq!(second.survivors, vec![identity(901, "200")]);
    assert_eq!(teardown.tracker().root_pid(), 900);

    // Everything is gone.
    let empty = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        .observe();
    let third = teardown.step(&empty);
    assert_eq!(third.phase, TeardownPhase::Verify);
    assert_eq!(third.signal, None, "verification signals nothing");
    assert!(third.targets.is_empty());
    assert!(third.survivors.is_empty());
    assert_eq!(teardown.phase(), TeardownPhase::Done);

    // A ladder that is over does nothing, however many times it is asked.
    let fourth = teardown.step(&empty);
    assert_eq!(fourth.phase, TeardownPhase::Done);
    assert!(fourth.targets.is_empty());
}

/// The verify round reports survivors rather than claiming success from having sent a signal.
#[test]
fn verification_reports_a_survivor_instead_of_claiming_success() {
    let table = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        .with(901, row(901, 'R', 900, 901, 900, "200"));
    let observed = table.observe();
    let mut teardown = Teardown::new(
        900,
        SessionTracker::capture_root(900, &observed),
        Duration::from_millis(50),
    );
    teardown.step(&observed);
    // Nothing died: the KILL round targets it again, and the verify round still sees it.
    teardown.step(&observed);
    let verified = teardown.step(&observed);
    assert_eq!(verified.phase, TeardownPhase::Verify);
    assert_eq!(verified.survivors, vec![identity(901, "200")]);
}

/// A teardown armed with no identity still terminates the root's own tree from the first round?
///
/// No: it adopts nothing, so it signals nothing. The state machine still advances, which is what
/// lets a caller pump it to `Done` without special-casing an unobservable session.
#[test]
fn a_teardown_without_a_captured_identity_signals_nothing() {
    let table = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        .with(901, row(901, 'S', 900, 901, 900, "200"));
    let observed = table.observe();
    let mut teardown = Teardown::new(900, None, Duration::from_millis(50));
    let first = teardown.step(&observed);
    assert_eq!(first.signal, Some(Signal::Terminate));
    assert!(first.targets.is_empty());
    let second = teardown.step(&observed);
    assert!(second.targets.is_empty());
    let third = teardown.step(&observed);
    assert!(third.survivors.is_empty());
}

// ---- foreground signalling --------------------------------------------------------------------

/// The refusal that makes a terminal survive a `SIGKILL` aimed at its prompt.
#[test]
fn a_foreground_sigkill_at_the_shell_itself_is_refused() {
    assert_eq!(
        foreground_target(900, Some(900), Signal::Kill),
        Err(ForegroundRefusal::KillShell)
    );
    // Every other signal is legitimate at the prompt: that is what `Ctrl-C` and `Ctrl-Z` are.
    assert_eq!(
        foreground_target(900, Some(900), Signal::Interrupt),
        Ok(900)
    );
    assert_eq!(foreground_target(900, Some(900), Signal::Stop), Ok(900));
    assert_eq!(
        foreground_target(900, Some(900), Signal::Terminate),
        Ok(900)
    );
}

/// A job in its own group is always signalable, even with `SIGKILL`.
#[test]
fn a_foreground_job_in_its_own_group_is_signalable_with_any_permitted_signal() {
    for signal in [
        Signal::Interrupt,
        Signal::Terminate,
        Signal::Kill,
        Signal::Stop,
        Signal::Hangup,
    ] {
        assert_eq!(foreground_target(900, Some(901), signal), Ok(901));
    }
}

/// An unresolved or nonsensical foreground group is refused rather than defaulted.
#[test]
fn an_unresolved_foreground_group_is_refused() {
    assert_eq!(
        foreground_target(900, None, Signal::Interrupt),
        Err(ForegroundRefusal::Unresolved)
    );
    // Group `0` is the caller's own group; signalling it would kill the harness.
    assert_eq!(
        foreground_target(900, Some(0), Signal::Interrupt),
        Err(ForegroundRefusal::Unresolved)
    );
    // `tpgid` is signed and a negative value means "no foreground group".
    assert_eq!(
        foreground_target(900, Some(u32::MAX), Signal::Interrupt),
        Ok(u32::MAX)
    );
}

// ---- the activity decision --------------------------------------------------------------------

/// A table with a live shell at its prompt plus whatever else the case needs.
struct World {
    table: Table,
    root_identity: ProcessIdentity,
}

impl World {
    fn new(root_started: &str) -> Self {
        Self {
            table: Table::new().with(900, row(900, 'S', 1, 900, 900, root_started)),
            root_identity: identity(900, root_started),
        }
    }

    fn with(mut self, pid: u32, stat: String) -> Self {
        self.table = std::mem::take(&mut self.table).with(pid, stat);
        self
    }

    fn facts<'a>(
        &'a self,
        observed: &'a Snapshot,
        foreground_group: Option<u32>,
        shell: ActivityState,
    ) -> ActivityFacts<'a> {
        ActivityFacts {
            quiescent: false,
            exited: false,
            managed_range_empty: false,
            managed: false,
            managed_task_count: None,
            linux: true,
            observed,
            root_pid: 900,
            root_identity: Some(&self.root_identity),
            descendant_count: 0,
            foreground_group,
            shell,
        }
    }
}

/// The trusted case: a complete table, the root carrying its captured identity, the prompt in front.
#[test]
fn a_verified_shell_at_its_prompt_with_positive_evidence_is_idle() {
    let world = World::new("100");
    let observed = world.table.observe();
    let facts = world.facts(&observed, Some(900), ActivityState::Idle);
    assert_eq!(decide_activity(&facts), ActivityState::Idle);

    // The same shape with a busy record is busy.
    let facts = world.facts(&observed, Some(900), ActivityState::Busy);
    assert_eq!(decide_activity(&facts), ActivityState::Busy);
}

/// A foreground group that is not the shell means a command is running, whatever the prompt says.
#[test]
fn a_foreground_group_other_than_the_shell_is_busy() {
    let world = World::new("100").with(901, row(901, 'R', 900, 901, 900, "200"));
    let observed = world.table.observe();

    // The process table shows the job, so no foreground group is even consulted.
    let mut facts = world.facts(&observed, Some(901), ActivityState::Idle);
    facts.descendant_count = 1;
    assert_eq!(decide_activity(&facts), ActivityState::Busy);

    // With no adopted descendant, the foreground group is the second witness to the same fact.
    let facts = world.facts(&observed, Some(901), ActivityState::Idle);
    assert_eq!(decide_activity(&facts), ActivityState::Busy);

    // A shell in front with a stale `idle` record and no descendants is the trusted idle case.
    let facts = world.facts(&observed, Some(900), ActivityState::Idle);
    assert_eq!(decide_activity(&facts), ActivityState::Idle);
}

/// No foreground group cannot authorize a reclaim: a terminal with no foreground group is exactly
/// the state a wedged one is in.
#[test]
fn a_missing_foreground_group_is_unknown() {
    let world = World::new("100");
    let observed = world.table.observe();
    let facts = world.facts(&observed, None, ActivityState::Idle);
    assert_eq!(decide_activity(&facts), ActivityState::Unknown);
}

/// Teardown outranks every other fact.
#[test]
fn a_quiescent_terminal_is_idle_without_consulting_anything_else() {
    let world = World::new("100");
    let observed = world.table.observe();
    let mut facts = world.facts(&observed, None, ActivityState::Unknown);
    facts.quiescent = true;
    assert_eq!(decide_activity(&facts), ActivityState::Idle);
}

/// Exit plus a proven-empty managed range is two positive facts, so it is idle.
#[test]
fn an_exited_terminal_with_an_empty_managed_range_is_idle() {
    let world = World::new("100");
    let observed = world.table.observe();
    let mut facts = world.facts(&observed, None, ActivityState::Unknown);
    facts.exited = true;
    facts.managed = true;
    facts.managed_range_empty = true;
    assert_eq!(decide_activity(&facts), ActivityState::Idle);

    // Exit alone is not enough: the range may still hold survivors.
    let facts = world.facts(&observed, None, ActivityState::Unknown);
    let mut facts = facts;
    facts.exited = true;
    facts.managed = true;
    assert_eq!(decide_activity(&facts), ActivityState::Unknown);
}

/// A shell that exited normally leaves a session behind, and the session is what answers.
#[test]
fn an_exited_unmanaged_shell_falls_back_to_its_session() {
    // The shell's row is gone; one job remains in its session.
    let table = Table::new().with(901, row(901, 'R', 1, 901, 900, "200"));
    let observed = table.observe();
    let root_identity = identity(900, "100");
    let facts = ActivityFacts {
        quiescent: false,
        exited: true,
        managed_range_empty: false,
        managed: false,
        managed_task_count: None,
        linux: true,
        observed: &observed,
        root_pid: 900,
        root_identity: Some(&root_identity),
        descendant_count: 0,
        foreground_group: None,
        shell: ActivityState::Unknown,
    };
    assert_eq!(decide_activity(&facts), ActivityState::Busy);

    // An empty session is idle.
    let empty = Table::new()
        .with(950, row(950, 'S', 1, 950, 950, "400"))
        .observe();
    let facts = ActivityFacts {
        observed: &empty,
        ..facts
    };
    assert_eq!(decide_activity(&facts), ActivityState::Idle);
}

/// The session fallback requires a *complete* table and Linux.
///
/// An incomplete table can only say "I did not see one", and reporting `idle` from that is the
/// mistake the completeness flag exists to prevent.
#[test]
fn the_session_fallback_needs_a_complete_linux_observation() {
    let table = Table::new().with(901, row(901, 'R', 1, 901, 900, "200"));
    let observed = table.observe();
    let root_identity = identity(900, "100");
    let base = ActivityFacts {
        quiescent: false,
        exited: true,
        managed_range_empty: false,
        managed: false,
        managed_task_count: None,
        linux: true,
        observed: &observed,
        root_pid: 900,
        root_identity: Some(&root_identity),
        descendant_count: 0,
        foreground_group: None,
        shell: ActivityState::Unknown,
    };
    assert_eq!(decide_activity(&base), ActivityState::Busy);

    let not_linux = ActivityFacts {
        linux: false,
        ..base.clone()
    };
    assert_eq!(decide_activity(&not_linux), ActivityState::Unknown);

    let managed = ActivityFacts {
        managed: true,
        ..base.clone()
    };
    assert_eq!(decide_activity(&managed), ActivityState::Unknown);
}

/// A recycled root pid makes the terminal's own evidence unusable.
#[test]
fn a_recycled_root_pid_makes_the_answer_unknown() {
    let world = World::new("100");
    // The same pid 900 is now a different process.
    let recycled = Table::new().with(900, row(900, 'S', 1, 900, 900, "999"));
    let observed = recycled.observe();
    let facts = world.facts(&observed, Some(900), ActivityState::Idle);
    assert_eq!(decide_activity(&facts), ActivityState::Unknown);
}

/// A managed range overrides the prompt, because it can hold work the process tree cannot show.
#[test]
fn a_managed_range_task_count_overrides_an_idle_prompt() {
    let world = World::new("100");
    let observed = world.table.observe();
    let base = world.facts(&observed, Some(900), ActivityState::Idle);

    let one = ActivityFacts {
        managed: true,
        managed_task_count: Some(1),
        ..base.clone()
    };
    assert_eq!(decide_activity(&one), ActivityState::Idle);

    let two = ActivityFacts {
        managed: true,
        managed_task_count: Some(2),
        ..base.clone()
    };
    assert_eq!(decide_activity(&two), ActivityState::Busy);

    // A count below one is not evidence that the range is empty.
    let zero = ActivityFacts {
        managed: true,
        managed_task_count: Some(0),
        ..base.clone()
    };
    assert_eq!(decide_activity(&zero), ActivityState::Unknown);

    // Nor is an unavailable count.
    let unavailable = ActivityFacts {
        managed: true,
        managed_task_count: None,
        ..base
    };
    assert_eq!(decide_activity(&unavailable), ActivityState::Unknown);
}

/// An incomplete observation cannot authorize the trusted branch.
#[test]
fn an_incomplete_process_table_is_unknown() {
    // A pid listed in `/proc` whose stat line is unreadable is the ordinary exit race.
    let table = Table::new()
        .with(900, row(900, 'S', 1, 900, 900, "100"))
        .with(901, String::new());
    let observed = table.observe();
    assert!(!observed.complete());

    let world = World::new("100");
    let facts = world.facts(&observed, Some(900), ActivityState::Idle);
    assert_eq!(decide_activity(&facts), ActivityState::Unknown);
}

// ---- the runner protocol ----------------------------------------------------------------------

/// The well-formed Linux request and each of its facts.
#[test]
fn a_complete_linux_launch_request_is_accepted() {
    let request = parse_linux_launch_request(&json!({
        "cwd": "/home/user/project",
        "env": {"PATH": "/usr/bin", "LANG": "C"},
        "pipeFds": [0, 1, 2],
        "control": "pipe",
    }))
    .expect("a complete request");

    assert_eq!(request.cwd, "/home/user/project");
    assert_eq!(
        request.env.get("PATH").map(String::as_str),
        Some("/usr/bin")
    );
    assert_eq!(request.env.len(), 2);
    assert_eq!(request.pipe_fds, vec![0, 1, 2]);
    assert!(request.control);
}

/// The optional members default rather than being required.
#[test]
fn the_optional_linux_request_members_default_to_absent() {
    let request = parse_linux_launch_request(&json!({"cwd": "/", "env": {}})).expect("minimal");
    assert!(request.env.is_empty());
    assert!(request.pipe_fds.is_empty());
    assert!(!request.control);
}

/// Every malformation upstream refuses, plus the coercions JavaScript would have accepted.
#[test]
fn malformed_linux_launch_requests_are_refused() {
    for value in [
        // Not an object.
        json!(null),
        json!([]),
        json!("cwd"),
        // Missing a required member.
        json!({"cwd": "/"}),
        json!({"env": {}}),
        // An extra member: written by something that does not know this protocol.
        json!({"cwd": "/", "env": {}, "extra": 1}),
        // A wrong-typed member.
        json!({"cwd": 1, "env": {}}),
        json!({"cwd": "/", "env": {"PATH": 1}}),
        json!({"cwd": "/", "env": []}),
        json!({"cwd": "/", "env": {}})
            .as_object()
            .map(|_| json!({"cwd": "/", "env": {"A": null}}))
            .unwrap(),
        // A control flag that is neither absent nor exactly "pipe".
        json!({"cwd": "/", "env": {}, "control": "socket"}),
        json!({"cwd": "/", "env": {}, "control": true}),
        // A descriptor outside 0..=2, or not an integer.
        json!({"cwd": "/", "env": {}, "pipeFds": [3]}),
        json!({"cwd": "/", "env": {}, "pipeFds": [-1]}),
        json!({"cwd": "/", "env": {}, "pipeFds": ["1"]}),
        json!({"cwd": "/", "env": {}, "pipeFds": [1.5]}),
        json!({"cwd": "/", "env": {}, "pipeFds": {"0": 0}}),
    ] {
        assert_eq!(
            parse_linux_launch_request(&value),
            Err(ProtocolError::LinuxLaunchRequest),
            "{value} must be refused"
        );
    }
}

/// The Windows start message mirrors the Linux request's discipline.
#[test]
fn the_windows_start_request_is_validated_the_same_way() {
    let request = parse_windows_start_request(&json!({
        "type": "start",
        "cwd": "C:\\work",
        "env": {"PATH": "C:\\Windows"},
        "control": "pipe",
    }))
    .expect("a complete start request");
    assert_eq!(request.cwd, "C:\\work");
    assert!(request.control);

    for value in [
        json!({"cwd": "C:\\work", "env": {}}),
        json!({"type": "terminate", "cwd": "C:\\work", "env": {}}),
        json!({"type": "start", "cwd": "C:\\work", "env": {}, "extra": 1}),
        json!({"type": "start", "env": {}}),
        json!({"type": "start", "cwd": "C:\\work"}),
        json!({"type": "start", "cwd": "C:\\work", "env": {}, "control": "socket"}),
        json!(null),
    ] {
        assert_eq!(
            parse_windows_start_request(&value),
            Err(ProtocolError::WindowsStartRequest),
            "{value} must be refused"
        );
    }
}

/// The terminate message is payload-free, and a message that carries one is refused.
#[test]
fn only_the_exact_payload_free_terminate_message_is_accepted() {
    assert!(is_windows_terminate_request(&json!({"type": "terminate"})));
    assert!(!is_windows_terminate_request(
        &json!({"type": "terminate", "reason": "cleanup"})
    ));
    assert!(!is_windows_terminate_request(&json!({"type": "start"})));
    assert!(!is_windows_terminate_request(&json!("terminate")));
    assert!(!is_windows_terminate_request(
        &json!([{"type": "terminate"}])
    ));
}

/// Both result branches, and the exit-code constraints.
#[test]
fn the_windows_runner_result_branches_decode() {
    assert_eq!(
        parse_windows_runner_result(&json!({"type": "target-exit", "exitCode": 7})),
        Ok(WindowsRunnerResult::TargetExit { exit_code: 7 })
    );
    assert_eq!(
        parse_windows_runner_result(&json!({"type": "target-exit", "exitCode": 0})),
        Ok(WindowsRunnerResult::TargetExit { exit_code: 0 })
    );
    assert_eq!(
        parse_windows_runner_result(&json!({
            "type": "error",
            "error": {"name": "Error", "message": "boom", "code": "ENOENT"},
        })),
        Ok(WindowsRunnerResult::Error(SerializedRunnerError {
            name: "Error".to_owned(),
            message: "boom".to_owned(),
            code: Some("ENOENT".to_owned()),
            syscall: None,
            path: None,
        }))
    );
}

/// A result that is not one of the two branches is refused by name.
#[test]
fn unknown_windows_runner_results_are_refused() {
    assert_eq!(
        parse_windows_runner_result(&json!({"type": "target-signal", "signal": "SIGKILL"})),
        Err(ProtocolError::UnknownWindowsResult(
            "target-signal".to_owned()
        ))
    );
    assert_eq!(
        parse_windows_runner_result(&json!({"exitCode": 0})),
        Err(ProtocolError::WindowsResult)
    );
    assert_eq!(
        parse_windows_runner_result(&json!([])),
        Err(ProtocolError::WindowsResult)
    );
}

/// An exit code must be a non-negative integer that fits a `u32`.
#[test]
fn an_impossible_exit_code_is_refused() {
    for value in [
        json!({"type": "target-exit", "exitCode": -1}),
        json!({"type": "target-exit", "exitCode": 1.5}),
        json!({"type": "target-exit", "exitCode": "0"}),
        json!({"type": "target-exit", "exitCode": null}),
        json!({"type": "target-exit", "exitCode": 4_294_967_296_i64}),
        json!({"type": "target-exit"}),
        // An extra member is refused even on a valid-looking result.
        json!({"type": "target-exit", "exitCode": 0, "signal": null}),
    ] {
        assert_eq!(
            parse_windows_runner_result(&value),
            Err(ProtocolError::TargetExit),
            "{value} must be refused"
        );
    }
}

/// The error branch's own strictness: exact keys, string members only.
#[test]
fn a_malformed_runner_error_result_is_refused() {
    for value in [
        json!({"type": "error"}),
        json!({"type": "error", "error": {}}),
        json!({"type": "error", "error": {"name": "Error"}}),
        json!({"type": "error", "error": {"name": "Error", "message": "m", "stack": "…"}}),
        json!({"type": "error", "error": {"name": "Error", "message": "m", "code": 1}}),
        json!({"type": "error", "error": "boom"}),
    ] {
        assert_eq!(
            parse_windows_runner_result(&value),
            Err(ProtocolError::RunnerErrorResult),
            "{value} must be refused"
        );
    }
    // A non-string `type` never reaches the branch dispatch at all.
    assert_eq!(
        parse_windows_runner_result(&json!({
            "type": 1,
            "error": {"name": "Error", "message": "m"},
        })),
        Err(ProtocolError::WindowsResult)
    );
    // An `error`-shaped object that names a different type is still refused, with its own error.
    assert_eq!(
        parse_windows_runner_result(&json!({
            "type": "warning",
            "error": {"name": "Error", "message": "m"},
        })),
        Err(ProtocolError::UnknownWindowsResult("warning".to_owned()))
    );
}

/// A serialized error round-trips, and absent optional fields stay absent rather than null.
#[test]
fn a_serialized_runner_error_omits_absent_fields() {
    let error = SerializedRunnerError {
        name: "Error".to_owned(),
        message: "spawn failed".to_owned(),
        code: Some("ENOENT".to_owned()),
        syscall: None,
        path: Some("/nope".to_owned()),
    };
    let encoded = error.to_json();
    assert_eq!(
        encoded,
        json!({"name": "Error", "message": "spawn failed", "code": "ENOENT", "path": "/nope"})
    );
    // The encoder's output is exactly what the decoder accepts.
    assert_eq!(
        parse_windows_runner_result(&json!({"type": "error", "error": encoded})),
        Ok(WindowsRunnerResult::Error(error))
    );
}

/// Rust's I/O kinds are mapped back to the POSIX codes a caller branches on.
#[test]
fn an_io_error_is_projected_onto_its_posix_code() {
    let missing = SerializedRunnerError::from_io_error(&std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no such file",
    ));
    assert_eq!(missing.code.as_deref(), Some("ENOENT"));

    let denied = SerializedRunnerError::from_io_error(&std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "denied",
    ));
    assert_eq!(denied.code.as_deref(), Some("EACCES"));

    // A kind with no POSIX name reports no code rather than a made-up one.
    let other = SerializedRunnerError::from_io_error(&std::io::Error::other("misc"));
    assert_eq!(other.code, None);
    assert_eq!(other.name, "Error");
}

/// The launch locator must be absolute and named exactly `launch-request.json`.
#[test]
fn the_launch_locator_must_be_an_absolute_launch_request_path() {
    let files: LaunchFiles =
        linux_launch_files_from_locator("/run/dsh/launch-1/launch-request.json").expect("absolute");
    assert_eq!(files.directory, PathBuf::from("/run/dsh/launch-1"));
    assert_eq!(
        files.request,
        PathBuf::from("/run/dsh/launch-1/launch-request.json")
    );
    // The startup-error path is derived, never carried on the wire.
    assert_eq!(
        files.startup_error,
        PathBuf::from("/run/dsh/launch-1/startup-error.json")
    );

    for locator in [
        "launch-request.json",
        "run/dsh/launch-request.json",
        "/run/dsh/launch/startup-error.json",
        "/run/dsh/launch/../launch-request.json.bak",
        "/run/dsh/launch",
        "",
    ] {
        assert_eq!(
            linux_launch_files_from_locator(locator),
            Err(ProtocolError::LinuxLaunchLocator),
            "{locator:?} must be refused"
        );
    }
}

/// A `..` component is *not* rejected outright once the file name checks pass, because the parent
/// already normalized it; what matters is that the error file is always the derived sibling.
#[test]
fn the_derived_error_file_is_always_a_sibling_of_the_request() {
    let files = linux_launch_files_from_locator("/a/b/../c/launch-request.json").expect("absolute");
    assert_eq!(
        files.startup_error,
        Path::new("/a/b/../c/startup-error.json"),
        "derived, not accepted from the requester"
    );
}
