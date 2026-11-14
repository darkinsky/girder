//! Tests for the session-to-workspace index.
//!
//! Three properties carry the weight. A relative path must be refused, because it names a directory
//! only given a cwd and two relative paths that look alike can be different places — the whole point
//! of the index is to keep those apart. Two spellings of one absolute directory must collapse to one
//! workspace, or a shell-completed trailing slash would create a second entry for the same checkout.
//! And a session must be in exactly one workspace at a time, because a session runs in one directory
//! and an index that listed it under two would answer "where does this session live" with two
//! answers.
//!
//! A fourth property is a limitation rather than a feature, and it is tested so that the limitation
//! stays visible: `..` is not resolved, so `/a/../b` and `/b` are different workspaces here.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/session_workspace.rs"]
mod session_workspace;

use session_workspace::{Association, Index, WorkspaceError, WorkspaceId};

/// A workspace id, asserting the path is accepted.
fn workspace(path: &str) -> WorkspaceId {
    WorkspaceId::from_path(path).expect("a well-formed absolute path must be accepted")
}

/// The error a path is refused with.
///
/// Panics when the path is accepted, so a test that intends a refusal fails loudly rather than
/// asserting against a workspace id.
fn refused(path: &str) -> WorkspaceError {
    match WorkspaceId::from_path(path) {
        Ok(id) => panic!("`{path}` should have been refused, but it became `{id}`"),
        Err(error) => error,
    }
}

