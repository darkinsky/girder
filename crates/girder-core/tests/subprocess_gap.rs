//! Tests for process identity and process-table inspection.
//!
//! Corresponds to upstream `packages/subprocess/subprocess-local/tests/process-inspector.spec.ts`
//! and the `snapshot`/`isAlive`/`foregroundPgid` cases of `terminal.spec.ts`. Every property pinned
//! here is one upstream pins, plus the cases upstream's stricter parser refuses.
//!
//! # Why most tests use a fake table
//!
//! A real process table cannot be made to contain a `Z` row on demand, cannot be made to hand the
//! same pid to two processes, and differs between machines. [`Table`] is the injected OS boundary
//! upstream has for exactly this reason (`ProcessInspectorInternals`), so the parser, the tree walk,
//! the session filter and the fence are all driven deterministically.
//!
//! The last group of tests drives a **real** process instead, because a fence that only ever ran
//! against a map proves nothing about how it reads `/proc`.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than
//! a copy. Inside a `#[path]` module `crate::` names *this* test binary rather than `girder-core`,
//! so every reference to the library goes through `girder_core::{...}`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/subprocess_gap.rs"]
mod subprocess_gap;

use std::collections::BTreeMap;
use std::sync::Mutex;

use subprocess_gap::{
    GroupLiveness, HostProbe, InspectError, Probe, ProcessIdentity, Signal, Snapshot,
    foreground_group, group_member, is_alive, parse_proc_stat, quiescent, signal_group,
    signal_process, stat_of,
};

// ---- the fake table ---------------------------------------------------------------------------

/// A scripted `/proc`, with every delivery recorded.
///
/// Signals are recorded rather than delivered: a test that signalled a real pid it did not spawn
/// would be testing the kernel, and on a recycled id it would be testing its own luck.
#[derive(Default)]
struct Table {
    files: Mutex<BTreeMap<String, String>>,
    entries: Mutex<Option<Vec<u32>>>,
    signals: Mutex<Vec<(i32, Signal)>>,
}

impl Table {
    fn new() -> Self {
        Self::default()
    }

    fn with(mut self, pid: u32, stat: &str) -> Self {
        self.files
            .get_mut()
            .expect("fresh lock")
            .insert(format!("/proc/{pid}/stat"), stat.to_owned());
        // `/proc` must list the pids whose rows exist, or the observation will not look at them.
        let entries = self.entries.get_mut().expect("fresh lock");
        let mut pids = entries.clone().unwrap_or_default();
        pids.push(pid);
        *entries = Some(pids);
        self
    }

    fn signals(&self) -> Vec<(i32, Signal)> {
        self.signals.lock().expect("fresh lock").clone()
    }
}

impl Probe for Table {
    fn read(&self, path: &str) -> Option<String> {
        self.files.lock().expect("fresh lock").get(path).cloned()
    }

    fn entries(&self, path: &str) -> Option<Vec<u32>> {
        (path == "/proc")
            .then(|| self.entries.lock().expect("fresh lock").clone())
            .flatten()
    }

    fn signal(&self, pid: i32, signal: Signal) {
        self.signals.lock().expect("fresh lock").push((pid, signal));
    }
}

/// One scripted `/proc/<pid>/stat` row.
///
/// The shape is the kernel's: `pid (comm) state ppid pgrp session tty_nr tpgid ... starttime ...`.
/// `comm` is fixed because no test needs two different names in one table; the test that does uses
/// [`stat_line`] directly.
struct RowSpec<'a> {
    pid: u32,
    parent: u32,
    group: u32,
    session: u32,
    state: char,
    /// `tpgid`, or a non-positive value for "no foreground group".
    foreground: i32,
    started: &'a str,
}

impl RowSpec<'_> {
    fn line(&self) -> String {
        stat_line(
            self.pid,
            "cmd",
            self.state,
            self.parent,
            self.group,
            self.session,
            self.foreground,
            self.started,
        )
    }
}

