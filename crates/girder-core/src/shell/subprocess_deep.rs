//! What a terminal session *is*: shell lifecycle evidence, the session teardown ladder, and the
//! private protocol a privileged runner speaks.
//!
//! # Where this comes from
//!
//! Upstream `packages/subprocess` is 4,919 effective lines across three packages. Read as
//! capabilities rather than files, it is nine categories:
//!
//! | # | Category | Upstream | Lines |
//! | --- | --- | --- | --- |
//! | 1 | Service vocabulary: spawn spec, stdio modes, error codes, the env scrub | `subprocess/src/*` | 207 |
//! | 2 | Local spawn and managed binding: group-leader spawn, `TERM`→`KILL`, drain, reap | `subprocess-local/src/{spawn,managed-owner,index}.ts` | ~900 |
//! | 3 | Bounded collection: tail-keeping collector, spill file, offset reads | `output.ts` | 313 |
//! | 4 | Process-table inspection: `/proc` parse, pid+start fence, tree/session/liveness | `process-inspector.ts`, `windows-inspector.ts` | 906 |
//! | 5 | Linux containment: transient systemd scope, selector, `execve` bootstrap | `linux-scope*.ts`, `linux-execve.ts` | 919 |
//! | 6 | Privileged runner protocol: strict wire decoding, private launch files | `runner-protocol.ts`, `runner-launch.ts`, `spawn-runner.ts`, `bin.ts`, `control-spawn.ts` | 1,194 |
//! | 7 | Terminal primitive: PTY handle, session teardown, shell activity | `terminal.ts`, `shell-activity.ts` | 610 |
//! | 8 | Windows process control: job objects over FFI | `win32-process/src/*`, `windows-job.ts` | ~1,180 |
//! | 9 | Output framing: `read_to_end` versus bounded streaming tails | (spread across 2, 3) | — |
//!
//! # What the local tree already covers
//!
//! - `subprocess.rs` — category 2, in full: group-leader spawn, `SIGTERM`→`SIGKILL`, concurrent
//!   drain, reaping on every path including timeout and cancellation.
//! - `subprocess_gap.rs` — category 4's core: `parse_proc_stat`, the pid-plus-start-time
//!   [`ProcessIdentity`] fence, tree/session/liveness, foreground group, and fenced signalling.
//! - `process_group.rs` — post-exit containment of a group that outlived its leader.
//! - `launch_environment.rs` — category 1's scrub, as an ambient allowlist plus a forbidden-prefix
//!   table.
//! - `spill.rs`, `spill_policy.rs`, `retention.rs`, `terminal_buffer.rs` — most of category 3,
//!   including a byte-exact bounded tail and absolute-offset reads that report loss explicitly.
//! - `terminal_local.rs` — the *transport* half of category 7: a shell over pipes, explicitly and
//!   honestly not a PTY.
//!
//! # The three gaps this module closes
//!
//! **Category 7's decision half.** `terminal_local.rs` can start a shell and move bytes, and
//! `subprocess_gap.rs` can say which processes exist. Neither can answer the question the terminal
//! handle exists for: *is this shell waiting for me?* Upstream answers it from two independent
//! pieces of evidence that must agree — positive prompt evidence written by the shell's own
//! startup integration (`shell-activity.ts`), and the process table (`terminal.ts`
//! `inspectActivity`). Silence is never evidence: a shell that printed nothing might be idle at a
//! prompt or blocked on a command, and reporting the wrong one either freezes a live pane or
//! reclaims a busy one. Nothing in the local tree generates shell integration at all — there is no
//! `PROMPT_COMMAND` and no `--rcfile` anywhere in the crate.
//!
//! **Category 7's teardown ladder.** Upstream's terminal teardown is not "kill the pid": it tracks
//! every descendant it has ever adopted, re-adopts on each round *only while the root pid provably
//! still carries the spawned shell's start identity*, and escalates `SIGTERM` → grace → `SIGKILL` →
//! grace → verify. The identity fence is what stops a recycled root pid from donating a stranger's
//! children to this session. `process_group.rs` tears down a group, which is a different and weaker
//! statement: a terminal session is not a group, because a shell's jobs move themselves into their
//! own groups.
//!
//! **Category 6 in full.** The privileged-runner protocol is a security boundary, and every one of
//! its parsers is written as an allowlist: exact key sets, no extra members, no coercion. A launch
//! request carries a complete environment and a cwd across a process boundary, and a runner result
//! carries an error back; both are hostile input to the receiving side. There is no local
//! counterpart — the crate spawns directly and has no runner — so nothing here can be reused.
//!
//! # What is ported, symbol by symbol
//!
//! | Upstream | Here |
//! | --- | --- |
//! | `ShellActivity` | [`ShellActivity`] |
//! | `ShellActivity.invalidate` | [`ShellActivity::invalidate`] |
//! | `ShellActivity.inspect` | [`ShellActivity::inspect`] |
//! | `ShellActivity.dispose` | [`ShellActivity::startup_file`] + [`ShellActivity::directory`] |
//! | `prepareShellActivity` | [`prepare_shell_activity`] |
//! | `quote` (shell-activity.ts) | [`quote`] |
//! | `state` record regex `^(\d+):(\d+):(idle\|busy)\n?$` | [`parse_state_record`] |
//! | `SubprocessTerminalActivity` | [`ActivityState`], [`ActivityObservation`] |
//! | `LocalTerminalHandle.descendants` | [`SessionTracker::descendants`] |
//! | `LocalTerminalHandle.survivors` / `unionMembers` | [`survivors`], [`union_members`] |
//! | `LocalTerminalHandle.stopDescendants` | [`Teardown`] |
//! | `LocalTerminalHandle.inspectActivity` | [`decide_activity`] |
//! | `LocalTerminalHandle.signalForeground`'s guards | [`foreground_target`] |
//! | `runner-protocol.ts` (all parsers) | [`parse_linux_launch_request`], [`parse_windows_start_request`], [`is_windows_terminate_request`], [`parse_windows_runner_result`], [`linux_launch_files_from_locator`] |
//! | `serializeRunnerError` / `deserializeRunnerError` | [`SerializedRunnerError`] |
//! | `createLinuxLaunchFiles` / `writeLinuxStartupError` / `readLinuxStartupError` | [`LaunchFiles`] (paths only) |
//!
//! # Deliberately not ported
//!
//! - **The PTY itself** (`node-pty`), and with it `isStdinWaiting`'s `/proc/<pid>/mem` decoding.
//!   A PTY needs a dependency and `unsafe`, and `terminal_local.rs` states that decision. What is
//!   ported is everything that *decides* about a terminal, which is where the semantics live and
//!   where a pipe-backed shell and a PTY-backed one agree.
//! - **Category 5** (systemd scopes) and **category 8** (Windows job objects). Both need a
//!   substrate the local execution layer does not have, and both are already declined in
//!   `subprocess_gap.rs` with the same reasoning.
//! - **Every filesystem operation.** Upstream's `ShellActivity` reads its own state file, and its
//!   launcher writes startup files. Here the *content* of those files and the *transitions* of that
//!   state machine are produced, and the caller owns the writes — which is what makes both testable
//!   without a `/tmp` and lets one implementation serve a local and a remote execution world.
//!
//! # Why the activity decision is a pure function
//!
//! Upstream's `inspectActivity` is a dozen lines of nested conditionals over six sources of truth,
//! and it is the single place where a wrong answer reclaims a pane that was mid-command. Extracting
//! it as [`decide_activity`] over explicit facts makes every branch nameable in a test, which the
//! upstream method cannot be because it reads live state between the branches.

