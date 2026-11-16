//! Subagent naming, the parent-owned child catalog, and catalog traversal.
//!
//! # What the measurement said
//!
//! Upstream `packages/subagent` is 9,900 lines across 43 source files in eleven packages. Read by
//! capability rather than by package, it is eight families:
//!
//! | family | upstream files and symbols | lines |
//! | --- | --- | --- |
//! | external CLI drivers | `subagent-acp`, `-claude-code`, `-codex`, `-dsh-sdk` | 3,481 |
//! | orchestration | `manager.ts`, `activation.ts`, `lifecycle.ts`, `child-agent.ts`, `types.ts` | 2,303 |
//! | model-facing tools | `tool-subagent`, `tool-subagent-control` | 1,322 |
//! | discovery and naming | `list-children.ts`, `catalog.ts`, `projection.ts`, `projection-types.ts`, `descriptor.ts`, `names.ts` | 1,037 |
//! | service seam and depth | `index.ts`, `depth.ts` | 655 |
//! | control, archive, errors | `control.ts`, `control-types.ts`, `archive-admission.ts`, `out-of-process.ts`, `error.ts`, `client.ts` | 554 |
//! | results and continuation | `continuation-messages.ts`, `structured.ts`, `assistant-output.ts` | 406 |
//! | in-process fork/spawn shims | `subagent-fork-in-process`, `subagent-spawn-in-process` | 142 |
//! | **total** | 43 files, eleven packages | **9,900** |
//!
//! The local crate has six `subagent*` modules totalling 2,316 lines, covering orchestration in the
//! single-parent case ([`crate::subagent`]), the depth ceiling ([`crate::subagent_depth`]), a
//! permit-based admission pool ([`crate::subagent_pool`]), interrupt plumbing
//! ([`crate::subagent_interrupt`]), a provider registry ([`crate::subagent_providers`]), and the
//! list/wait/send control tools plus the [`crate::SubagentRegistry`]
//! ([`crate::subagent_tools`]).
//!
//! Three gaps stand out, and nothing in this crate covers any of them:
//!
//! 1. **Discovery and naming** (1,037 lines: `names.ts` 113, `list-children.ts` 168, `catalog.ts`
//!    194, `projection-types.ts` 109, `projection.ts` 184, `descriptor.ts` 269). No module can say
//!    what a child is called, whether a name is free, or what a parent's children are, so
//!    `subagent_list` ([`crate::subagent_list_tool`]) can only report the rows its own
//!    `SubagentRegistry` minted — a child recorded by another parent, or by an earlier process, is
//!    invisible.
//! 2. **Structured results** (`structured.ts`, 155 lines, `STRUCTURED_OUTPUT_TOOL`): capturing the
//!    child's schema-validated final value needs the tool-dispatch pipeline and the
//!    `tools/result` two-phase commit this crate does not model.
//! 3. **Durable descriptors** (`descriptor.ts` + `projection.ts`, 453 lines): folding
//!    `subagent/descriptor` records needs that event in [`crate::SessionEvent`], which the enum does
//!    not have.
//!
//! This module ports the *first* gap, in the part that is a pure function of catalog facts: naming,
//! catalog rows, and the traversal/resolution walk (about 390 of those 1,037 lines). The projection
//! folds and the descriptor schema are left out and named as gap 3 above, because porting them
//! without their event vocabulary would produce readers that cannot be fed.
//!
//! # Why names are a correctness problem, not cosmetics
//!
//! Upstream's names are *addressing*: [`agent_address`] is how a parent tells the model which child
//! to send to, [`resolve_agent_path`] turns that text back into a Session id, and
//! [`SubagentNameReservations`] is what makes the round trip unique. Two children called
//! `agent-1` make a `send` reach the wrong one; a name that is not a valid path segment makes a
//! `send` reach nothing at all. So the validator here is exact — 1–32 lower-case letters, digits, or
//! hyphens starting with a letter, never `lead` — and the reservation is taken *synchronously*,
//! before any spawn awaits, because concurrent siblings that each checked "is this free?" would
//! both be told yes.
//!
//! # Why the catalogs are a walk, not a tree
//!
//! Upstream discovery does not load child Agents; it reads each parent Session's
//! `subagentCatalog` projection and walks those records depth-first ([`list_descendants`]). That is
//! what lets it report a grandchild that is not resident, and what makes it *bounded*: the walk
//! reads one catalog at a time and releases it before descending. The Rust port keeps the shape but
//! takes the catalogs from a closure instead of a `sessionQuery` service, because this crate has no
//! Cordis context: a caller that does have catalog access supplies it, and the traversal logic —
//! cycle guard, pre-order, diagnostics, path composition — is testable without one.
//!
//! The traversal's edge cases are the interesting part, and each is a decision:
//!
//! - **Cycles** are cut by a visited set seeded with the root, so a malformed catalog that names its
//!   own ancestor terminates instead of hanging.
//! - **An unreadable child catalog** becomes a [`DiagnosticReason::Corrupt`] or
//!   [`DiagnosticReason::Unavailable`] row rather than aborting the walk, because one damaged branch
//!   must not hide its healthy siblings. A *root* read failure does abort: there is nothing to
//!   report it against.
//! - **An unknown child mode** becomes [`DiagnosticReason::Unsupported`], and the branch is still
//!   traversed, because the mode is a fact about the *writer* and the child's own catalog may still
//!   be readable.
//! - **An external child** is a leaf: it has no local Session, so descending would ask for a catalog
//!   that cannot exist.
//! - **A path** is only complete when every link is named; a historical unnamed entry yields `None`
//!   for the whole subtree, so a model is never handed a truncated name that resolves elsewhere.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

