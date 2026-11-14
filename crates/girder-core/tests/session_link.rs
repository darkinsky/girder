//! Tests for the cross-session relationship index.
//!
//! The properties that carry the weight are the ones a naive implementation gets wrong. A link must
//! be findable from both of its ends even though it is stored once, because the UI question is
//! symmetric and the storage question is not. A self-link must be refused, because it is the one
//! edge that cannot be true and the one a caller produces by accident. And traversal must terminate
//! on a cycle, because cycles are legitimate here and are exactly what the fork forest forbids.
//!
//! The rest pins behaviour a caller depends on afterwards: duplicates are refused on the ordered
//! pair, reachability follows direction, and hubs are ordered so that the same graph always produces
//! the same answer.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/session_link.rs"]
mod session_link;

use session_link::{Graph, Kind, Link, LinkError};

/// A link with a negligible timestamp and no note.
fn link(from: &str, to: &str, kind: Kind) -> Link {
    Link {
        from: from.to_owned(),
        to: to.to_owned(),
        kind,
        at_ms: 1_000,
        note: None,
    }
}

/// A graph holding exactly these links, asserting each one is accepted.
fn graph(links: &[(&str, &str)]) -> Graph {
    let mut graph = Graph::new();
    for (from, to) in links {
        graph
            .add(link(from, to, Kind::References))
            .expect("a well-formed link must be accepted");
    }
    graph
}

/// The `from -> to` pairs of a list of links, in order.
fn pairs(links: &[&Link]) -> Vec<(String, String)> {
    links
        .iter()
        .map(|link| (link.from.clone(), link.to.clone()))
        .collect()
}

#[test]
fn a_single_link_is_stored_once_but_visible_from_both_ends() {
    let graph = graph(&[("a", "b")]);

    let from_a = graph.related("a");
    let from_b = graph.related("b");

    assert_eq!(from_a.len(), 1, "a must see the link that starts at it");
    assert_eq!(from_b.len(), 1, "b must see the link that ends at it");
    assert_eq!(
        pairs(&from_a),
        vec![("a".to_owned(), "b".to_owned())],
        "the stored direction is reported unchanged"
    );
    assert_eq!(
        pairs(&from_b),
        vec![("a".to_owned(), "b".to_owned())],
        "the reverse query returns the one stored link, not a mirrored copy"
    );
    assert_eq!(from_a[0], from_b[0], "both queries name the same link");
}

#[test]
fn a_session_referencing_itself_is_refused() {
    let mut graph = Graph::new();

    let error = graph
        .add(link("a", "a", Kind::References))
        .expect_err("a self-link must be refused");

    assert_eq!(
        error,
        LinkError::SelfLink {
            session: "a".to_owned(),
        }
    );
    assert!(
        graph.related("a").is_empty(),
        "a refused link must not be stored even partially"
    );
    assert!(graph.hubs(10).is_empty(), "a refused link is not a hub");
}

#[test]
fn the_same_ordered_pair_is_refused_a_second_time() {
    let mut graph = graph(&[("a", "b")]);

    let error = graph
        .add(link("a", "b", Kind::Related))
        .expect_err("a duplicate pair must be refused even with a different kind");

    assert_eq!(
        error,
        LinkError::Duplicate {
            from: "a".to_owned(),
            to: "b".to_owned(),
        }
    );
    assert_eq!(
        graph.related("a").len(),
        1,
        "the refused link did not add a second edge"
    );
}

#[test]
fn the_reverse_pair_is_a_different_edge_and_is_accepted() {
    let mut graph = graph(&[("a", "b")]);
    graph
        .add(link("b", "a", Kind::Related))
        .expect("mutual reference is a separate fact and must be recordable");

    assert_eq!(
        pairs(&graph.related("a")),
        vec![
            ("a".to_owned(), "b".to_owned()),
            ("b".to_owned(), "a".to_owned()),
        ]
    );
    assert_eq!(graph.related("a").len(), graph.related("b").len());
}

#[test]
fn the_recorded_kind_is_reported_unchanged() {
    // The kind is the only thing distinguishing a continuation from a mere reference once the edge
    // exists, so a link that came back with a different kind would make the graph's answers wrong
    // in a way no count would reveal.
    let mut graph = Graph::new();
    graph
        .add(link("a", "b", Kind::Continues))
        .expect("a continuation is a valid edge");
    graph
        .add(link("a", "c", Kind::Related))
        .expect("a kinship edge is valid even from the same source");

    let related = graph.related("a");
    assert_eq!(
        related.iter().map(|link| link.kind).collect::<Vec<_>>(),
        vec![Kind::Continues, Kind::Related]
    );
    assert_eq!(
        graph.related("b")[0].kind,
        Kind::Continues,
        "the reverse query reports the kind as stored"
    );
}