//!
//! # Status: ported, not registered — and the reason is architectural
//!
//! Its three families have consumers upstream, and none of them exists here:
//!
//! | Upstream consumer | What it is | Why it has no slot |
//! | --- | --- | --- |
//! | `terminal.ts`'s `private survivors` / `unionMembers` | discovery of surviving children of a **pty terminal** | this build has no pty terminal handle; `crate::shell::subprocess::kill_tree` signals a process group directly |
//! | `prepareShellActivity` in `subprocess-local/index.ts` | argv and environment for launching a shell under node-pty | no node-pty; the executor spawns `sh -c` |
//! | `parseStateRecord` | decoding a shell integration's activity line | no shell integration is installed |
//!
//! **This is not backlog.** Porting it onto this substrate would mean inventing a pty handle to be a
//! child of, and the teardown ladder exists to handle *that* handle's survivors. The 50 ms sleep in
//! [`crate::shell::subprocess::kill_tree`] is a real limitation, but the fix is a `/proc` scan with start-time
//! fencing, not this module — which waits for a shell to *report* its state rather than reading it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::shell::process_identity::{ProcessIdentity, Signal, Snapshot};

// ================================================================================================
// Category 7, part one: shell lifecycle evidence
// ================================================================================================

/// What a terminal's shell is doing, as far as anyone can prove.
///
/// Upstream: `SubprocessTerminalActivity['state']` in `subprocess/src/types.ts`.
///
/// The three values are not a scale. [`Idle`] is a positive claim that requires evidence — prompt
/// integration wrote a record *and* the process table agrees nothing is running. [`Unknown`] is the
/// honest default and the answer whenever the evidence is incomplete, because both other answers
/// authorize an action: `Idle` lets a caller reclaim the pane, `Busy` makes it wait. Guessing
/// either one wrongly is worse than admitting the gap.
///
/// [`Idle`]: Self::Idle
/// [`Unknown`]: Self::Unknown
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActivityState {
    /// The shell is at a prompt with no foreground or background work.
    Idle,
    /// The shell, or something in its session, is running.
    Busy,
    /// No observation could prove either.
    Unknown,
}

impl ActivityState {
    /// The wire spelling, identical to upstream's union members.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Busy => "busy",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ActivityState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One observation of a terminal's activity, with the revision that makes it comparable.
///
/// Upstream: `SubprocessTerminalActivity`. The revision exists so a caller polling at its own rate
/// can skip an unchanged answer without comparing states: two `unknown`s a second apart are the
/// same fact, while an `unknown` that became a second `unknown` *because the process table changed*
/// is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivityObservation {
    /// What the observation proved.
    pub state: ActivityState,
    /// Monotonic counter, bumped whenever the underlying evidence changed.
    pub revision: u64,
}

/// One parsed shell state record: `pid:sequence:state`.
///
/// Upstream parses this with `/^(\d+):(\d+):(idle|busy)\n?$/u` and reads only the pid and the
/// state; the sequence number is carried here because it is the shell's own statement about how
/// many prompts it has reached, which is worth having when a test needs to say "the third prompt".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateRecord<'a> {
    /// The shell's own pid, as text.
    ///
    /// Text, not a number, because upstream compares it to `String(pid)` and so `007` is not pid 7.
    /// Keeping the comparison textual is strictly the safer direction: a padded record can only
    /// make the answer [`ActivityState::Unknown`], never a false [`ActivityState::Idle`].
    ///
    /// [`ActivityState::Unknown`]: crate::shell::subprocess_deep::ActivityState::Unknown
    /// [`ActivityState::Idle`]: crate::shell::subprocess_deep::ActivityState::Idle
    pub pid: &'a str,
    /// The shell's prompt sequence number, as text.
    pub sequence: &'a str,
    /// The state the shell wrote.
    pub state: ActivityState,
}

/// Parses one state record, or reports that the shell wrote something this build cannot trust.
///
/// Upstream: the `exec` in `ShellActivity.inspect`, promoted to a function so the grammar is stated
/// in one place. Bash writes `unknown` as its third field when a trap is installed, so a
/// well-formed *record* can still be an unparseable *state*: that is [`None`], which the caller
/// turns into [`ActivityState::Unknown`], and it is why the shell writes it at all.
///
/// # Where this is stricter than upstream
///
/// `\d+` in JavaScript matches only ASCII digits, so the check here is `is_ascii_digit` rather than
/// `parse::<u64>` — Rust's integer parser also accepts a leading `+`, which `\d+` does not.
#[must_use]
pub fn parse_state_record(text: &str) -> Option<StateRecord<'_>> {
    // `\n?` in the upstream pattern allows exactly one trailing newline, which is what both the
    // bash and the zsh integration write.
    let body = text.strip_suffix('\n').unwrap_or(text);
    // A second newline would make `$` in a non-multiline JavaScript pattern unreachable.
    if body.contains('\n') {
        return None;
    }
    let mut parts = body.split(':');
    let pid = parts.next()?;
    let sequence = parts.next()?;
    let state = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    // `\d+` requires at least one digit, and `\d` is ASCII-only in JavaScript.
    if pid.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if sequence.is_empty() || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let state = match state {
        "idle" => ActivityState::Idle,
        "busy" => ActivityState::Busy,
        _ => return None,
    };
    Some(StateRecord {
        pid,
        sequence,
        state,
    })
}

/// Wraps `value` in the single quotes a POSIX shell will not expand, escaping embedded quotes.
///
/// Upstream: `quote` in `subprocess-local/src/shell-activity.ts`. The `'\''` dance is the only
/// portable way to put an arbitrary string inside single quotes: close the quote, emit a backslash-
/// escaped quote, reopen. Every path this build substitutes is either a temp directory or an
/// environment value, and an environment value can contain anything.
#[must_use]
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Which shell's startup integration this build knows how to write.
///
/// Upstream: the `shell === 'bash' || shell === 'zsh'` check inside `prepareShellActivity`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    /// Bash 4.4 or newer, hooked through `PS0` and `PROMPT_COMMAND`.
    Bash,
    /// Zsh, hooked through `add-zle-hook-widget` and `precmd_functions`.
    Zsh,
}

impl ShellKind {
    /// Recognises the shell by the *basename* of its program path.
    ///
    /// By basename and not by full path, because upstream does the same: `/usr/local/bin/zsh` must
    /// be recognised exactly like `zsh`, and a path ending in `/mybash` must not be.
    #[must_use]
    pub fn of_program(program: &str) -> Option<Self> {
        match Path::new(program).file_name()?.to_str()? {
            "bash" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            _ => None,
        }
    }

    /// The startup file's name inside the private directory.
    #[must_use]
    pub const fn startup_name(self) -> &'static str {
        match self {
            Self::Bash => "bashrc",
            Self::Zsh => ".zshenv",
        }
    }
}

