//! Tests for sandbox path checks (`sandbox_path`).
//!
//! The properties that matter are all about what the *filesystem* does with a path rather than what
//! its text looks like: a `..` after a symlink, a link that leaves the root, and a name that merely
//! shares a prefix are the cases where a string comparison gives the comfortable but wrong answer.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.
//!
//! Every test runs in its own scratch directory because two of them depend on the parent directory
//! being writable and traversable: a shared root would make the symlink cases depend on each other.

#[path = "../src/sandbox_path.rs"]
mod sandbox_path;

use std::path::{Path, PathBuf};

use sandbox_path::{Sandbox, SandboxError, Verdict};

/// A scratch root under the system temporary directory, fresh for this process and test name.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-sandbox-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory created");
    // The canonical form is what a sandbox reports, so tests compare against this rather than against
    // the spelling they happened to use: `/tmp` is itself a symlink on some hosts.
    std::fs::canonicalize(&dir).expect("scratch directory resolves")
}

/// A sandbox rooted at a fresh scratch directory, cleaned up when the test ends.
struct Fixture {
    /// The canonical scratch root.
    root: PathBuf,
    /// A sandbox over it.
    sandbox: Sandbox,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = scratch(name);
        let sandbox = Sandbox::new(&root).expect("sandbox builds");
        Self { root, sandbox }
    }

    /// A path inside the root, as the string a tool call would carry.
    fn inside(&self, name: &str) -> String {
        self.root.join(name).to_string_lossy().into_owned()
    }

    /// Creates a symlink at `link` pointing at `target`.
    fn link(&self, link: &str, target: &Path) -> String {
        let path = self.root.join(link);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("link parent created");
        }
        std::os::unix::fs::symlink(target, &path).expect("symlink created");
        path.to_string_lossy().into_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Whether a verdict is the `Outside` one, for tests that only care about the conclusion.
fn is_outside(verdict: &Verdict) -> bool {
    matches!(verdict, Verdict::Outside { .. })
}

// ---- building a sandbox -----------------------------------------------------------------------

#[test]
fn a_relative_root_is_refused() {
    let error = Sandbox::new("relative/root").expect_err("a relative root cannot name a sandbox");

    assert_eq!(
        error,
        SandboxError::NotAbsolute {
            root: "relative/root".to_owned()
        }
    );
}

#[test]
fn a_missing_root_is_refused() {
    let missing = std::env::temp_dir().join(format!(
        "girder-sandbox-{}-absent-directory",
        std::process::id()
    ));

    let error = Sandbox::new(&missing).expect_err("a missing root cannot name a sandbox");

    assert!(matches!(error, SandboxError::Missing { .. }), "{error:?}");
}

#[test]
fn a_file_root_is_refused() {
    let fixture = Fixture::new("file-root");
    let file = fixture.root.join("not-a-directory");
    std::fs::write(&file, b"x").expect("file written");

    let error = Sandbox::new(&file).expect_err("a file cannot contain a path");

    assert!(matches!(error, SandboxError::Missing { .. }), "{error:?}");
}

#[test]
fn the_root_is_canonical() {
    let fixture = Fixture::new("canonical-root");
    let spelled = fixture.root.join(".");

    let sandbox = Sandbox::new(&spelled).expect("sandbox builds");

    assert_eq!(sandbox.root, fixture.root);
}

// ---- the simple cases -------------------------------------------------------------------------

#[test]
fn a_path_inside_is_inside() {
    let fixture = Fixture::new("inside");
    std::fs::create_dir_all(fixture.root.join("src")).expect("directory created");

    assert_eq!(
        fixture.sandbox.check(&fixture.inside("src/main.rs")),
        Verdict::Inside
    );
}

#[test]
fn the_root_itself_is_inside() {
    let fixture = Fixture::new("root-itself");

    assert_eq!(fixture.sandbox.check(&fixture.inside("")), Verdict::Inside);
}

#[test]
fn a_path_outside_is_outside() {
    let fixture = Fixture::new("outside");

    let verdict = fixture.sandbox.check("/etc/passwd");

    match verdict {
        Verdict::Outside { resolved, root } => {
            assert_eq!(resolved, "/etc/passwd");
            assert_eq!(root, fixture.root.to_string_lossy());
        }
        other => panic!("expected Outside, got {other:?}"),
    }
}

// ---- lexical escapes --------------------------------------------------------------------------

#[test]
fn parent_segments_cannot_climb_out() {
    let fixture = Fixture::new("dotdot");

    // `..` here is resolved against the root, so this names the parent of the root, not the root.
    let verdict = fixture
        .sandbox
        .check(&format!("{}/../etc/passwd", fixture.root.display()));

    assert!(is_outside(&verdict), "{verdict:?}");
}