/// An association at a negligible time.
fn association(session: &str, path: &str) -> Association {
    Association {
        session: session.to_owned(),
        workspace: workspace(path),
        at_ms: 1_000,
    }
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

#[test]
fn a_trailing_slash_names_the_same_workspace() {
    assert_eq!(workspace("/a/b/"), workspace("/a/b"));
    assert_eq!(workspace("/a/b//"), workspace("/a/b"));
    assert_eq!(workspace("/a/b").as_str(), "/a/b");
}

#[test]
fn a_lone_dot_segment_names_the_same_workspace() {
    assert_eq!(workspace("/a/./b"), workspace("/a/b"));
    assert_eq!(workspace("/a/b/."), workspace("/a/b"));
    assert_eq!(workspace("/./a/b"), workspace("/a/b"));
    assert_eq!(workspace("/a/././b"), workspace("/a/b"));
}

#[test]
fn repeated_separators_name_the_same_workspace() {
    assert_eq!(workspace("/a//b///c"), workspace("/a/b/c"));
    assert_eq!(workspace("/a//b///c").as_str(), "/a/b/c");
}

#[test]
fn every_spelling_of_the_root_is_one_workspace() {
    for spelling in ["/", "//", "/.", "/./", "//.", "///"] {
        assert_eq!(
            workspace(spelling).as_str(),
            "/",
            "`{spelling}` should normalize to the root"
        );
    }
}

#[test]
fn normalization_leaves_the_path_in_its_own_terms() {
    // Not a case change, not a trim, not a symlink resolution: a directory whose name carries a
    // space or an unusual character is a directory like any other.
    assert_eq!(workspace("/a b/c").as_str(), "/a b/c");
    assert_eq!(workspace("/Ünïcode/dir").as_str(), "/Ünïcode/dir");
    assert_eq!(workspace("/a/b/").as_str(), "/a/b");
}

#[test]
fn normalization_is_idempotent() {
    let once = workspace("/a/b/./c//");
    let twice = workspace(once.as_str());
    assert_eq!(once, twice);
}

#[test]
fn the_identifier_renders_its_normalized_path() {
    assert_eq!(workspace("/a/b/").to_string(), "/a/b");
    assert_eq!(workspace("/a/b/").as_str(), "/a/b");
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[test]
fn a_relative_path_is_refused() {
    // The core refusal: `src` in one process and `src` in another are different directories, so an
    // identifier built from one would silently merge two workspaces.
    for path in ["a", "a/b", "./a", "../a", ".", "..", "src/"] {
        let error = refused(path);
        assert_eq!(
            error,
            WorkspaceError::NotAbsolute {
                path: path.to_owned()
            },
            "`{path}` must be refused as relative"
        );
    }
}

#[test]
fn a_refused_relative_path_keeps_its_spelling() {
    // The message is shown to whoever supplied the path, so it must echo what they sent rather than
    // a normalized form they would not recognise.
    match WorkspaceId::from_path("../a/b") {
        Err(WorkspaceError::NotAbsolute { path }) => assert_eq!(path, "../a/b"),
        other => panic!("expected NotAbsolute, got {other:?}"),
    }
}

#[test]
fn an_empty_path_is_refused() {
    for path in ["", " ", "\t", "  \n "] {
        assert_eq!(
            refused(path),
            WorkspaceError::Empty,
            "`{path:?}` must be refused as empty"
        );
    }
}

#[test]
fn emptiness_is_checked_before_absoluteness() {
    // An empty string is also not absolute. Reporting "not absolute" would send a caller looking for
    // a missing prefix when the real problem is that there is no path at all.
    assert_eq!(
        WorkspaceId::from_path("").unwrap_err(),
        WorkspaceError::Empty
    );
    assert!(matches!(
        WorkspaceId::from_path("x").unwrap_err(),
        WorkspaceError::NotAbsolute { .. }
    ));
}

// ---------------------------------------------------------------------------
// Association and query
// ---------------------------------------------------------------------------

#[test]
fn an_association_is_findable_from_both_ends() {
    let mut index = Index::new();
    index.associate(association("s1", "/a/project"));

    assert_eq!(index.workspace_of("s1"), Some(&workspace("/a/project")));
    assert_eq!(index.sessions_in(&workspace("/a/project")), vec!["s1"]);
    assert_eq!(index.count_in(&workspace("/a/project")), 1);
    assert_eq!(index.associated_at("s1"), Some(1_000));
}

#[test]
fn two_workspaces_are_told_apart() {
    // The question the flat layout cannot answer on its own: one session was started in `/a/project`
    // and another in `/b/project`, and the listing has to separate them.
    let mut index = Index::new();
    index.associate(association("s1", "/a/project"));
    index.associate(association("s2", "/b/project"));

    assert_eq!(index.sessions_in(&workspace("/a/project")), vec!["s1"]);
    assert_eq!(index.sessions_in(&workspace("/b/project")), vec!["s2"]);
    assert_ne!(
        index.workspace_of("s1"),
        index.workspace_of("s2"),
        "two sessions from two directories must not share a workspace"
    );
    assert_eq!(index.len(), 2);
}

#[test]
fn a_workspace_lists_its_sessions_in_a_stable_order() {
    let mut index = Index::new();
    for session in ["s3", "s1", "s2"] {
        index.associate(association(session, "/a/project"));
    }
    assert_eq!(
        index.sessions_in(&workspace("/a/project")),
        vec!["s1", "s2", "s3"]
    );
}

#[test]
fn an_unknown_session_has_no_workspace() {
    let index = Index::new();
    assert_eq!(index.workspace_of("nope"), None);
    assert_eq!(index.associated_at("nope"), None);
}

#[test]
fn a_workspace_nothing_is_in_is_empty_and_not_an_error() {
    let mut index = Index::new();
    index.associate(association("s1", "/a/project"));

    assert!(index.sessions_in(&workspace("/other")).is_empty());
    assert_eq!(index.count_in(&workspace("/other")), 0);
}

// ---------------------------------------------------------------------------
// Re-association
// ---------------------------------------------------------------------------

#[test]
fn a_session_re_associated_elsewhere_moves() {
    // A session runs in one directory at a time. Resuming it from another checkout must move it, not
    // add a second home: an index that kept the old one would claim the session is in a directory it
    // has not run in since.
    let mut index = Index::new();
    index.associate(association("s1", "/a/project"));
    index.associate(Association {
        session: "s1".to_owned(),
        workspace: workspace("/b/project"),
        at_ms: 2_000,
    });

    assert_eq!(index.workspace_of("s1"), Some(&workspace("/b/project")));
    assert!(index.sessions_in(&workspace("/a/project")).is_empty());
    assert_eq!(index.count_in(&workspace("/a/project")), 0);
    assert_eq!(index.sessions_in(&workspace("/b/project")), vec!["s1"]);
    assert_eq!(index.count_in(&workspace("/b/project")), 1);
    // The session is in exactly one workspace, so the total is unchanged by the move.
    assert_eq!(index.len(), 1);
}

#[test]
fn re_associating_with_the_same_workspace_does_not_duplicate_the_session() {
    let mut index = Index::new();
    index.associate(association("s1", "/a/project"));
    index.associate(Association {
        session: "s1".to_owned(),
        workspace: workspace("/a/project/"),
        at_ms: 2_000,
    });

    assert_eq!(index.sessions_in(&workspace("/a/project")), vec!["s1"]);
    assert_eq!(index.count_in(&workspace("/a/project")), 1);
    // The later observation is the one reported, because it is the one that describes the present.
    assert_eq!(index.associated_at("s1"), Some(2_000));
}

#[test]
fn the_later_association_wins_even_with_an_earlier_timestamp() {
    // Call order decides, not the clock. Two hosts describing one session may disagree by minutes,
    // and letting the larger number win would make which directory a session is in depend on skew.
    let mut index = Index::new();
    index.associate(Association {
        session: "s1".to_owned(),
        workspace: workspace("/a/project"),
        at_ms: 5_000,
    });
    index.associate(Association {
        session: "s1".to_owned(),
        workspace: workspace("/b/project"),
        at_ms: 1_000,
    });

    assert_eq!(index.workspace_of("s1"), Some(&workspace("/b/project")));
    assert_eq!(index.associated_at("s1"), Some(1_000));
}

#[test]
fn a_workspace_emptied_by_a_move_is_forgotten() {
    // An emptied workspace must not keep a key in the reverse index: a caller iterating workspaces
    // would otherwise walk history instead of the present.
    let mut index = Index::new();
    index.associate(association("s1", "/a/project"));
    index.associate(association("s1", "/b/project"));

    // Re-associating back into `/a/project` must start a fresh set rather than inherit a stale one.
    index.associate(association("s1", "/a/project"));
    assert_eq!(index.sessions_in(&workspace("/a/project")), vec!["s1"]);
    assert!(index.sessions_in(&workspace("/b/project")).is_empty());
}

#[test]
fn re_association_leaves_other_sessions_alone() {
    let mut index = Index::new();
    index.associate(association("s1", "/a/project"));
    index.associate(association("s2", "/a/project"));
    index.associate(association("s1", "/b/project"));

    assert_eq!(index.sessions_in(&workspace("/a/project")), vec!["s2"]);
    assert_eq!(index.sessions_in(&workspace("/b/project")), vec!["s1"]);
    assert_eq!(index.count_in(&workspace("/a/project")), 1);
}

// ---------------------------------------------------------------------------
// The `..` limitation
// ---------------------------------------------------------------------------

#[test]
fn parent_segments_are_not_resolved() {
    // A documented limitation, tested so it stays documented. Resolving `..` needs the filesystem:
    // `..` of a symlink is not the `..` of the spelling, and a case-insensitive volume has more than
    // one spelling of a name. This is a pure function over a string, so it cannot ask.
    let dotdot = workspace("/a/../b");
    let plain = workspace("/b");

    assert_eq!(dotdot.as_str(), "/a/../b");
    assert_ne!(dotdot, plain);
    assert_ne!(workspace("/a/b/.."), workspace("/a"));
}

#[test]
fn the_dotdot_limitation_shows_up_as_two_workspaces() {
    // The consequence a caller can observe: two paths naming one directory list separately, and a
    // caller that cares must canonicalize before associating.
    let mut index = Index::new();
    index.associate(association("s1", "/a/../b"));
    index.associate(association("s2", "/b"));

    assert_eq!(index.sessions_in(&workspace("/a/../b")), vec!["s1"]);
    assert_eq!(index.sessions_in(&workspace("/b")), vec!["s2"]);
    assert_eq!(index.len(), 2);
}

// ---------------------------------------------------------------------------
// Empty index
// ---------------------------------------------------------------------------

#[test]
fn a_new_index_answers_every_question_emptily() {
    let index = Index::new();
    assert!(index.is_empty());
    assert_eq!(index.len(), 0);
    assert!(index.sessions_in(&workspace("/a")).is_empty());
    assert_eq!(index.count_in(&workspace("/a")), 0);
    assert_eq!(index.workspace_of("s1"), None);
    assert_eq!(index.associated_at("s1"), None);
}

#[test]
fn the_default_index_is_empty() {
    assert_eq!(Index::default(), Index::new());
    assert!(Index::default().is_empty());
}