/// A terminal request, in the shape [`prepare_shell_activity`] needs.
///
/// Upstream takes the whole `SubprocessTerminalSpawnSpec`; only these five facts are read, so only
/// these five are accepted — a caller cannot accidentally make the decision depend on `rows`.
#[derive(Clone, Debug)]
pub struct ShellActivityRequest<'a> {
    /// Program and arguments, as requested.
    pub argv: &'a [String],
    /// The scrubbed environment the child will receive.
    pub env: &'a BTreeMap<String, String>,
    /// Whether the caller opted in with `shellActivity: true`.
    pub requested: bool,
    /// Whether the execution world is POSIX; Windows has no startup-file mechanism here.
    pub posix: bool,
    /// The private directory the caller created for this launch.
    pub directory: PathBuf,
}

/// One shell's startup integration: the file to write, and the launch that makes the shell read it.
///
/// Upstream: `ShellActivity`. This is the evidence side of readiness, and it is deliberately the
/// *only* path by which ordinary terminal output can influence a lifecycle decision — it cannot.
/// A prompt-looking string printed by a command is output; a record in a private file written by a
/// `PROMPT_COMMAND` hook is evidence. Conflating the two is how a pane that ran `echo '$ '` gets
/// reclaimed while its command is still running.
///
/// # The revision protocol
///
/// `revision` bumps on exactly two events, matching upstream: a state record that differs from the
/// last one observed, and an explicit [`ShellActivity::invalidate`]. Nothing else moves it, so a
/// caller's poll loop cannot manufacture a change by asking twice.
#[derive(Clone, Debug)]
pub struct ShellActivity {
    directory: PathBuf,
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    kind: ShellKind,
    startup_file: PathBuf,
    startup_content: String,
    revision: u64,
    observed: String,
    invalidated: Option<String>,
    state: ActivityState,
}

impl ShellActivity {
    /// The private directory the caller owns and must remove.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The program and arguments the launch must use.
    ///
    /// For Bash this differs from the request: the private `--rcfile` replaces `~/.bashrc`, and the
    /// private file sources the real one first so the user's environment still applies. For Zsh the
    /// argv is unchanged and `ZDOTDIR` does the redirect.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The environment the launch must use, with any temporary redirect applied.
    #[must_use]
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Which shell this integration is for.
    #[must_use]
    pub const fn kind(&self) -> ShellKind {
        self.kind
    }

    /// The file the caller must write [`ShellActivity::startup_content`] to.
    #[must_use]
    pub fn startup_file(&self) -> &Path {
        &self.startup_file
    }

    /// The exact startup-file content, byte for byte.
    #[must_use]
    pub fn startup_content(&self) -> &str {
        &self.startup_content
    }

    /// The last state this handle concluded.
    #[must_use]
    pub const fn state(&self) -> ActivityState {
        self.state
    }

    /// Discards prompt evidence before input or a foreground signal is delivered.
    ///
    /// Upstream: `ShellActivity.invalidate`. `record` is the state file's *current* content, which
    /// upstream re-reads rather than reusing its last observation: a record the shell wrote after the
    /// dispatch was already ordered is not evidence that the dispatch has finished.
    ///
    /// The next [`ShellActivity::inspect`] therefore reports [`ActivityState::Unknown`] until the
    /// shell writes a *new* record. That gap is the point — it is the window in which a caller must
    /// not reclaim the pane.
    pub fn invalidate(&mut self, record: &str) {
        self.invalidated = Some(record.to_owned());
        self.revision = self.revision.saturating_add(1);
        self.state = ActivityState::Unknown;
    }

    /// Reads the latest top-level shell transition, fenced against input since that transition.
    ///
    /// Upstream: `ShellActivity.inspect`. `record` is the state file's content; the caller reads it,
    /// because this module performs no I/O.
    ///
    /// A record whose pid is not `pid` is rejected even when it parses: the private directory is
    /// per-launch, but a shell that forked writes its own pid from a subshell, and a subshell's
    /// prompt is not the top-level shell's prompt.
    pub fn inspect(&mut self, pid: u32, record: &str) -> ActivityObservation {
        if record != self.observed {
            record.clone_into(&mut self.observed);
            self.revision = self.revision.saturating_add(1);
        }
        let parsed = parse_state_record(record);
        let state = if self.invalidated.as_deref() == Some(record) {
            ActivityState::Unknown
        } else {
            match parsed {
                Some(parsed) if parsed.pid == pid.to_string() => parsed.state,
                _ => ActivityState::Unknown,
            }
        };
        self.state = state;
        ActivityObservation {
            state,
            revision: self.revision,
        }
    }
}

/// Decides whether a launch gets startup integration at all, and builds it if so.
///
/// Upstream: `prepareShellActivity` in `subprocess-local/src/shell-activity.ts`.
///
/// # Why so much is refused
///
/// Every refusal is a case where the integration would be *wrong* rather than merely absent:
///
/// - Not requested — the caller owns the launch, and an unrequested hook is a surprise in a
///   user's shell.
/// - Windows — there is no `--rcfile`/`ZDOTDIR` mechanism, so the hook would silently do nothing
///   while [`ShellActivity::inspect`] reported [`ActivityState::Unknown`] forever.
/// - `argv` that is not exactly `[shell, "-i"]` — a caller that passed its own arguments or
///   wrapped the executable in something else (`env FOO=1 bash -i`) has chosen a launch whose
///   startup path this build cannot predict, so it must not rewrite it.
/// - A shell that is neither bash nor zsh — no hook exists.
///
/// [`ActivityState::Unknown`]: crate::shell::subprocess_deep::ActivityState::Unknown
#[must_use]
pub fn prepare_shell_activity(request: &ShellActivityRequest<'_>) -> Option<ShellActivity> {
    if !request.requested || !request.posix {
        return None;
    }
    let [program, flag] = request.argv else {
        return None;
    };
    if flag != "-i" {
        return None;
    }
    let kind = ShellKind::of_program(program)?;
    let directory = request.directory.clone();
    let state_file = quote(&directory.join("state").to_string_lossy());
    let guards_file = quote(&directory.join("guards").to_string_lossy());
    let startup_file = directory.join(kind.startup_name());

    let (argv, env, startup_content) = match kind {
        ShellKind::Bash => {
            let content = bash_startup(&state_file, &guards_file);
            let argv = vec![
                program.clone(),
                "--rcfile".to_owned(),
                startup_file.to_string_lossy().into_owned(),
                "-i".to_owned(),
            ];
            (argv, request.env.clone(), content)
        }
        ShellKind::Zsh => {
            let inherited_zdotdir = request.env.get("ZDOTDIR").cloned();
            let content = zsh_startup(inherited_zdotdir.as_deref(), &state_file, &guards_file);
            let mut env = request.env.clone();
            env.insert(
                "ZDOTDIR".to_owned(),
                directory.to_string_lossy().into_owned(),
            );
            (request.argv.to_vec(), env, content)
        }
    };

    Some(ShellActivity {
        directory,
        argv,
        env,
        kind,
        startup_file,
        startup_content,
        revision: 0,
        observed: String::new(),
        invalidated: None,
        state: ActivityState::Unknown,
    })
}

