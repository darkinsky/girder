//! Tests for the branch index over forked sessions.
//!
//! Three properties carry the weight, because each one is what stops the structure from silently
//! becoming something else: a child gets exactly one parent, a link can never close a loop, and a
//! cut past the parent's last event is refused rather than stored. The rest of the cases pin the
//! behaviour a client depends on afterwards — the ancestors of a chain, the order of siblings, and
//! a rendering whose indentation is the only thing distinguishing depth.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/session_branch.rs"]
mod session_branch;

use session_branch::{BranchError, Forest, Link};

/// A link with a negligible timestamp.
fn link(child: &str, parent: &str, from_seq: u64) -> Link {
    Link {
        child: child.to_owned(),
        parent: parent.to_owned(),
        from_seq,
        at_ms: 1_000,
    }
}

/// A forest holding exactly these links, asserting each one is accepted.
fn forest(links: &[(&str, &str, u64)]) -> Forest {
    let mut forest = Forest::new();
    for (child, parent, from_seq) in links {
        forest
            .add(link(child, parent, *from_seq))
            .expect("a well-formed link must be accepted");
    }
    forest
}

/// The ids of a list of links, as children, in order.
fn children_of(forest: &Forest, session: &str) -> Vec<String> {
    forest
        .children(session)
        .iter()
        .map(|link| link.child.clone())
        .collect()
}

// ---- an empty forest --------------------------------------------------------------------------

#[test]
fn an_empty_forest_has_no_children_and_no_ancestors() {
    let forest = Forest::new();

    assert!(forest.children("a").is_empty());
    assert!(forest.ancestors("a").is_empty());
}

#[test]
fn an_empty_forest_renders_its_root_alone() {
    // A session nobody has forked is a leaf, and drawing a leaf is one line plus a newline.
    assert_eq!(Forest::new().render("a"), "a\n");
}

#[test]
fn a_forest_with_no_recorded_links_is_the_default() {
    // `Default` and `new` must agree, or a caller's `Forest::default()` would behave differently
    // from the documented constructor.
    assert_eq!(Forest::new().render("a"), Forest::default().render("a"));
}

// ---- a single fork ----------------------------------------------------------------------------

#[test]
fn one_fork_records_one_child() {
    let forest = forest(&[("a", "root", 4)]);

    assert_eq!(children_of(&forest, "root"), vec!["a"]);
    assert!(
        forest.children("a").is_empty(),
        "a child is not its own parent"
    );
}

#[test]
fn a_child_knows_its_parent_and_cut() {
    let forest = forest(&[("a", "root", 7)]);

    let ancestors = forest.ancestors("a");
    assert_eq!(ancestors.len(), 1);
    assert_eq!(ancestors[0].child, "a");
    assert_eq!(ancestors[0].parent, "root");
    assert_eq!(ancestors[0].from_seq, 7);
    assert_eq!(ancestors[0].at_ms, 1_000);
}

#[test]
fn a_root_has_no_ancestors() {
    let forest = forest(&[("a", "root", 1)]);

    assert!(
        forest.ancestors("root").is_empty(),
        "the root is where the walk stops"
    );
}

#[test]
fn a_session_the_forest_never_saw_is_a_root() {
    // Unknown and root are deliberately the same answer: neither has recorded ancestry.
    let forest = forest(&[("a", "root", 1)]);

    assert!(forest.ancestors("stranger").is_empty());
    assert!(forest.children("stranger").is_empty());
}

#[test]
fn a_fork_of_an_empty_parent_is_legal() {
    // A session with no completed turn forks empty, so a cut of zero is data, not an error.
    let mut forest = Forest::new();
    forest.record_events("root", 0);

    forest
        .add(link("a", "root", 0))
        .expect("a cut at zero is inside a parent with no events");
}

#[test]
fn a_cut_at_the_very_end_of_the_parent_is_legal() {
    // The bound is inclusive: `from_seq` is the number of events copied, so `len` is reachable.
    let mut forest = Forest::new();
    forest.record_events("root", 9);

    forest
        .add(link("a", "root", 9))
        .expect("a cut at the last event is inside the parent");
}

// ---- many children ----------------------------------------------------------------------------

#[test]
fn one_session_can_have_many_children() {
    let forest = forest(&[("a", "root", 4), ("b", "root", 4), ("c", "root", 8)]);

    assert_eq!(children_of(&forest, "root"), vec!["a", "b", "c"]);
}