/// One `/proc/<pid>/stat` line, with `comm` spelled out.
///
/// After the closing `)` the fields are: `state`(0) `ppid`(1) `pgrp`(2) `session`(3)
/// `tty_nr`(4) `tpgid`(5),
/// then flags/minflt/cminflt/majflt(6..=9), then 9 more (10..=18), then `starttime` at index 19 —
/// which is why the padding below is thirteen zeros and not twelve.
#[allow(
    clippy::too_many_arguments,
    reason = "a stat line has this many interesting fields; a struct would only rename the problem"
)]
fn stat_line(
    pid: u32,
    comm: &str,
    state: char,
    parent: u32,
    group: u32,
    session: u32,
    foreground: i32,
    started: &str,
) -> String {
    format!(
        "{pid} ({comm}) {state} {parent} {group} {session} 0 {foreground} 0 0 0 0 0 0 0 0 0 0 0 0 0 {started} 0"
    )
}

/// The common row: `comm` of `cmd` and no foreground group.
fn row(pid: u32, parent: u32, group: u32, session: u32, state: char, started: &str) -> String {
    RowSpec {
        pid,
        parent,
        group,
        session,
        state,
        foreground: -1,
        started,
    }
    .line()
}

// ---- parsing ----------------------------------------------------------------------------------

/// Upstream's `parseProcStat` spec: every field this module reads comes out of the right position.
#[test]
fn a_stat_line_yields_the_fields_the_inspector_asks_for() {
    let stat =
        parse_proc_stat(&stat_line(42, "bash", 'S', 7, 42, 42, 900, "123456")).expect("valid");

    assert_eq!(stat.pid, 42);
    assert_eq!(stat.parent_pid, 7);
    assert_eq!(stat.group, 42);
    assert_eq!(stat.session, 42);
    assert_eq!(stat.state, 'S');
    assert_eq!(stat.foreground_group, 900);
    assert_eq!(stat.started, "123456");
}

/// The kernel does not escape `comm`, so a name containing a space and a parenthesis must not shift
/// the fields. This is the case a plain `split_whitespace` gets wrong.
#[test]
fn a_parenthesized_comm_does_not_shift_the_fields() {
    let line = stat_line(42, "a b) c", 'R', 7, 42, 42, -1, "999");
    let stat = parse_proc_stat(&line).expect("valid");

    assert_eq!(stat.pid, 42);
    assert_eq!(stat.parent_pid, 7);
    assert_eq!(stat.state, 'R');
    assert_eq!(stat.started, "999");
}

/// A comm containing `)` is only safe because the fields are taken after the *last* `)`. Taking the
/// first one — the obvious `find` — would make this line parse as pid 42 with a state of `c`.
#[test]
fn the_last_close_parenthesis_is_the_field_boundary() {
    let stat = parse_proc_stat(&stat_line(42, "x)y", 'D', 8, 42, 42, -1, "5")).expect("valid");

    assert_eq!(stat.parent_pid, 8);
    assert_eq!(stat.state, 'D');
}

/// Upstream's numeric check is `Number.isSafeInteger`, and `Number('')` is `0`: a truncated line
/// parses there as pid 0 with a plausible start identity. Here it is refused.
#[test]
fn a_truncated_line_is_refused_rather_than_read_as_pid_zero() {
    // Nothing after the comm: no state, no fields.
    assert_eq!(parse_proc_stat("42 (bash)"), None);
    // A state but no ppid.
    assert_eq!(parse_proc_stat("42 (bash) S"), None);
    // Fields stop before `starttime` (index 19).
    let short = "42 (bash) S 7 42 42 0 -1 0 0 0 0 0 0 0 0 0";
    assert_eq!(parse_proc_stat(short), None);
}

/// A start identity that is absent must be refused rather than defaulted, because an empty identity
/// compares equal to every other empty one and so silently defeats the pid-reuse fence this module
/// exists for.
#[test]
fn a_missing_start_identity_is_refused() {
    // The line stops at `tpgid`, so there is no `starttime` to read at index 19.
    let missing = "42 (bash) S 7 42 42 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0";
    assert_eq!(parse_proc_stat(missing), None);
}

/// A garbage line is a `None`, never a partial row: a fence built on a half-parsed row would match
/// the wrong process.
#[test]
fn unparseable_text_is_refused() {
    assert_eq!(parse_proc_stat(""), None);
    assert_eq!(parse_proc_stat("not a stat line"), None);
    assert_eq!(parse_proc_stat("(comm only)"), None);
    assert_eq!(parse_proc_stat("nope (bash) S 1 2 3 0 -1"), None);
}

