//! Layered execution plans for tool calls that depend on one another.
//!
//! The scheduler in `tool_schedule` answers one question: which calls may overlap without touching
//! the same resource. That question is necessary and not sufficient, because its `Call` carries no
//! edges. It cannot say that one call must *follow* another — because the second consumes the
//! first's output, or because the second reads a file the first rewrites — and a plan built from
//! resources alone may therefore put two calls in one layer whose only correct order is sequential.
//! The order that survives in the loop today is the order execution happens to use, not a property
//! of the plan.
//!
//! This module adds the edges and computes one plan from both kinds:
//!
//! - A **dependency** ([`Node::depends_on`]) is a hard edge. The depended-on call is always in a
//!   strictly earlier layer, and the edge is never traded away to fill a layer: a plan that breaks
//!   it hands a tool arguments that refer to something that has not happened yet.
//! - A **resource conflict** ([`Node::affects`]) is a "not in the same layer" edge. Two calls that
//!   require the same resource land in different layers, in an order this module chooses; they do
//!   not constrain each other any further. Calls in different layers never overlap, so a conflict
//!   costs one step of parallelism rather than the rest of the plan.
//!
//! Dependencies win in the only place the two can disagree. When a conflicting pair is already
//! ordered by a dependency, the dependency decides which one comes first and the conflict merely
//! confirms that they cannot share a layer. When nothing orders the pair, placement order decides —
//! shallowest node first, input order within a level — which is what makes the result stable enough
//! to test.
//!
//! # How the layers are computed
//!
//! 1. Ids are indexed and validated. A duplicate id is an error, because two nodes behind one id
//!    make every later edge ambiguous; an unknown dependency is an error, because a silently
//!    dropped edge is exactly the ordering bug this module exists to prevent.
//! 2. Cycles are searched for on the dependency edges only, and the search reports the nodes on the
//!    cycle. A resource conflict cannot be a genuine cycle: it says "not at the same time", not
//!    "before", so it is given a direction here and can never close one.
//! 3. Each node gets a *dependency level*: the longest path from a root over dependency edges. This
//!    is the earliest layer the node could occupy if resources did not matter.
//! 4. Nodes are considered in (level, input order) — a topological order by construction, because a
//!    dependency always lowers the level — and each one is placed in the earliest layer at or after
//!    its level that holds no conflicting resource.
//!
//! Step 4 is greedy and can use more layers than an optimal packing would. That is deliberate. A
//! plan is read by a loop and by a human looking at a log, and both need the same input to produce
//! the same layers every time; a search would trade determinism for a saving the caller cannot
//! observe anyway. Earliest-fit does guarantee the property that matters: a node occupies the first
//! layer it is allowed to occupy, so it is delayed only by a dependency or by a real conflict, never
//! by the order in which it was described to this module.
//!
//! # What the plan does not model
//!
//! There is no read/write flag on [`Node`], so a declared resource is treated as exclusive: two
//! calls that name it are never co-located, even if both only read. That is the conservative
//! direction, and a caller that knows a call only reads can simply not declare the resource. The
//! opposite mistake — omitting the path a writer will touch — is not recoverable at this layer,
//! which is why [`Node::affects`] documents the asymmetry rather than assuming it.
//!
//! The module also does not move data between calls. A dependency says *when*, not *what*: the
//! caller that built the call is the only place that can substitute one result into another call's
//! arguments, and doing it here would put the interpretation of every tool in one function.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// One call in a plan, as the layering decision needs to see it.
///
/// The three fields are exactly the facts a layer assignment needs. Tool arguments are deliberately
/// absent: this module never parses them, because guessing which JSON value names a resource or an
/// earlier call would move every tool's semantics into one place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// The call id, unique within one plan.
    ///
    /// Ids are compared exactly, without trimming: they are the caller's own keys, and two ids that
    /// differ only in whitespace are two different calls as far as this module can tell.
    pub id: String,

    /// The tool name.
    ///
    /// The layering never branches on it; resource exclusivity and dependencies are the whole input.
    /// It is carried into [`Plan::reasons`] because "`c1` cannot share layer 0" names a call, and a
    /// reader of a log wants the tool that made it worth serialising.
    pub tool: String,

    /// The ids of the calls that must complete before this one may start.
    ///
    /// A hard edge: it is never traded away to pack a layer. An id that names no node is
    /// [`GraphError::UnknownDependency`] rather than a no-op, because an edge dropped silently is a
    /// call that runs too early.
    pub depends_on: Vec<String>,

    /// The resources this call requires exclusively, such as a file path.
    ///
    /// Two calls that name the same resource are never placed in the same layer. Declaring nothing
    /// is a claim, not a default: it asserts that this call may overlap with anything. A caller must
    /// therefore not omit the path a writer will touch — an over-declared resource costs some
    /// parallelism, an under-declared one costs a lost write.
    pub affects: Vec<String>,
}