/// The root agent's fixed name; no child may take it.
///
/// Upstream `packages/subagent/subagent/src/names.ts`: `ROOT_AGENT_NAME`. A child named `lead` would
/// be addressable as an ancestor and could shadow the root in a path, so the reservation refuses it.
pub const ROOT_AGENT_NAME: &str = "lead";

/// The longest a name may be, in bytes.
///
/// Upstream `names.ts`: the `NAME_PATTERN` regex `/^[a-z][a-z0-9-]{0,31}$/u`, i.e. one leading letter
/// plus at most 31 more characters. The bound exists so a 36-character UUID — or a whole sentence —
/// can never be mistaken for a name.
pub const MAX_AGENT_NAME_LEN: usize = 32;

/// Whether `value` is a valid child name.
///
/// Upstream `names.ts`: `isAgentName`. Valid names are 1–32 lower-case letters, digits, or hyphens
/// starting with a letter, other than [`ROOT_AGENT_NAME`].
#[must_use]
pub fn is_agent_name(value: &str) -> bool {
    if value == ROOT_AGENT_NAME || value.len() > MAX_AGENT_NAME_LEN {
        return false;
    }
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    characters.all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    })
}

/// Appends a child's name to its creator's path.
///
/// Upstream `names.ts`: `joinAgentPath`. The root's path is `Some("")`; `None` means an ancestor
/// below the root has no name (a historical catalog entry), and any such link makes the whole path
/// unknown, which is why `None` propagates rather than being skipped.
#[must_use]
pub fn join_agent_path(parent_path: Option<&str>, name: Option<&str>) -> Option<String> {
    let parent_path = parent_path?;
    let name = name?;
    if parent_path.is_empty() {
        Some(name.to_owned())
    } else {
        Some(format!("{parent_path}/{name}"))
    }
}

/// Selects the model-facing address of one tree member.
///
/// Upstream `names.ts`: `agentAddress`. The root is [`ROOT_AGENT_NAME`]; otherwise the full path,
/// falling back to the Session id when no complete path exists — an id is ugly but unambiguous,
/// which is the right failure for an address.
#[must_use]
pub fn agent_address(path: Option<&str>, id: &str) -> String {
    match path {
        Some("") => ROOT_AGENT_NAME.to_owned(),
        Some(path) => path.to_owned(),
        None => id.to_owned(),
    }
}

