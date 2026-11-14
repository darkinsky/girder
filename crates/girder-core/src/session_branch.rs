//! Branch relationships between forked sessions: who came from whom, and at which event.
//!
//! # The gap this fills
//!
//! [`crate::SessionCatalog::fork`] copies a prefix of one session's log into a new one and returns
//! the new id (`session_manage.rs` in the tests exercises exactly that). The copy is faithful —
//! `DirectoryCatalog::fork` writes the source's events up to a turn boundary and nothing else — and
//! that is *all* it writes. The child's log begins with the parent's first event and carries no
//! marker that says where it came from, `SessionRecord` has no parent field, and
//! [`crate::SessionCatalog::list`] deliberately returns rows with `events` empty. So after a fork,
//! the relationship exists nowhere: not in the child's log, not in either summary, not in the
//! catalog. Fork the same session three times and the three children are indistinguishable from
//! three unrelated sessions that happen to open the same way.
//!
//! A client that wants to draw a session tree therefore has nothing to draw it from. Recovering
//! the edges by comparing logs is not a workaround but a guess: a fork is a *prefix* copy, so two
//! sessions that share a prefix are ambiguous between "one was forked from the other" and "both
//! were forked from a third, or from the same turn of the same parent", and a log comparison would
//! have to read every event of every session to even reach that ambiguity.
//!
//! # Why the index lives outside the log
//!
//! The log is the source of truth for the *conversation*: an append-only sequence of things the
//! user and the model said. A branch edge is not one of those things — it is a fact about the file,
//! recorded once, never replayed into a prompt, and never sent to a provider. Putting it in the log
//! would mean a new [`crate::SessionEvent`] variant that every reader in this crate has to handle
//! and every projection has to decide whether to show, in exchange for keeping one string next to
//! the conversation it describes. It would also change the on-disk format, which is a migration
//! with a version attached ([`crate::session_migration`]), not a feature.
//!
//! So this module is the index: a small, pure, in-memory graph of [`Link`]s that a caller fills in
//! when it performs a fork, and hands to whatever draws the tree. Nothing here writes, decodes, or
//! recompresses a session; it is a data structure with rules, in the same spirit as
//! [`crate::session_filter`] and [`crate::session_rewind`].
//!
//! # The three rules, and why each one is load-bearing
//!
//! **One parent.** A session is forked from exactly one place, so an incoming edge count above one
//! is a data error, not a fan-in. Allowing a second parent turns the structure into a directed
//! graph, and the two operations a client actually asks for — "the ancestors of this session" and
//! "draw the tree" — both stop being well defined: a graph has many ancestor paths and no single
//! root to draw from. [`BranchError::AlreadyHasParent`] is therefore a rejection, not a merge.
//!
//! **No cycles.** With one parent per node, a cycle is only reachable by a link that closes a loop
//! through existing edges (A forked B, B forked C, then C claims to have forked A — or a session
//! claiming to have forked itself). Such a link names a cause that is also its own effect, which no
//! fork can produce, and it would make [`Forest::ancestors`] walk forever instead of reaching a
//! root. [`BranchError::Cycle`] is checked against the *existing* edges before anything is stored,
//! so the structure can never enter the bad state at all.
//!
//! **`from_seq` must name a real point in the parent.** A link is a record of what happened, so a
//! cut beyond the parent's last event is bad data rather than a request to clamp. This is a
//! deliberate difference from [`crate::SessionCatalog::fork`], which takes `at_seq` as a *request*
//! and clamps it to the log length: clamping a request is polite, while silently storing a record
//! of an event that never existed would put a number in a client's tree that it could never
//! resolve back to anything.
//!
//! Bounds checking needs the parent's event count, which only the caller has ([`crate::SessionStore::
//! event_count`], or the length of a record it just read). [`Forest::record_events`] supplies it.
//! A parent whose count was never reported — a session on another host, say — cannot be checked, so
//! a link from it is accepted and the bound is simply unknown; refusing it would forbid branching
//! from exactly the sessions a remote catalog describes by id and summary alone. The check is
//! therefore "refuse what is provably wrong", never "refuse what is merely unverified".

