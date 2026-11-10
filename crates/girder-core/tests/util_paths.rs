//! Tests for the code-language table and workspace path rules.
//!
//! Both are pure lookups, so the tests are about the cases a naive version gets wrong: a name with no
//! extension, a Windows absolute path, and a `..` that climbs.

use std::path::Path;

use girder_core::{
    for_extension, for_path, has_drive_prefix, is_absolute, is_unc, normalize_path, read_hint,
    relative_to, resolve_in_workspace, workspace_contains, workspace_display,
};

// ---- code language ----------------------------------------------------------------------------

#[test]
fn a_known_extension_maps_to_a_grammar_id() {
    assert_eq!(for_extension("rs"), Some("rust"));
    assert_eq!(for_extension(".ts"), Some("typescript"));
    assert_eq!(
        for_extension("RS"),
        Some("rust"),
        "the extension is case-insensitive"
    );
}

#[test]
fn an_unknown_extension_is_not_guessed() {
    // Guessing would highlight a language the file is not.
    assert_eq!(for_extension("xyz"), None);
    assert_eq!(for_extension(""), None);
}

#[test]
fn a_path_maps_by_its_final_extension() {
    assert_eq!(for_path("src/main.rs"), Some("rust"));
    assert_eq!(for_path("a/b/c.tsx"), Some("tsx"));
    assert_eq!(
        for_path(r"a\b\c.py"),
        Some("python"),
        "a Windows separator splits too"
    );
}

#[test]
fn a_named_file_without_an_extension_is_recognised() {
    assert_eq!(for_path("Dockerfile"), Some("dockerfile"));
    assert_eq!(for_path("Makefile"), Some("makefile"));
    assert_eq!(for_path("CMakeLists.txt"), Some("cmake"));
}

#[test]
fn a_name_with_no_extension_stays_unlisted() {
    // `Makefile` is listed by name; an arbitrary extensionless file is not.
    assert_eq!(for_path("run"), None);
}

#[test]
fn a_read_hint_collapses_a_variant_onto_its_base_language() {
    // A recorded hint outlives the code that wrote it, so it is a short stable id.
    assert_eq!(read_hint("a.tsx"), Some("typescript"));
    assert_eq!(read_hint("a.jsx"), Some("javascript"));
    assert_eq!(read_hint("a.jsonc"), Some("json"));
    assert_eq!(read_hint("a.rs"), Some("rust"));
    assert_eq!(read_hint("a.xyz"), None);
}

// ---- absolute paths ---------------------------------------------------------------------------

#[test]
fn a_posix_path_is_absolute() {
    assert!(is_absolute("/a/b"));
    assert!(is_absolute("/"));
}

#[test]
fn a_windows_drive_path_is_absolute() {
    // A host that accepts both spellings must classify both, or a Windows client's absolute path is read
    // as workspace-relative and joined onto the root.
    assert!(has_drive_prefix("C:/a"));
    assert!(has_drive_prefix("C:\\a"));
    assert!(is_absolute("C:/a"));
    assert!(is_absolute("C:\\a"));
}

#[test]
fn a_unc_path_is_absolute() {
    assert!(is_unc(r"\\server\share"));
    assert!(is_absolute(r"\\server\share"));
}

#[test]
fn a_relative_path_is_not_absolute() {
    assert!(!is_absolute("a/b"));
    assert!(!is_absolute("./a"));
    assert!(!is_absolute(""));
    assert!(!has_drive_prefix("1:/a"), "a digit is not a drive");
}

// ---- resolution -------------------------------------------------------------------------------

#[test]
fn a_relative_path_joins_the_root() {
    let root = Path::new("/work");

    assert_eq!(
        resolve_in_workspace(Some(root), "a/b"),
        std::path::PathBuf::from("/work/a/b")
    );
}

#[test]
fn a_leading_dot_slash_is_dropped() {
    // Two spellings of one relative path must produce one absolute form.
    let root = Path::new("/work");

    assert_eq!(
        resolve_in_workspace(Some(root), "./a"),
        resolve_in_workspace(Some(root), "a")
    );
}

#[test]
fn an_absolute_path_is_not_joined() {
    // Joining it would make the root a prefix of something that already names a place.
    let root = Path::new("/work");

    assert_eq!(
        resolve_in_workspace(Some(root), "/etc/hosts"),
        std::path::PathBuf::from("/etc/hosts")
    );
}

#[test]
fn a_path_with_no_root_resolves_to_itself() {
    assert_eq!(
        resolve_in_workspace(None, "a/b"),
        std::path::PathBuf::from("a/b")
    );
}

// ---- relativity and display -------------------------------------------------------------------

#[test]
fn a_path_inside_the_root_is_relative() {
    let root = Path::new("/work");

    assert_eq!(
        relative_to(root, Path::new("/work/a/b")),
        Some(std::path::PathBuf::from("a/b"))
    );
}

#[test]
fn a_path_outside_the_root_has_no_relative_form() {
    // A relative path climbing out with `..` looks like one that stays inside.
    assert_eq!(
        relative_to(Path::new("/work"), Path::new("/etc/hosts")),
        None
    );
}

#[test]
fn display_prefers_the_relative_form_and_falls_back() {
    let root = Path::new("/work");

    assert_eq!(workspace_display(Some(root), Path::new("/work/a")), "a");
    assert_eq!(
        workspace_display(Some(root), Path::new("/etc/hosts")),
        "/etc/hosts",
        "a path outside the root is shown as itself, so a reader can see it"
    );
    assert_eq!(workspace_display(None, Path::new("/work/a")), "/work/a");
}

// ---- normalisation and containment ------------------------------------------------------------

#[test]
fn normalisation_drops_dot_segments() {
    assert_eq!(
        normalize_path(Path::new("a/./b")),
        std::path::PathBuf::from("a/b")
    );
}

#[test]
fn normalisation_undoes_a_parent_segment() {
    assert_eq!(
        normalize_path(Path::new("a/b/../c")),
        std::path::PathBuf::from("a/c")
    );
}

#[test]
fn a_parent_below_the_root_is_dropped() {
    // `../..` from `/` is still `/`; keeping it would make a containment check believe an escape
    // happened when the filesystem says it did not.
    assert_eq!(
        normalize_path(Path::new("/../..")),
        std::path::PathBuf::from("/")
    );
}

#[test]
fn containment_holds_for_a_child() {
    assert!(workspace_contains(
        Path::new("/work"),
        Path::new("/work/a/b")
    ));
    assert!(workspace_contains(
        Path::new("/work"),
        Path::new("/work/a/../b")
    ));
}

#[test]
fn containment_fails_for_a_sibling() {
    assert!(!workspace_contains(
        Path::new("/work"),
        Path::new("/workspace/a")
    ));
}

#[test]
fn containment_fails_after_climbing_out() {
    assert!(!workspace_contains(
        Path::new("/work"),
        Path::new("/work/../etc/passwd")
    ));
}