/// Removes the last name from a descendant path.
///
/// Upstream `names.ts`: `parentAgentPath`. A one-segment path yields `""`, the root's path.
#[must_use]
pub fn parent_agent_path(path: &str) -> &str {
    match path.rfind('/') {
        Some(separator) => path.get(..separator).unwrap_or(""),
        None => "",
    }
}

/// Which kind of execution a catalog row records.
///
/// Upstream `projection-types.ts`: the `mode` discriminant of `SubagentCatalogEntry`. The catalog
/// format grew by adding values, so an unrecognized mode is [`CatalogMode::Unknown`] rather than a
/// parse failure: a newer writer's row must not make an older reader drop the whole catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CatalogMode {
    /// A terminal one-shot child: it ran once and cannot be resumed.
    OneShot,
    /// A resumable conversation.
    Continuable,
    /// An execution with no local child Session.
    External,
    /// A mode this reader does not understand, recorded by a newer writer.
    Unknown,
}

impl CatalogMode {
    /// The wire spelling, matching upstream's `mode` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OneShot => "one-shot",
            Self::Continuable => "continuable",
            Self::External => "external",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the mode owns a local Session that a walk may descend into.
    ///
    /// Upstream `list-children.ts`: `entry.mode === 'external' ? [] : await readChildren(...)`.
    /// `Unknown` is descended because the mode describes the writer, not the child's log.
    #[must_use]
    pub const fn is_local(self) -> bool {
        !matches!(self, Self::External)
    }

    /// Whether an entry of this mode may carry a name.
    ///
    /// Upstream `catalog.ts`: only the v3 `one-shot`, `continuable`, and `external` schemas declare
    /// `name`; the v1 `unknown` schema does not.
    #[must_use]
    pub const fn admits_name(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

/// Why a catalog entry could not be built.
///
/// The constructor validates what the upstream Zod schemas validate at the same boundary, so no
/// [`SubagentCatalogEntry`] can exist in a state a reader would have to second-guess.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum CatalogEntryError {
    /// The recorded name is not a valid name.
    #[error("catalog entry names child {name:?}, which is not a valid agent name")]
    InvalidName {
        /// The rejected name.
        name: String,
    },

    /// An unknown-mode entry declared a name, which upstream's schema cannot express.
    #[error("catalog entry of unknown mode cannot carry the name {name:?}")]
    NameOnUnknownMode {
        /// The name the row carried.
        name: String,
    },
}

/// One current direct-child discovery row, materialized from parent facts.
///
/// Upstream `projection-types.ts`: `SubagentCatalogEntry`. The fields are private because two of
/// them must agree: a name has to be a valid name, and only a mode that admits one may carry it.
/// Both are checked in [`SubagentCatalogEntry::new`], which is the only way to build a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentCatalogEntry {
    /// The durable child Session id, stable across activations.
    id: String,
    /// When the child was created, as the parent recorded it.
    created_at: u64,
    /// How the child runs.
    mode: CatalogMode,
    /// The creation label; a continuable child always has one.
    label: Option<String>,
    /// The child's name, unique among the parent's children; absent on historical entries.
    name: Option<String>,
}

impl SubagentCatalogEntry {
    /// Builds one catalog row, validating the name and its mode.
    ///
    /// Upstream `catalog.ts`: the `eventDataSchema` union. A v3 row must carry a name that
    /// [`is_agent_name`] accepts, a v0–v2 row carries none, and the v1 `unknown` schema has no name
    /// field at all.
    pub fn new(
        id: impl Into<String>,
        created_at: u64,
        mode: CatalogMode,
        label: Option<impl Into<String>>,
        name: Option<&str>,
    ) -> Result<Self, CatalogEntryError> {
        let name = match name {
            Some(name) => {
                if !mode.admits_name() {
                    return Err(CatalogEntryError::NameOnUnknownMode {
                        name: name.to_owned(),
                    });
                }
                if !is_agent_name(name) {
                    return Err(CatalogEntryError::InvalidName {
                        name: name.to_owned(),
                    });
                }
                Some(name.to_owned())
            }
            None => None,
        };
        Ok(Self {
            id: id.into(),
            created_at,
            mode,
            label: label.map(Into::into),
            name,
        })
    }

