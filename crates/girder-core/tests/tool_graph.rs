//! Tests for the layered call planner.
//!
//! Three properties matter more than the shape of any single plan. The first is that a dependency
//! *always* separates two calls: a plan that packs a dependent pair into one layer hands a tool an
//! argument produced by a call that has not run, and the failure appears later as a wrong result
//! rather than as an error. The second is that a resource conflict separates two calls as well, but
//! only by one layer — the point of a plan is to keep the rest of the work parallel, so a planner
//! that serialised everything would pass the ordering tests and be useless. The third is that a
//! cycle is reported *by node*: the caller has to know which edges to break, and "there is a cycle"
//! does not name one.
//!
//! Determinism is pinned alongside correctness, because a plan that changes between two identical
//! runs cannot be compared in a test or read usefully in a log.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/tool_graph.rs"]
mod tool_graph;

use tool_graph::{GraphError, Node, Plan, happens_before, plan};

/// Builds a node from an id, a tool name, its dependencies, and the resources it requires.
fn node(id: &str, tool: &str, depends_on: &[&str], affects: &[&str]) -> Node {
    Node {
        id: id.to_owned(),
        tool: tool.to_owned(),
        depends_on: depends_on.iter().map(|id| (*id).to_owned()).collect(),
        affects: affects
            .iter()
            .map(|resource| (*resource).to_owned())
            .collect(),
    }
}

/// Plans a slice, treating a failure as a test bug in the cases that must succeed.
fn plan_ok(nodes: &[Node]) -> Plan {
    match plan(nodes) {
        Ok(plan) => plan,
        Err(error) => panic!("expected a plan, got {error}"),
    }
}

/// Plans a slice that must fail, returning the error.
fn plan_err(nodes: &[Node]) -> GraphError {
    match plan(nodes) {
        Ok(plan) => panic!("expected an error, got {plan:?}"),
        Err(error) => error,
    }
}

// ---- trivial inputs ----------------------------------------------------------------------------

#[test]
fn empty_input_plans_zero_layers() {
    let outcome = plan_ok(&[]);
    assert!(outcome.layers.is_empty());
    assert!(outcome.reasons.is_empty());
}

#[test]
fn independent_calls_share_one_layer() {
    let nodes = [
        node("a", "read", &[], &["a.txt"]),
        node("b", "read", &[], &["b.txt"]),
        node("c", "glob", &[], &["src"]),
    ];
    let outcome = plan_ok(&nodes);
    assert_eq!(outcome.layers, vec![vec!["a", "b", "c"]]);
    assert_eq!(outcome.reasons.len(), 1);
}

#[test]
fn layer_ids_follow_input_order() {
    // Placement is by level and then input order, so a test can compare whole plans.
    let nodes = [
        node("z", "read", &[], &[]),
        node("a", "read", &[], &[]),
        node("m", "read", &[], &[]),
    ];
    assert_eq!(plan_ok(&nodes).layers, vec![vec!["z", "a", "m"]]);
}

// ---- dependency layering: the core case --------------------------------------------------------

#[test]
fn a_chain_becomes_one_layer_per_call() {
    // write -> read -> summarize: each call consumes the previous one's output, so no two may
    // overlap no matter what they declare.
    let nodes = [
        node("write", "write", &[], &["report.md"]),
        node("read", "read", &["write"], &["report.md"]),
        node("summarize", "bash", &["read"], &["summary.txt"]),
    ];
    let outcome = plan_ok(&nodes);
    assert_eq!(
        outcome.layers,
        vec![vec!["write"], vec!["read"], vec!["summarize"]]
    );
    assert_eq!(outcome.reasons.len(), 3);
}