/// The Bash integration file.
///
/// Three facts make this more than a `PROMPT_COMMAND` hook:
///
/// 1. The real `~/.bashrc` is sourced first, so the integration never costs the user their
///    environment. That is also why the file is passed as `--rcfile`: it replaces the login file
///    only in the sense of being read instead, and this line puts the original back.
/// 2. The hook is only installed on `4.4`+, which is the first version whose `PS0` runs before a
///    command rather than after — without it a busy shell would look idle.
/// 3. A read-only `PROMPT_COMMAND`/`PS0` (the `declare -r` probe) is respected rather than
///    overwritten: a user who locked those down gets no integration instead of a broken prompt.
///
/// `activity=unknown` whenever `trap -p` printed anything: a shell with a trap installed can run
/// that trap between prompts, so its prompt is not proof that no work is pending.
fn bash_startup(state_file: &str, guards_file: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("[[ ! -r ~/.bashrc ]] || builtin source ~/.bashrc".to_owned());
    lines.push(
        "if (( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4) )) && \
         [[ ! $(declare -p PROMPT_COMMAND PS0 2>/dev/null) =~ declare\\ -[^[:space:]]*r ]]; then"
            .to_owned(),
    );
    lines.push("  __dsh_shell_pid=$BASHPID; __dsh_shell_sequence=0".to_owned());
    lines.push("  __dsh_shell_idle() {".to_owned());
    lines.push("    local result=$?".to_owned());
    lines.push("    if [[ $BASHPID == \"$__dsh_shell_pid\" ]]; then".to_owned());
    lines.push("      (( ++__dsh_shell_sequence ))".to_owned());
    lines.push("      local activity=idle".to_owned());
    lines.push(format!("      builtin trap -p >| {guards_file}"));
    lines.push(format!(
        "      [[ ! -s {guards_file} ]] || activity=unknown"
    ));
    lines.push(format!(
        "      builtin printf '%s:%s:%s\\n' \"$BASHPID\" \"$__dsh_shell_sequence\" \
         \"$activity\" >| {state_file}"
    ));
    lines.push("    fi".to_owned());
    lines.push("    return \"$result\"".to_owned());
    lines.push("  }".to_owned());
    lines.push(
        "  if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == \"declare -a \"* ]]; then".to_owned(),
    );
    lines.push("    PROMPT_COMMAND+=(__dsh_shell_idle)".to_owned());
    lines.push("  else".to_owned());
    lines.push("    PROMPT_COMMAND=\"${PROMPT_COMMAND}\"$'\\n'\"__dsh_shell_idle\"".to_owned());
    lines.push("  fi".to_owned());
    let busy = quote(&format!(
        "$(builtin printf '%s:%s:busy' \"$__dsh_shell_pid\" \"$__dsh_shell_sequence\" >| \
         {state_file})"
    ));
    lines.push(format!("  PS0+={busy}"));
    lines.push("fi".to_owned());
    lines.push(String::new());
    lines.join("\n")
}

/// The Zsh integration file.
///
/// Zsh has no `--rcfile`, so the redirect is `ZDOTDIR`: the private `.zshenv` is read instead of the
/// real one, sources the real one first, and then installs the hooks. `precmd` alone is not enough —
/// a bare prompt can be reached with a line already buffered — so `line-init`/`line-finish` from
/// `zle` contribute the "is the user mid-edit" half, and `preexec` contributes the busy half.
fn zsh_startup(inherited_zdotdir: Option<&str>, state_file: &str, guards_file: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(match inherited_zdotdir {
        None => "unset ZDOTDIR".to_owned(),
        Some(value) => format!("ZDOTDIR={}", quote(value)),
    });
    lines.push(
        "[[ ! -r ${ZDOTDIR:-$HOME}/.zshenv ]] || builtin source \"${ZDOTDIR:-$HOME}/.zshenv\""
            .to_owned(),
    );
    lines.push("typeset -g __dsh_shell_pid=$$ __dsh_shell_sequence=0".to_owned());
    lines.push("__dsh_shell_activity() {".to_owned());
    lines.push("  (( ZSH_SUBSHELL == 0 && $$ == __dsh_shell_pid )) || return".to_owned());
    lines.push("  (( ++__dsh_shell_sequence ))".to_owned());
    lines.push(format!(
        "  builtin printf '%s:%s:%s\\n' \"$$\" \"$__dsh_shell_sequence\" \"$1\" >| {state_file}"
    ));
    lines.push("  return 0".to_owned());
    lines.push("}".to_owned());
    lines.push("__dsh_shell_idle() {".to_owned());
    lines.push(format!("  {{ builtin trap; zle -F; }} >| {guards_file}"));
    lines.push(
        "  if [[ $CONTEXT != start || -n $BUFFER ]]; then __dsh_shell_activity busy".to_owned(),
    );
    lines.push(format!(
        "  elif [[ -s {guards_file} || -n ${{(k)functions[(I)TRAP*]}} ]]; then \
         __dsh_shell_activity unknown"
    ));
    lines.push("  else __dsh_shell_activity idle; fi".to_owned());
    lines.push("}".to_owned());
    lines.push("__dsh_shell_busy() { __dsh_shell_activity busy }".to_owned());
    lines.push("__dsh_shell_init() {".to_owned());
    lines.push("  autoload -Uz add-zle-hook-widget add-zsh-hook".to_owned());
    lines.push("  add-zle-hook-widget line-init __dsh_shell_idle".to_owned());
    lines.push("  add-zle-hook-widget line-finish __dsh_shell_busy".to_owned());
    lines.push("  add-zsh-hook preexec __dsh_shell_busy".to_owned());
    lines.push("  precmd_functions=(${precmd_functions:#__dsh_shell_init})".to_owned());
    lines.push("}".to_owned());
    lines.push("typeset -ga precmd_functions".to_owned());
    lines.push("precmd_functions+=(__dsh_shell_init)".to_owned());
    lines.push(String::new());
    lines.join("\n")
}

// ================================================================================================
// Category 7, part two: the session teardown ladder
// ================================================================================================

/// Tracks the processes a terminal session has ever owned, fenced against pid reuse.
///
/// Upstream: the `trackedDescendants` / `descendants()` pair in
/// `subprocess-local/src/terminal.ts`.
///
/// # Why a session and not a group
///
/// A shell that runs `cmd &` puts `cmd` in *its own* process group when job control is on, so
/// signalling the shell's group does not reach it. The session is the container that does: every
/// process the shell spawns inherits its session id unless it deliberately calls `setsid`.
///
/// # Why adoption stops when the root is gone
///
/// Membership is re-read on every round, and a pid is recycled. Once the root pid no longer carries
/// the spawned shell's start identity, the tree and session hanging off that number belong to
/// whoever holds it now. Adoption therefore stops, while every already-adopted member keeps its own
/// identity and is rechecked before each signal. That combination is the whole safety argument: a
/// teardown can only ever reach processes this handle provably started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTracker {
    root_pid: u32,
    root_identity: Option<ProcessIdentity>,
    tracked: Vec<ProcessIdentity>,
}

impl SessionTracker {
    /// Starts tracking a session from a root pid and its captured identity.
    ///
    /// `root_identity` is [`Option`] because the capture can legitimately fail: the shell may have
    /// exited between the spawn and the snapshot that would have described it. A tracker without
    /// one never adopts anything, so its teardown can only reach members it already held — which,
    /// for a fresh tracker, is nothing. That is the correct answer: this build must not guess.
    #[must_use]
    pub fn new(root_pid: u32, root_identity: Option<ProcessIdentity>) -> Self {
        Self {
            root_pid,
            root_identity,
            tracked: Vec::new(),
        }
    }