/// An execution plan over a set of [`Node`]s.
///
/// `layers` and `reasons` are parallel arrays: `reasons[i]` explains `layers[i]`. The reason is kept
/// beside the plan because a scheduler that cannot say why it serialised two calls is
/// indistinguishable, from the outside, from one that serialises everything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The layers, in the order they must run. Calls inside one layer may run in parallel.
    ///
    /// Every id from the input appears exactly once, and every call appears in a later layer than
    /// each call it depends on. An id list inside a layer follows input order, so the same input
    /// always produces the same plan.
    pub layers: Vec<Vec<String>>,

    /// Why each layer looks the way it does, one entry per layer.
    pub reasons: Vec<String>,
}

/// Why a set of calls cannot be turned into a plan.
///
/// Every variant names the calls involved. "There is a cycle" is not actionable on its own: the
/// caller has to know which edges to break, and a model reading the error has to know which calls to
/// rewrite.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    /// The dependency edges contain a cycle, so no order satisfies them.
    ///
    /// `involved` lists the nodes on one cycle, in the order the search walked it: each entry
    /// depends on the next, and the last depends on the first.
    #[error("dependency cycle among {}", .involved.join(", "))]
    Cycle {
        /// The ids on the cycle, in dependency order.
        involved: Vec<String>,
    },

    /// A call depends on an id no other call declares.
    #[error("`{node}` depends on `{missing}`, which no call in the plan declares")]
    UnknownDependency {
        /// The call holding the dangling edge.
        node: String,
        /// The dependency id that does not exist.
        missing: String,
    },

    /// Two calls share one id.
    #[error("duplicate call id `{id}`")]
    Duplicate {
        /// The id that appeared more than once.
        id: String,
    },
}

/// One frame of the iterative depth-first cycle search.
struct Frame {
    /// The node being explored.
    node: usize,
    /// Which dependency of that node is examined next.
    next: usize,
}

/// Whether a node has been reached by the cycle search.
///
/// Three states rather than a visited flag: `Visiting` means "on the current path", which is exactly
/// the condition that closes a cycle, while `Done` means "explored, and no cycle was found through
/// it". A two-state flag cannot tell a back edge from a second path to the same node.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    /// Never reached.
    Unvisited,
    /// On the current depth-first path.
    Visiting,
    /// Fully explored without finding a cycle through it.
    Done,
}