/// The identity a row carries is the pair, not the pid.
#[test]
fn a_stat_lines_identity_carries_the_start_time() {
    let stat = parse_proc_stat(&row(42, 1, 42, 42, 'S', "777")).expect("valid");

    assert_eq!(stat.identity(), ProcessIdentity::new(42, "777"));
}

// ---- quiescence -------------------------------------------------------------------------------

/// Zombie and dead states answer "present in the table" but never "still running".
#[test]
fn zombie_and_dead_states_are_quiescent() {
    for state in ['Z', 'X', 'x'] {
        assert!(quiescent(state), "{state} should be quiescent");
    }
    for state in ['R', 'S', 'D', 'T', 't', 'I'] {
        assert!(!quiescent(state), "{state} should not be quiescent");
    }
}

// ---- snapshots --------------------------------------------------------------------------------

fn four_rows() -> Table {
    Table::new()
        // A shell, its child, and a grandchild, plus one unrelated process.
        .with(10, &row(10, 1, 10, 10, 'S', "100"))
        .with(11, &row(11, 10, 10, 10, 'R', "101"))
        .with(12, &row(12, 11, 10, 10, 'S', "102"))
        .with(99, &row(99, 1, 99, 99, 'S', "103"))
}

/// Upstream's `snapshot` spec: the table is read once and answers every later question from memory.
#[test]
fn a_snapshot_reads_the_table_once_and_answers_from_it() {
    let table = four_rows();
    let snapshot = Snapshot::observe(&table).expect("readable");

    assert!(snapshot.complete());
    assert_eq!(snapshot.len(), 4);
    assert!(!snapshot.is_empty());
    assert_eq!(table.signals(), Vec::new());
}

/// An unreadable table is an error, not an empty snapshot: "I could not look" and "nothing is there"
/// are opposite facts, and a teardown that confused them would report success.
#[test]
fn an_unreadable_table_is_an_error_rather_than_an_empty_snapshot() {
    let table = Table::new();
    assert_eq!(Snapshot::observe(&table), Err(InspectError::Unreadable));
}

/// A row that vanishes between the enumeration and the read makes the observation incomplete, which
/// is the flag a caller must check before concluding "no survivor".
#[test]
fn a_vanished_row_makes_the_observation_incomplete() {
    let mut table = Table::new().with(10, &row(10, 1, 10, 10, 'S', "100"));
    // Listed in `/proc`, but its stat line is gone — the exit race.
    table
        .entries
        .get_mut()
        .expect("fresh lock")
        .as_mut()
        .expect("entries")
        .push(404);
    let snapshot = Snapshot::observe(&table).expect("readable");

    assert!(!snapshot.complete());
    assert_eq!(snapshot.len(), 1);
}

/// Upstream's `tree` returns the root and its transitive descendants, children first. The order is
/// what makes a caller's kill loop safe: a parent killed first would orphan its child to init, where
/// a group signal could no longer reach it.
#[test]
fn a_tree_is_every_transitive_descendant_children_first() {
    let snapshot = Snapshot::observe(&four_rows()).expect("readable");
    let tree = snapshot.tree(10);

    assert_eq!(
        tree,
        vec![
            ProcessIdentity::new(12, "102"),
            ProcessIdentity::new(11, "101"),
            ProcessIdentity::new(10, "100"),
        ]
    );
}

/// A parent the table does not list yields nothing rather than the whole table: an unknown root is
/// not a root at the top of the tree.
#[test]
fn an_unknown_root_yields_an_empty_tree() {
    let snapshot = Snapshot::observe(&four_rows()).expect("readable");

    assert_eq!(snapshot.tree(777), Vec::new());
}

/// A table whose parent links form a cycle yields each pid once instead of looping. The kernel never
/// produces one; a test double or a partial read can, and a walk that hung would take the harness with
/// it.
#[test]
fn a_parent_cycle_terminates_and_lists_each_pid_once() {
    let table = Table::new()
        .with(20, &row(20, 21, 20, 20, 'S', "1"))
        .with(21, &row(21, 20, 20, 20, 'S', "2"));
    let snapshot = Snapshot::observe(&table).expect("readable");
    let tree = snapshot.tree(20);

    assert_eq!(tree.len(), 2);
    assert_eq!(
        tree.iter().map(|identity| identity.pid).collect::<Vec<_>>(),
        vec![21, 20]
    );
}