    /// Captures the root's identity from an observation, as upstream does at handle construction.
    #[must_use]
    pub fn capture_root(root_pid: u32, observed: &Snapshot) -> Option<ProcessIdentity> {
        observed
            .tree(root_pid)
            .into_iter()
            .find(|member| member.pid == root_pid)
    }

    /// The root pid this tracker was built for.
    #[must_use]
    pub const fn root_pid(&self) -> u32 {
        self.root_pid
    }

    /// Every member adopted so far that was still running at the last observation.
    #[must_use]
    pub fn tracked(&self) -> &[ProcessIdentity] {
        &self.tracked
    }

    /// Whether the root pid still provably carries the spawned shell's start identity *now*.
    #[must_use]
    pub fn root_verified(&self, observed: &Snapshot) -> bool {
        let Some(identity) = &self.root_identity else {
            return false;
        };
        observed
            .tree(self.root_pid)
            .into_iter()
            .find(|member| member.pid == self.root_pid)
            .is_some_and(|root| root.started == identity.started)
    }

    /// Re-adopts the session's members from one observation, and returns the live ones.
    ///
    /// Upstream: `LocalTerminalHandle.descendants`. New members come from the root's tree and from
    /// the session, and only while the root is verified; already-adopted members are unioned in
    /// first so their order — and therefore a teardown's signalling order — is stable across rounds.
    /// The result is filtered to what the observation showed as running, which is what makes a
    /// repeated poll cheap and what keeps a zombie from being reported as a survivor.
    pub fn descendants(&mut self, observed: &Snapshot) -> Vec<ProcessIdentity> {
        let tree = observed.tree(self.root_pid);
        let mut groups: Vec<Vec<ProcessIdentity>> = vec![std::mem::take(&mut self.tracked)];
        if self.root_verified(observed) {
            groups.push(tree);
            groups.push(observed.session(self.root_pid));
        }
        let union: Vec<ProcessIdentity> = union_members(&groups)
            .into_iter()
            .filter(|member| member.pid != self.root_pid)
            .collect();
        self.tracked = survivors(&union, observed);
        self.tracked.clone()
    }
}

/// The members of `groups` that were running in `observed`.
///
/// Upstream: `LocalTerminalHandle.survivors`. A zombie is not a survivor: it cannot be signalled,
/// it holds no descriptors, and reporting it makes a teardown claim a failure that did not happen.
#[must_use]
pub fn survivors(members: &[ProcessIdentity], observed: &Snapshot) -> Vec<ProcessIdentity> {
    members
        .iter()
        .filter(|member| observed.alive(member))
        .cloned()
        .collect()
}

/// Concatenates identity lists, keeping the first occurrence of each pid-and-start pair.
///
/// Upstream: `LocalTerminalHandle.unionMembers`. Order is the contract: the arguments' order is
/// preserved and duplicates are dropped at their *later* position, so a caller that passes "already
/// tracked" first gets a stable escalation order out of a rebuilt index.
#[must_use]
pub fn union_members(groups: &[Vec<ProcessIdentity>]) -> Vec<ProcessIdentity> {
    let mut members: Vec<ProcessIdentity> = Vec::new();
    let mut seen: std::collections::BTreeSet<(u32, String)> = std::collections::BTreeSet::new();
    for group in groups {
        for member in group {
            let key = (member.pid, member.started.clone());
            if !seen.insert(key) {
                continue;
            }
            members.push(member.clone());
        }
    }
    members
}

/// Which rung of the teardown ladder a round belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeardownPhase {
    /// Ask politely: `SIGTERM` to every adopted descendant.
    Terminate,
    /// Escalate: `SIGKILL` to whatever survived the grace window, re-adopted.
    Kill,
    /// Prove the session is empty rather than assuming the last round worked.
    Verify,
    /// The ladder is over; further rounds are no-ops.
    Done,
}

/// One round of the teardown ladder: what to signal, what to wait for, and what came back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeardownRound {
    /// The rung this round performed.
    pub phase: TeardownPhase,
    /// The signal to deliver to `targets`, or [`None`] for a verification round.
    pub signal: Option<Signal>,
    /// The exact identities to signal, empty when there is nothing to do.
    pub targets: Vec<ProcessIdentity>,
    /// What the observation showed still running when this round began.
    pub survivors: Vec<ProcessIdentity>,
    /// How long to wait before asking again.
    pub grace: Duration,
}

/// The ordered escalation a terminal session teardown performs.
///
/// Upstream: `LocalTerminalHandle.stopDescendants` and the `closeOnce` that calls it twice. Written
/// as an explicit state machine here because the interesting part is *what the round reads* — the
/// survivors of the previous round unioned with a fresh adoption — and a caller driving it round by
/// round can assert exactly that.
///
/// # Why the ladder is not "TERM, sleep, KILL"
///
/// Because a `SIGTERM` handler gets to run. A shell job that traps `TERM` and then starts another
/// child has escaped the set the first round was computed over; the `KILL` round therefore
/// re-adopts before it signals rather than reusing the first round's list. And because `SIGKILL`
/// can still leave a process momentarily unscheduled, the ladder verifies with a third
/// observation before it claims success: a teardown that reports "empty" from the fact that it sent
/// a signal is reporting its own intent, not the world.
#[derive(Clone, Debug)]
pub struct Teardown {
    tracker: SessionTracker,
    grace: Duration,
    phase: TeardownPhase,
    armed: Vec<ProcessIdentity>,
}

impl Teardown {
    /// Arms a ladder for one session.
    #[must_use]
    pub fn new(root_pid: u32, root_identity: Option<ProcessIdentity>, grace: Duration) -> Self {
        Self {
            tracker: SessionTracker::new(root_pid, root_identity),
            grace,
            phase: TeardownPhase::Terminate,
            armed: Vec::new(),
        }
    }

    /// The rung the next [`Teardown::step`] will perform.
    #[must_use]
    pub const fn phase(&self) -> TeardownPhase {
        self.phase
    }

    /// The tracker this ladder drives, so a caller can inspect what it has adopted.
    #[must_use]
    pub const fn tracker(&self) -> &SessionTracker {
        &self.tracker
    }

    /// Advances the ladder by one round against a fresh observation.
    ///
    /// The caller is responsible for delivering `signal` to `targets` and for waiting `grace`
    /// before the next call — that split is what lets this be tested without sleeping.
    pub fn step(&mut self, observed: &Snapshot) -> TeardownRound {
        match self.phase {
            TeardownPhase::Terminate => {
                let targets = self.tracker.descendants(observed);
                self.armed.clone_from(&targets);
                self.phase = TeardownPhase::Kill;
                TeardownRound {
                    phase: TeardownPhase::Terminate,
                    signal: Some(Signal::Terminate),
                    targets,
                    survivors: Vec::new(),
                    grace: self.grace,
                }
            }
            TeardownPhase::Kill => {
                let captured = survivors(&self.armed, observed);
                let targets =
                    union_members(&[captured.clone(), self.tracker.descendants(observed)]);
                self.armed.clone_from(&targets);
                self.phase = TeardownPhase::Verify;
                TeardownRound {
                    phase: TeardownPhase::Kill,
                    signal: Some(Signal::Kill),
                    targets,
                    survivors: captured,
                    grace: self.grace,
                }
            }
            TeardownPhase::Verify => {
                let captured = survivors(&self.armed, observed);
                let combined = union_members(&[captured, self.tracker.descendants(observed)]);
                let remaining = survivors(&combined, observed);
                self.armed = Vec::new();
                self.phase = TeardownPhase::Done;
                TeardownRound {
                    phase: TeardownPhase::Verify,
                    signal: None,
                    targets: Vec::new(),
                    survivors: remaining,
                    grace: Duration::ZERO,
                }
            }
            TeardownPhase::Done => TeardownRound {
                phase: TeardownPhase::Done,
                signal: None,
                targets: Vec::new(),
                survivors: Vec::new(),
                grace: Duration::ZERO,
            },
        }
    }
}