#[test]
fn a_deep_climb_is_outside() {
    let fixture = Fixture::new("deep-dotdot");

    let verdict = fixture
        .sandbox
        .check(&format!("{}/a/b/../../../..", fixture.root.display()));

    assert!(is_outside(&verdict), "{verdict:?}");
}

#[test]
fn parent_segments_that_stay_inside_are_inside() {
    let fixture = Fixture::new("dotdot-inside");
    std::fs::create_dir_all(fixture.root.join("a/b")).expect("directory created");

    let verdict = fixture
        .sandbox
        .check(&format!("{}/a/b/../c", fixture.root.display()));

    assert_eq!(verdict, Verdict::Inside);
}

#[test]
fn climbing_above_the_filesystem_root_stays_at_the_root() {
    let fixture = Fixture::new("root-climb");

    // `/..` is `/` for the kernel; reporting it as an escape would invent a location that does not
    // exist, so this must be Outside for the real reason rather than the arithmetic one.
    let verdict = fixture.sandbox.check("/../../etc/passwd");

    assert!(is_outside(&verdict), "{verdict:?}");
}

// ---- prefix similarity ------------------------------------------------------------------------

#[test]
fn a_sibling_with_a_shared_prefix_is_not_a_child() {
    let fixture = Fixture::new("prefix");
    let sibling = fixture.root.with_file_name(format!(
        "{}-sibling",
        fixture
            .root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&sibling).expect("sibling directory created");

    // The sibling's name begins with the root's name, so a string prefix check would accept it.
    assert!(
        sibling
            .to_string_lossy()
            .as_ref()
            .starts_with(fixture.root.to_string_lossy().as_ref())
    );

    let verdict = fixture.sandbox.check(&sibling.to_string_lossy());

    assert!(is_outside(&verdict), "{verdict:?}");

    let _ = std::fs::remove_dir_all(&sibling);
}

// ---- symlinks ---------------------------------------------------------------------------------

#[test]
fn a_symlink_leaving_the_root_is_outside() {
    let fixture = Fixture::new("link-out");
    let link = fixture.link("escape", Path::new("/etc/passwd"));

    let verdict = fixture.sandbox.check(&link);

    match verdict {
        Verdict::Outside { resolved, .. } => assert_eq!(resolved, "/etc/passwd"),
        other => panic!("expected Outside, got {other:?}"),
    }
}

#[test]
fn a_symlinked_directory_leaving_the_root_is_outside() {
    let fixture = Fixture::new("link-dir-out");
    let link = fixture.link("tmp-link", Path::new("/tmp"));

    assert!(is_outside(&fixture.sandbox.check(&link)), "link itself");
    // The interesting form: a path *through* the link. It looks inside the root as a string.
    let through = format!("{link}/something");
    assert!(is_outside(&fixture.sandbox.check(&through)), "{through}");
}

#[test]
fn a_parent_segment_after_a_symlink_follows_the_filesystem() {
    let fixture = Fixture::new("link-dotdot");
    let elsewhere = fixture.root.with_file_name("elsewhere-for-link-dotdot");
    std::fs::create_dir_all(&elsewhere).expect("target directory created");
    std::fs::write(elsewhere.join("secret"), b"x").expect("secret written");
    let link = fixture.link("hop", &elsewhere);

    // Lexically this pops `hop` and lands back in the root, which is inside. The kernel follows the
    // link to `elsewhere` and then goes up, which is outside. This test is the reason the module
    // exists: a lexical check answers "inside" here.
    let through = format!("{link}/../secret");
    let verdict = fixture.sandbox.check(&through);

    assert!(is_outside(&verdict), "{verdict:?}");

    let _ = std::fs::remove_dir_all(&elsewhere);
}

#[test]
fn a_symlink_pointing_back_inside_is_inside() {
    let fixture = Fixture::new("link-in");
    std::fs::create_dir_all(fixture.root.join("real")).expect("directory created");
    let link = fixture.link("alias", &fixture.root.join("real"));

    assert_eq!(fixture.sandbox.check(&link), Verdict::Inside);
    let through = format!("{link}/file.txt");
    assert_eq!(fixture.sandbox.check(&through), Verdict::Inside);
}

#[test]
fn a_symlink_to_a_relative_target_is_resolved_from_its_directory() {
    let fixture = Fixture::new("link-relative");
    std::fs::create_dir_all(fixture.root.join("a/b")).expect("directory created");
    // Relative targets are read from the link's own directory, not from the process's cwd.
    let link = fixture.link("a/up", Path::new(".."));

    // `..` from `a` is the root itself: inside.
    assert_eq!(fixture.sandbox.check(&link), Verdict::Inside);
    // And a path through it escapes: `a/up/..` from the root is the root's parent.
    let through = format!("{link}/..");
    assert!(is_outside(&fixture.sandbox.check(&through)), "{through}");
}

#[test]
fn a_symlink_loop_is_unknown_rather_than_inside() {
    let fixture = Fixture::new("link-loop");
    let first = fixture.root.join("first");
    let second = fixture.root.join("second");
    std::os::unix::fs::symlink(&second, &first).expect("first link created");
    std::os::unix::fs::symlink(&first, &second).expect("second link created");

    let verdict = fixture.sandbox.check(&first.to_string_lossy());

    // Either conclusion would be a claim the check cannot support, so it must say so.
    assert!(matches!(verdict, Verdict::Unknown { .. }), "{verdict:?}");
}

#[test]
fn a_dangling_symlink_follows_to_its_missing_target() {
    let fixture = Fixture::new("link-dangling");
    let link = fixture.link("dangling", &fixture.root.join("never-created"));

    let verdict = fixture.sandbox.check(&link);

    // A link whose target does not exist still has a target, and following it is the honest answer;
    // here that target is inside, so the verdict is Inside rather than Unknown.
    assert_eq!(verdict, Verdict::Inside);
}

// ---- paths that do not exist yet --------------------------------------------------------------

#[test]
fn a_new_file_inside_is_inside() {
    let fixture = Fixture::new("new-file");

    assert_eq!(
        fixture.sandbox.check(&fixture.inside("src/a/b/new.rs")),
        Verdict::Inside
    );
}

#[test]
fn a_new_file_through_a_missing_parent_that_climbs_out_is_outside() {
    let fixture = Fixture::new("new-file-escape");

    let verdict = fixture.sandbox.check(&format!(
        "{}/missing/../../etc/passwd",
        fixture.root.display()
    ));

    assert!(is_outside(&verdict), "{verdict:?}");
}

#[test]
fn a_new_file_under_a_symlinked_directory_that_leaves_is_outside() {
    let fixture = Fixture::new("new-file-link");
    let link = fixture.link("out", Path::new("/tmp"));

    let verdict = fixture
        .sandbox
        .check(&format!("{link}/not-created-yet.txt"));

    assert!(is_outside(&verdict), "{verdict:?}");
}

// ---- relative paths ---------------------------------------------------------------------------

#[test]
fn a_relative_path_is_taken_from_the_root() {
    let fixture = Fixture::new("relative");

    assert_eq!(fixture.sandbox.check("src/main.rs"), Verdict::Inside);
    assert_eq!(fixture.sandbox.check("./src/main.rs"), Verdict::Inside);
    assert_eq!(fixture.sandbox.check("src/../Cargo.toml"), Verdict::Inside);
}

#[test]
fn a_relative_path_can_climb_out() {
    let fixture = Fixture::new("relative-escape");

    assert!(
        is_outside(&fixture.sandbox.check("../outside.txt")),
        "one up"
    );
    assert!(
        is_outside(&fixture.sandbox.check("src/../../outside.txt")),
        "two up"
    );
}

#[test]
fn an_absolute_path_is_not_reanchored_to_the_root() {
    let fixture = Fixture::new("absolute");
    // If an absolute path were joined onto the root, this would report Inside; it names `/etc`, which
    // has nothing to do with the root.
    std::fs::write(fixture.root.join("etc"), b"x").expect("file written");

    let verdict = fixture.sandbox.check("/etc");

    assert!(is_outside(&verdict), "{verdict:?}");
}

// ---- the reported details ---------------------------------------------------------------------

#[test]
fn an_outside_verdict_names_both_places() {
    let fixture = Fixture::new("details");

    let verdict = fixture.sandbox.check("/etc");

    match verdict {
        Verdict::Outside { resolved, root } => {
            assert_eq!(resolved, "/etc");
            assert_eq!(root, fixture.root.to_string_lossy());
        }
        other => panic!("expected Outside, got {other:?}"),
    }
}

#[test]
fn an_unknown_verdict_explains_itself() {
    let fixture = Fixture::new("unknown-detail");
    let first = fixture.root.join("a");
    let second = fixture.root.join("b");
    std::os::unix::fs::symlink(&second, &first).expect("link created");
    std::os::unix::fs::symlink(&first, &second).expect("link created");

    match fixture.sandbox.check(&first.to_string_lossy()) {
        Verdict::Unknown { detail } => assert!(!detail.is_empty(), "detail is empty"),
        other => panic!("expected Unknown, got {other:?}"),
    }
}