/// Turns calls with dependencies and resource requirements into layers.
///
/// The result is deterministic: the same input always produces the same layers and the same reasons.
/// Two calls that depend on each other's completion are always in different layers, and two calls
/// that declare a common resource are likewise never co-located.
///
/// # Errors
///
/// Returns [`GraphError::Duplicate`] if two nodes share an id, [`GraphError::UnknownDependency`] if a
/// dependency names an absent call, and [`GraphError::Cycle`] if the dependency edges cannot be
/// ordered. Validation runs in that order, so the first problem reported is the one at the bottom of
/// the dependency stack.
pub fn plan(nodes: &[Node]) -> Result<Plan, GraphError> {
    let index = index_nodes(nodes)?;
    let dependencies = resolve_dependencies(nodes, &index)?;
    if let Some(involved) = find_cycle(nodes, &dependencies) {
        return Err(GraphError::Cycle { involved });
    }

    let levels = dependency_levels(&dependencies);
    let order = placement_order(&levels);
    let required: Vec<BTreeSet<String>> = nodes
        .iter()
        .map(|node| normalize_all(&node.affects))
        .collect();

    // Each layer remembers which resource pulled each node into it, so a conflict can be reported by
    // resource and by owner rather than by rescanning the earlier nodes every time.
    let mut layers: Vec<Vec<usize>> = Vec::new();
    let mut owners: Vec<BTreeMap<String, usize>> = Vec::new();
    let mut explanations: Vec<Vec<String>> = Vec::new();
    let mut placed_at: Vec<usize> = vec![0; nodes.len()];

    for &node in &order {
        // The earliest layer this call's own dependencies allow. Longest-path levels already respect
        // transitive edges, but the *placed* layer of a dependency may be later than its level when
        // that dependency itself was delayed by a conflict, so the placed layer is what counts.
        let minimum = dependencies[node]
            .iter()
            .map(|&dependency| placed_at[dependency] + 1)
            .max()
            .unwrap_or(0);

        let mut chosen = None;
        let mut blocker: Option<(usize, String, usize)> = None;
        for (candidate, owned) in owners.iter().enumerate().skip(minimum) {
            if let Some((resource, owner)) = first_conflict(owned, &required[node]) {
                // Only the first blocker is recorded: it is the layer the call was most obviously
                // kept out of, and listing every one would make the reason grow with the number of
                // conflicts rather than with the number of decisions.
                if blocker.is_none() {
                    blocker = Some((candidate, resource, owner));
                }
            } else {
                chosen = Some(candidate);
                break;
            }
        }

        let layer = if let Some(layer) = chosen {
            layer
        } else {
            // `minimum` can never exceed the layer count, because every dependency already has a
            // layer below it; a new layer is therefore always the one the call can use.
            layers.push(Vec::new());
            owners.push(BTreeMap::new());
            explanations.push(Vec::new());
            layers.len() - 1
        };

        let mut added: Vec<String> = Vec::new();
        if !dependencies[node].is_empty() {
            added.push(format!(
                "`{}` follows {} and cannot join their layer",
                nodes[node].id,
                join_ids(nodes, &dependencies[node]),
            ));
        }
        if let Some((blocked, resource, owner)) = &blocker {
            added.push(format!(
                "`{}` (`{}`) is kept out of layer {blocked} by `{}` (`{}`): both require `{resource}`",
                nodes[node].id, nodes[node].tool, nodes[*owner].id, nodes[*owner].tool,
            ));
        }

        for resource in &required[node] {
            owners[layer].insert(resource.clone(), node);
        }
        explanations[layer].extend(added);
        layers[layer].push(node);
        placed_at[node] = layer;
    }

    let reasons = layers
        .iter()
        .zip(&explanations)
        .enumerate()
        .map(|(position, (layer_ids, layer_notes))| {
            describe_layer(nodes, layer_ids, position, layer_notes)
        })
        .collect();

    Ok(Plan {
        reasons,
        layers: layers
            .into_iter()
            .map(|layer_ids| {
                layer_ids
                    .into_iter()
                    .map(|node| nodes[node].id.clone())
                    .collect()
            })
            .collect(),
    })
}

/// Whether `a` is guaranteed to run before `b`.
///
/// True exactly when `a` sits in an earlier layer than `b`: layers run in order, so every call in an
/// earlier layer has finished before any call in a later one starts. Two calls in the same layer are
/// not ordered with respect to each other, and a node is not before itself. An id the plan does not
/// contain is not before anything either — a typo reports "no ordering" instead of panicking, and the
/// caller already has the plan to check the id against.
#[must_use]
pub fn happens_before(plan: &Plan, a: &str, b: &str) -> bool {
    match (layer_of(plan, a), layer_of(plan, b)) {
        (Some(first), Some(second)) => first < second,
        _ => false,
    }
}