#[test]
fn a_diamond_puts_the_join_after_both_branches() {
    // setup feeds two independent readers, which both feed merge. The readers share layer 1, and the
    // merge waits for the deepest of them.
    let nodes = [
        node("setup", "bash", &[], &["work"]),
        node("left", "read", &["setup"], &["left.txt"]),
        node("right", "read", &["setup"], &["right.txt"]),
        node("merge", "write", &["left", "right"], &["out.txt"]),
    ];
    let outcome = plan_ok(&nodes);
    assert_eq!(outcome.layers.len(), 3);
    assert_eq!(outcome.layers[0], vec!["setup"]);
    assert_eq!(outcome.layers[1], vec!["left", "right"]);
    assert_eq!(outcome.layers[2], vec!["merge"]);
}

#[test]
fn a_dependency_separates_calls_that_share_no_resource() {
    // The central claim: dependency alone forces a new layer. If this passed only because both calls
    // touched `a.txt`, the resource rule would be doing the work and the edge would be untested.
    let nodes = [
        node("first", "write", &[], &["one.txt"]),
        node("second", "read", &["first"], &["two.txt"]),
    ];
    let outcome = plan_ok(&nodes);
    assert_eq!(outcome.layers, vec![vec!["first"], vec!["second"]]);
}

#[test]
fn a_dependency_onto_a_root_still_pushes_one_layer_down() {
    // `second` depends on the root, so it may not share layer 0 even though nothing else blocks it.
    let nodes = [
        node("root", "bash", &[], &[]),
        node("second", "read", &["root"], &[]),
        node("third", "read", &[], &[]),
    ];
    let outcome = plan_ok(&nodes);
    assert_eq!(outcome.layers[0], vec!["root", "third"]);
    assert_eq!(outcome.layers[1], vec!["second"]);
}

#[test]
fn a_dependency_is_transitive_through_a_skipped_level() {
    // `c` depends on `b`, which depends on `a`; the longest-path level keeps `c` two layers down even
    // though `a` is not named directly.
    let nodes = [
        node("a", "bash", &[], &[]),
        node("b", "read", &["a"], &[]),
        node("c", "read", &["b"], &[]),
    ];
    assert_eq!(
        plan_ok(&nodes).layers,
        vec![vec!["a"], vec!["b"], vec!["c"]]
    );
}

#[test]
fn a_repeated_dependency_is_counted_once() {
    let nodes = [
        node("a", "bash", &[], &[]),
        node("b", "read", &["a", "a"], &[]),
    ];
    assert_eq!(plan_ok(&nodes).layers, vec![vec!["a"], vec!["b"]]);
}

// ---- cycles: the core rejection case -----------------------------------------------------------

#[test]
fn a_two_node_cycle_names_both_nodes() {
    let nodes = [
        node("first", "write", &["second"], &[]),
        node("second", "write", &["first"], &[]),
    ];
    match plan_err(&nodes) {
        GraphError::Cycle { mut involved } => {
            involved.sort();
            assert_eq!(involved, vec!["first".to_owned(), "second".to_owned()]);
        }
        other => panic!("expected a cycle, got {other:?}"),
    }
}

#[test]
fn a_longer_cycle_names_every_node_on_it_and_not_the_ones_feeding_it() {
    // `root` leads into the cycle but is not part of it: reporting it would send the caller after an
    // edge it does not need to touch.
    let nodes = [
        node("root", "bash", &[], &[]),
        node("a", "write", &["root", "c"], &[]),
        node("b", "write", &["a"], &[]),
        node("c", "write", &["b"], &[]),
    ];
    match plan_err(&nodes) {
        GraphError::Cycle { mut involved } => {
            involved.sort();
            assert_eq!(
                involved,
                vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]
            );
        }
        other => panic!("expected a cycle, got {other:?}"),
    }
}

#[test]
fn a_self_dependency_is_a_cycle() {
    let nodes = [node("loop", "bash", &["loop"], &[])];
    match plan_err(&nodes) {
        GraphError::Cycle { involved } => assert_eq!(involved, vec!["loop".to_owned()]),
        other => panic!("expected a cycle, got {other:?}"),
    }
}