#[test]
fn siblings_are_ordered_by_cut_then_by_id() {
    // Two forks at the same turn have no structural order, so the id breaks the tie: insertion
    // order must not leak into the output, or two runs would render differently.
    let forest = forest(&[("c", "root", 8), ("b", "root", 4), ("a", "root", 4)]);

    assert_eq!(children_of(&forest, "root"), vec!["a", "b", "c"]);
}

#[test]
fn a_deep_chain_reports_every_ancestor_nearest_first() {
    let forest = forest(&[("a", "root", 1), ("b", "a", 2), ("c", "b", 3)]);

    let ancestors = forest.ancestors("c");
    let chain: Vec<(&str, &str)> = ancestors
        .iter()
        .map(|link| (link.child.as_str(), link.parent.as_str()))
        .collect();

    assert_eq!(chain, vec![("c", "b"), ("b", "a"), ("a", "root")]);
}

// ---- one parent, and only one (the core rule) -------------------------------------------------

#[test]
fn a_second_parent_is_refused() {
    // Two parents would make this a graph, and "the ancestors of a session" would stop being a path.
    let mut forest = forest(&[("a", "root", 4)]);

    let error = forest
        .add(link("a", "other", 5))
        .expect_err("a child with a parent cannot gain another");

    assert_eq!(
        error,
        BranchError::AlreadyHasParent {
            session: "a".to_owned(),
            parent: "root".to_owned(),
        }
    );
}

#[test]
fn the_same_link_twice_is_refused() {
    // A retried fork is still two records for one child, and only one of them can be true.
    let mut forest = forest(&[("a", "root", 4)]);

    let error = forest
        .add(link("a", "root", 4))
        .expect_err("a duplicate is still a second parent");

    assert!(matches!(error, BranchError::AlreadyHasParent { .. }));
}

#[test]
fn a_refused_second_parent_leaves_the_forest_unchanged() {
    let mut forest = forest(&[("a", "root", 4)]);
    let before = forest.render("root");

    let _ = forest.add(link("a", "other", 5));

    assert_eq!(forest.render("root"), before);
    assert_eq!(children_of(&forest, "other"), Vec::<String>::new());
}

#[test]
fn a_second_child_of_the_same_parent_is_not_a_second_parent() {
    // The rule is one parent per child, not one child per parent: this must stay legal.
    let mut forest = forest(&[("a", "root", 4)]);

    forest
        .add(link("b", "root", 4))
        .expect("a parent can be forked more than once");
}

// ---- cycles (the core rule) -------------------------------------------------------------------

#[test]
fn a_session_cannot_fork_from_itself() {
    let mut forest = Forest::new();

    let error = forest
        .add(link("a", "a", 0))
        .expect_err("a self-link has no root");

    assert_eq!(
        error,
        BranchError::Cycle {
            session: "a".to_owned(),
        }
    );
}

#[test]
fn a_two_session_cycle_is_refused() {
    // a forked from root, then root claims to have forked from a.
    let mut forest = forest(&[("a", "root", 4)]);

    let error = forest
        .add(link("root", "a", 2))
        .expect_err("closing the loop would make `ancestors` run forever");

    assert_eq!(
        error,
        BranchError::Cycle {
            session: "root".to_owned(),
        }
    );
}

#[test]
fn a_cycle_through_a_long_chain_is_refused() {
    // a → b → c → root, and then a claims to fork from c: not a direct reversal, but the same loop.
    let mut forest = forest(&[("b", "a", 1), ("c", "b", 2)]);

    let error = forest
        .add(link("a", "c", 3))
        .expect_err("an ancestor is still an ancestor at depth three");

    assert!(matches!(error, BranchError::Cycle { .. }));
}

#[test]
fn a_refused_cycle_leaves_the_forest_walkable() {
    // The point of refusing the link is that the structure never enters the state that loops.
    let mut forest = forest(&[("a", "root", 4)]);

    let _ = forest.add(link("root", "a", 2));

    assert_eq!(forest.ancestors("a").len(), 1);
    assert_eq!(forest.ancestors("root").len(), 0);
}

#[test]
fn a_sibling_link_is_not_a_cycle() {
    // a and b are both children of root; b forking from a is a new edge, not a loop.
    let mut forest = forest(&[("a", "root", 4), ("b", "root", 4)]);

    let error = forest
        .add(link("b", "a", 1))
        .expect_err("b already has a parent");

    assert!(
        matches!(error, BranchError::AlreadyHasParent { .. }),
        "the refusal must name the real problem, not a cycle"
    );
}