/// The layer holding `id`, if the plan contains it.
///
/// Scanning is acceptable here because a plan is small, and keeping [`Plan`] to the two fields the
/// task specifies means it stays trivially comparable and constructible in a test.
fn layer_of(plan: &Plan, id: &str) -> Option<usize> {
    plan.layers
        .iter()
        .position(|layer| layer.iter().any(|entry| entry == id))
}

/// Indexes the nodes by id, rejecting a repeated id.
///
/// The id is the only handle a dependency can name, so two nodes behind one id would make every edge
/// that mentions it ambiguous. Refusing is the only safe answer; quietly keeping the first or the
/// last would decide a question the caller cannot see.
fn index_nodes(nodes: &[Node]) -> Result<BTreeMap<&str, usize>, GraphError> {
    let mut index = BTreeMap::new();
    for (position, node) in nodes.iter().enumerate() {
        if index.insert(node.id.as_str(), position).is_some() {
            return Err(GraphError::Duplicate {
                id: node.id.clone(),
            });
        }
    }
    Ok(index)
}

/// Resolves every declared dependency to a node position.
///
/// Duplicate entries in one `depends_on` list are collapsed: a call that names the same dependency
/// twice still waits for it once, and counting it twice would corrupt the in-degree used by the
/// level calculation.
fn resolve_dependencies(
    nodes: &[Node],
    index: &BTreeMap<&str, usize>,
) -> Result<Vec<Vec<usize>>, GraphError> {
    let mut resolved = Vec::with_capacity(nodes.len());
    for node in nodes {
        let mut positions = BTreeSet::new();
        for dependency in &node.depends_on {
            match index.get(dependency.as_str()) {
                Some(&position) => {
                    positions.insert(position);
                }
                None => {
                    return Err(GraphError::UnknownDependency {
                        node: node.id.clone(),
                        missing: dependency.clone(),
                    });
                }
            }
        }
        resolved.push(positions.into_iter().collect());
    }
    Ok(resolved)
}

/// Finds one dependency cycle, reporting the nodes on it in dependency order.
///
/// The search is iterative rather than recursive: the depth of a chain is the length of the plan the
/// model asked for, and a recursion depth bounded by user input is a stack overflow waiting for the
/// request that finally exceeds it.
fn find_cycle(nodes: &[Node], dependencies: &[Vec<usize>]) -> Option<Vec<String>> {
    let mut marks = vec![Mark::Unvisited; nodes.len()];
    let mut stack: Vec<Frame> = Vec::new();

    for start in 0..dependencies.len() {
        if marks[start] != Mark::Unvisited {
            continue;
        }
        marks[start] = Mark::Visiting;
        stack.push(Frame {
            node: start,
            next: 0,
        });

        while let Some(frame) = stack.pop() {
            if frame.next >= dependencies[frame.node].len() {
                marks[frame.node] = Mark::Done;
                continue;
            }
            let child = dependencies[frame.node][frame.next];
            // The frame goes back first so the child is explored on top of it, which keeps the stack
            // equal to the current path — the property the cycle extraction below relies on.
            stack.push(Frame {
                node: frame.node,
                next: frame.next + 1,
            });
            match marks[child] {
                Mark::Unvisited => {
                    marks[child] = Mark::Visiting;
                    stack.push(Frame {
                        node: child,
                        next: 0,
                    });
                }
                Mark::Visiting => {
                    // The child is still on the path, so the frames from the child to the top of the
                    // stack are exactly the cycle: each one depends on the next.
                    return Some(
                        stack
                            .iter()
                            .skip_while(|frame| frame.node != child)
                            .map(|frame| nodes[frame.node].id.clone())
                            .collect(),
                    );
                }
                Mark::Done => {}
            }
        }
    }

    None
}