// ---- rejected input ---------------------------------------------------------------------------

#[test]
fn an_unknown_dependency_is_rejected() {
    let nodes = [
        node("a", "write", &[], &[]),
        node("b", "read", &["ghost"], &[]),
    ];
    match plan_err(&nodes) {
        GraphError::UnknownDependency { node, missing } => {
            assert_eq!(node, "b");
            assert_eq!(missing, "ghost");
        }
        other => panic!("expected an unknown dependency, got {other:?}"),
    }
}

#[test]
fn an_unknown_dependency_is_rejected_even_when_a_cycle_would_win_first() {
    // Validation order is part of the contract: a dangling edge is reported before the remaining
    // edges are searched for a cycle, because the missing node may be what breaks the cycle.
    let nodes = [
        node("a", "write", &["b"], &[]),
        node("b", "write", &["a", "ghost"], &[]),
    ];
    assert_eq!(
        plan_err(&nodes),
        GraphError::UnknownDependency {
            node: "b".to_owned(),
            missing: "ghost".to_owned(),
        }
    );
}

#[test]
fn a_duplicate_id_is_rejected() {
    let nodes = [
        node("same", "write", &[], &["a.txt"]),
        node("same", "read", &[], &["b.txt"]),
    ];
    assert_eq!(
        plan_err(&nodes),
        GraphError::Duplicate {
            id: "same".to_owned(),
        }
    );
}

// ---- resource conflicts -----------------------------------------------------------------------

#[test]
fn two_writers_of_one_file_do_not_share_a_layer() {
    let nodes = [
        node("first", "write", &[], &["out.txt"]),
        node("second", "write", &[], &["out.txt"]),
    ];
    let outcome = plan_ok(&nodes);
    assert_eq!(outcome.layers, vec![vec!["first"], vec!["second"]]);
    assert!(outcome.reasons[1].contains("out.txt"));
}

#[test]
fn a_conflict_delays_only_the_conflicting_call() {
    // The third call touches nothing in layer 0, so it stays parallel: serialising it too would be
    // the safe-but-useless plan the module must not produce.
    let nodes = [
        node("first", "write", &[], &["shared.txt"]),
        node("second", "write", &[], &["shared.txt"]),
        node("third", "write", &[], &["other.txt"]),
    ];
    let outcome = plan_ok(&nodes);
    assert_eq!(outcome.layers, vec![vec!["first", "third"], vec!["second"]]);
}

#[test]
fn a_trailing_separator_names_the_same_resource() {
    let nodes = [
        node("first", "write", &[], &["dir/"]),
        node("second", "write", &[], &["dir"]),
    ];
    assert_eq!(plan_ok(&nodes).layers.len(), 2);
}

#[test]
fn an_empty_resource_name_conflicts_with_nothing() {
    let nodes = [
        node("first", "write", &[], &[""]),
        node("second", "write", &[], &["  "]),
    ];
    assert_eq!(plan_ok(&nodes).layers, vec![vec!["first", "second"]]);
}

// ---- dependencies and conflicts together ------------------------------------------------------

#[test]
fn a_conflict_pair_stays_in_the_order_the_dependency_gave() {
    // `second` depends on `first` and also touches the same file. The dependency decides the order;
    // the conflict only confirms that they cannot share a layer.
    let nodes = [
        node("first", "write", &[], &["out.txt"]),
        node("second", "edit", &["first"], &["out.txt"]),
        node("third", "read", &["second"], &["out.txt"]),
    ];
    let outcome = plan_ok(&nodes);
    assert_eq!(
        outcome.layers,
        vec![vec!["first"], vec!["second"], vec!["third"]]
    );
}