    /// The child's durable Session id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// When the child was created.
    #[must_use]
    pub const fn created_at(&self) -> u64 {
        self.created_at
    }

    /// How the child runs.
    #[must_use]
    pub const fn mode(&self) -> CatalogMode {
        self.mode
    }

    /// The creation label, when one was recorded.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// The child's name, when this entry records one.
    ///
    /// `None` is not an error: a catalog written before v3 records no names, and the child is then
    /// addressable only by Session id.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// Collects the names recorded in one parent's catalog.
///
/// Upstream `catalog.ts`: `catalogNames`. The result is the `committed` set that
/// [`SubagentNameReservations::reserve`] checks a new name against, which is why it is a set and why
/// unnamed historical entries contribute nothing.
#[must_use]
pub fn catalog_names(entries: &[SubagentCatalogEntry]) -> BTreeSet<String> {
    entries
        .iter()
        .filter_map(|entry| entry.name())
        .map(str::to_owned)
        .collect()
}

/// Counts the named children in one catalog.
///
/// Upstream has no equivalent: its bound is the name space itself, and the catalog is a chunked list
/// rather than a fixed-size array. This exists because a caller that surfaces "this parent has N
/// named children" needs the count of *named* rows specifically — unnamed historical entries are not
/// addressable, so counting every row would overstate how much of the name space is spent.
#[must_use]
pub fn named_child_count(entries: &[SubagentCatalogEntry]) -> usize {
    entries
        .iter()
        .filter(|entry| entry.name().is_some())
        .count()
}

/// Why a name could not be reserved.
///
/// Upstream `names.ts`: the two `SubagentError`s `SiblingNameReservations.reserve` throws. The
/// `code()` values are the upstream codes, so a caller that already branches on them keeps working.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    /// The requested name is not a valid name.
    #[error(
        "agent name {name:?} is invalid; use 1-32 lower-case letters, digits, or hyphens \
         starting with a letter, other than \"{ROOT_AGENT_NAME}\""
    )]
    Invalid {
        /// The rejected name.
        name: String,
    },

    /// Another sibling — committed or in flight — already holds the name.
    #[error(
        "agent name {name:?} is already used by another subagent you created; choose another name"
    )]
    Duplicate {
        /// The clashing name.
        name: String,
    },
}

impl NameError {
    /// The stable failure code, matching upstream's `SubagentError` code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => "INVALID_AGENT_NAME",
            Self::Duplicate { .. } => "DUPLICATE_AGENT_NAME",
        }
    }
}

/// One name claimed by an in-flight creation.
///
/// Upstream `names.ts`: `SiblingNameReservation`. Releasing consumes the reservation, exactly as
/// [`crate::SubagentSlot`] consumes a pool slot: upstream's `release()` closure is documented as
/// safe to call twice, but a Rust caller should not have to know that, and a type that cannot be
/// released twice needs no such documentation. Dropping without releasing also returns the name,
/// because a creation that is abandoned mid-startup must not leak the name it claimed.
#[derive(Debug)]
pub struct Reservation<'a> {
    /// The claimed name.
    name: String,
    /// The creator whose catalog will receive it.
    parent: String,
    /// The pool to return it to.
    reservations: &'a SubagentNameReservations,
}

impl Reservation<'_> {
    /// The claimed name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.reservations.release(&self.parent, &self.name);
    }
}

/// Names claimed by in-flight creations, keyed by the exact creator.
///
/// Upstream `names.ts`: `SiblingNameReservations`. The map is behind a [`Mutex`] because the claim
/// must be made *synchronously*: two concurrent siblings that each read "is `agent-1` free?" and
/// then awaited a spawn would both be told yes. Reserving before the await is the whole point, and
/// it is why this type exists instead of a `committed` set the caller maintains.
///
/// The key is the creator's identity, not its name: a name identifies a child within one parent, so
/// two different parents may both have an `agent-1`.
#[derive(Debug, Default)]
pub struct SubagentNameReservations {
    /// Parent identity to the names its in-flight creations hold.
    pending: Mutex<BTreeMap<String, BTreeSet<String>>>,
}

