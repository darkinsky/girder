//! Scheduling tool calls into batches that may run in parallel.
//!
//! The loop executes the calls the model asked for one at a time, in model order, even though
//! [`crate::tool_batching::choose`] can already name a batch `parallel` — that decision is made per
//! batch and from names and flags alone, so it cannot see *which* resource a call touches. Before a
//! batch may overlap at all, something has to answer the resource question: two calls that both
//! write `a.txt` must not run at the same time, because whichever finishes last decides the file's
//! contents and the earlier write is silently lost.
//!
//! This module answers only that question. It takes the calls as the caller describes them, groups
//! them into batches whose members may safely overlap, and explains each group. It does not run
//! anything, and it does not decide *whether* to overlap a batch it produced: a batch of one is
//! simply a batch with nothing to overlap with.
//!
//! The rules, in the order they are applied:
//!
//! 1. Read-only calls never conflict with each other. Two `read` calls on the same file see the same
//!    bytes, so they may share a batch; [`crate::tool_batching`] reaches the same conclusion through
//!    the `read_only` flag, and this module agrees with it rather than replacing it.
//! 2. A call that changes state conflicts with every call in a batch that touches one of the same
//!    resources, read or write. Conservative because a read racing a write can see a half-written
//!    file, and because "it happened to be atomic" is not a property the scheduler may assume.
//! 3. A write that does not say what it affects is assumed to affect anything, so it gets a batch of
//!    its own that nothing else may join. An empty `affects` on a read is harmless: the call is
//!    already known not to change anything.
//!
//! Batches run in order, so a call that cannot share an earlier batch is placed in a later one
//! rather than dropped. Placement is deterministic: calls are considered in input order, and the
//! first batch that admits a call is the one it joins, so the same input always yields the same
//! plan. That property is what makes a plan testable at all.
//!
//! Resource comparison is exact string equality on the declared paths (after trimming). It does not
//! know that `dir/a.txt` lives inside `dir`, so a caller that wants that relationship modelled must
//! declare the directory as well. Under-declaring is the safe direction for reads but not for
//! writes, which is why a write that declares nothing is isolated rather than trusted.

use std::collections::BTreeSet;

/// One tool call, as the scheduling decision needs to see it.
///
/// The scheduler deliberately does not parse tool arguments: it cannot know which argument of which
/// tool names a resource, and guessing from a JSON value would put the interpretation of every tool
/// in one place. The caller that built the call knows what it will touch and says so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The position the caller assigned to this call.
    ///
    /// Plans refer to calls by this value rather than by their position in the slice, so a caller
    /// that filters calls after building them can still map a batch back to its own list. It is
    /// carried through unchanged; the scheduler compares nothing else about it.
    pub index: usize,
    /// The tool name, consulted only by [`is_read_only`].
    pub name: String,
    /// The resources this call affects, such as file paths.
    ///
    /// Empty means "nothing beyond what the name implies" for a read-only tool and "unknown" for
    /// anything else; the two cases are treated very differently. See the module documentation.
    pub affects: Vec<String>,
}

impl Call {
    /// Builds a call that declares no affected resources.
    ///
    /// For a write this is the conservative form: it will be given a batch of its own.
    #[must_use]
    pub fn new(index: usize, name: impl Into<String>) -> Self {
        Self {
            index,
            name: name.into(),
            affects: Vec::new(),
        }
    }

    /// Builds a call that declares what it touches.
    #[must_use]
    pub fn affecting(
        index: usize,
        name: impl Into<String>,
        affects: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            index,
            name: name.into(),
            affects: affects.into_iter().collect(),
        }
    }
}

/// An execution plan.
///
/// `batches` and `reasons` are parallel arrays: `reasons[i]` explains `batches[i]`. The reason is
/// kept next to the plan because a scheduler that cannot say why it serialised two calls is
/// indistinguishable, from the outside, from one that serialises everything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The batches, in the order they must run. Calls inside one batch may run concurrently.
    ///
    /// Each entry is a [`Call::index`] value, listed in the order the calls appeared in the input.
    pub batches: Vec<Vec<usize>>,
    /// Why each batch looks the way it does, one entry per batch.
    pub reasons: Vec<String>,
}

/// A batch under construction.
///
/// Reads and writes are tracked separately: a read only conflicts with the writes of a batch, while
/// a write conflicts with everything the batch touches.
#[derive(Debug, Default)]
struct Batch {
    /// The [`Call::index`] values placed here, in input order.
    calls: Vec<usize>,
    /// Resources read by at least one call in this batch.
    reads: BTreeSet<String>,
    /// Resources written by at least one call in this batch.
    writes: BTreeSet<String>,
    /// Whether a call that changes state without declaring resources lives here.
    ///
    /// Such a call is treated as touching everything, so the batch only ever holds it.
    exclusive: bool,
    /// Calls that were held back from an earlier batch, and by what.
    notes: Vec<String>,
}

impl Batch {
    /// A batch holding one call that changes state without declaring what it affects.
    fn exclusive(call: usize) -> Self {
        Self {
            calls: vec![call],
            exclusive: true,
            ..Self::default()
        }
    }

    /// The resource that keeps a call out of this batch, if any.
    ///
    /// Returns the lexicographically smallest conflicting resource so that the explanation does not
    /// depend on hash order; a plan that is stable but whose reasons are not would still fail a
    /// comparison test and, worse, make two identical runs look different in a log.
    fn blocks(&self, read_only: bool, affected: &BTreeSet<String>) -> Option<String> {
        if self.exclusive {
            // Nothing may share a batch with a call whose reach is unknown: the unknown call could
            // be writing the very resource the other call is about to use.
            return Some("a call that declares no affected resources".to_owned());
        }
        if let Some(resource) = affected.intersection(&self.writes).next() {
            return Some(resource.clone());
        }
        // A read is only unsafe next to a writer; a read next to a read is safe, which is the whole
        // point of putting read-only calls together.
        if read_only {
            return None;
        }
        affected.intersection(&self.reads).next().cloned()
    }