#[test]
fn related_returns_both_directions_in_a_chain() {
    let graph = graph(&[("b", "a"), ("c", "b")]);

    let related = pairs(&graph.related("b"));
    assert_eq!(
        related,
        vec![
            ("b".to_owned(), "a".to_owned()),
            ("c".to_owned(), "b".to_owned())
        ],
        "b is pointed at by c and points at a, so both edges belong to it"
    );
}

#[test]
fn reachability_follows_the_recorded_direction() {
    let graph = graph(&[("a", "b"), ("b", "c")]);

    assert!(
        graph.reachable("a", "b"),
        "a directly references b, so b is reachable"
    );
    assert!(
        graph.reachable("a", "c"),
        "reachability is transitive along two edges"
    );
    assert!(
        !graph.reachable("c", "a"),
        "the edge is directed, so the reverse is not reachable"
    );
    assert!(
        !graph.reachable("a", "z"),
        "a session that appears in no link is unreachable"
    );
    assert!(
        !graph.reachable("z", "a"),
        "an unknown source reaches nothing but itself"
    );
    assert!(
        !graph.reachable("b", "a"),
        "a continues from nothing it points at"
    );
}

#[test]
fn a_session_reaches_itself_through_the_empty_path() {
    let graph = graph(&[("a", "b")]);

    assert!(graph.reachable("a", "a"));
    assert!(
        graph.reachable("unlinked", "unlinked"),
        "reflexivity does not depend on the session being present"
    );
}

#[test]
fn a_cycle_terminates_instead_of_looping() {
    let graph = graph(&[("a", "b"), ("b", "c"), ("c", "a")]);

    // Reaching these answers requires the walk to survive the loop; without a visited set it would
    // descend forever. `c` -> `a` closes the cycle, and `a` -> `c` goes all the way around it.
    assert!(graph.reachable("a", "c"));
    assert!(graph.reachable("c", "a"));
    assert!(graph.reachable("a", "b"));

    // The negative answer is the one that must exhaust the whole loop before returning.
    assert!(
        !graph.reachable("a", "outside"),
        "the walk must leave the cycle rather than spinning inside it and never answering"
    );

    assert_eq!(
        graph.related("a").len(),
        2,
        "a cycle does not duplicate links"
    );
}

#[test]
fn hubs_are_ordered_by_connection_count() {
    // b relates to three sessions, a to two, c to one, d to one.
    let graph = graph(&[("b", "a"), ("b", "c"), ("b", "d"), ("a", "c")]);

    assert_eq!(
        graph.hubs(10),
        vec![
            ("b".to_owned(), 3),
            ("a".to_owned(), 2),
            ("c".to_owned(), 2),
            ("d".to_owned(), 1),
        ],
        "counts are distinct counterpart sessions, ordered descending, ties by id"
    );
}

#[test]
fn hubs_truncate_after_sorting_and_respect_zero() {
    let graph = graph(&[("b", "a"), ("b", "c"), ("b", "d")]);

    assert_eq!(
        graph.hubs(2),
        vec![("b".to_owned(), 3), ("a".to_owned(), 1)],
        "the limit takes the top of the sorted list, not the first two recorded"
    );
    assert!(
        graph.hubs(0).is_empty(),
        "a zero limit is an empty answer, not an unlimited one"
    );
}

#[test]
fn a_mutual_pair_counts_as_one_neighbour_per_side() {
    let mut graph = Graph::new();
    graph
        .add(link("a", "b", Kind::Related))
        .expect("first direction is accepted");
    graph
        .add(link("b", "a", Kind::Related))
        .expect("reverse direction is accepted");

    assert_eq!(
        graph.hubs(10),
        vec![("a".to_owned(), 1), ("b".to_owned(), 1)],
        "two edges to the same peer are one neighbour each, not two"
    );
}

#[test]
fn an_empty_graph_answers_every_question_without_panicking() {
    let graph = Graph::new();

    assert!(graph.related("a").is_empty());
    assert!(graph.hubs(10).is_empty());
    assert!(graph.hubs(0).is_empty());
    assert!(graph.reachable("a", "a"), "reflexive even when empty");
    assert!(!graph.reachable("a", "b"));
}

#[test]
fn related_is_ordered_stably_across_repeated_reads() {
    let mut graph = Graph::new();
    for (from, to, at_ms) in [("a", "b", 2_000), ("c", "a", 1_000), ("a", "d", 3_000)] {
        graph
            .add(Link {
                from: from.to_owned(),
                to: to.to_owned(),
                kind: Kind::Related,
                at_ms,
                note: None,
            })
            .expect("distinct pairs are accepted");
    }

    let first = pairs(&graph.related("a"));
    let second = pairs(&graph.related("a"));

    assert_eq!(first, second, "the same graph answers the same way twice");
    assert_eq!(
        first,
        vec![
            ("c".to_owned(), "a".to_owned()),
            ("a".to_owned(), "b".to_owned()),
            ("a".to_owned(), "d".to_owned()),
        ],
        "ordered by timestamp, not by insertion"
    );
}