/// Why a foreground signal cannot be delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ForegroundRefusal {
    /// No foreground process group could be resolved.
    #[error("cannot resolve foreground process group for terminal")]
    Unresolved,
    /// The foreground group *is* the shell, and the signal would be a `SIGKILL`.
    #[error("refusing to SIGKILL the terminal shell; terminate the terminal session instead")]
    KillShell,
}

/// Resolves the process group a foreground signal is aimed at, or refuses.
///
/// Upstream: the guards at the top of `LocalTerminalHandle.signalForeground`.
///
/// The `SIGKILL`-the-shell refusal is the interesting one: a shell sitting at its prompt *is* the
/// foreground group, so `Ctrl-C`-style delivery targets the shell legitimately, while a `SIGKILL`
/// aimed there would destroy the terminal instead of interrupting a command. The caller's recovery
/// is [`Teardown`], which is written to handle a shell that has already exited.
///
/// # A deliberate deviation
///
/// Group `0` is refused as [`ForegroundRefusal::Unresolved`]. Upstream never sees a zero here
/// because its `/proc` reader filters non-positive `tpgid`s first; refusing it again is cheap, and
/// group `0` means "the caller's own group", which is the last thing a harness should signal.
pub fn foreground_target(
    shell_pid: u32,
    foreground: Option<u32>,
    signal: Signal,
) -> Result<u32, ForegroundRefusal> {
    let Some(group) = foreground.filter(|group| *group > 0) else {
        return Err(ForegroundRefusal::Unresolved);
    };
    if signal == Signal::Kill && group == shell_pid {
        return Err(ForegroundRefusal::KillShell);
    }
    Ok(group)
}

// ================================================================================================
// Category 7, part three: the activity decision
// ================================================================================================

/// Everything [`decide_activity`] is allowed to look at.
///
/// Upstream reads these from the live handle, between the branches. Passing them in is what makes
/// the decision reviewable: this struct is the complete list of things that can make a pane look
/// idle, and adding one is a visible change rather than a new `if` inside a method.
#[derive(Clone, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each boolean is a distinct fact upstream reads from a different object; naming them \
              is the point of this struct"
)]
pub struct ActivityFacts<'a> {
    /// Whether the handle has already recorded quiescence (terminate succeeded).
    pub quiescent: bool,
    /// Whether node-pty's (here: the transport's) top-level exit has been observed.
    pub exited: bool,
    /// Whether the managed range has been proven empty after that exit.
    pub managed_range_empty: bool,
    /// Whether a managed-range owner exists at all.
    pub managed: bool,
    /// The owner's own task count, when it can report one.
    pub managed_task_count: Option<usize>,
    /// Whether the execution world is Linux, where a session scan is meaningful.
    pub linux: bool,
    /// The one process-table observation this decision is made against.
    pub observed: &'a Snapshot,
    /// The terminal's top-level pid.
    pub root_pid: u32,
    /// The start identity captured when the terminal was created.
    pub root_identity: Option<&'a ProcessIdentity>,
    /// How many live descendants [`SessionTracker::descendants`] reported.
    pub descendant_count: usize,
    /// The foreground process group, when the terminal driver published one.
    pub foreground_group: Option<u32>,
    /// What the shell's own startup integration last wrote.
    pub shell: ActivityState,
}

/// Decides what a terminal is doing, from positive evidence only.
///
/// Upstream: `LocalTerminalHandle.inspectActivity`. The order of the branches is the specification,
/// and it is not the order a reader expects:
///
/// 1. Already quiescent → [`ActivityState::Idle`]. Teardown proved it.
/// 2. Exited *and* the managed range is empty → idle. Two independent facts, both positive.
/// 3. Live descendants → busy. A session member is running, whatever the shell's prompt says: a
///    shell that printed a prompt and then ran a job is busy.
/// 4. Exited, unmanaged, on Linux, with a complete table and no root row → the shell is gone, so
///    fall back to whether anything in its session is still alive. This is the only branch that
///    answers from the session rather than the root, and it exists because a shell that exits
///    normally cannot write a final record.
/// 5. Complete table, root present and still carrying the captured start identity → the terminal
///    is trusted: no foreground group is unknown, a foreground group that is not the shell is busy,
///    and a foreground group that *is* the shell defers to the shell's own record.
/// 6. Everything else → unknown.
///
/// # Why a managed range overrides the prompt
///
/// In branch 5, an idle prompt is not accepted while a managed owner reports tasks. A native range
/// can hold work that never appears in the process tree — that is the whole reason the owner
/// exists — so the prompt's claim is downgraded. One task is the shell itself; zero or an
/// unavailable count is not evidence of anything, and the answer becomes unknown.
#[must_use]
pub fn decide_activity(facts: &ActivityFacts<'_>) -> ActivityState {
    if facts.quiescent {
        return ActivityState::Idle;
    }
    if facts.exited && facts.managed_range_empty {
        return ActivityState::Idle;
    }
    if facts.descendant_count > 0 {
        return ActivityState::Busy;
    }
    let root = facts
        .observed
        .tree(facts.root_pid)
        .into_iter()
        .find(|member| member.pid == facts.root_pid);
    if facts.exited && !facts.managed && facts.linux && facts.observed.complete() && root.is_none()
    {
        let alive = facts
            .observed
            .session(facts.root_pid)
            .into_iter()
            .any(|member| facts.observed.alive(&member));
        return if alive {
            ActivityState::Busy
        } else {
            ActivityState::Idle
        };
    }
    let verified = match (root.as_ref(), facts.root_identity) {
        (Some(root), Some(identity)) => root.started == identity.started,
        _ => false,
    };
    if !(facts.observed.complete() && verified) {
        return ActivityState::Unknown;
    }
    let state = match facts.foreground_group {
        None => return ActivityState::Unknown,
        Some(group) if group == facts.root_pid => facts.shell,
        Some(_) => return ActivityState::Busy,
    };
    if state == ActivityState::Idle && facts.managed {
        return match facts.managed_task_count {
            Some(1) => ActivityState::Idle,
            // `undefined` or a count below one: the owner cannot say the range is empty.
            None | Some(0) => ActivityState::Unknown,
            Some(_) => ActivityState::Busy,
        };
    }
    state
}

// ================================================================================================
// Category 6: the privileged runner protocol
// ================================================================================================