impl SubagentNameReservations {
    /// Creates an empty pool.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Claims a name for one creator, before the creation awaits anything.
    ///
    /// With `requested` absent, the first free `agent-N` (ordinal from 1) is claimed, exactly as
    /// upstream does. A requested name is validated first and then checked against both the
    /// creator's committed catalog and its in-flight claims, because a committed-only check would
    /// let a concurrent sibling take the name in the window between the check and the commit.
    pub fn reserve(
        &self,
        parent: &str,
        committed: &BTreeSet<String>,
        requested: Option<&str>,
    ) -> Result<Reservation<'_>, NameError> {
        let mut pending = self.lock();
        let taken: BTreeSet<String> = pending.get(parent).cloned().unwrap_or_default();
        let is_taken = |name: &str| committed.contains(name) || taken.contains(name);
        let name = match requested {
            None => {
                let mut ordinal = 1_u64;
                loop {
                    let candidate = format!("agent-{ordinal}");
                    if !is_taken(&candidate) {
                        break candidate;
                    }
                    ordinal = ordinal.saturating_add(1);
                }
            }
            Some(requested) => {
                if !is_agent_name(requested) {
                    return Err(NameError::Invalid {
                        name: requested.to_owned(),
                    });
                }
                if is_taken(requested) {
                    return Err(NameError::Duplicate {
                        name: requested.to_owned(),
                    });
                }
                requested.to_owned()
            }
        };
        pending
            .entry(parent.to_owned())
            .or_default()
            .insert(name.clone());
        Ok(Reservation {
            name,
            parent: parent.to_owned(),
            reservations: self,
        })
    }

    /// The names this creator's in-flight creations currently hold.
    ///
    /// Exposed for diagnostics and tests: a caller deciding whether to spawn at all may want to know
    /// how much of the name space is already claimed, which the returned [`Reservation`] cannot say.
    #[must_use]
    pub fn pending_for(&self, parent: &str) -> BTreeSet<String> {
        self.lock().get(parent).cloned().unwrap_or_default()
    }

    /// Whether `name` is free for `parent`.
    ///
    /// Advisory only: a caller that checks and then reserves in two steps can still lose the race, so
    /// the decision belongs to [`Self::reserve`]. This exists for a UI that wants to grey out a taken
    /// name before the user submits.
    #[must_use]
    pub fn is_free(&self, parent: &str, committed: &BTreeSet<String>, name: &str) -> bool {
        is_agent_name(name)
            && !self
                .lock()
                .get(parent)
                .is_some_and(|set| set.contains(name))
            && !committed.contains(name)
    }

    /// Returns one name to the pool, dropping the creator's entry when it becomes empty.
    ///
    /// Upstream's release does the same dirty-check — `if (pending.size === 0 && this.pending.get(parent) === pending)`
    /// — so a stale release cannot drop a set that a later reservation replaced.
    fn release(&self, parent: &str, name: &str) {
        let mut pending = self.lock();
        if let Some(names) = pending.get_mut(parent) {
            names.remove(name);
            if names.is_empty() {
                pending.remove(parent);
            }
        }
    }

    /// Locks the map, ignoring a poisoned mutex.
    ///
    /// A panic while the lock is held cannot leave the map inconsistent: every mutation is a single
    /// `BTreeMap`/`BTreeSet` operation, and the only panic possible is an allocation failure. Refusing
    /// every later reservation because one caller panicked would be the worse failure.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, BTreeSet<String>>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Why a child catalog could not be read.
///
/// Upstream `list-children.ts` distinguishes a corrupt log (`SESSION_QUERY_CORRUPT_SESSION`,
/// `SESSION_QUERY_SOURCE_CONFLICT`) from one that merely could not be read, and reports which to the
/// caller instead of failing the whole listing. This type keeps that distinction in the error so the
/// walk can turn it into the matching diagnostic.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum CatalogError {
    /// The catalog was read but its data is invalid or conflicting.
    #[error("subagent catalog for {id} is corrupt")]
    Corrupt {
        /// The Session whose catalog is damaged.
        id: String,
    },

    /// The catalog could not be read at all.
    #[error("subagent catalog for {id} is unavailable")]
    Unavailable {
        /// The Session whose catalog could not be read.
        id: String,
    },
}