/// Upstream's `session` filter: membership is the session id, and a pid in another session is not a
/// member however related it is by parentage.
#[test]
fn a_session_lists_only_its_own_members() {
    let table = Table::new()
        .with(30, &row(30, 1, 30, 30, 'S', "10"))
        .with(31, &row(31, 30, 30, 30, 'S', "11"))
        .with(32, &row(32, 1, 32, 32, 'S', "12"));
    let snapshot = Snapshot::observe(&table).expect("readable");

    assert_eq!(
        snapshot
            .session(30)
            .iter()
            .map(|id| id.pid)
            .collect::<Vec<_>>(),
        vec![30, 31]
    );
    assert_eq!(snapshot.session(999), Vec::new());
}

/// The batch liveness question matches pid *and* start identity, so a recycled pid is not the process
/// that was observed.
#[test]
fn a_snapshot_matches_identity_not_merely_pid() {
    let snapshot = Snapshot::observe(&four_rows()).expect("readable");

    assert!(snapshot.alive(&ProcessIdentity::new(10, "100")));
    // Same pid, different occupant.
    assert!(!snapshot.alive(&ProcessIdentity::new(10, "999")));
    assert!(!snapshot.alive(&ProcessIdentity::new(999, "100")));
}

/// A zombie is in the table but is not alive: counting it would make a teardown report a survivor it
/// cannot do anything about.
#[test]
fn a_zombie_row_is_not_alive() {
    let table = Table::new().with(40, &row(40, 1, 40, 40, 'Z', "500"));
    let snapshot = Snapshot::observe(&table).expect("readable");

    assert_eq!(snapshot.len(), 1);
    assert!(!snapshot.alive(&ProcessIdentity::new(40, "500")));
}

// ---- re-reading liveness ----------------------------------------------------------------------

/// Upstream's `isAlive` reads one stat line, never the table. That is what makes a signalling round
/// affordable, and it is also why the answer can differ from an earlier snapshot — which is the point.
#[test]
fn re_reading_liveness_sees_a_state_change_a_snapshot_cannot() {
    let table = four_rows();
    let snapshot = Snapshot::observe(&table).expect("readable");
    let identity = ProcessIdentity::new(11, "101");
    assert!(snapshot.alive(&identity));
    assert!(is_alive(&table, &identity));

    // The process became a zombie after the snapshot was taken.
    table
        .files
        .lock()
        .expect("fresh lock")
        .insert("/proc/11/stat".to_owned(), row(11, 10, 10, 10, 'Z', "101"));

    // The stale observation still says alive; the re-read says what is true now.
    assert!(snapshot.alive(&identity));
    assert!(!is_alive(&table, &identity));
}

/// A re-read of a pid that is not the recorded process is not alive, which is the whole fence.
#[test]
fn a_recycled_pid_is_not_alive() {
    let table = Table::new().with(50, &row(50, 1, 50, 50, 'S', "2000"));

    assert!(!is_alive(&table, &ProcessIdentity::new(50, "1000")));
}

// ---- group membership -------------------------------------------------------------------------

/// A live member is reported by its pid; the teardown ladder uses that pid on its next rung.
#[test]
fn a_group_with_a_running_member_reports_it() {
    let table = Table::new()
        .with(60, &row(60, 1, 60, 60, 'Z', "1"))
        .with(61, &row(61, 60, 60, 60, 'R', "2"));

    assert_eq!(group_member(&table, 60, None), GroupLiveness::Live(61));
}

/// A group whose every entry is zombie or dead is provably done — the answer that lets a teardown
/// stop escalating.
#[test]
fn a_group_of_only_zombies_is_all_quiescent() {
    let table = Table::new()
        .with(60, &row(60, 1, 60, 60, 'Z', "1"))
        .with(61, &row(61, 60, 60, 60, 'Z', "2"));

    assert_eq!(group_member(&table, 60, None), GroupLiveness::AllQuiescent);
}