/// Why a runner-protocol payload was refused.
///
/// Upstream throws `Error`s with these exact messages; keeping them byte-identical matters because
/// they cross a process boundary and are read by a caller that is not this build.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// The Linux launch request was not the exact documented shape.
    #[error("subprocess runner received an invalid Linux launch request")]
    LinuxLaunchRequest,
    /// The request locator was not an absolute `launch-request.json`.
    #[error("subprocess runner received an invalid Linux launch-request locator")]
    LinuxLaunchLocator,
    /// The Windows start message was not the exact documented shape.
    #[error("subprocess runner received an invalid Windows start request")]
    WindowsStartRequest,
    /// An error result's `error` member was not a bounded runner error.
    #[error("subprocess runner emitted an invalid error result")]
    RunnerErrorResult,
    /// An error result carried a `type` other than `error`.
    #[error("subprocess runner emitted an unknown error result")]
    UnknownErrorResult,
    /// The Windows result was not an object with a string `type`.
    #[error("subprocess runner emitted an invalid Windows result")]
    WindowsResult,
    /// The `target-exit` result was malformed or carried an impossible exit code.
    #[error("subprocess runner emitted an invalid target-exit result")]
    TargetExit,
    /// The Windows result carried a `type` this build does not know.
    #[error("subprocess runner emitted an unknown Windows result: {0}")]
    UnknownWindowsResult(String),
}

/// The bounded, Node-shaped error record that crosses the runner boundary.
///
/// Upstream: `SerializedRunnerError`. Bounded is load-bearing: a raw `Error` carries a stack, a
/// prototype, and whatever a plugin attached to it, none of which survives a JSON round trip or
/// belongs on a wire. These five fields are the ones a caller can act on, and `code` is the one it
/// branches on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerializedRunnerError {
    /// The error's name (`Error`, `TypeError`, …).
    pub name: String,
    /// The human-readable message.
    pub message: String,
    /// A machine-readable code (`ENOENT`, `EACCES`, …), when there is one.
    pub code: Option<String>,
    /// The failing syscall (`spawn`, `open`, …), when there is one.
    pub syscall: Option<String>,
    /// The path involved, when there is one.
    pub path: Option<String>,
}

impl SerializedRunnerError {
    /// Projects an I/O failure onto the wire shape.
    ///
    /// Upstream reads `code`/`syscall`/`path` off the caught error. Rust's `io::Error` carries a
    /// kind instead of a code, so the kinds that a spawn or launch can produce are mapped back to
    /// their POSIX names — the caller on the other side branches on `code`, and `NotFound` would
    /// not match the `ENOENT` it has always seen.
    #[must_use]
    pub fn from_io_error(error: &std::io::Error) -> Self {
        Self {
            name: "Error".to_owned(),
            message: error.to_string(),
            code: io_error_code(error).map(ToOwned::to_owned),
            syscall: None,
            path: None,
        }
    }

    /// The wire object, with absent optional fields absent rather than null.
    ///
    /// Upstream spreads only the members that exist; a `null` would fail the receiver's
    /// `typeof === 'string'` check and turn a preserved error into a protocol violation.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("name".to_owned(), Value::String(self.name.clone()));
        object.insert("message".to_owned(), Value::String(self.message.clone()));
        if let Some(code) = &self.code {
            object.insert("code".to_owned(), Value::String(code.clone()));
        }
        if let Some(syscall) = &self.syscall {
            object.insert("syscall".to_owned(), Value::String(syscall.clone()));
        }
        if let Some(path) = &self.path {
            object.insert("path".to_owned(), Value::String(path.clone()));
        }
        Value::Object(object)
    }
}

/// The POSIX name for the I/O kinds a launch can produce, where upstream would have had one.
fn io_error_code(error: &std::io::Error) -> Option<&'static str> {
    Some(match error.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::AlreadyExists => "EEXIST",
        std::io::ErrorKind::BrokenPipe => "EPIPE",
        std::io::ErrorKind::WouldBlock => "EAGAIN",
        std::io::ErrorKind::InvalidInput => "EINVAL",
        std::io::ErrorKind::TimedOut => "ETIMEDOUT",
        std::io::ErrorKind::WriteZero => "ENOSPC",
        _ => return None,
    })
}

/// The one-shot Linux launch request: the target's cwd and its complete environment.
///
/// Upstream: `LinuxLaunchRequest`. The environment is *complete*, not a delta: the bootstrap execs
/// the target with exactly these entries, because the privileged runner's own environment is
/// systemd's and must not leak into the target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxLaunchRequest {
    /// The working directory the target must start in.
    pub cwd: String,
    /// The complete environment the target must receive.
    pub env: BTreeMap<String, String>,
    /// Pipe descriptors whose shared status flags the runner may change; `0`, `1`, `2` only.
    pub pipe_fds: Vec<u8>,
    /// Whether a separate control channel was requested.
    pub control: bool,
}

/// The three private paths one Linux launch owns.
///
/// Upstream: `LinuxLaunchFiles`. Derived rather than carried on the wire: a locator is a directory
/// plus a fixed file name, so a compromised request cannot name the startup-error file it wants
/// written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchFiles {
    /// The private directory.
    pub directory: PathBuf,
    /// The one-shot launch request.
    pub request: PathBuf,
    /// The pre-exec failure, published beside the consumed request.
    pub startup_error: PathBuf,
}

/// The one Windows start message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsStartRequest {
    /// The working directory the target must start in.
    pub cwd: String,
    /// The complete environment the target must receive.
    pub env: BTreeMap<String, String>,
    /// Whether a separate control channel was requested.
    pub control: bool,
}

/// The single direct result a connected Windows runner sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowsRunnerResult {
    /// The target exited with this code.
    TargetExit {
        /// A non-negative exit code.
        exit_code: u32,
    },
    /// The runner failed before the target could be committed.
    Error(SerializedRunnerError),
}

/// Whether `value` is an object whose own keys are exactly `required` plus a subset of `optional`.
///
/// Upstream: `hasExactKeys`. The "no extras" half is the security-relevant one: a payload with an
/// unexpected member is a payload written by something that does not know this protocol, and
/// quietly ignoring the member is how a future version's semantics get silently dropped.
fn has_exact_keys(value: &Map<String, Value>, required: &[&str], optional: &[&str]) -> bool {
    required.iter().all(|key| value.contains_key(*key))
        && value
            .keys()
            .all(|key| required.contains(&key.as_str()) || optional.contains(&key.as_str()))
}

/// A JSON object whose every value is a string, as a map.
fn string_record(value: &Value) -> Option<BTreeMap<String, String>> {
    let object = value.as_object()?;
    object
        .iter()
        .map(|(key, entry)| Some((key.clone(), entry.as_str()?.to_owned())))
        .collect()
}

/// A bounded runner error object, if this is one.
///
/// Each optional member is checked *when present* rather than skipped when it fails the string
/// test: upstream's condition is `value.code === undefined || typeof value.code === 'string'`, so a
/// numeric `code` is a protocol violation and not an absent field. Collapsing the two would let a
/// runner's numeric errno through as "no code", and the caller branches on `code`.
fn runner_error(value: &Value) -> Option<SerializedRunnerError> {
    let object = value.as_object()?;
    if !has_exact_keys(object, &["name", "message"], &["code", "syscall", "path"]) {
        return None;
    }
    Some(SerializedRunnerError {
        name: object.get("name")?.as_str()?.to_owned(),
        message: object.get("message")?.as_str()?.to_owned(),
        code: optional_text(object, "code").ok()?,
        syscall: optional_text(object, "syscall").ok()?,
        path: optional_text(object, "path").ok()?,
    })
}

