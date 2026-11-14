//! Relationships between sessions that are not forks: references, continuations, and kinship.
//!
//! # The gap this fills
//!
//! The branch index in `session_branch` answers one question: which session was *copied* into which.
//! Its edges are created by a fork, they carry the cut they were taken at, and a child has exactly
//! one parent. That covers a lineage of logs, and it covers nothing else.
//!
//! Sessions also relate without copying anything. A session says "the analysis over there concluded
//! X, so I will start from X"; a session is opened because the previous one ran out of room; two
//! sessions work the same problem from different angles and a person wants them shown together.
//! None of those copies a log prefix, so none of them produces a branch, and the fork graph cannot
//! represent them: its one-parent rule is exactly wrong here (a session may reference many others),
//! its `from_seq` field has no meaning (nothing was inherited), and its cycle rule would reject a
//! perfectly sane "A and B discuss the same thing, as do B and C, as do C and A".
//!
//! The rest of the crate does not record these edges either. `SessionRecord` carries no fields for
//! them, the log has no event for them, and `session.resume` re-attaches *one* log rather than
//! merging another's history into it. A message can be tagged `Recall` because it was lifted out of
//! another session's log, but that tag is a property of one message inside one session, not an edge
//! between two sessions: it does not say which session it came from, it is not queryable in the
//! other direction, and it disappears with the compaction that drops the message.
//!
//! This module is the edge index that is missing: a small, pure, in-memory graph of [`Link`]s. Like
//! `session_branch`, it writes nothing, decodes nothing, and never touches a log; a caller records a
//! link when it observes the relationship and asks the graph questions afterwards.
//!
//! # Why the query is symmetric but the storage is not
//!
//! "This session referenced that one" is directed: the referencing session chose to depend on the
//! other, and "what does A rest on" is not the same question as "what rests on A". But the question
//! a UI asks most often — "what is related to the session I am looking at" — is symmetric, and
//! answering it by scanning every link in the graph would make the cost of opening a session
//! proportional to the number of relationships in the whole store.
//!
//! So each link is stored once, in the direction the caller stated, and indexed under *both* ends:
//! [`Graph::related`] reads the outgoing index and the incoming index and returns both. Storing a
//! second, mirrored copy instead would double every write, give the two copies a way to disagree,
//! and make "delete/rewrite a link" a two-place operation — all to save one lookup. The direction is
//! kept because it is information: [`Kind::References`] and [`Kind::Continues`] do not mean the same
//! thing read backwards.
//!
//! # Why a self-link is refused
//!
//! `from == to` is a caller bug far more often than a fact — the wrong variable passed twice, or an
//! id compared against itself in a loop that should have skipped it. It is also the one edge that
//! can never be true: "A references A" says nothing about A that its own log does not, and it makes
//! [`Graph::related`] return the same session in its own related list, which every consumer would
//! then have to filter out. [`LinkError::SelfLink`] is therefore a rejection rather than a silently
//! dropped edge, because silent dropping hides the bug instead of reporting it.
//!
//! # Why a duplicate is refused
//!
//! [`Kind`] is an attribute of an edge, not a parallel edge. Two records of the same ordered pair
//! would leave "how does A relate to B" with two answers and no rule for choosing, and would inflate
//! [`Graph::hubs`] for a session that is merely referenced twice. The *reverse* pair is a different
//! edge and is accepted: it records that B also references A, which is a separate fact and a
//! legitimate one (mutual reference happens, and refusing it would make the second writer's honest
//! observation unrecordable).
//!
//! # Why traversal needs a visited set
//!
//! Unlike the fork forest, this graph may legitimately contain cycles: A references B, B continues
//! C, and C is related to A. That is not a data error, so [`Graph::add`] does not reject it, and
//! [`Graph::reachable`] cannot assume a walk terminates — it marks each session it has expanded and
//! never expands one twice. That is the whole defence: a cycle makes the walk visit every node once
//! and stop, rather than descending until the stack or the process gives out.
//!
//! Reflexivity follows from the same idea: a session reaches itself through the empty path, so
//! [`Graph::reachable`] answers `true` for `from == to` even in a graph with no links at all.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// How two sessions relate.
///
/// The variants are deliberately about *why* a caller would draw the edge, not about what the
/// sessions contain: a consumer that wants to style the edge differently per kind can, and a
/// consumer that only wants "these belong together" can ignore the kind and use
/// [`Graph::related`], which does not filter on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `from` used a conclusion, result, or transcript from `to`.
    ///
    /// Directional, and the weakest claim of the three: nothing was inherited and nothing continues,
    /// so `to` may be entirely unrelated to `from` apart from the one thing `from` took from it.
    References,
    /// `from` carries on the work of `to` without having forked it.
    ///
    /// This is the "pick up where the last session left off" edge. It is not a fork: no events were
    /// copied, so `from` starts empty and everything it knows about `to` arrived as conversation or
    /// as recalled context. It is directional because the continuation has an original.
    Continues,
    /// `from` and `to` concern the same subject, with neither derived from the other.
    ///
    /// The one kind that is naturally symmetric. It is still stored once, in whichever direction
    /// the caller recorded it, and [`Graph::related`] surfaces it from both ends.
    Related,
}

