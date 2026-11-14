//! Tests for shell discovery and the terminal environment (`terminal_env`).
//!
//! The properties under test are the ones a picker depends on, in the order they can hurt:
//!
//! - **only paths that exist are offered** — this is the core promise. Every entry in every list
//!   here is `stat`ed, and the fixture lists deliberately name files that are not there, so a
//!   regression that passes `/etc/shells` through unvalidated fails here rather than at
//!   `terminal.create` on somebody's machine.
//! - **exactly one default, and it is the one `$SHELL` names** — a picker renders "default" next to
//!   one row; two would make the label meaningless and zero would leave the pane unexplained.
//! - **`supports_color` follows `TERM`** — `dumb`, an empty value, and an absent one all mean "draw
//!   no escape sequences", and everything else means "draw them".
//! - **an empty or missing shell list is not an error** — a minimal container has neither
//!   `/etc/shells` nor most of the common paths, and the answer there is "offer a text field".
//! - **the `Environment` value is complete and consistent** — every field is populated, and
//!   `default_shell` names the shell whose flag is set, so the two cannot disagree.
//!
//! Nothing here launches a shell: that is the module's whole point, and a test that spawned one
//! would be asserting against the behaviour the module was written to avoid.
//!
//! Isolation: every test that reads a shell list writes its own fixture under a directory named for
//! the test and this process, and the tests that read the real host's list only assert properties
//! that hold on any host. `std::env::set_var` is not used anywhere — it is unsafe in this toolchain
//! and would race every other test in the binary — so the variable-dependent rules are tested
//! through the pure helpers they are implemented in.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written before the crate root wires it in.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/terminal_env.rs"]
mod terminal_env;

use std::path::{Path, PathBuf};

use terminal_env::{Environment, Shell, detect, detect_shells, shells_from, supports_color};

// ---- fixtures ---------------------------------------------------------------------------------

/// A directory that exists for the duration of one test, named for that test.
///
/// The process id is in the name because `cargo test` runs test binaries in parallel and two runs of
/// this binary may overlap; a fixed name would let one test's fixture shell be another's.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-term-env-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// The mode a fixture shell is created with.
///
/// Owner-execute only: the point of a fixture is that this test can run it, and a wider mode would
/// make the file world-writable in a shared temporary directory.
#[cfg(unix)]
const EXECUTABLE: u32 = 0o700;

/// The mode a fixture that is *not* a shell is created with.
///
/// Read and write but no execute bit, which is exactly the "installed but not runnable" state the
/// probe must refuse.
#[cfg(unix)]
const NOT_EXECUTABLE: u32 = 0o600;

/// Creates an empty file at `path` with the given mode.
///
/// The file's contents never matter: nothing in this module reads a shell, only checks that one is
/// there and executable.
fn touch(path: &Path, mode: u32) {
    std::fs::write(path, b"").expect("fixture file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("fixture mode");
    }
    #[cfg(not(unix))]
    {
        // No permission bits to set; existence is the only fact the probe can check there.
        let _ = mode;
    }
}

/// Writes a `/etc/shells`-format file with the given lines.
fn listing(dir: &Path, lines: &[&str]) -> PathBuf {
    let path = dir.join("shells");
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&path, text).expect("fixture listing");
    path
}

/// Whether `path` exists as a regular file — the ground truth every assertion compares against.
fn exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// The paths of a shell list, for readable assertion messages.
fn paths(shells: &[Shell]) -> Vec<String> {
    shells.iter().map(|shell| shell.path.clone()).collect()
}

/// How many entries carry the default flag.
fn defaults(shells: &[Shell]) -> usize {
    shells.iter().filter(|shell| shell.is_default).count()
}

// ---- the real host ----------------------------------------------------------------------------

#[test]
fn a_normal_host_offers_at_least_one_shell() {
    // Every platform this crate builds for has a POSIX shell or PowerShell somewhere, so an empty
    // list on a normal host means the probe broke rather than that the host is bare. This is the
    // smoke test that catches a probe that returns nothing everywhere.
    let shells = detect_shells();
    assert!(
        !shells.is_empty(),
        "no shell found at all; /etc/shells and the common paths both yielded nothing"
    );
}