/// Why a recursive listing row is a diagnostic rather than a child.
///
/// Upstream `control-types.ts`: the `reason` of `SubagentListEntry`'s diagnostic arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagnosticReason {
    /// The child catalog could not be read because its data is invalid or conflicting.
    Corrupt,
    /// The parent catalog recorded a child mode this reader does not understand.
    Unsupported,
    /// The child catalog could not be read.
    Unavailable,
}

impl DiagnosticReason {
    /// The wire spelling, matching upstream's `reason` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Corrupt => "corrupt",
            Self::Unsupported => "unsupported",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Whether a listed child is resident right now.
///
/// Upstream `list-children.ts` computes this from the live Session store. It deliberately says
/// nothing durable: an `Inactive` child still has a catalog and may still be resumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Activity {
    /// A live Session was observed for this child.
    Running,
    /// The child exists in the catalog but no Session is resident.
    Inactive,
}

impl Activity {
    /// The wire spelling, matching upstream's `activity` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Inactive => "inactive",
        }
    }
}

/// One child row of a recursive listing.
///
/// Upstream `control-types.ts`: `SubagentCatalogRow` plus the `parentId`, `depth`, `path`, and
/// `hasChildren` fields `listDescendants` adds. `created_at` is dropped, as upstream drops it when it
/// builds the row — the listing is for addressing a child, not for dating it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubagentListRow {
    /// The child's durable Session id.
    id: String,
    /// The parent whose catalog contains this child.
    parent_id: String,
    /// Edge distance from the requested root; direct children are 1.
    depth: usize,
    /// Slash-separated names from the requested root, absent when the path is incomplete.
    path: Option<String>,
    /// Whether the child is resident.
    activity: Activity,
    /// How the child runs.
    mode: CatalogMode,
    /// The creation label, when one was recorded.
    label: Option<String>,
    /// The child's name, when its catalog entry records one.
    name: Option<String>,
    /// Whether this child's own catalog holds a direct child.
    has_children: bool,
}

impl SubagentListRow {
    /// The child's durable Session id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The parent whose catalog contains this child.
    #[must_use]
    pub fn parent_id(&self) -> &str {
        &self.parent_id
    }

    /// Edge distance from the requested root; direct children are 1.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// The slash-separated path from the requested root, when every link is named.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// How this child is addressable: [`agent_address`] of its path or id.
    #[must_use]
    pub fn address(&self) -> String {
        agent_address(self.path.as_deref(), &self.id)
    }

    /// Whether the child is resident.
    #[must_use]
    pub const fn activity(&self) -> Activity {
        self.activity
    }

    /// How the child runs.
    #[must_use]
    pub const fn mode(&self) -> CatalogMode {
        self.mode
    }

    /// The creation label, when one was recorded.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    /// The child's name, when one was recorded.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Whether this child's own catalog holds a direct child.
    #[must_use]
    pub const fn has_children(&self) -> bool {
        self.has_children
    }
}

/// One row of a recursive listing: a child, or a branch that could not be described.
///
/// Upstream `control-types.ts`: `SubagentListEntry`. A diagnostic is a *row*, not an error, because a
/// listing that dropped the branch would look like a parent with fewer children than it has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubagentListEntry {
    /// A child that was described successfully.
    Child(SubagentListRow),
    /// A branch the walk could not describe.
    Diagnostic(DiagnosticRow),
}