/// One recorded relationship between two sessions.
///
/// The field order reads as the sentence the edge states: "`from` relates to `to` as `kind`".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The session the relationship starts from — the one that references, continues, or is
    /// associated with the other.
    pub from: String,
    /// The session the relationship points at.
    pub to: String,
    /// What the relationship is.
    pub kind: Kind,
    /// When the relationship was recorded, in milliseconds since the epoch.
    ///
    /// Kept for ordering and display only. Nothing in this module decides anything from it: a
    /// caller's clock that jumps backwards must not be able to change which links are accepted, and
    /// the graph's answers are a function of structure alone. It is nonetheless part of the record
    /// because "A continued B" is a claim made at a time, and a list of edges shown to a person is
    /// unreadable without it.
    pub at_ms: u64,
    /// Free-form note supplied by whoever recorded the link, if any.
    ///
    /// The kind is a closed set, so anything the caller wants to say beyond it — which conclusion
    /// was borrowed, why the sessions were judged related — has to live here. It is never parsed and
    /// never used for lookup.
    pub note: Option<String>,
}

/// A rejected link.
///
/// Two variants, each naming the rule broken and the ids involved, so a caller can report the
/// problem without matching on strings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    /// The link named the same session at both ends.
    #[error("session `{session}` cannot be related to itself")]
    SelfLink {
        /// The session that appeared as both `from` and `to`.
        session: String,
    },
    /// A link for this ordered pair already exists.
    ///
    /// The kind of the existing link is not repeated here: the caller still holds the attempted
    /// link, and the pair is the whole identity of an edge.
    #[error("a relationship from `{from}` to `{to}` is already recorded")]
    Duplicate {
        /// The source session of the refused link.
        from: String,
        /// The target session of the refused link.
        to: String,
    },
}

/// The relationship graph: every recorded [`Link`], indexed by both ends.
///
/// A graph rather than a forest: a session may reference many others, be referenced by many, and
/// participate in a cycle. There is no root and no single ancestry, so the questions are
/// reachability and neighbourhood rather than ancestors and rendering.
///
/// The three collections below are the same data in three shapes, kept in step by [`Graph::add`] and
/// by nothing else. There is no public way to remove or rewrite a link: a recorded relationship is a
/// statement about what happened, and an index that could silently disagree with itself would be
/// worse than no index.
#[derive(Clone, Debug, Default)]
pub struct Graph {
    /// Every link, in the order recorded. The maps hold indices into this vector.
    links: Vec<Link>,
    /// Source id to the indices of the links that start there.
    ///
    /// A `BTreeMap` rather than a hash map so iteration order is a function of the data: two graphs
    /// holding the same links answer [`Graph::hubs`] and [`Graph::related`] identically, without a
    /// second sort to make that true.
    by_from: BTreeMap<String, Vec<usize>>,
    /// Target id to the indices of the links that end there.
    ///
    /// The mirror of `by_from`, and the reason [`Graph::related`] costs one lookup per direction
    /// instead of a scan. A session that only ever appears as a `to` is present here and absent from
    /// `by_from`, which is exactly what makes the symmetric answer possible.
    by_to: BTreeMap<String, Vec<usize>>,
    /// The ordered pairs already recorded.
    ///
    /// Duplicate detection as a set lookup rather than a scan of `links`, and — because it is keyed
    /// by the pair and not by the whole link — it also encodes the decision that a second edge
    /// between the same two sessions is refused even when its kind differs. The reverse pair is a
    /// distinct element, so it is not refused.
    pairs: BTreeSet<(String, String)>,
}