// ---- a cut past the parent --------------------------------------------------------------------

#[test]
fn a_cut_past_a_known_parent_is_refused() {
    let mut forest = Forest::new();
    forest.record_events("root", 6);

    let error = forest
        .add(link("a", "root", 7))
        .expect_err("there is no event seven");

    assert_eq!(
        error,
        BranchError::SeqOutOfRange {
            seq: 7,
            parent_events: 6,
        }
    );
}

#[test]
fn a_cut_past_an_unmeasured_parent_is_accepted() {
    // The bound is unknown rather than zero: refusing would forbid forking a parent the caller
    // only knows by id and summary (a remote catalog row).
    let mut forest = Forest::new();

    forest
        .add(link("a", "root", 1_000))
        .expect("an unmeasured parent cannot be out of range");
}

#[test]
fn a_recorded_length_applies_only_to_later_links() {
    // The length is reported after the first link exists; that link must not be invalidated.
    let mut forest = forest(&[("a", "root", 9)]);
    forest.record_events("root", 2);

    assert_eq!(forest.ancestors("a").len(), 1);
    assert!(matches!(
        forest.add(link("b", "root", 9)),
        Err(BranchError::SeqOutOfRange { .. })
    ));
}

#[test]
fn a_grown_parent_accepts_a_later_cut() {
    // A log only grows, so a newer report can make an earlier refusal valid.
    let mut forest = Forest::new();
    forest.record_events("root", 2);
    assert!(forest.add(link("a", "root", 5)).is_err());

    forest.record_events("root", 5);

    forest
        .add(link("a", "root", 5))
        .expect("the parent has grown to include the cut");
}

#[test]
fn the_longest_reported_parent_wins() {
    // Two reports, latest wins; the check must use the current value rather than the first one.
    let mut forest = Forest::new();
    forest.record_events("root", 1);
    forest.record_events("root", 8);

    forest
        .add(link("a", "root", 8))
        .expect("the second report is the one that counts");
}

// ---- rendering --------------------------------------------------------------------------------

#[test]
fn render_marks_depth_with_indentation() {
    // `a` is the only child of `root`, so it takes the corner and the bar does not continue: depth
    // then shows as the four spaces that indent `b` under it.
    let forest = forest(&[("a", "root", 4), ("b", "a", 2)]);

    assert_eq!(forest.render("root"), "root\n└── a @4\n    └── b @2\n");
}

#[test]
fn render_uses_a_corner_for_the_last_child() {
    let forest = forest(&[("a", "root", 4), ("b", "root", 8)]);

    assert_eq!(forest.render("root"), "root\n├── a @4\n└── b @8\n");
}

#[test]
fn render_keeps_the_bar_past_a_subtree_that_has_siblings() {
    // The vertical bar under `a` has to run down through `a`'s child, because `b` follows it.
    let forest = forest(&[("a", "root", 1), ("b", "root", 2), ("c", "a", 1)]);

    assert_eq!(
        forest.render("root"),
        "root\n├── a @1\n│   └── c @1\n└── b @2\n"
    );
}

#[test]
fn render_of_a_subtree_starts_at_that_session() {
    // A client can draw any component from any node; the node becomes the root of the drawing.
    let forest = forest(&[("a", "root", 4), ("b", "a", 2), ("c", "b", 1)]);

    assert_eq!(forest.render("a"), "a\n└── b @2\n    └── c @1\n");
}

#[test]
fn render_of_a_deep_chain_indents_every_level() {
    let forest = forest(&[("a", "root", 1), ("b", "a", 2), ("c", "b", 3)]);

    assert_eq!(
        forest.render("root"),
        "root\n└── a @1\n    └── b @2\n        └── c @3\n"
    );
}

#[test]
fn render_of_a_leaf_is_one_line() {
    let forest = forest(&[("a", "root", 1)]);

    assert_eq!(forest.render("a"), "a\n");
}

#[test]
fn render_does_not_show_other_roots() {
    // A forest can hold several components; drawing one must not pull in the other.
    let forest = forest(&[("a", "root", 1), ("x", "other", 1)]);

    assert!(!forest.render("root").contains("x @"));
    assert!(!forest.render("other").contains("a @"));
}