use std::collections::BTreeMap;

/// One branch relationship.
///
/// Direction is child-to-parent on purpose: [`Link::child`] is the session that was created and
/// [`Link::parent`] is the session it was copied from, which is the direction a fork happens in.
/// The field order also matches how the edge is spoken ("`child` forked from `parent` at
/// `from_seq`"), so a struct literal reads correctly without the comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The session that was created by the fork.
    pub child: String,
    /// The session it was forked from.
    pub parent: String,
    /// How many of the parent's events the child inherited.
    ///
    /// A count rather than an index of the last copied event: this is the same quantity
    /// [`crate::SessionCatalog::fork`]'s `at_seq` means (it copies `take(at_seq)` events), so the
    /// caller can pass through what it already has instead of doing off-by-one arithmetic. `0` is
    /// a legitimate value — a fork of a session with no completed turn copies nothing — and it is
    /// distinct from "unknown", which is why the bound is checked against a recorded length rather
    /// than assumed.
    pub from_seq: u64,
    /// When the fork happened, in milliseconds since the epoch.
    ///
    /// Kept because two children of one session at the same cut are told apart only by time, and a
    /// client showing a tree wants to order siblings by when they were made. It is not used to
    /// decide anything: ordering and validation are done on structure and sequence, so a clock that
    /// jumps backwards cannot change what the forest accepts.
    pub at_ms: u64,
}

/// A rejected branch record.
///
/// Every variant names the rule it breaks, so a caller can tell a programming error from stale data
/// without parsing a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BranchError {
    /// The session already has a parent, so the new link would give it a second one.
    ///
    /// `parent` is the session that *already owns* `session`, not the one the caller attempted:
    /// the attempted parent is in the caller's hand, and the useful fact on rejection is which
    /// earlier link is being contradicted.
    #[error(
        "session `{session}` already has parent `{parent}`, so it cannot also fork from another"
    )]
    AlreadyHasParent {
        /// The session that already has a parent.
        session: String,
        /// The parent recorded by the earlier link.
        parent: String,
    },
    /// The link would close a loop, so the tree would stop being a tree.
    ///
    /// `session` is the child of the rejected link — the node whose ancestry would become infinite.
    #[error("session `{session}` cannot fork from its own descendant: the link would form a cycle")]
    Cycle {
        /// The child of the link that was refused.
        session: String,
    },
    /// `from_seq` is beyond the parent's last event.
    #[error("fork point {seq} is past the last event of a parent with {parent_events} events")]
    SeqOutOfRange {
        /// The cut the caller asked to record.
        seq: u64,
        /// How many events the parent was reported to have.
        parent_events: u64,
    },
}

/// The branch tree: every recorded [`Link`], indexed by child and by parent.
///
/// A forest rather than a tree because the index may hold more than one root — every session that
/// was never forked is a root, and a caller tracking several conversations holds several
/// components. [`Forest::render`] draws one component at a time, from the root the caller names.
///
/// The three maps are the same data in three shapes, kept in step by [`Forest::add`] and by nothing
/// else; there is no public way to remove or rewrite a link, because the logs themselves are
/// append-only and an index that could disagree with them would be worse than no index.
#[derive(Clone, Debug, Default)]
pub struct Forest {
    /// Every link, in the order recorded. The maps below hold indices into this vector.
    links: Vec<Link>,
    /// Child id to the index of the one link that owns it. This map *is* the one-parent invariant:
    /// a second link for the same child is detected by looking the child up here.
    by_child: BTreeMap<String, usize>,
    /// Parent id to the indices of its links. A `BTreeMap` rather than a hash map so iteration
    /// order is a function of the data, which keeps a rendered tree identical across runs.
    by_parent: BTreeMap<String, Vec<usize>>,
    /// Session id to how many events it had, as last reported by [`Forest::record_events`].
    ///
    /// Only used to bound-check `from_seq`. A session absent from this map is unverified, not
    /// empty: see the module documentation for why an unverified parent is accepted.
    lengths: BTreeMap<String, u64>,
}