impl SubagentListEntry {
    /// The entry's Session id, for either arm.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Child(row) => row.id(),
            Self::Diagnostic(row) => row.id(),
        }
    }

    /// The child row, when this entry is one.
    #[must_use]
    pub const fn row(&self) -> Option<&SubagentListRow> {
        match self {
            Self::Child(row) => Some(row),
            Self::Diagnostic(_) => None,
        }
    }

    /// The diagnostic row, when this entry is one.
    #[must_use]
    pub const fn diagnostic(&self) -> Option<&DiagnosticRow> {
        match self {
            Self::Child(_) => None,
            Self::Diagnostic(row) => Some(row),
        }
    }
}

/// A branch of the tree that could not be described, with the position it occupied.
///
/// Upstream `control-types.ts`: the diagnostic arm of `SubagentListEntry`, carrying the same
/// `parentId`/`depth`/`path` position fields as a child so a caller can render the gap in place.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticRow {
    /// The candidate's Session id.
    id: String,
    /// The parent whose catalog contains it.
    parent_id: String,
    /// Edge distance from the requested root.
    depth: usize,
    /// The path the candidate would have had, when every link is named.
    path: Option<String>,
    /// Why the branch could not be described.
    reason: DiagnosticReason,
}

impl DiagnosticRow {
    /// The candidate's Session id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The parent whose catalog contains it.
    #[must_use]
    pub fn parent_id(&self) -> &str {
        &self.parent_id
    }

    /// Edge distance from the requested root.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// The path the candidate would have had, when every link is named.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Why the branch could not be described.
    #[must_use]
    pub const fn reason(&self) -> DiagnosticReason {
        self.reason
    }
}

/// Where the walk is: one catalog entry, its parent, its distance, and its parent's path.
struct Position {
    /// The entry being visited.
    entry: SubagentCatalogEntry,
    /// The parent whose catalog held it.
    parent_id: String,
    /// Edge distance from the requested root.
    depth: usize,
    /// The parent's path, `Some("")` for the root.
    parent_path: Option<String>,
}

/// Walks reachable parent catalogs in stable pre-order, without loading child Agents.
///
/// Upstream `list-children.ts`: `listDescendants`. `children` reads one parent's direct-child rows
/// and reports a failure as [`CatalogError`]; `is_running` answers whether a child id currently has a
/// resident Session, which is the only fact a catalog cannot carry.
///
/// The returned order is pre-order with siblings in catalog order — the stack pushes each level
/// reversed so the first recorded sibling is visited first — because a model reading the listing
/// expects a parent before its children and siblings in the order it created them.
///
/// The root's own read failure is returned, because there is no row to report it against; every
/// deeper failure becomes a [`DiagnosticReason`] row and the walk continues with its siblings.
pub fn list_descendants<C, R>(
    root: &str,
    mut children: C,
    mut is_running: R,
) -> Result<Vec<SubagentListEntry>, CatalogError>
where
    C: FnMut(&str) -> Result<Vec<SubagentCatalogEntry>, CatalogError>,
    R: FnMut(&str) -> bool,
{
    let mut stack: Vec<Position> = children(root)?
        .into_iter()
        .rev()
        .map(|entry| Position {
            entry,
            parent_id: root.to_owned(),
            depth: 1,
            parent_path: Some(String::new()),
        })
        .collect();
    let mut visited = BTreeSet::from([root.to_owned()]);
    let mut rows = Vec::new();
    while let Some(position) = stack.pop() {
        let Position {
            entry,
            parent_id,
            depth,
            parent_path,
        } = position;
        if !visited.insert(entry.id().to_owned()) {
            // A catalog that names an ancestor would otherwise walk forever.
            continue;
        }
        let path = join_agent_path(parent_path.as_deref(), entry.name());
        let nested = if entry.mode().is_local() {
            match children(entry.id()) {
                Ok(nested) => nested,
                Err(error) => {
                    rows.push(SubagentListEntry::Diagnostic(DiagnosticRow {
                        id: entry.id().to_owned(),
                        parent_id,
                        depth,
                        path,
                        reason: match error {
                            CatalogError::Corrupt { .. } => DiagnosticReason::Corrupt,
                            CatalogError::Unavailable { .. } => DiagnosticReason::Unavailable,
                        },
                    }));
                    continue;
                }
            }
        } else {
            Vec::new()
        };
        if entry.mode() == CatalogMode::Unknown {
            // The mode is unreadable, but the child's own catalog is not: report the row and keep
            // descending, because its children are still describable.
            rows.push(SubagentListEntry::Diagnostic(DiagnosticRow {
                id: entry.id().to_owned(),
                parent_id,
                depth,
                path: path.clone(),
                reason: DiagnosticReason::Unsupported,
            }));
        } else {
            rows.push(SubagentListEntry::Child(SubagentListRow {
                id: entry.id().to_owned(),
                parent_id,
                depth,
                path: path.clone(),
                activity: if is_running(entry.id()) {
                    Activity::Running
                } else {
                    Activity::Inactive
                },
                mode: entry.mode(),
                label: entry.label().map(str::to_owned),
                name: entry.name().map(str::to_owned),
                has_children: !nested.is_empty(),
            }));
        }
        // Catalog event order defines siblings; pushing reversed visits the first one next.
        for nested_entry in nested.into_iter().rev() {
            stack.push(Position {
                entry: nested_entry,
                parent_id: entry.id().to_owned(),
                depth: depth.saturating_add(1),
                parent_path: path.clone(),
            });
        }
    }
    Ok(rows)
}