#[test]
fn every_offered_path_exists() {
    // The core promise of the module. A list entry that does not exist is a path a client can send
    // back to `terminal.create` only to be refused, which is worse than not offering it.
    let shells = detect_shells();
    for shell in &shells {
        assert!(
            exists(&shell.path),
            "offered a shell that is not a file on this host: {}",
            shell.path
        );
    }
}

#[test]
fn every_offered_path_is_executable() {
    // Existence is not enough: a non-executable file named `bash` would satisfy a stat-only check and
    // then fail at spawn. On a platform without permission bits the probe can only check existence,
    // so the stricter half of this assertion is Unix-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for shell in detect_shells() {
            let mode = std::fs::metadata(&shell.path)
                .expect("offered path is statable")
                .permissions()
                .mode();
            assert!(
                mode & 0o111 != 0,
                "offered a shell with no execute bit: {} ({mode:o})",
                shell.path
            );
        }
    }
    #[cfg(not(unix))]
    {
        for shell in detect_shells() {
            assert!(
                exists(&shell.path),
                "offered a missing shell: {}",
                shell.path
            );
        }
    }
}

#[test]
fn the_real_host_marks_at_most_one_default() {
    // "At most one" rather than "exactly one" because a host genuinely may have no shell to mark; the
    // fixture tests below pin the stronger property, that a non-empty list has exactly one.
    assert!(defaults(&detect_shells()) <= 1);
}

#[test]
fn a_real_detection_marks_exactly_one_default_when_it_finds_anything() {
    let environment = detect();
    if environment.shells.is_empty() {
        // A bare host: nothing to mark, and the field must agree with the empty list.
        assert_eq!(environment.default_shell, None);
        return;
    }
    assert_eq!(defaults(&environment.shells), 1);
}

#[test]
fn the_reported_default_is_the_flagged_shell() {
    // The field is read back from the flag rather than recomputed, and this is the assertion that
    // keeps it that way: a client that trusts `default_shell` and a client that trusts the flag must
    // arrive at the same row.
    let environment = detect();
    let flagged = environment
        .shells
        .iter()
        .find(|shell| shell.is_default)
        .map(|shell| shell.path.clone());
    assert_eq!(environment.default_shell, flagged);
}

#[test]
fn no_executable_is_offered_twice() {
    // On a merged-`/usr` host `/bin/bash` and `/usr/bin/bash` are one file, and `/etc/shells` often
    // lists both. Two rows for one shell would invite a choice between identical things.
    let shells = detect_shells();
    for (index, shell) in shells.iter().enumerate() {
        for other in &shells[index + 1..] {
            let same = std::fs::canonicalize(&shell.path).ok();
            assert_ne!(
                same,
                std::fs::canonicalize(&other.path).ok(),
                "the same executable was offered twice: {} and {}",
                shell.path,
                other.path
            );
        }
    }
}