/// Nothing matched, or the table could not be read: neither proves the group is gone, so neither may
/// answer `AllQuiescent`.
#[test]
fn an_unobservable_group_is_not_reported_as_finished() {
    let table = Table::new().with(70, &row(70, 1, 70, 70, 'S', "1"));

    // No member of group 999 exists.
    assert_eq!(group_member(&table, 999, None), GroupLiveness::Unobservable);
    // An unreadable table is equally unprovable.
    assert_eq!(
        group_member(&Table::new(), 70, None),
        GroupLiveness::Unobservable
    );
}

/// Upstream's `knownMember` fast path: a still-running known member is confirmed from its own stat
/// line, so a poll of a long-lived group costs one read instead of a `/proc` scan.
#[test]
fn a_known_live_member_is_confirmed_without_scanning_the_table() {
    let table = four_rows();
    // Make the table unreadable as a table: only the fast path can still answer.
    let mut unreadable = Table::new().with(11, &row(11, 10, 10, 10, 'R', "101"));
    unreadable.entries = Mutex::new(None);

    assert_eq!(
        group_member(&unreadable, 10, Some(11)),
        GroupLiveness::Live(11)
    );
    // Without the hint, the same table cannot be scanned.
    assert_eq!(
        group_member(&unreadable, 10, None),
        GroupLiveness::Unobservable
    );
    // And the hint is checked, not trusted: a known member in another group proves nothing.
    assert_eq!(group_member(&table, 99, Some(11)), GroupLiveness::Live(99));
}

/// A stale hint is discarded rather than believed: the known member exited, so the scan runs.
#[test]
fn a_stale_known_member_falls_back_to_the_scan() {
    let table = Table::new()
        .with(60, &row(60, 1, 60, 60, 'Z', "1"))
        .with(61, &row(61, 60, 60, 60, 'Z', "2"));

    assert_eq!(
        group_member(&table, 60, Some(61)),
        GroupLiveness::AllQuiescent
    );
}

// ---- foreground group -------------------------------------------------------------------------

/// A terminal with a foreground group reports it, which is what a terminal-signal call aims at.
#[test]
fn a_foreground_group_is_read_from_the_stat_line() {
    let table = Table::new().with(80, &stat_line(80, "bash", 'S', 1, 80, 80, 900, "1"));

    assert_eq!(foreground_group(&table, 80), Some(900));
}

/// `tpgid` of `-1` is the kernel's "no foreground group" — distinct from a pid, and not something a
/// caller can signal.
#[test]
fn no_foreground_group_is_none() {
    let table = Table::new()
        .with(80, &stat_line(80, "bash", 'S', 1, 80, 80, -1, "1"))
        .with(81, &stat_line(81, "bash", 'S', 1, 81, 81, 0, "1"));

    assert_eq!(foreground_group(&table, 80), None);
    assert_eq!(foreground_group(&table, 81), None);
    // An unknown pid has no answer either.
    assert_eq!(foreground_group(&table, 999), None);
}

// ---- signalling -------------------------------------------------------------------------------

/// The fence: a signal aimed at a pid that now belongs to somebody else is *not delivered*, and the
/// caller can tell that it was not.
#[test]
fn a_fenced_signal_is_not_delivered_to_a_recycled_pid() {
    let table = Table::new().with(90, &row(90, 1, 90, 90, 'S', "2000"));

    assert!(!signal_process(
        &table,
        &ProcessIdentity::new(90, "1000"),
        Signal::Terminate
    ));
    assert_eq!(table.signals(), Vec::new());
}

/// The matching identity *is* signalled, once, with the signal asked for.
#[test]
fn a_matching_identity_is_signalled_once() {
    let table = Table::new().with(90, &row(90, 1, 90, 90, 'S', "1000"));

    assert!(signal_process(
        &table,
        &ProcessIdentity::new(90, "1000"),
        Signal::Kill
    ));
    assert_eq!(table.signals(), vec![(90, Signal::Kill)]);
}

/// A process that already exited is not signalled, and the caller learns it was already gone rather
/// than that it killed something.
#[test]
fn an_exited_process_is_not_signalled() {
    let table = Table::new().with(90, &row(90, 1, 90, 90, 'Z', "1000"));

    assert!(!signal_process(
        &table,
        &ProcessIdentity::new(90, "1000"),
        Signal::Terminate
    ));
    assert_eq!(table.signals(), Vec::new());
}