    /// Adds a call and the resources it contributes.
    fn admit(&mut self, call: usize, read_only: bool, affected: &BTreeSet<String>) {
        self.calls.push(call);
        if read_only {
            self.reads.extend(affected.iter().cloned());
        } else {
            self.writes.extend(affected.iter().cloned());
        }
    }

    /// One line explaining this batch.
    ///
    /// The notes are appended in placement order, which is input order, so the text is as stable as
    /// the batches themselves.
    fn reason(&self, position: usize) -> String {
        let calls = self
            .calls
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let mut text = if self.exclusive {
            // Naming the consequence rather than the rule: a reader of the log wants to know why the
            // call is alone, and "it declares nothing" is the reason only because of the consequence.
            format!(
                "batch {position}: call {calls} changes state without declaring its resources, so it runs alone"
            )
        } else if self.calls.len() == 1 {
            format!("batch {position}: call {calls} runs alone")
        } else {
            format!(
                "batch {position}: calls {calls} run together because none of them writes a resource another touches"
            )
        };
        for note in &self.notes {
            text.push_str("; ");
            text.push_str(note);
        }
        text
    }
}

/// Normalizes a declared resource into the form used for comparison.
///
/// Trimming whitespace and trailing separators means `a.txt` and `a.txt/` name the same thing to a
/// caller that built one path by joining and the other by hand. A resource that normalizes to empty
/// is dropped by [`resources`]: it names nothing, and matching every such call against every other
/// would serialise the whole plan for no benefit.
fn normalize(resource: &str) -> String {
    resource.trim().trim_end_matches('/').to_owned()
}

/// The declared resources of a call, normalized and deduplicated.
///
/// A `BTreeSet` rather than a `HashSet` because it is iterated when a conflict is reported, and that
/// report must not depend on hashing order.
fn resources(affects: &[String]) -> BTreeSet<String> {
    affects
        .iter()
        .map(|resource| normalize(resource))
        .filter(|resource| !resource.is_empty())
        .collect()
}

/// Places a call into the first batch that can hold it, or into a new batch.
///
/// Returning the notes rather than pushing them here keeps the borrow of the chosen batch short and
/// makes the "why" visible at the call site.
fn describe_hold(call: usize, position: usize, blocker: &str) -> String {
    format!("call {call} was held back from batch {position} by `{blocker}`")
}

/// Puts a group of calls into batches that may run one batch at a time.
///
/// Calls that touch a conflicting resource are guaranteed to land in different batches; calls that
/// do not conflict are kept as early as possible. The result is stable: the same input produces the
/// same batches and the same reasons on every run.
///
/// The plan is not a promise that execution is safe — it is only as good as the `affects` lists it
/// was given — but it is a promise that two calls it puts in one batch share no declared resource,
/// and that every call whose reach is unknown is alone.
#[must_use]
pub fn plan(calls: &[Call]) -> Plan {
    let mut batches: Vec<Batch> = Vec::new();

    for call in calls {
        let read_only = is_read_only(&call.name);
        let affected = resources(&call.affects);

        // A write that lists no resource cannot be placed next to anything: its reach is unknown, and
        // the cost of being wrong is a lost write rather than a failed call.
        if !read_only && affected.is_empty() {
            batches.push(Batch::exclusive(call.index));
            continue;
        }

        let mut blocker: Option<(usize, String)> = None;
        let mut target: Option<usize> = None;
        for (position, batch) in batches.iter().enumerate() {
            if let Some(resource) = batch.blocks(read_only, &affected) {
                if blocker.is_none() {
                    blocker = Some((position, resource));
                }
            } else {
                target = Some(position);
                break;
            }
        }

        let position = if let Some(position) = target {
            position
        } else {
            // A new batch starts empty, so the call always fits; `admit` extends it with the
            // resources, which keeps the read/write split in one place.
            batches.push(Batch::default());
            batches.len() - 1
        };

        let batch = &mut batches[position];
        if let Some((earlier, resource)) = blocker {
            // Only the first blocker is recorded: it is the batch the call was most obviously kept
            // out of, and listing every one would make the reason grow with the number of writes
            // rather than with the number of interesting decisions.
            batch
                .notes
                .push(describe_hold(call.index, earlier, &resource));
        }
        batch.admit(call.index, read_only, &affected);
    }

    Plan {
        reasons: batches
            .iter()
            .enumerate()
            .map(|(position, batch)| batch.reason(position))
            .collect(),
        batches: batches.into_iter().map(|batch| batch.calls).collect(),
    }
}

/// Whether a tool only observes state and may therefore overlap with any other read-only call.
///
/// The list is an allow-list, and that direction is deliberate: a tool added later is treated as a
/// writer until someone says otherwise, so a forgotten entry serialises a call that could have run
/// in parallel rather than corrupting a file that two calls were editing.
///
/// `terminal_read` is absent even though it reads: it returns only what a terminal printed since the
/// previous read, so two of them overlap into a race over which call consumes the output.
/// `present` is absent because publishing a file writes it.
#[must_use]
pub fn is_read_only(tool: &str) -> bool {
    matches!(
        tool,
        "read"
            | "grep"
            | "glob"
            | "lsp"
            | "web_fetch"
            | "web_search"
            | "session_search"
            | "instructions"
            | "skill"
            | "job_list"
            | "job_output"
            | "subagent_list"
            | "workflow_status"
    )
}