/// Reads one optional string member, keeping "absent" and "present but not a string" apart.
///
/// `Ok(None)` is the member's own absence and `Err(())` is its presence with the wrong type, which
/// refuses the whole record. Collapsing the two would turn a numeric `code` into "no code", and
/// `code` is what the caller on the other side branches on.
fn optional_text(object: &Map<String, Value>, key: &str) -> Result<Option<String>, ProtocolError> {
    match object.get(key) {
        None => Ok(None),
        Some(entry) => entry
            .as_str()
            .map(|text| Some(text.to_owned()))
            .ok_or(ProtocolError::RunnerErrorResult),
    }
}

/// A stdio descriptor list: an array whose every member is exactly `0`, `1`, or `2`.
///
/// # Where this is stricter than upstream
///
/// Upstream compares with `===`, so a JSON `1.0` is accepted as descriptor `1` because JavaScript
/// has one number type. Here [`Value::as_u64`] answers `Some` only for an integer literal, so a
/// `1.0` is refused. A parent writing `1.0` for a descriptor is a bug on its side, and refusing it
/// keeps the bootstrap from having to decide what it meant.
fn stdio_fds(value: &Value) -> Option<Vec<u8>> {
    let items = value.as_array()?;
    let mut fds = Vec::with_capacity(items.len());
    for item in items {
        let fd = u8::try_from(item.as_u64()?).ok().filter(|fd| *fd <= 2)?;
        fds.push(fd);
    }
    Some(fds)
}

/// Strictly decodes the one-shot Linux launch request.
///
/// Upstream: `consumeLinuxLaunchRequest` minus its file operations. The file is written by the
/// parent, read once by the privileged runner, and unlinked; the content is the entire environment
/// of a process that is about to exec arbitrary target argv, so it is validated as an allowlist.
pub fn parse_linux_launch_request(value: &Value) -> Result<LinuxLaunchRequest, ProtocolError> {
    let object = value.as_object().ok_or(ProtocolError::LinuxLaunchRequest)?;
    if !has_exact_keys(object, &["cwd", "env"], &["control", "pipeFds"]) {
        return Err(ProtocolError::LinuxLaunchRequest);
    }
    let pipe_fds = match object.get("pipeFds") {
        None => Vec::new(),
        Some(fds) => stdio_fds(fds).ok_or(ProtocolError::LinuxLaunchRequest)?,
    };
    let cwd = object
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::LinuxLaunchRequest)?;
    let env = string_record(object.get("env").ok_or(ProtocolError::LinuxLaunchRequest)?)
        .ok_or(ProtocolError::LinuxLaunchRequest)?;
    let control = match object.get("control") {
        None => false,
        Some(flag) if flag.as_str() == Some("pipe") => true,
        Some(_) => return Err(ProtocolError::LinuxLaunchRequest),
    };
    Ok(LinuxLaunchRequest {
        cwd: cwd.to_owned(),
        env,
        pipe_fds,
        control,
    })
}

/// Strictly decodes the single Windows start message.
///
/// Upstream: `parseWindowsStartRequest`. Identical discipline to the Linux request, minus the
/// pipe-descriptor list, because Windows has no `pre_exec` to record them in.
pub fn parse_windows_start_request(value: &Value) -> Result<WindowsStartRequest, ProtocolError> {
    let object = value
        .as_object()
        .ok_or(ProtocolError::WindowsStartRequest)?;
    if !has_exact_keys(object, &["type", "cwd", "env"], &["control"]) {
        return Err(ProtocolError::WindowsStartRequest);
    }
    if object.get("type").and_then(Value::as_str) != Some("start") {
        return Err(ProtocolError::WindowsStartRequest);
    }
    let cwd = object
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::WindowsStartRequest)?;
    let env = string_record(
        object
            .get("env")
            .ok_or(ProtocolError::WindowsStartRequest)?,
    )
    .ok_or(ProtocolError::WindowsStartRequest)?;
    let control = match object.get("control") {
        None => false,
        Some(flag) if flag.as_str() == Some("pipe") => true,
        Some(_) => return Err(ProtocolError::WindowsStartRequest),
    };
    Ok(WindowsStartRequest {
        cwd: cwd.to_owned(),
        env,
        control,
    })
}

/// Whether a payload is the exact, payload-free Windows terminate control.
///
/// Upstream: `isWindowsTerminateRequest`. A predicate rather than a parser, because the terminate
/// message has no content: `{"type":"terminate","reason":...}` must be *refused*, not interpreted.
#[must_use]
pub fn is_windows_terminate_request(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| has_exact_keys(object, &["type"], &[]))
        && value.get("type").and_then(Value::as_str) == Some("terminate")
}

/// Strictly decodes one of the two Windows direct-result branches.
///
/// Upstream: `parseWindowsRunnerResult`. The `target-exit` code must be a non-negative safe
/// integer: a negative code is a signal in POSIX terms and this build would report it as an exit,
/// and a float or a string would render as something the caller never agreed to.
pub fn parse_windows_runner_result(value: &Value) -> Result<WindowsRunnerResult, ProtocolError> {
    let object = value.as_object().ok_or(ProtocolError::WindowsResult)?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::WindowsResult)?;
    if kind == "error" {
        return parse_error_result(value).map(WindowsRunnerResult::Error);
    }
    if kind == "target-exit" {
        if !has_exact_keys(object, &["type", "exitCode"], &[]) {
            return Err(ProtocolError::TargetExit);
        }
        let code = object
            .get("exitCode")
            .and_then(Value::as_i64)
            .and_then(|code| u32::try_from(code).ok())
            .ok_or(ProtocolError::TargetExit)?;
        return Ok(WindowsRunnerResult::TargetExit { exit_code: code });
    }
    Err(ProtocolError::UnknownWindowsResult(kind.to_owned()))
}

/// Decodes the `error` branch of a runner result, with upstream's two distinct refusals.
fn parse_error_result(value: &Value) -> Result<SerializedRunnerError, ProtocolError> {
    let object = value.as_object().ok_or(ProtocolError::RunnerErrorResult)?;
    if !has_exact_keys(object, &["type", "error"], &[]) {
        return Err(ProtocolError::RunnerErrorResult);
    }
    let error = runner_error(object.get("error").unwrap_or(&Value::Null))
        .ok_or(ProtocolError::RunnerErrorResult)?;
    if object.get("type").and_then(Value::as_str) != Some("error") {
        return Err(ProtocolError::UnknownErrorResult);
    }
    Ok(error)
}

/// Derives the three private paths of a launch from an absolute request locator.
///
/// Upstream: `linuxLaunchFilesFromLocator`. The locator must be absolute and must be named
/// `launch-request.json`; both checks exist so that the startup-error path a *runner* writes is
/// always the sibling of a directory the *parent* created. Without them, `../` in a locator would
/// let one launch publish a failure into another launch's directory.
pub fn linux_launch_files_from_locator(request_path: &str) -> Result<LaunchFiles, ProtocolError> {
    let path = Path::new(request_path);
    if !path.is_absolute()
        || path.file_name().and_then(|name| name.to_str()) != Some("launch-request.json")
    {
        return Err(ProtocolError::LinuxLaunchLocator);
    }
    let directory = path
        .parent()
        .ok_or(ProtocolError::LinuxLaunchLocator)?
        .to_path_buf();
    Ok(LaunchFiles {
        request: path.to_path_buf(),
        startup_error: directory.join("startup-error.json"),
        directory,
    })
}