/// A group signal goes to the negative pid — the leader *and* its descendants. A positive one would
/// leave the pipeline this exists to reap.
#[test]
fn a_group_signal_addresses_the_negative_pid() {
    let table = Table::new();

    assert!(signal_group(&table, 4242, Signal::Terminate));
    assert_eq!(table.signals(), vec![(-4242, Signal::Terminate)]);
}

/// The harness's own group is group `0`, and init's is group `1`. Signalling either would kill the
/// caller or the machine's init, so neither is ever aimed at — even when a recorded id says so.
#[test]
fn the_callers_own_group_and_init_are_never_signalled() {
    let table = Table::new();

    assert!(!signal_group(&table, 0, Signal::Kill));
    assert!(!signal_group(&table, 1, Signal::Kill));
    assert_eq!(table.signals(), Vec::new());
}

/// A pid of zero names the caller's group, not a process, so the single-process verb refuses it too.
#[test]
fn pid_zero_is_not_a_single_process() {
    let table = Table::new().with(0, &row(0, 1, 0, 0, 'S', "1"));

    assert!(!signal_process(
        &table,
        &ProcessIdentity::new(0, "1"),
        Signal::Terminate
    ));
    assert_eq!(table.signals(), Vec::new());
}

/// Upstream's signal set, spelled the way it crosses into logs and wire frames.
#[test]
fn every_signal_renders_its_upstream_name() {
    assert_eq!(Signal::Interrupt.as_str(), "SIGINT");
    assert_eq!(Signal::Terminate.as_str(), "SIGTERM");
    assert_eq!(Signal::Kill.as_str(), "SIGKILL");
    assert_eq!(Signal::Stop.as_str(), "SIGTSTP");
    assert_eq!(Signal::Hangup.as_str(), "SIGHUP");
    assert_eq!(Signal::Kill.to_string(), "SIGKILL");
}

// ---- against the real process table -----------------------------------------------------------

/// The one thing a fake cannot prove: that [`HostProbe`] and [`parse_proc_stat`] read a real
/// `/proc` correctly, and that the harness's own process is found, alive, and identified.
#[test]
fn the_real_process_table_identifies_this_process() {
    let own = std::process::id();
    let probe = HostProbe;

    let stat = stat_of(&probe, own).expect("this process has a stat line");
    assert_eq!(stat.pid, own);
    assert!(!stat.started.is_empty());
    // This test is running, so its own state cannot be zombie or dead.
    assert!(!quiescent(stat.state));

    let identity = stat.identity();
    assert!(is_alive(&probe, &identity));

    // The snapshot lists it with the same identity, which is the agreement between the two sources.
    let snapshot = Snapshot::observe(&probe).expect("the real table is readable");
    assert!(!snapshot.is_empty());
    assert!(snapshot.alive(&identity));
    assert!(snapshot.tree(own).iter().any(|entry| entry.pid == own));
}

/// A pid that cannot be ours is not alive, whatever is on the host — `u32::MAX` is above every
/// platform's pid ceiling.
#[test]
fn a_pid_above_the_ceiling_is_not_alive() {
    assert!(!is_alive(&HostProbe, &ProcessIdentity::new(u32::MAX, "1")));
    assert_eq!(stat_of(&HostProbe, u32::MAX), None);
}

/// A spawned child is observed alive, then observed not alive after it is reaped. This is the fence
/// working end to end: the same pid, a different answer, because the process is a zombie in between.
#[test]
fn a_real_child_is_alive_while_running_and_not_after_reaping() {
    let probe = HostProbe;
    let mut child = std::process::Command::new("sleep")
        .arg("5")
        .spawn()
        .expect("sleep starts");
    let pid = child.id();
    let identity = stat_of(&probe, pid)
        .expect("child has a stat line")
        .identity();

    assert!(is_alive(&probe, &identity));

    child.kill().expect("the child was running");
    let status = child.wait().expect("the child was reaped");
    assert!(!status.success());

    // Once reaped it is gone from the table entirely, so no identity can match it.
    assert!(!is_alive(&probe, &identity));
}