impl Graph {
    /// An empty graph.
    ///
    /// `const` for the same reason the other builders in this crate are: an empty index is a value,
    /// not work, and a caller can hold one in a `static` if it needs to.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            links: Vec::new(),
            by_from: BTreeMap::new(),
            by_to: BTreeMap::new(),
            pairs: BTreeSet::new(),
        }
    }

    /// Records one relationship.
    ///
    /// Both rules are checked before anything is stored, so a rejected link leaves the graph
    /// exactly as it was — a caller that ignores an error does not get a half-recorded edge.
    ///
    /// # Errors
    ///
    /// [`LinkError::SelfLink`] when `link.from` and `link.to` are the same id, and
    /// [`LinkError::Duplicate`] when that ordered pair is already present.
    pub fn add(&mut self, link: Link) -> Result<(), LinkError> {
        // Checked first because it is the one case where the caller is almost certainly passing the
        // wrong value, and because the remaining checks would otherwise treat it as a normal edge.
        if link.from == link.to {
            return Err(LinkError::SelfLink { session: link.from });
        }

        let pair = (link.from.clone(), link.to.clone());
        if self.pairs.contains(&pair) {
            return Err(LinkError::Duplicate {
                from: link.from,
                to: link.to,
            });
        }

        // The index is taken before the push so the maps and the vector agree on the position; the
        // id strings are cloned for the maps because the link keeps its own copy.
        let index = self.links.len();
        self.by_from
            .entry(link.from.clone())
            .or_default()
            .push(index);
        self.by_to.entry(link.to.clone()).or_default().push(index);
        self.pairs.insert(pair);
        self.links.push(link);
        Ok(())
    }

    /// Every relationship that touches `session`, in either direction.
    ///
    /// This is the symmetric query: a link recorded as `a -> b` is returned for both `a` and `b`,
    /// and so is one recorded as `b -> a`. A session with a mutual pair therefore yields two links,
    /// which is the honest answer — two edges were recorded.
    ///
    /// Ordered by `at_ms` and then by the pair, so a caller rendering "related sessions" gets the
    /// same order on every run and after every reload. Two links recorded at the same millisecond
    /// are ordered by their ids, which is arbitrary but stable, rather than by insertion, which is
    /// an accident of how the caller happened to walk its data.
    ///
    /// An unknown session yields an empty list rather than an error: "no recorded relationships" is
    /// a legitimate answer for a session nobody has linked yet, and telling it apart from "no such
    /// session" is the catalog's job, not this index's.
    #[must_use]
    pub fn related(&self, session: &str) -> Vec<&Link> {
        let outgoing = self.by_from.get(session).into_iter().flatten();
        let incoming = self.by_to.get(session).into_iter().flatten();
        let mut links: Vec<&Link> = outgoing
            .chain(incoming)
            .filter_map(|index| self.links.get(*index))
            .collect();
        links.sort_by(|left, right| {
            left.at_ms
                .cmp(&right.at_ms)
                .then_with(|| left.from.cmp(&right.from))
                .then_with(|| left.to.cmp(&right.to))
        });
        links
    }

    /// Whether `to` can be reached from `from` by following relationships in their recorded
    /// direction.
    ///
    /// Directed on purpose: the edge says `from` depends on `to`, and a caller asking "does this
    /// session depend on that one, however indirectly" must not be told yes because the dependency
    /// runs the other way. Use [`Graph::related`] when the symmetric neighbourhood is what is
    /// wanted.
    ///
    /// A session reaches itself through the empty path, so `from == to` is `true` even for a session
    /// that appears in no link at all.
    ///
    /// The walk is breadth-first over outgoing edges and never expands a session twice. That guard
    /// is what makes a cycle (A -> B -> C -> A) terminate instead of looping forever: the second
    /// visit to A is recognised and skipped, so the walk ends after every reachable session has been
    /// expanded once.
    #[must_use]
    pub fn reachable(&self, from: &str, to: &str) -> bool {
        if from == to {
            return true;
        }

        let mut expanded: BTreeSet<&str> = BTreeSet::new();
        let mut frontier: VecDeque<&str> = VecDeque::new();
        expanded.insert(from);
        frontier.push_back(from);

        while let Some(cursor) = frontier.pop_front() {
            let Some(indices) = self.by_from.get(cursor) else {
                continue;
            };
            for index in indices {
                let Some(link) = self.links.get(*index) else {
                    continue;
                };
                if link.to == to {
                    return true;
                }
                // `insert` reports whether the id was new, so this is both the visited check and
                // the enqueue decision: a cycle finds every member already in the set.
                if expanded.insert(link.to.as_str()) {
                    frontier.push_back(link.to.as_str());
                }
            }
        }

        false
    }

    /// The most connected sessions, highest first, at most `limit` of them.
    ///
    /// "Connected" counts *distinct counterpart sessions*, not links. A mutual pair (A -> B and
    /// B -> A) is one neighbour each rather than two, because a hub is a session that many others
    /// relate to, and counting the same counterpart twice would rank a session that traded one
    /// edge with a busy one. For the same reason a session already recorded in both directions does
    /// not get to appear twice in the count.
    ///
    /// Ties are broken by session id, ascending. Without a tie-break the result would depend on map
    /// iteration order, which is a property of the data — but the *visible* top-N would still vary
    /// with `limit`, and a caller showing "the top 3" would see the list reshuffle when it asked for
    /// 4. The id order is arbitrary but stable.
    ///
    /// A session that appears in no link is not a hub, so it is absent rather than present with a
    /// count of zero. `limit == 0` yields an empty list.
    #[must_use]
    pub fn hubs(&self, limit: usize) -> Vec<(String, usize)> {
        let mut neighbours: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for link in &self.links {
            neighbours
                .entry(link.from.as_str())
                .or_default()
                .insert(link.to.as_str());
            neighbours
                .entry(link.to.as_str())
                .or_default()
                .insert(link.from.as_str());
        }

        let mut hubs: Vec<(String, usize)> = neighbours
            .into_iter()
            .map(|(session, peers)| (session.to_owned(), peers.len()))
            .collect();
        hubs.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        hubs.truncate(limit);
        hubs
    }
}