#[test]
fn a_dependency_still_wins_against_a_remote_conflict() {
    // `b` depends on `a` but touches a file `a` never names, while `c` conflicts with `a` and does
    // not depend on it. Layer 1 must hold both `b` and `c`; layer 2 is where `d` lands after `c`.
    let nodes = [
        node("a", "write", &[], &["a.txt"]),
        node("b", "write", &["a"], &["b.txt"]),
        node("c", "write", &[], &["a.txt"]),
        node("d", "write", &["c"], &["d.txt"]),
    ];
    let outcome = plan_ok(&nodes);
    assert_eq!(outcome.layers[0], vec!["a"]);
    // `c` is placed before `b` because placement follows dependency level first: `c` is a root and
    // `b` is not, so `c` is considered while layer 0 is being filled and displaced into layer 1.
    assert_eq!(outcome.layers[1], vec!["c", "b"]);
    assert_eq!(outcome.layers[2], vec!["d"]);
}

#[test]
fn every_node_appears_exactly_once_across_layers() {
    let nodes = [
        node("a", "write", &[], &["x"]),
        node("b", "write", &["a"], &["x"]),
        node("c", "write", &[], &["x"]),
        node("d", "write", &["b", "c"], &["y"]),
    ];
    let outcome = plan_ok(&nodes);
    let mut seen: Vec<&str> = outcome
        .layers
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, vec!["a", "b", "c", "d"]);
}

// ---- happens_before ---------------------------------------------------------------------------

#[test]
fn happens_before_follows_the_layers() {
    // `b` and `c` both depend on `a` and touch different files, so they share layer 1 and neither is
    // ordered against the other: saying one runs first would promise something the plan does not.
    let nodes = [
        node("a", "write", &[], &["x"]),
        node("b", "read", &["a"], &["y"]),
        node("c", "write", &["a"], &["z"]),
    ];
    let outcome = plan_ok(&nodes);
    assert_eq!(outcome.layers[1], vec!["b", "c"]);
    assert!(happens_before(&outcome, "a", "b"));
    assert!(!happens_before(&outcome, "b", "a"));
    // Same layer: not ordered either way.
    assert!(!happens_before(&outcome, "b", "c"));
    assert!(!happens_before(&outcome, "c", "b"));
}

#[test]
fn happens_before_reports_the_order_a_conflict_imposed() {
    // Two calls that only conflict are still ordered by the plan, because one of them was placed a
    // layer later; `happens_before` reports that placement rather than the absence of an edge.
    let nodes = [
        node("first", "write", &[], &["shared"]),
        node("second", "write", &[], &["shared"]),
    ];
    let outcome = plan_ok(&nodes);
    assert!(happens_before(&outcome, "first", "second"));
    assert!(!happens_before(&outcome, "second", "first"));
}

#[test]
fn happens_before_is_transitive_through_a_layer() {
    let nodes = [
        node("a", "write", &[], &[]),
        node("b", "read", &["a"], &[]),
        node("c", "read", &["b"], &[]),
    ];
    let outcome = plan_ok(&nodes);
    assert!(happens_before(&outcome, "a", "c"));
}

#[test]
fn happens_before_is_false_for_a_node_and_itself() {
    let outcome = plan_ok(&[node("only", "read", &[], &[])]);
    assert!(!happens_before(&outcome, "only", "only"));
}

#[test]
fn happens_before_is_false_for_an_unknown_id() {
    let outcome = plan_ok(&[node("known", "read", &[], &[])]);
    assert!(!happens_before(&outcome, "known", "ghost"));
    assert!(!happens_before(&outcome, "ghost", "known"));
}

// ---- determinism ------------------------------------------------------------------------------

#[test]
fn planning_twice_yields_the_same_plan() {
    let nodes = [
        node("a", "write", &[], &["x", "y"]),
        node("b", "write", &[], &["y", "z"]),
        node("c", "read", &["a"], &["x"]),
        node("d", "write", &["b", "c"], &["z"]),
    ];
    assert_eq!(plan_ok(&nodes), plan_ok(&nodes));
}