/// The earliest layer each node could occupy if resources did not matter.
///
/// The value is the longest path from a root over dependency edges, computed by peeling roots in
/// topological order: a node is ready when its in-degree reaches zero, and its level is then one more
/// than the largest level among its dependencies. Longest rather than shortest path because *every*
/// dependency must have finished, not merely one of them.
fn dependency_levels(dependencies: &[Vec<usize>]) -> Vec<usize> {
    let mut in_degree: Vec<usize> = dependencies.iter().map(Vec::len).collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); dependencies.len()];
    for (node, deps) in dependencies.iter().enumerate() {
        for &dependency in deps {
            dependents[dependency].push(node);
        }
    }

    let mut levels = vec![0_usize; dependencies.len()];
    let mut ready: VecDeque<usize> = in_degree
        .iter()
        .enumerate()
        .filter(|&(_, &degree)| degree == 0)
        .map(|(node, _)| node)
        .collect();

    while let Some(node) = ready.pop_front() {
        for &dependent in &dependents[node] {
            levels[dependent] = levels[dependent].max(levels[node] + 1);
            in_degree[dependent] -= 1;
            if in_degree[dependent] == 0 {
                ready.push_back(dependent);
            }
        }
    }

    levels
}

/// The order to place nodes in: shallowest first, input order within a level.
///
/// This is a topological order by construction — a dependency always sits at least one level below
/// its dependent — so a node is only ever placed after everything it waits for. Among independent
/// nodes, input order decides, which is what keeps the plan reproducible and lets a caller recognise
/// its own request in the output.
fn placement_order(levels: &[usize]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..levels.len()).collect();
    order.sort_by_key(|&node| levels[node]);
    order
}

/// Normalizes a declared resource into the form used for comparison.
///
/// Trimming whitespace and trailing separators means `a.txt` and `a.txt/` name the same thing to a
/// caller that built one path by joining and the other by hand.
fn normalize(resource: &str) -> String {
    resource.trim().trim_end_matches('/').to_owned()
}

/// The declared resources of a node, normalized and deduplicated.
///
/// A `BTreeSet` rather than a `HashSet` because it is iterated when a conflict is reported, and that
/// report must not depend on hashing order. Resources that normalize to empty are dropped: they name
/// nothing, so matching them would serialise the plan for no benefit.
fn normalize_all(affects: &[String]) -> BTreeSet<String> {
    affects
        .iter()
        .map(|resource| normalize(resource))
        .filter(|resource| !resource.is_empty())
        .collect()
}

/// The first resource a layer already claims, with the node that claimed it.
///
/// Iterating the incoming resources in sorted order makes "first" a property of the input rather than
/// of hash order, so the same plan produces the same explanation on every run.
fn first_conflict(
    owners: &BTreeMap<String, usize>,
    required: &BTreeSet<String>,
) -> Option<(String, usize)> {
    for resource in required {
        if let Some(&owner) = owners.get(resource) {
            return Some((resource.clone(), owner));
        }
    }
    None
}

/// Formats a set of node positions as a comma-separated list of their ids.
fn join_ids(nodes: &[Node], positions: &[usize]) -> String {
    positions
        .iter()
        .map(|&position| format!("`{}`", nodes[position].id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One line explaining a layer: what runs in it, and what put each call there.
fn describe_layer(
    nodes: &[Node],
    layer: &[usize],
    position: usize,
    layer_notes: &[String],
) -> String {
    let ids: Vec<&str> = layer.iter().map(|&node| nodes[node].id.as_str()).collect();
    let mut text = match ids.as_slice() {
        [] => format!("layer {position}: nothing to run"),
        [only] => format!("layer {position}: `{only}` runs alone"),
        // Naming the consequence rather than the rule: a reader wants to know why these calls may
        // overlap, and "no two of them require the same resource" is the reason that is checkable.
        _ => format!(
            "layer {position}: `{}` run together because no two of them require the same resource",
            ids.join("`, `")
        ),
    };
    for note in layer_notes {
        text.push_str("; ");
        text.push_str(note);
    }
    text
}