/// Resolves a slash-separated name by reading only the catalogs along its path.
///
/// Upstream `list-children.ts`: `resolveAgentPath`. `children` reads one parent's direct-child rows,
/// exactly as in [`list_descendants`].
///
/// Returns `Ok(None)` when the text is not a name in this tree — an invalid segment, a segment that
/// no catalog has, or a non-final segment whose child owns no local Session — and an error only when
/// a catalog could not be read. That split matters: "no such child" is the model's mistake and worth
/// reporting as such, while "the catalog is unreadable" is the harness's problem and must not be
/// disguised as a missing child.
pub fn resolve_agent_path<C>(
    root: &str,
    name: &str,
    mut children: C,
) -> Result<Option<String>, CatalogError>
where
    C: FnMut(&str) -> Result<Vec<SubagentCatalogEntry>, CatalogError>,
{
    let segments: Vec<&str> = name.split('/').collect();
    if segments.is_empty() || !segments.iter().all(|segment| is_agent_name(segment)) {
        return Ok(None);
    }
    let mut current = root.to_owned();
    for (index, segment) in segments.iter().enumerate() {
        let entries = children(&current)?;
        let Some(child) = entries.iter().find(|entry| entry.name() == Some(*segment)) else {
            return Ok(None);
        };
        // External and unknown children own no local Session, so they must be the last segment.
        if index + 1 < segments.len() && !child.mode().is_local() {
            return Ok(None);
        }
        child.id().clone_into(&mut current);
    }
    Ok(Some(current))
}

/// The direct-child rows of one parent, in catalog order.
///
/// Upstream `list-children.ts`: `listChildren`, minus the `sessionQuery` observation this crate has
/// no context to make. Provided here so a caller that holds a [`BTreeMap`] of catalogs — the usual
/// shape in this crate — does not have to re-spell the lookup for every closure.
pub fn list_children(
    catalogs: &BTreeMap<String, Vec<SubagentCatalogEntry>>,
    parent: &str,
) -> Result<Vec<SubagentCatalogEntry>, CatalogError> {
    match catalogs.get(parent) {
        Some(entries) => Ok(entries.clone()),
        // An absent catalog is an unreadable one, not an empty one: a parent with no children
        // records no catalog at all, and a parent whose log was never written is a read failure.
        None => Err(CatalogError::Unavailable {
            id: parent.to_owned(),
        }),
    }
}

/// Whether `id` is a valid Session id shape for a catalog row.
///
/// Upstream validates a `SessionId` through its Zod brand at the wire boundary. The local brand
/// ([`crate::SessionId`]) validates at construction instead, so this helper only exists to answer the
/// question for a raw string a caller is about to turn into an id, and to keep the validation in one
/// place rather than repeated at each call site.
#[must_use]
pub fn is_session_id(value: &str) -> bool {
    girder_core::SessionId::new(value).is_ok()
}