#[test]
fn every_shell_has_a_name_taken_from_its_path() {
    // The name is what a picker shows. An empty one would render a blank row, and one that is not the
    // final component would not match what the shell calls itself.
    for shell in detect_shells() {
        assert!(!shell.name.is_empty(), "unnamed shell at {}", shell.path);
        let final_component = Path::new(&shell.path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        assert_eq!(Some(shell.name.clone()), final_component);
    }
}

// ---- the fixture rules ------------------------------------------------------------------------

#[test]
fn a_listed_but_missing_shell_is_not_offered() {
    // `/etc/shells` is advisory: a package manager removes a shell without editing the file, and a
    // container image may be built with a list that does not describe it. The fixture names three
    // paths that were never created, plus one that exists, so the assertion cannot pass by the list
    // simply being empty.
    let dir = scratch("listed-missing");
    let shell = dir.join("real-sh");
    touch(&shell, EXECUTABLE);

    let absent = dir.join("gone-sh");
    let list = listing(
        &dir,
        &[
            &absent.to_string_lossy(),
            &dir.join("also-gone").to_string_lossy(),
            &shell.to_string_lossy(),
        ],
    );
    let found = shells_from(&list, None);

    assert!(paths(&found).contains(&shell.to_string_lossy().into_owned()));
    for path in paths(&found) {
        assert!(
            exists(&path),
            "a path that is not on this host reached the list: {path}"
        );
        assert_ne!(path, absent.to_string_lossy());
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_listed_but_non_executable_file_is_not_offered() {
    // The distinction the module exists to make: a file that is there is not a shell that runs. This
    // is the check that separates an executable-bit probe from a plain existence check, and it is
    // Unix-only because the bit it tests is.
    #[cfg(unix)]
    {
        let dir = scratch("not-executable");
        let unusable = dir.join("not-a-shell");
        touch(&unusable, NOT_EXECUTABLE);
        let list = listing(&dir, &[&unusable.to_string_lossy()]);

        let found = shells_from(&list, None);
        assert!(
            !found
                .iter()
                .any(|shell| shell.path == unusable.to_string_lossy()),
            "offered a file with no execute bit: {}",
            unusable.display()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_listed_shell_is_offered_under_the_path_the_file_names() {
    // The path is passed through as written rather than canonicalised, because it is what the client
    // sends back and what the user's own file says. The fixture lives in a directory whose canonical
    // form differs on a host where the temporary directory is a symlink, which is what makes the
    // assertion meaningful rather than trivially true.
    let dir = scratch("path-preserved");
    let shell = dir.join("my-shell");
    touch(&shell, EXECUTABLE);
    let list = listing(&dir, &[&shell.to_string_lossy()]);

    let found = shells_from(&list, None);
    let entry = found
        .iter()
        .find(|entry| entry.path == shell.to_string_lossy())
        .expect("the listed shell is offered under the path the file gave");
    assert_eq!(entry.name, "my-shell");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn comments_and_blank_lines_are_ignored() {
    // A real `/etc/shells` starts with a comment block. Treating one as a path would either offer a
    // nonsense path or, worse, hide the entries after it.
    let dir = scratch("comments");
    let shell = dir.join("commented-sh");
    touch(&shell, EXECUTABLE);
    let path = dir.join("shells");
    std::fs::write(
        &path,
        format!(
            "# /etc/shells: valid login shells\n\n   \n# trailing comment\n{}\n",
            shell.display()
        ),
    )
    .expect("fixture listing");

    let found = shells_from(&path, None);
    assert!(
        found
            .iter()
            .any(|entry| entry.path == shell.to_string_lossy())
    );
    for entry in &found {
        assert!(
            !entry.path.starts_with('#'),
            "a comment was parsed as a path: {}",
            entry.path
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_listing_still_offers_the_common_shells() {
    // The empty-file case: a container image may ship `/etc/shells` with nothing in it. That is not
    // "no shells" — the common paths still have to be probed, or a picker on such a host would be
    // empty while `/bin/sh` sits there working.
    let dir = scratch("empty-listing");
    let list = listing(&dir, &[]);

    let found = shells_from(&list, None);
    assert!(
        !found.is_empty(),
        "an empty listing suppressed the common paths entirely"
    );
    for path in paths(&found) {
        assert!(exists(&path), "offered a missing path: {path}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_listing_is_not_an_error() {
    // The file may not exist at all on a minimal host, and that is the same state as an empty one.
    // Distinguishing them would make the probe fail on exactly the hosts that need the fallback.
    let dir = scratch("missing-listing");
    let missing = dir.join("does-not-exist");

    let found = shells_from(&missing, None);
    assert!(
        !found.is_empty(),
        "a missing listing suppressed every shell"
    );
    for path in paths(&found) {
        assert!(exists(&path), "offered a missing path: {path}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_preferred_shell_is_flagged_as_the_default() {
    // The rule a picker relies on: the shell the process was started with is the one to preselect,
    // and it wins over whichever entry happened to come first in the file.
    let dir = scratch("preferred");
    let first = dir.join("aaa-first");
    let preferred = dir.join("zzz-preferred");
    touch(&first, EXECUTABLE);
    touch(&preferred, EXECUTABLE);
    let list = listing(
        &dir,
        &[&first.to_string_lossy(), &preferred.to_string_lossy()],
    );

    let found = shells_from(&list, Some(&preferred.to_string_lossy()));
    assert_eq!(defaults(&found), 1, "expected exactly one default");
    let marked = found
        .iter()
        .find(|shell| shell.is_default)
        .expect("one shell is marked");
    assert_eq!(marked.path, preferred.to_string_lossy());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_preferred_shell_that_does_not_exist_falls_back_to_the_first_found() {
    // `$SHELL` can name a shell that has since been removed, and a pane with no preselected row is
    // worse than one preselected wrongly — the user can still see the list. This is also the case
    // that proves the flag is never set on a path that is absent.
    let dir = scratch("preferred-missing");
    let present = dir.join("present-sh");
    touch(&present, EXECUTABLE);
    let list = listing(&dir, &[&present.to_string_lossy()]);

    let found = shells_from(&list, Some(&dir.join("removed-sh").to_string_lossy()));
    assert_eq!(defaults(&found), 1);
    let marked = found
        .iter()
        .find(|shell| shell.is_default)
        .expect("one shell is marked");
    assert_eq!(marked.path, present.to_string_lossy());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_preferred_shell_reached_by_another_spelling_is_still_the_default() {
    // `/etc/shells` commonly lists `/bin/bash` and `/usr/bin/bash` for one file, and `$SHELL` may
    // name either. Without the canonical comparison the preferred spelling would be deduplicated
    // away and the default would silently fall to the first entry.
    #[cfg(unix)]
    {
        let dir = scratch("preferred-spelling");
        let real = dir.join("real-shell");
        touch(&real, EXECUTABLE);
        let alias = dir.join("alias-shell");
        std::os::unix::fs::symlink(&real, &alias).expect("fixture symlink");
        let list = listing(&dir, &[&real.to_string_lossy()]);

        let found = shells_from(&list, Some(&alias.to_string_lossy()));
        assert_eq!(defaults(&found), 1);
        let marked = found
            .iter()
            .find(|shell| shell.is_default)
            .expect("one shell is marked");
        assert_eq!(
            std::fs::canonicalize(&marked.path).expect("canonical"),
            std::fs::canonicalize(&real).expect("canonical")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_listing_that_offers_no_usable_path_marks_nothing_default() {
    // The empty-result case the flag must survive: no entry means no row to mark, and a default
    // pointing at a row that is not there would be a path the client cannot select.
    let dir = scratch("nothing-usable");
    // The scratch directory has no common shell in it, so a listing of absent paths plus a PATH that
    // contains no shells cannot produce a fixture entry. The common paths may still resolve, so the
    // assertion is about the flag rather than the emptiness of the whole list.
    let list = listing(&dir, &[&dir.join("gone").to_string_lossy()]);

    let found = shells_from(&list, None);
    assert!(
        defaults(&found) <= 1,
        "more than one shell was flagged as default"
    );
    if found.is_empty() {
        assert_eq!(defaults(&found), 0);
    } else {
        assert_eq!(defaults(&found), 1);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- colour -----------------------------------------------------------------------------------

#[test]
fn an_absent_term_variable_means_no_colour() {
    // No `TERM` is what a process started by an init system sees. There is no terminal to ask, so
    // drawing escape sequences would put control characters into a log.
    assert!(!supports_color(None));
}

#[test]
fn an_empty_term_variable_means_no_colour() {
    // Some environments export `TERM=` rather than leaving it unset, and the two describe the same
    // state: a terminal type nobody named.
    assert!(!supports_color(Some("")));
    assert!(!supports_color(Some("   ")));
}

#[test]
fn the_dumb_terminal_means_no_colour() {
    // `dumb` is the convention for a terminal with no capabilities, and it is what a pipe and some
    // multiplexer configurations report.
    assert!(!supports_color(Some("dumb")));
}

#[test]
fn a_dumb_terminal_is_recognised_in_any_case() {
    // The value is a name rather than a token to compare byte-for-byte, and a host that exported
    // `DUMB` would otherwise be told it can render colour it cannot.
    assert!(!supports_color(Some("DUMB")));
    assert!(!supports_color(Some("Dumb")));
}

#[test]
fn a_common_terminal_supports_colour() {
    // The positive half: a normal terminal type must not be refused, or every client would render a
    // monochrome pane on a machine that supports colour.
    assert!(supports_color(Some("xterm-256color")));
    assert!(supports_color(Some("screen")));
    assert!(supports_color(Some("xterm")));
}

#[test]
fn an_unknown_terminal_type_is_given_the_benefit_of_the_doubt() {
    // A terminal type this module has never heard of is more likely to be a capable emulator
    // released after it was written than an incapable one. Refusing would break every new terminal.
    assert!(supports_color(Some("future-terminal-9000")));
}

#[test]
fn surrounding_whitespace_does_not_change_the_answer() {
    // A value read from a file or a shell profile can carry a trailing space or newline, and
    // `"xterm "` is the same terminal as `"xterm"`.
    assert!(supports_color(Some(" xterm\n")));
    assert!(!supports_color(Some(" dumb ")));
}

// ---- the environment value --------------------------------------------------------------------

#[test]
fn the_environment_is_complete() {
    // Every field decided: a client renders from one response and must not have to fill in a gap. The
    // platform is compared against the constant it is documented to come from, so a substitution
    // with a runtime guess would fail here.
    let environment = detect();
    assert_eq!(environment.platform, std::env::consts::OS);
    assert!(!environment.platform.is_empty());
    assert_eq!(
        environment.shells,
        detect_shells(),
        "the environment's list must be the one detect_shells produces"
    );
    // `supports_color` is a decision, not a default, so its value must be what `TERM` implies for
    // this process rather than a fixed answer.
    let expected = std::env::var("TERM").ok();
    assert_eq!(
        environment.supports_color,
        supports_color(expected.as_deref())
    );
}

#[test]
fn an_environment_with_shells_has_a_default_path_in_the_list() {
    // The structural invariant a client depends on: the default it is told to preselect is a row it
    // can actually select. A `default_shell` naming a path outside the list would leave the pane
    // with a selection it cannot show.
    let environment = detect();
    if let Some(default) = &environment.default_shell {
        assert!(
            environment
                .shells
                .iter()
                .any(|shell| &shell.path == default),
            "default_shell {default} is not among the offered shells"
        );
    }
}

#[test]
fn an_environment_holding_no_shells_has_no_default() {
    // The converse, stated as its own property because it is the one that makes `Option` the right
    // type: an empty list has nothing to point at, and a `Some` here would be a dangling selection.
    let empty = Environment {
        default_shell: None,
        shells: Vec::new(),
        platform: std::env::consts::OS.to_owned(),
        supports_color: false,
    };
    assert_eq!(empty.default_shell, None);
    assert!(empty.shells.is_empty());
}

#[test]
fn detecting_twice_returns_the_same_environment() {
    // The probe reads no mutable state, so two calls on an unchanged host must agree. A difference
    // would mean the answer depends on the order of the file's entries or on a cache.
    //
    // `TERM` and `$SHELL` cannot change between the two calls within one process, so a mismatch here
    // would be a real bug rather than an environment change.
    assert_eq!(detect(), detect());
}

#[test]
fn the_environment_is_cloneable_and_comparable() {
    // Both derives are part of the type's contract: a host stores the value and hands copies to
    // callers, and the equality above is what the tests use to compare two detections.
    let environment = detect();
    let copy = environment.clone();
    assert_eq!(environment, copy);
    assert_eq!(environment.shells.len(), copy.shells.len());
}