impl Forest {
    /// An empty forest.
    ///
    /// `const` for the same reason the other builders in this crate are: a default empty index is
    /// a value, not work, and a caller can hold one in a `static` if it needs to.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            links: Vec::new(),
            by_child: BTreeMap::new(),
            by_parent: BTreeMap::new(),
            lengths: BTreeMap::new(),
        }
    }

    /// Records how many events a session has, so a later link from it can be bounds-checked.
    ///
    /// The caller has this number already — it is [`crate::SessionStore::event_count`] for a local
    /// session, or the length of the record a `read` returned — and the forest cannot obtain it
    /// without decoding logs, which is the cost this module exists to avoid.
    ///
    /// The latest report wins. A session that grew since the previous report keeps its links: the
    /// old cuts were valid against the length they were recorded with, and a log only ever grows,
    /// so a later report can only make existing cuts *more* valid. Lowering a count is therefore
    /// harmless too, and affects only links added afterwards — the forest never retroactively
    /// invalidates a link that was accepted.
    pub fn record_events(&mut self, session: &str, events: u64) {
        self.lengths.insert(session.to_owned(), events);
    }

    /// Records one fork.
    ///
    /// The checks run in the order a malformed record is most likely to be recognised, and each
    /// one is answered before anything is stored, so a rejected link leaves the forest exactly as
    /// it was:
    ///
    /// 1. A cycle, which includes the one-step case of a session forking from itself. This is
    ///    checked first because a self-link is almost always a caller bug (the wrong variable
    ///    passed as the parent) rather than the wrong data, and the cycle answer names it directly.
    /// 2. A second parent for a child that already has one.
    /// 3. A `from_seq` past the last event of a parent whose length has been reported.
    ///
    /// # Errors
    ///
    /// [`BranchError::Cycle`] when the child is the parent or one of its ancestors,
    /// [`BranchError::AlreadyHasParent`] when the child is already owned, and
    /// [`BranchError::SeqOutOfRange`] when the cut is past a known parent length.
    pub fn add(&mut self, link: Link) -> Result<(), BranchError> {
        if self.is_ancestor(&link.child, &link.parent) {
            return Err(BranchError::Cycle {
                session: link.child,
            });
        }

        if let Some(&index) = self.by_child.get(link.child.as_str()) {
            // The map and the vector are written together, so a missing link here means the index
            // is corrupt; falling back to the attempted parent keeps the error well formed instead
            // of panicking on data the caller cannot have produced.
            let owner = self
                .links
                .get(index)
                .map_or_else(|| link.parent.clone(), |existing| existing.parent.clone());
            return Err(BranchError::AlreadyHasParent {
                session: link.child,
                parent: owner,
            });
        }

        if let Some(&parent_events) = self.lengths.get(link.parent.as_str())
            && link.from_seq > parent_events
        {
            return Err(BranchError::SeqOutOfRange {
                seq: link.from_seq,
                parent_events,
            });
        }

        // The index is taken before the push so the maps and the vector agree on the position; the
        // id strings are cloned for the maps because the link keeps its own copy.
        let index = self.links.len();
        self.by_child.insert(link.child.clone(), index);
        self.by_parent
            .entry(link.parent.clone())
            .or_default()
            .push(index);
        self.links.push(link);
        Ok(())
    }

    /// Whether `candidate` is `of` itself or one of `of`'s ancestors.
    ///
    /// The walk goes up through existing parents and stops at the first session with none, so it
    /// visits at most the depth of the tree. It terminates because [`Forest::add`] refuses the
    /// links that would make it loop: this is the check that lets [`Forest::ancestors`] and
    /// [`Forest::render`] walk without a visited set.
    fn is_ancestor(&self, candidate: &str, of: &str) -> bool {
        let mut cursor = of;
        loop {
            if cursor == candidate {
                return true;
            }
            let Some(&index) = self.by_child.get(cursor) else {
                return false;
            };
            let Some(link) = self.links.get(index) else {
                return false;
            };
            cursor = link.parent.as_str();
        }
    }

    /// Every branch that starts at `session`, shallowest cut first.
    ///
    /// Ordered by `from_seq` and then by child id: two forks at the same turn of one session are
    /// siblings with no structural order between them, and naming the tie-break keeps a rendered
    /// tree and a list identical across runs instead of following insertion order.
    #[must_use]
    pub fn children(&self, session: &str) -> Vec<&Link> {
        let mut links: Vec<&Link> = self
            .by_parent
            .get(session)
            .map(|indices| {
                indices
                    .iter()
                    .filter_map(|index| self.links.get(*index))
                    .collect()
            })
            .unwrap_or_default();
        links.sort_by(|left, right| {
            left.from_seq
                .cmp(&right.from_seq)
                .then_with(|| left.child.cmp(&right.child))
        });
        links
    }

    /// The links from `session` up to its root, nearest first.
    ///
    /// Nearest first because that is the direction the walk runs and the direction a breadcrumb
    /// reads: `session`'s own fork point, then its parent's, then its grandparent's. The last
    /// element is the link whose parent is the root, so the *root id* is that link's parent — an
    /// empty result means `session` is a root, or is not in the forest at all. Those two are
    /// deliberately not distinguished: from the tree's point of view a session nobody has forked
    /// and a session nobody has told the forest about are both roots with no recorded ancestry.
    #[must_use]
    pub fn ancestors(&self, session: &str) -> Vec<&Link> {
        let mut path = Vec::new();
        let mut cursor = session;
        while let Some(&index) = self.by_child.get(cursor) {
            let Some(link) = self.links.get(index) else {
                break;
            };
            path.push(link);
            cursor = link.parent.as_str();
        }
        path
    }

    /// Draws the tree under `root` as indented text, one session per line.
    ///
    /// ```text
    /// root
    /// ├── a @1
    /// │   └── d @1
    /// └── b @2
    /// ```
    ///
    /// The root is the first line with no branch marker; every other line is a child, with `@N`
    /// naming the parent event count it was forked at (so a reader can tell *where* a branch left
    /// its parent, not just that it did). Box-drawing prefixes are the only hierarchy signal, which
    /// is what makes the output readable in a terminal and diffable in a test.
    ///
    /// A `root` with no children renders as its own id and a newline, and one the forest has never
    /// seen renders identically: an empty tree is a valid answer, and a missing entry is not
    /// distinguished from a leaf.
    #[must_use]
    pub fn render(&self, root: &str) -> String {
        let mut out = String::new();
        out.push_str(root);
        out.push('\n');
        self.render_children(root, "", &mut out);
        out
    }

    /// Appends the descendants of `session`, indented by `prefix`.
    ///
    /// Recursion mirrors the shape being drawn. Depth is the length of a fork chain, which is
    /// bounded by how many times a person branched a conversation; the alternative — an explicit
    /// stack of (session, prefix) pairs — would be more code for a bound that is small in every
    /// real tree.
    fn render_children(&self, session: &str, prefix: &str, out: &mut String) {
        let children = self.children(session);
        let last = children.len().saturating_sub(1);
        for (position, link) in children.iter().enumerate() {
            let is_last = position == last;
            out.push_str(prefix);
            out.push_str(if is_last { "└── " } else { "├── " });
            out.push_str(&link.child);
            out.push_str(" @");
            out.push_str(&link.from_seq.to_string());
            out.push('\n');

            // The last child ends its parent's line of descent, so its descendants are indented by
            // spaces; any other child is followed by a sibling, so the vertical bar has to continue
            // down past its subtree.
            let mut next_prefix = String::with_capacity(prefix.len() + 4);
            next_prefix.push_str(prefix);
            next_prefix.push_str(if is_last { "    " } else { "│   " });
            self.render_children(&link.child, &next_prefix, out);
        }
    }
}
