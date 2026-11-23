//! Shared Team task boards: the durable task DAG, its command semantics, and the
//! `agentTeam` projection.
//!
//! # Where this comes from
//!
//! Upstream keeps this in `packages/team` (1,345 production source lines:
//! `agent-team/src` 1,110 plus `tool-agent-team/src` 235):
//!
//! - `agent-team/src/types.ts` — `TeamId`, `TeamTaskId`, `TeamTaskStatus`,
//!   `TeamTaskSnapshot`, `TeamTaskView`, `TeamProjection`, `TeamTaskAction`,
//!   `CreateTeamTaskRequest`, `UpdateTeamTaskRequest`.
//! - `agent-team/src/validation.ts` — `requiredText`, `writeScope`.
//! - `agent-team/src/task-graph.ts` — `assertTaskGraphCandidate`, `TeamTaskGraphViolation`.
//! - `agent-team/src/task-view.ts` — `taskReady`, `projectTaskView`.
//! - `agent-team/src/projection.ts` — `emptyTeamState`, `applyProjectionEvent`,
//!   `teamProjectionView`, `teamProjectionDefinition`.
//! - `agent-team/src/task-board.ts` — `TeamTaskBoard.create/get/list/update` and its
//!   action table.
//! - `agent-team/src/index.ts` — `TeamService.isEnabled/setEnabled`, the task limits and
//!   the removed-config-key rejection.
//! - `agent-team/src/journal.ts` — `TeamJournal.transact/appendAndFlush` ordering.
//! - `agent-team/src/error.ts` — `TeamError` and its stable codes.
//!
//! The domain above is what `team/tools` calls. The four model-facing tools live there.
//!
//! # Why this gap and not `packages/web`
//!
//! Both packages were measured before choosing. `packages/web` is 3,787 production
//! source lines across seven packages, and its capability families are provider
//! adapters (`web-search-deepseek`, `web-deepseek-api`/`transport`, Exa and Perplexity
//! manifests), the tool surface (`tool-web` 1,035: HTML→markdown, output caps,
//! source/summary caps, presentation cards), and the `ctx.web` seam (`web` 507:
//! registry + selection).
//!
//! This crate already covers the seam completely — [`girder_core::Web`] has the
//! provider registry, duplicate rejection, configured/exactly-one/ambiguous selection,
//! and the seam-side `maxResults` truncation — and the tool surface partially
//! ([`girder_core::web::tools`] registers both tools with argument validation and text
//! presentation). What remained for `web` was provider adapters plus HTML→markdown,
//! which is a *new dependency* rather than a local capability, and whose upstream
//! behaviour is transport-heavy (undici, turndown, domino).
//!
//! The decisive measurement is coverage by capability family, not raw line count:
//!
//! | family | `packages/web` | local | `packages/team` | local |
//! |---|---|---|---|---|
//! | seam / registry / selection | 507 | [`girder_core::Web`], complete | — | — |
//! | tool schema + validation + render | 1,035 | [`girder_core::web::tools`], partial | 235 | **none** |
//! | provider adapters | 2,245 | one generic `EndpointProvider` | — | — |
//! | **domain model + invariants** | 0 | — | **1,110** | **none** |
//!
//! Two of `web`'s four families are already here and one of the remaining two is
//! mostly adapter glue, so `web`'s real gap is roughly 1,000 lines of provider
//! translation. `team`'s gap is *total*: `grep` over `crates/` finds zero task boards,
//! zero write scopes, zero blocked-by edges and zero `agentTeam` projection — only the
//! `team/task` event name in [`girder_core::session::migration`]'s upgrade tables, which
//! recognizes the event without understanding it. `team` is the larger gap, and it is
//! the one whose behaviour is *decidable*: every rule below is a pure function of
//! snapshots and one command, so it can be pinned exactly without a socket, an agent
//! tree, or a session store.
//!
//! # What is deliberately not here, and why
//!
//! - **`tool-agent-team`'s four tool registrations** and `TEAM_POLICY` live in
//!   [`crate::team::tools`]. They replay this board from the session log. A name that is
//!   not the caller and not an owner already on the board is refused: this build does not
//!   hold the subagent tree `resolveName` walks.
//! - **`TeamService`'s Cordis lifecycle** (`Service.init`, `agent/created` hook,
//!   `sessionProjections.register`). This crate has its own [`girder_core::Registry`]
//!   and no projection registry, so the service is represented by [`TeamTaskBoard`],
//!   which owns exactly the state the upstream service delegates to its journal.
//! - **`ctx.subagents.rootOf/listDescendants`** — the subagent tree is not owned here.
//!   A board is bound to one root, and the list of continuable descendants is an
//!   argument to [`TeamTaskBoard::update`] (see [`TeamMember`]), which is precisely
//!   what upstream uses it for: validating a `reassign` target.
//! - **`abortSignal.throwIfAborted()`**. Cancellation in this crate is cooperative at
//!   the tool boundary; these functions are synchronous and terminate, so the only
//!   observable difference would be a partial mutation, and no mutation below can be
//!   partial (the append happens once, last).
//! - **`teamProjectionView`'s `WeakMap` memo.** It exists to avoid recomputing a view
//!   per wire read; the view here is a pure function of state and callers can memoize
//!   it themselves. Porting an identity-keyed cache would add a leak surface for no
//!   behavioural gain.
//!
//! # The three invariants that carry the weight
//!
//! 1. **A task revision is a compare-and-set.** Every command names the revision it
//!    read and is refused if the board moved. Without it two members claiming one task
//!    both succeed and the board forgets an owner.
//! 2. **The dependency graph is validated after every replacement, not just on the
//!    candidate's own edges.** A `set_dependencies` that creates a cycle through three
//!    other tasks is only visible in the whole-graph walk, and a cycle means no task in
//!    it is ever ready — a board that accepts one is permanently wedged.
//! 3. **A deleted task is a tombstone, not a disappearance.** Its id must never be
//!    reused (the next id is derived from the highest one ever seen), it must not free
//!    its slot in the `maxTasks` budget, and a live task must not be blocked by one.
//!
//! # Why `girder_core::` inside the module
//!
//! `team_gap` is not declared in `lib.rs` yet — wiring belongs to a separate change. The
//! test compiles this exact source by `#[path]`, and under that inclusion `crate::` names
//! the test binary, so every path out of the module goes through the crate root's public
//! re-exports.

#![allow(
    dead_code,
    reason = "this module is compiled privately by the `#[path]` test until lib.rs re-exports it; every item here is public surface for that wiring, so 'unused within this test binary' is not dead code"
)]

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use girder_core::{Brand, Id, IdError, SessionId};

// ---- limits ---------------------------------------------------------------------------

/// Upstream `DEFAULT_MAX_TASKS`: the non-deleted task cap one Team starts with.
pub const DEFAULT_MAX_TASKS: usize = 256;

/// Upstream `numericTaskIdPattern` prefix: generated ids are `task-<n>`.
pub const TASK_ID_PREFIX: &str = "task-";

/// Upstream `requiredText(subject, 'subject', 200)`.
pub const MAX_SUBJECT_CHARS: usize = 200;

/// Upstream `requiredText(description, 'description', 16_384)`.
pub const MAX_DESCRIPTION_CHARS: usize = 16_384;

/// Upstream `teamProjectionDefinition.key`.
pub const TEAM_PROJECTION_KEY: &str = "agentTeam";

/// Upstream `teamProjectionDefinition.stateVersion`.
pub const TEAM_PROJECTION_STATE_VERSION: u64 = 7;

/// The only `team/task` payload version this module applies.
pub const TEAM_TASK_EVENT_VERSION: u64 = 2;

/// Upstream `Number.MAX_SAFE_INTEGER`: a numeric task id suffix above this is rejected,
/// because a JavaScript client could not round-trip it.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

// ---- branded identities ---------------------------------------------------------------

/// Marker for the Team identity family. Upstream `TeamId`.
pub struct TeamIdBrand;

impl Brand for TeamIdBrand {
    const DOMAIN: &'static str = "team";
}

/// One root Session's implicit Team identity. Upstream `TeamId`.
pub type TeamId = Id<TeamIdBrand>;

/// Marker for the task identity family. Upstream `TeamTaskId`.
pub struct TeamTaskIdBrand;

impl Brand for TeamTaskIdBrand {
    const DOMAIN: &'static str = "team-task";
}

/// Stable identifier for one task in a Team. Upstream `TeamTaskId`.
pub type TeamTaskId = Id<TeamTaskIdBrand>;

/// Upstream `TeamId(id)`: one root Session identity *is* its Team identity.
///
/// A free function rather than an inherent method, because [`TeamId`] is an alias for
/// [`Id`] and an inherent `impl` on a foreign type is not permitted.
///
/// # Errors
///
/// Returns whatever [`Id::new`] rejects in the session identity.
pub fn team_id_of_root(root: &SessionId) -> Result<TeamId, IdError> {
    Id::new(root.as_str())
}

/// Upstream ``TeamTaskId(`task-${n}`)``: the id a freshly created task receives.
///
/// # Errors
///
/// Returns [`IdError`] when the rendered id is not a valid identifier. The prefix and a
/// decimal suffix always are, so this cannot fail for a `u64`.
pub fn numbered_task_id(number: u64) -> Result<TeamTaskId, IdError> {
    Id::new(format!("{TASK_ID_PREFIX}{number}"))
}

/// Whether a task id is one upstream's persisted schema accepts.
///
/// Upstream only constrains the *numeric* form: an id shaped `task-<digits>` must have a
/// suffix that is a safe integer, so a hand-written `task-9007199254740993` is refused at
/// the projection boundary rather than silently ordered wrong by a JavaScript client. A
/// non-numeric suffix is accepted as-is.
#[must_use]
pub fn is_valid_task_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix(TASK_ID_PREFIX) else {
        return true;
    };
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return true;
    }
    suffix
        .parse::<u64>()
        .is_ok_and(|number| number <= MAX_SAFE_INTEGER)
}

/// The numeric suffix of an id shaped `task-<digits>`, for id allocation.
///
/// An id outside that shape contributes nothing, which is why the sequence is derived
/// from the maximum suffix ever seen rather than from the task count.
#[must_use]
pub fn task_number(id: &TeamTaskId) -> Option<u64> {
    let suffix = id.as_str().strip_prefix(TASK_ID_PREFIX)?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse::<u64>().ok()
}

// ---- errors ---------------------------------------------------------------------------

/// A failure raised by the Team domain. Upstream `TeamError`, with its `code` as a
/// closed enum rather than an open string.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{message}")]
pub struct TeamError {
    /// The stable, machine-routable code (`TEAM_*`).
    code: TeamErrorCode,
    /// The human-readable explanation.
    message: String,
}

impl TeamError {
    /// Builds an error from a code and a message.
    #[must_use]
    pub fn new(code: TeamErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Its machine-routable code.
    #[must_use]
    pub fn code(&self) -> TeamErrorCode {
        self.code
    }

    /// The code as the wire string upstream publishes.
    #[must_use]
    pub fn code_str(&self) -> &'static str {
        self.code.as_str()
    }
}

/// Every `TEAM_*` code the domain raises. Upstream keeps these as open strings; a closed
/// enum is what lets an exhaustive match catch a new one at compile time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TeamErrorCode {
    /// The Team is off for this Session.
    Disabled,
    /// An argument violates its shape, length, or combination rule.
    InvalidArgument,
    /// A write scope is not a workspace-relative prefix.
    InvalidWriteScope,
    /// A deployment limit is not a positive safe integer.
    InvalidConfig,
    /// The non-deleted task cap, or the numeric id space, is exhausted.
    TaskLimit,
    /// No task has that id.
    TaskNotFound,
    /// The caller's expected revision is not the current one.
    StaleRevision,
    /// The task is a tombstone.
    TaskDeleted,
    /// The caller is neither the owner nor the Lead.
    TaskUnauthorized,
    /// Another member owns the task.
    TaskAlreadyClaimed,
    /// The task is not ready, or is blocked.
    TaskBlocked,
    /// The action is not legal from the task's current status.
    InvalidTransition,
    /// A live task still depends on this one.
    HasDependents,
    /// The dependency graph would be self-referential or cyclic.
    DependencyCycle,
    /// The named member is not a continuable descendant of the board's root.
    MemberNotFound,
    /// The action is reserved to the Team Lead.
    LeadRequired,
    /// The append is visible but durability was not confirmed.
    PersistenceUnconfirmed,
}

impl TeamErrorCode {
    /// The wire string. Upstream's literal, unchanged.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "TEAM_DISABLED",
            Self::InvalidArgument => "TEAM_INVALID_ARGUMENT",
            Self::InvalidWriteScope => "TEAM_INVALID_WRITE_SCOPE",
            Self::InvalidConfig => "TEAM_INVALID_CONFIG",
            Self::TaskLimit => "TEAM_TASK_LIMIT",
            Self::TaskNotFound => "TEAM_TASK_NOT_FOUND",
            Self::StaleRevision => "TEAM_TASK_STALE_REVISION",
            Self::TaskDeleted => "TEAM_TASK_DELETED",
            Self::TaskUnauthorized => "TEAM_TASK_UNAUTHORIZED",
            Self::TaskAlreadyClaimed => "TEAM_TASK_ALREADY_CLAIMED",
            Self::TaskBlocked => "TEAM_TASK_BLOCKED",
            Self::InvalidTransition => "TEAM_TASK_INVALID_TRANSITION",
            Self::HasDependents => "TEAM_TASK_HAS_DEPENDENTS",
            Self::DependencyCycle => "TEAM_TASK_DEPENDENCY_CYCLE",
            Self::MemberNotFound => "TEAM_MEMBER_NOT_FOUND",
            Self::LeadRequired => "TEAM_LEAD_REQUIRED",
            Self::PersistenceUnconfirmed => "TEAM_TASK_PERSISTENCE_UNCONFIRMED",
        }
    }

    /// Parses a wire string, for a caller reading a remote error.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "TEAM_DISABLED" => Self::Disabled,
            "TEAM_INVALID_ARGUMENT" => Self::InvalidArgument,
            "TEAM_INVALID_WRITE_SCOPE" => Self::InvalidWriteScope,
            "TEAM_INVALID_CONFIG" => Self::InvalidConfig,
            "TEAM_TASK_LIMIT" => Self::TaskLimit,
            "TEAM_TASK_NOT_FOUND" => Self::TaskNotFound,
            "TEAM_TASK_STALE_REVISION" => Self::StaleRevision,
            "TEAM_TASK_DELETED" => Self::TaskDeleted,
            "TEAM_TASK_UNAUTHORIZED" => Self::TaskUnauthorized,
            "TEAM_TASK_ALREADY_CLAIMED" => Self::TaskAlreadyClaimed,
            "TEAM_TASK_BLOCKED" => Self::TaskBlocked,
            "TEAM_TASK_INVALID_TRANSITION" => Self::InvalidTransition,
            "TEAM_TASK_HAS_DEPENDENTS" => Self::HasDependents,
            "TEAM_TASK_DEPENDENCY_CYCLE" => Self::DependencyCycle,
            "TEAM_MEMBER_NOT_FOUND" => Self::MemberNotFound,
            "TEAM_LEAD_REQUIRED" => Self::LeadRequired,
            "TEAM_TASK_PERSISTENCE_UNCONFIRMED" => Self::PersistenceUnconfirmed,
            _ => return None,
        })
    }
}

// ---- normalized input -----------------------------------------------------------------

/// Normalizes one required human-authored string. Upstream `requiredText`.
///
/// The trim happens *before* the length check, so 200 characters of padding around a real
/// subject is accepted — the alternative rejects input a user cannot see the problem in.
///
/// The length counts Unicode scalar values; upstream counts UTF-16 code units, which
/// differ only for characters outside the Basic Multilingual Plane.
///
/// # Errors
///
/// [`TeamErrorCode::InvalidArgument`] when the trimmed text is empty or too long.
pub fn required_text(value: &str, field: &str, max_length: usize) -> Result<String, TeamError> {
    let text = value.trim();
    if text.is_empty() {
        return Err(TeamError::new(
            TeamErrorCode::InvalidArgument,
            format!("{field} must be non-empty"),
        ));
    }
    if text.chars().count() > max_length {
        return Err(TeamError::new(
            TeamErrorCode::InvalidArgument,
            format!("{field} exceeds {max_length} characters"),
        ));
    }
    Ok(text.to_owned())
}

/// Normalizes one workspace-relative path prefix. Upstream `writeScope`.
///
/// A write scope is *advisory*: it never locks a file. It exists so another member can
/// see that two in-progress tasks expect to touch the same prefix, which is why a
/// minimal normalization (backslashes, a leading `./`, trailing slashes) is the whole
/// job — rejecting an absolute path and any `.`/`..` segment is what keeps the
/// overlap test honest, since `a/../b` and `b` would otherwise look disjoint.
///
/// # Errors
///
/// [`TeamErrorCode::InvalidWriteScope`] for an empty, absolute, drive-lettered, or
/// dot-segmented scope.
pub fn write_scope(value: &str) -> Result<String, TeamError> {
    // Normalization is mechanical and ordered as upstream spells it: backslashes become
    // slashes, at most one leading `./` goes, then trailing slashes go. Everything after
    // is a rejection, never a rewrite — a scope the user did not write is worse than a
    // refused one.
    let slashes = value.replace('\\', "/");
    let without_current = slashes.strip_prefix("./").unwrap_or(&slashes);
    let normalized = without_current.trim_end_matches('/');
    let invalid = || {
        TeamError::new(
            TeamErrorCode::InvalidWriteScope,
            format!(
                "invalid workspace-relative write scope {}",
                serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
            ),
        )
    };
    if normalized.is_empty()
        || normalized.starts_with('/')
        || has_drive_letter(normalized)
        || normalized
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(invalid());
    }
    Ok(normalized.to_owned())
}

/// Whether a normalized scope begins with a Windows drive letter (`C:` / `c:`).
fn has_drive_letter(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(letter) if letter.is_ascii_alphabetic())
        && chars.next() == Some(':')
}

// ---- task values ----------------------------------------------------------------------

/// Durable task lifecycle. Upstream `TeamTaskStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TeamTaskStatus {
    /// Created, unowned, and waiting for its blockers.
    Pending,
    /// Claimed by a member.
    InProgress,
    /// Finished.
    Completed,
    /// Tombstone: retained for id uniqueness, hidden from the board.
    Deleted,
}

impl TeamTaskStatus {
    /// The wire string upstream publishes.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Deleted => "deleted",
        }
    }

    /// Parses a wire string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            "deleted" => Self::Deleted,
            _ => return None,
        })
    }
}

impl Serialize for TeamTaskStatus {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TeamTaskStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown team task status {raw:?}")))
    }
}

/// Whole durable task snapshot; every mutation increments [`revision`](Self::revision).
/// Upstream `TeamTaskSnapshot`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TeamTaskSnapshot {
    /// Team-local identity.
    pub id: TeamTaskId,
    /// Revision, starting at 1 and incrementing by exactly one per mutation.
    pub revision: u64,
    /// Concise title.
    pub subject: String,
    /// Complete details and acceptance criteria.
    pub description: String,
    /// Lifecycle status.
    pub status: TeamTaskStatus,
    /// The owning member, when one is assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_id: Option<SessionId>,
    /// The owner's agent path captured at assignment. Absent on unnamed historical owners.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_name: Option<String>,
    /// Tasks that must complete before this one is ready.
    #[serde(default)]
    pub blocked_by: Vec<TeamTaskId>,
    /// Advisory workspace-relative prefixes this task expects to modify.
    #[serde(default)]
    pub write_scopes: Vec<String>,
}

impl TeamTaskSnapshot {
    /// Validates the rules upstream's persisted Zod schema enforces beyond types: a
    /// positive revision, a safe numeric id suffix, and `ownerName` only with `ownerId`.
    ///
    /// # Errors
    ///
    /// A [`String`] describing the first violated rule, matching the message the
    /// projection records as its `failure`.
    pub fn validate(&self) -> Result<(), String> {
        if self.revision < 1 {
            return Err(format!(
                "team task {} revision must be positive",
                self.id.as_str()
            ));
        }
        if !is_valid_task_id(self.id.as_str()) {
            return Err(format!(
                "team task {} numeric task id suffix must be a safe integer",
                self.id.as_str()
            ));
        }
        if self.owner_name.is_some() && self.owner_id.is_none() {
            return Err(format!(
                "team task {} ownerName requires ownerId",
                self.id.as_str()
            ));
        }
        if self.owner_name.as_deref() == Some("") {
            return Err(format!(
                "team task {} ownerName must be non-empty",
                self.id.as_str()
            ));
        }
        Ok(())
    }
}

/// Runtime-enriched task view returned to tools and hosts. Upstream `TeamTaskView`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamTaskView {
    /// Team-local identity.
    pub id: TeamTaskId,
    /// Current revision.
    pub revision: u64,
    /// Concise title.
    pub subject: String,
    /// Complete details.
    pub description: String,
    /// Lifecycle status.
    pub status: TeamTaskStatus,
    /// Blockers still recorded on the task.
    pub blocked_by: Vec<TeamTaskId>,
    /// Advisory write prefixes.
    pub write_scopes: Vec<String>,
    /// Owning member, when assigned.
    pub owner_id: Option<SessionId>,
    /// The owner's captured agent path.
    pub owner_name: Option<String>,
    /// Whether the task is `pending` and every blocker completed.
    pub ready: bool,
    /// Write scopes that overlap another *in-progress* task, one line each.
    pub write_scope_warnings: Vec<String>,
}

impl TeamTaskView {
    /// Projects the view into the model-facing object of upstream's `NamedTaskView`:
    /// `ownerId` and `ownerName` collapse into one `owner` field, and every absent
    /// optional field is omitted rather than emitted as null.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("id".to_owned(), Value::String(self.id.as_str().to_owned()));
        object.insert("revision".to_owned(), Value::from(self.revision));
        object.insert("subject".to_owned(), Value::String(self.subject.clone()));
        object.insert(
            "description".to_owned(),
            Value::String(self.description.clone()),
        );
        object.insert(
            "status".to_owned(),
            Value::String(self.status.as_str().to_owned()),
        );
        if let Some(owner) = self
            .owner_name
            .as_deref()
            .or(self.owner_id.as_ref().map(Id::as_str))
        {
            object.insert("owner".to_owned(), Value::String(owner.to_owned()));
        }
        object.insert(
            "blockedBy".to_owned(),
            Value::Array(
                self.blocked_by
                    .iter()
                    .map(|id| Value::String(id.as_str().to_owned()))
                    .collect(),
            ),
        );
        object.insert(
            "writeScopes".to_owned(),
            Value::Array(
                self.write_scopes
                    .iter()
                    .map(|scope| Value::String(scope.clone()))
                    .collect(),
            ),
        );
        object.insert("ready".to_owned(), Value::Bool(self.ready));
        object.insert(
            "writeScopeWarnings".to_owned(),
            Value::Array(
                self.write_scope_warnings
                    .iter()
                    .map(|warning| Value::String(warning.clone()))
                    .collect(),
            ),
        );
        Value::Object(object)
    }
}

/// Durable Team state published to clients through the Lead Session's `agentTeam`
/// projection. Upstream `TeamProjection`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TeamProjection {
    /// Whether Team mode is selected.
    pub enabled: bool,
    /// Non-deleted tasks, in creation order.
    pub tasks: Vec<TeamTaskView>,
    /// The first rejected persisted record, when one was seen.
    pub failure: Option<String>,
}

impl TeamProjection {
    /// Projects the state into the wire object (`TeamProjection`'s client schema).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("enabled".to_owned(), Value::Bool(self.enabled));
        object.insert(
            "tasks".to_owned(),
            Value::Array(self.tasks.iter().map(TeamTaskView::to_json).collect()),
        );
        if let Some(failure) = &self.failure {
            object.insert("failure".to_owned(), Value::String(failure.clone()));
        }
        Value::Object(object)
    }
}

// ---- task graph -----------------------------------------------------------------------

/// The dependency relation the shared graph validator rejected. Upstream
/// `TeamTaskGraphViolation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TeamTaskGraphViolation {
    /// A blocker is absent or a tombstone.
    Missing,
    /// The same blocker is listed twice.
    Duplicate,
    /// A task is blocked (directly or transitively) by itself.
    Cycle,
}

/// A rejected dependency relation, with the stable code it maps to on a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamTaskGraphError {
    /// What was wrong, as the message a command surfaces.
    pub message: String,
    /// Which category of relation was violated.
    pub violation: TeamTaskGraphViolation,
}

impl TeamTaskGraphError {
    /// The command error code this violation maps to. Upstream
    /// `TASK_GRAPH_ERROR_CODES`.
    #[must_use]
    pub fn code(&self) -> TeamErrorCode {
        match self.violation {
            TeamTaskGraphViolation::Missing => TeamErrorCode::TaskNotFound,
            TeamTaskGraphViolation::Duplicate => TeamErrorCode::InvalidArgument,
            TeamTaskGraphViolation::Cycle => TeamErrorCode::DependencyCycle,
        }
    }

    /// The wire string of [`code`](Self::code).
    #[must_use]
    pub fn code_str(&self) -> &'static str {
        self.code().as_str()
    }

    /// Converts into the command error the caller sees.
    #[must_use]
    pub fn into_team_error(self) -> TeamError {
        TeamError::new(self.code(), self.message)
    }
}

/// The task map a validation walk reads: the current snapshots with the candidate
/// replacing (or joining) them. Insertion order is kept so the walk visits the candidate
/// where upstream's `Map#set` would place it, which decides *which* violation is reported
/// when a graph has several.
fn with_candidate<'a>(
    current: &'a [TeamTaskSnapshot],
    candidate: &'a TeamTaskSnapshot,
) -> Vec<&'a TeamTaskSnapshot> {
    let mut tasks: Vec<&TeamTaskSnapshot> = current.iter().collect();
    match tasks.iter().position(|task| task.id == candidate.id) {
        Some(index) => {
            if let Some(slot) = tasks.get_mut(index) {
                *slot = candidate;
            }
        }
        None => tasks.push(candidate),
    }
    tasks
}

/// Validates the complete active task graph after replacing one candidate snapshot.
/// Upstream `assertTaskGraphCandidate`.
///
/// The whole graph is walked, not just the candidate: a `set_dependencies` can close a
/// cycle whose other edges were all legal when they were written, and a single-task check
/// would accept it. Deleted tasks are skipped as subjects *and* as blockers, because a
/// tombstone cannot complete and would otherwise block forever.
///
/// # Errors
///
/// A [`TeamTaskGraphError`] naming the first missing, duplicated, self-referential, or
/// cyclic edge.
pub fn assert_task_graph_candidate(
    current: &[TeamTaskSnapshot],
    candidate: &TeamTaskSnapshot,
) -> Result<(), TeamTaskGraphError> {
    let tasks = with_candidate(current, candidate);

    for task in &tasks {
        if task.status == TeamTaskStatus::Deleted {
            continue;
        }
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for blocker_id in &task.blocked_by {
            if blocker_id == &task.id {
                return Err(TeamTaskGraphError {
                    message: format!("team task {} cannot block itself", task.id.as_str()),
                    violation: TeamTaskGraphViolation::Cycle,
                });
            }
            if !seen.insert(blocker_id.as_str()) {
                return Err(TeamTaskGraphError {
                    message: format!(
                        "team task {} repeats blocker {}",
                        task.id.as_str(),
                        blocker_id.as_str()
                    ),
                    violation: TeamTaskGraphViolation::Duplicate,
                });
            }
            let blocker = tasks.iter().find(|candidate| candidate.id == *blocker_id);
            if blocker.is_none_or(|blocker| blocker.status == TeamTaskStatus::Deleted) {
                return Err(TeamTaskGraphError {
                    message: format!(
                        "blocker task {} for {} is missing or deleted",
                        blocker_id.as_str(),
                        task.id.as_str()
                    ),
                    violation: TeamTaskGraphViolation::Missing,
                });
            }
        }
    }

    // Depth-first over the blocker edges. `visiting` is the current path, so an edge back
    // into it is a cycle; `visited` is every fully-explored node, so a shared blocker is
    // walked once instead of once per dependent.
    let mut visiting: BTreeSet<&str> = BTreeSet::new();
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    for task in &tasks {
        visit_task(&tasks, &task.id, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn visit_task<'a>(
    tasks: &[&'a TeamTaskSnapshot],
    id: &'a TeamTaskId,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
) -> Result<(), TeamTaskGraphError> {
    if visiting.contains(id.as_str()) {
        return Err(TeamTaskGraphError {
            message: format!("task dependency cycle includes {}", id.as_str()),
            violation: TeamTaskGraphViolation::Cycle,
        });
    }
    if visited.contains(id.as_str()) {
        return Ok(());
    }
    let Some(task) = tasks.iter().find(|task| task.id == *id) else {
        return Ok(());
    };
    if task.status == TeamTaskStatus::Deleted {
        return Ok(());
    }
    visiting.insert(id.as_str());
    // A blocker id belongs to whatever `TeamTaskId` the edge named, not to this task's
    // lifetime; the recursion borrows it from the snapshot that holds the edge.
    for blocker_id in &task.blocked_by {
        visit_task(tasks, blocker_id, visiting, visited)?;
    }
    visiting.remove(id.as_str());
    visited.insert(id.as_str());
    Ok(())
}

// ---- task view ------------------------------------------------------------------------

/// Whether two normalized prefixes overlap on path components. Upstream `scopesOverlap`.
///
/// Component-wise rather than string-prefix: `src` and `src2` are disjoint, and a plain
/// `starts_with` would report a conflict between them.
#[must_use]
pub fn scopes_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|rest| rest.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether every current blocker of one task completed. Upstream `taskReady`.
///
/// A blocker that no longer exists counts as incomplete: absence is not evidence of
/// completion, and the projection rejects a dangling edge before a view is ever built.
#[must_use]
pub fn task_ready(state: &TeamState, task: &TeamTaskSnapshot) -> bool {
    task.blocked_by.iter().all(|id| {
        state
            .tasks
            .iter()
            .find(|candidate| candidate.id == *id)
            .is_some_and(|candidate| candidate.status == TeamTaskStatus::Completed)
    })
}

/// Derives one task view with saved owner identity, readiness, and advisory write
/// overlaps. Upstream `projectTaskView`.
///
/// Only *in-progress* peers are compared: two pending tasks that will edit the same file
/// are not a conflict yet, and warning about them would train a reader to ignore the
/// warning. The result carries no reference into `state`, so a committing caller may pass
/// its pre-append state.
#[must_use]
pub fn project_task_view(state: &TeamState, task: &TeamTaskSnapshot) -> TeamTaskView {
    let mut warnings = Vec::new();
    for other in &state.tasks {
        if other.id == task.id || other.status != TeamTaskStatus::InProgress {
            continue;
        }
        let overlaps = task.write_scopes.iter().any(|left| {
            other
                .write_scopes
                .iter()
                .any(|right| scopes_overlap(left, right))
        });
        if overlaps {
            let warning = format!("write scopes overlap with {}", other.id.as_str());
            if !warnings.contains(&warning) {
                warnings.push(warning);
            }
        }
    }
    TeamTaskView {
        id: task.id.clone(),
        revision: task.revision,
        subject: task.subject.clone(),
        description: task.description.clone(),
        status: task.status,
        blocked_by: task.blocked_by.clone(),
        write_scopes: task.write_scopes.clone(),
        owner_id: task.owner_id.clone(),
        owner_name: task.owner_name.clone(),
        ready: task.status == TeamTaskStatus::Pending && task_ready(state, task),
        write_scope_warnings: warnings,
    }
}

// ---- state and projection -------------------------------------------------------------

/// Checkpoint-safe state for the Team owned by one projected Session. Upstream
/// `TeamState` plus `TeamProjectionState`'s checkpoint fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamState {
    root: SessionId,
    enabled: Option<bool>,
    tasks: Vec<TeamTaskSnapshot>,
    next_task_number: u64,
    inherited_event_count: u64,
    failure: Option<String>,
}

impl TeamState {
    /// Upstream `emptyTeamState(rootId, inheritedEventCount = 0)`.
    #[must_use]
    pub fn empty(root: SessionId) -> Self {
        Self::empty_at(root, 0)
    }

    /// Empty state that excludes an inherited prefix from this root's mode selection.
    ///
    /// A forked Session inherits its parent's events; without the boundary the fork would
    /// inherit the parent's Team *selection*, which is a decision about a different tree.
    #[must_use]
    pub fn empty_at(root: SessionId, inherited_event_count: u64) -> Self {
        Self {
            root,
            enabled: None,
            tasks: Vec::new(),
            next_task_number: 1,
            inherited_event_count,
            failure: None,
        }
    }

    /// The root Session this Team belongs to. Upstream `TeamState.id`.
    #[must_use]
    pub fn root(&self) -> &SessionId {
        &self.root
    }

    /// This Team's branded identity. Upstream `TeamId(root)`.
    ///
    /// The root identity was already validated by [`Id::new`], and the team domain shares
    /// its character set, so re-branding cannot fail. `None` is returned rather than a
    /// panic if that invariant ever stops holding.
    #[must_use]
    pub fn team_id(&self) -> Option<TeamId> {
        team_id_of_root(&self.root).ok()
    }

    /// The saved selection, absent until an explicit selection or a task event.
    #[must_use]
    pub fn enabled(&self) -> Option<bool> {
        self.enabled
    }

    /// Whether Team tools and guidance are on for this Session.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled == Some(true)
    }

    /// Every task snapshot, tombstones included, in creation order.
    #[must_use]
    pub fn tasks(&self) -> &[TeamTaskSnapshot] {
        &self.tasks
    }

    /// The number the next created task will use.
    #[must_use]
    pub fn next_task_number(&self) -> u64 {
        self.next_task_number
    }

    /// The inherited prefix excluded from this root's mode selection.
    #[must_use]
    pub fn inherited_event_count(&self) -> u64 {
        self.inherited_event_count
    }

    /// The first rejected persisted record, retained once set.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Applies one committed `team/task` replacement. Upstream `applyTask`.
    ///
    /// # Errors
    ///
    /// A [`String`] when the revision is not contiguous (a first revision other than 1,
    /// or a jump), or when the resulting graph is invalid.
    pub fn apply_task(&mut self, task: TeamTaskSnapshot) -> Result<(), String> {
        let existing = self
            .tasks
            .iter()
            .position(|candidate| candidate.id == task.id);
        match existing {
            None if task.revision != 1 => {
                return Err(format!(
                    "team task {} must begin at revision 1",
                    task.id.as_str()
                ));
            }
            Some(index) => {
                let prior_revision = self
                    .tasks
                    .get(index)
                    .map_or(0, |candidate| candidate.revision);
                if task.revision != prior_revision.saturating_add(1) {
                    return Err(format!(
                        "team task {} revision is not contiguous",
                        task.id.as_str()
                    ));
                }
            }
            None => {}
        }
        assert_task_graph_candidate(&self.tasks, &task).map_err(|error| error.message)?;
        if let Some(number) = task_number(&task.id) {
            // The sequence advances past every numeric id ever applied, so a tombstoned id
            // is never reissued. At `MAX_SAFE_INTEGER` there is no next number, and
            // `create` refuses instead of wrapping to a reusable one.
            self.next_task_number = match number.checked_add(1) {
                Some(next) => self.next_task_number.max(next),
                None => self.next_task_number.max(number),
            };
        }
        match existing {
            Some(index) => {
                if let Some(slot) = self.tasks.get_mut(index) {
                    *slot = task;
                }
            }
            None => self.tasks.push(task),
        }
        if self.enabled.is_none() {
            self.enabled = Some(true);
        }
        Ok(())
    }

    /// Applies one committed session event. Upstream `applyProjectionEvent`.
    ///
    /// A failure is recorded in [`failure`](Self::failure) rather than returned, and the
    /// first one is sticky: the log is replayed from the start on every load, so a
    /// payload that is invalid now stays invalid, and a later *valid* event must not make
    /// the board look healthy while an earlier revision is missing.
    ///
    /// `type_name` and `data` are the event's `type` and `data` fields as they appear in
    /// the session log; `seq` is its sequence number, used only for the inherited-prefix
    /// boundary.
    pub fn apply_event(&mut self, seq: u64, type_name: &str, data: &Value) {
        match self.try_apply_event(seq, type_name, data) {
            Ok(()) => {}
            Err(message) => {
                if self.failure.is_none() {
                    self.failure = Some(message);
                }
            }
        }
    }

    fn try_apply_event(&mut self, seq: u64, type_name: &str, data: &Value) -> Result<(), String> {
        if type_name == "team/mode" {
            if seq < self.inherited_event_count {
                return Ok(());
            }
            let enabled = data
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| invalid_payload("team/mode"))?;
            reject_unknown_fields(data, "team/mode", &["enabled"])?;
            self.enabled = Some(enabled);
            return Ok(());
        }
        if self.failure.is_some() || type_name != "team/task" {
            return Ok(());
        }
        let team_id = data
            .get("teamId")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_payload("team/task"))?;
        if team_id != self.root.as_str() {
            return Ok(());
        }
        let version = data
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_payload("team/task"))?;
        // Upstream reads `version` through a permissive selector and then refuses anything
        // but 2 explicitly, so an unknown version is a stated failure — not a payload that
        // happens not to parse.
        if version != TEAM_TASK_EVENT_VERSION {
            return Err(format!("unsupported Agent Teams event version {version}"));
        }
        reject_unknown_fields(data, "team/task", &["version", "teamId", "task"])?;
        let task = data
            .get("task")
            .ok_or_else(|| invalid_payload("team/task"))?;
        let snapshot: TeamTaskSnapshot =
            serde_json::from_value(task.clone()).map_err(|_| invalid_payload("team/task"))?;
        snapshot
            .validate()
            .map_err(|_| invalid_payload("team/task"))?;
        // Decode-then-apply, as upstream does: a malformed persisted row must be reported as
        // an invalid payload rather than as a board that merely refused a transition.
        self.apply_task(snapshot)
            .map_err(|_| invalid_payload("team/task"))
    }
}

/// The message upstream wraps a schema failure in. Upstream `parsePersisted`.
fn invalid_payload(type_name: &str) -> String {
    format!("persisted Agent Teams {type_name} payload is invalid")
}

/// Rejects any field the persisted schema does not declare, as upstream's `.strict()` does.
///
/// A looser reader would silently drop a field a future build added, and then the
/// downgraded reader would re-persist the event without it.
fn reject_unknown_fields(data: &Value, type_name: &str, allowed: &[&str]) -> Result<(), String> {
    let Some(object) = data.as_object() else {
        return Err(invalid_payload(type_name));
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid_payload(type_name));
    }
    Ok(())
}

/// Replays a whole event log into one Team's state. Upstream `projectionDefinition.apply`
/// folded over a session log.
#[must_use]
pub fn replay_events(
    root: SessionId,
    inherited_event_count: u64,
    events: impl IntoIterator<Item = (u64, String, Value)>,
) -> TeamState {
    let mut state = TeamState::empty_at(root, inherited_event_count);
    for (seq, type_name, data) in events {
        state.apply_event(seq, &type_name, &data);
    }
    state
}

/// Durable client view of one Team state. Upstream `teamProjectionView`.
///
/// A projection failure retains the last valid tasks and sets `failure`; mode selections
/// still apply, because the selection is a separate event that a bad task payload cannot
/// invalidate.
#[must_use]
pub fn team_projection_view(state: &TeamState) -> TeamProjection {
    TeamProjection {
        enabled: state.is_enabled(),
        tasks: state
            .tasks
            .iter()
            .filter(|task| task.status != TeamTaskStatus::Deleted)
            .map(|task| project_task_view(state, task))
            .collect(),
        failure: state.failure.clone(),
    }
}

// ---- journal --------------------------------------------------------------------------

/// The journal could not confirm that a committed revision is durable.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{reason}")]
pub struct JournalError {
    /// Why the append was not confirmed.
    pub reason: String,
}

impl JournalError {
    /// Builds a failure from a description.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// The append half of the Lead Session log. Upstream `TeamJournal.appendAndFlush`.
///
/// A board commits state and then journals it; a journal that cannot confirm durability
/// leaves a revision visible but possibly lost on restart. That is why the two are
/// separate operations with a distinct error rather than one "save" call: the failure
/// tells the caller to *read the task before retrying*, because a blind retry would
/// append a second revision of the same change.
pub trait TeamTaskJournal {
    /// Appends one committed revision.
    ///
    /// # Errors
    ///
    /// [`JournalError`] when durability was not confirmed.
    fn append(&mut self, task: &TeamTaskSnapshot) -> Result<(), JournalError>;

    /// Every revision this journal holds, in commit order.
    fn committed(&self) -> &[TeamTaskSnapshot];
}

/// An in-memory journal, the shape a caller replaces with its session log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryJournal {
    committed: Vec<TeamTaskSnapshot>,
}

impl MemoryJournal {
    /// An empty journal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any revision was appended.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.committed.is_empty()
    }

    /// How many revisions were appended.
    #[must_use]
    pub fn len(&self) -> usize {
        self.committed.len()
    }

    /// Rebuilds state purely from the journal. Upstream's projection replay: the log is
    /// the source of truth, so a board that disagrees with this has drifted.
    ///
    /// # Errors
    ///
    /// A [`String`] when an appended revision is not contiguous or closes a cycle, which
    /// means the journal itself is corrupt.
    pub fn replay(&self, root: SessionId) -> Result<TeamState, String> {
        let mut state = TeamState::empty(root);
        for task in &self.committed {
            state.apply_task(task.clone())?;
        }
        Ok(state)
    }
}

impl TeamTaskJournal for MemoryJournal {
    fn append(&mut self, task: &TeamTaskSnapshot) -> Result<(), JournalError> {
        self.committed.push(task.clone());
        Ok(())
    }

    fn committed(&self) -> &[TeamTaskSnapshot] {
        &self.committed
    }
}

// ---- members --------------------------------------------------------------------------

/// One Team member as the board needs it: an identity and the agent path to display.
///
/// Upstream derives both from the live `Agent` through `ctx.subagents.namesOf`; this
/// crate does not own the subagent tree, so the caller supplies them. A member with no
/// name is one upstream would report by Session id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamMember {
    /// Session identity.
    pub id: SessionId,
    /// The agent path captured at assignment, when the member has one.
    pub name: Option<String>,
}

impl TeamMember {
    /// A member known only by Session identity.
    #[must_use]
    pub fn new(id: SessionId) -> Self {
        Self { id, name: None }
    }

    /// A member with an agent path.
    #[must_use]
    pub fn named(id: SessionId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: Some(name.into()),
        }
    }

    /// The label this member owns: its path, or its identity.
    #[must_use]
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or_else(|| self.id.as_str())
    }
}

// ---- requests -------------------------------------------------------------------------

/// Input for creating one shared task. Upstream `CreateTeamTaskRequest`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateTeamTaskRequest {
    /// Concise title, trimmed to at most [`MAX_SUBJECT_CHARS`].
    pub subject: String,
    /// Complete details, trimmed to at most [`MAX_DESCRIPTION_CHARS`].
    pub description: String,
    /// Tasks that must complete first.
    pub blocked_by: Vec<TeamTaskId>,
    /// Advisory workspace-relative prefixes this task expects to modify.
    pub write_scopes: Vec<String>,
}

/// A supported task mutation. Upstream `TeamTaskAction`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TeamTaskAction {
    /// Take an unowned ready task.
    Claim,
    /// Give back an in-progress task, clearing its owner.
    Release,
    /// Replace the title, description, and/or write scopes.
    Edit,
    /// Replace the complete blocker list.
    SetDependencies,
    /// Finish an in-progress task.
    Complete,
    /// Return a completed task to pending, clearing its owner.
    Reopen,
    /// Lead-only: assign or unassign a task.
    Reassign,
    /// Tombstone the task.
    Delete,
}

impl TeamTaskAction {
    /// The wire string upstream's tool schema enumerates.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Release => "release",
            Self::Edit => "edit",
            Self::SetDependencies => "set_dependencies",
            Self::Complete => "complete",
            Self::Reopen => "reopen",
            Self::Reassign => "reassign",
            Self::Delete => "delete",
        }
    }

    /// Parses a wire string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "claim" => Self::Claim,
            "release" => Self::Release,
            "edit" => Self::Edit,
            "set_dependencies" => Self::SetDependencies,
            "complete" => Self::Complete,
            "reopen" => Self::Reopen,
            "reassign" => Self::Reassign,
            "delete" => Self::Delete,
            _ => return None,
        })
    }
}

/// The three states a `reassign` can name.
///
/// Upstream distinguishes `undefined` (field absent) from an explicit unassign, but both
/// mean "no owner afterwards" once the action is already `reassign`; the enum keeps the
/// distinction readable instead of nesting two `Option`s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Assignee {
    /// Assign the task to this member.
    Member(SessionId),
    /// Assign the task to nobody: return it to pending and unowned.
    Nobody,
}

/// Compare-and-set mutation of one shared task. Upstream `UpdateTeamTaskRequest`.
///
/// Every optional field is only meaningful for some actions, and an action that requires
/// one rejects its absence rather than treating it as "no change": `set_dependencies`
/// with no list is a caller bug, not a request to clear the blockers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateTeamTaskRequest {
    /// The task to mutate.
    pub task_id: TeamTaskId,
    /// The revision the caller read.
    pub expected_revision: u64,
    /// The transition to apply.
    pub action: TeamTaskAction,
    /// New title, for `edit`.
    pub subject: Option<String>,
    /// New details, for `edit`.
    pub description: Option<String>,
    /// Complete blocker list, for `set_dependencies`.
    pub blocked_by: Option<Vec<TeamTaskId>>,
    /// Complete write-scope list, for `edit`.
    pub write_scopes: Option<Vec<String>>,
    /// Assignee, for `reassign`.
    pub owner: Option<Assignee>,
}

impl UpdateTeamTaskRequest {
    /// A request with only the compare-and-set fields set.
    #[must_use]
    pub fn new(task_id: TeamTaskId, expected_revision: u64, action: TeamTaskAction) -> Self {
        Self {
            task_id,
            expected_revision,
            action,
            subject: None,
            description: None,
            blocked_by: None,
            write_scopes: None,
            owner: None,
        }
    }
}

/// One page of the task board, as `team_task_list` returns it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TeamTaskList {
    /// The page's rows.
    pub tasks: Vec<TeamTaskView>,
    /// The next offset, present only when more rows remain.
    pub next_cursor: Option<u64>,
}

impl TeamTaskList {
    /// Projects the page into the model-facing object of upstream's
    /// `TASK_LIST_VALUE_SCHEMA`.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert(
            "tasks".to_owned(),
            Value::Array(self.tasks.iter().map(TeamTaskView::to_json).collect()),
        );
        if let Some(cursor) = self.next_cursor {
            object.insert("nextCursor".to_owned(), Value::from(cursor));
        }
        Value::Object(object)
    }
}

// ---- the board ------------------------------------------------------------------------

/// Owns Team task limits, authorization, transitions, and derived views. Upstream
/// `TeamTaskBoard` plus the parts of `TeamService` that delegate to it.
///
/// The board is bound to one root: upstream resolves the root from the caller's live
/// agent tree, and a board that accepted a second root would let two trees share a task
/// id space. Reads need no journal; every mutation commits state and then appends, in
/// that order, so a caller that sees a view always finds the revision in the journal.
#[derive(Debug)]
pub struct TeamTaskBoard<J: TeamTaskJournal = MemoryJournal> {
    state: TeamState,
    max_tasks: usize,
    journal: J,
}

impl TeamTaskBoard<MemoryJournal> {
    /// A board for one root Session with the given non-deleted task cap.
    ///
    /// # Errors
    ///
    /// [`TeamErrorCode::InvalidConfig`] when `max_tasks` is zero, which upstream's
    /// `z.number().min(1)` also rejects.
    pub fn new(root: SessionId, max_tasks: usize) -> Result<Self, TeamError> {
        Self::with_journal(root, max_tasks, MemoryJournal::new())
    }
}

impl<J: TeamTaskJournal> TeamTaskBoard<J> {
    /// A board over an arbitrary journal.
    ///
    /// # Errors
    ///
    /// [`TeamErrorCode::InvalidConfig`] when `max_tasks` is zero.
    pub fn with_journal(root: SessionId, max_tasks: usize, journal: J) -> Result<Self, TeamError> {
        Ok(Self {
            state: TeamState::empty(root),
            max_tasks: positive_limit("maxTasks", max_tasks)?,
            journal,
        })
    }

    /// A board resumed from an existing state, for a Session that already has a log.
    ///
    /// # Errors
    ///
    /// [`TeamErrorCode::InvalidConfig`] when `max_tasks` is zero.
    pub fn resume(state: TeamState, max_tasks: usize, journal: J) -> Result<Self, TeamError> {
        Ok(Self {
            state,
            max_tasks: positive_limit("maxTasks", max_tasks)?,
            journal,
        })
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> &TeamState {
        &self.state
    }

    /// The journal every committed revision was appended to.
    #[must_use]
    pub fn journal(&self) -> &J {
        &self.journal
    }

    /// The non-deleted task cap.
    #[must_use]
    pub fn max_tasks(&self) -> usize {
        self.max_tasks
    }

    /// Saves this root's Team selection. Upstream `TeamService.setEnabled`.
    ///
    /// Only the root may change its own tree's selection: a descendant choosing Team mode
    /// would change the tools its *parent* sees.
    ///
    /// # Errors
    ///
    /// [`TeamErrorCode::LeadRequired`] when `caller` is not the board's root.
    pub fn set_enabled(&mut self, caller: &TeamMember, enabled: bool) -> Result<(), TeamError> {
        if caller.id != *self.state.root() {
            return Err(TeamError::new(
                TeamErrorCode::LeadRequired,
                "change Team mode from the root Session",
            ));
        }
        self.state.enabled = Some(enabled);
        Ok(())
    }

    /// Whether Team tools and guidance are on for this board.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.state.is_enabled()
    }

    /// Turns Team mode on without naming a caller.
    ///
    /// The caller-checked entry point is [`set_enabled`](Self::set_enabled); this exists for
    /// a resumed board whose mode was restored from its log, and for a deployment whose
    /// `defaultEnabled` config (upstream's `Config.defaultEnabled`) starts every new root
    /// with the board on.
    pub fn enable(&mut self) {
        self.state.enabled = Some(true);
    }

    /// Creates one unowned pending task in the Lead log. Upstream
    /// `TeamTaskBoard.create`.
    ///
    /// The id comes from the state's sequence, which advances past every id ever applied —
    /// so a tombstoned `task-1` is never reissued, and a caller holding a stale id cannot
    /// be surprised by a different task under it.
    ///
    /// # Errors
    ///
    /// [`TeamErrorCode::Disabled`] when Team mode is off; [`TeamErrorCode::TaskLimit`] at
    /// the cap or when the id space is exhausted; [`TeamErrorCode::InvalidArgument`] for
    /// invalid text; the graph errors for a bad dependency; and
    /// [`TeamErrorCode::PersistenceUnconfirmed`] when the append is visible but not
    /// durable.
    ///
    /// `caller` is accepted for symmetry with [`update`](Self::update) and so the signature
    /// matches upstream, but creation needs no authority: any member of an enabled tree may
    /// add work to the board, and upstream itself only uses it to resolve the root this
    /// board is already bound to.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the request is consumed: both of its Vec fields move into the committed snapshot, so a reference would force every caller to clone instead"
    )]
    pub fn create(
        &mut self,
        caller: &TeamMember,
        request: CreateTeamTaskRequest,
    ) -> Result<TeamTaskView, TeamError> {
        self.require_enabled()?;
        let _ = caller;
        let active = self.count_active();
        if active >= self.max_tasks {
            return Err(TeamError::new(
                TeamErrorCode::TaskLimit,
                format!("Team task limit {} reached", self.max_tasks),
            ));
        }
        let number = self.state.next_task_number;
        if number > MAX_SAFE_INTEGER {
            return Err(TeamError::new(
                TeamErrorCode::TaskLimit,
                "Team task id space exhausted",
            ));
        }
        let id = numbered_task_id(number)
            .map_err(|error| TeamError::new(TeamErrorCode::TaskLimit, error.to_string()))?;
        if self.state.tasks.iter().any(|task| task.id == id) {
            return Err(TeamError::new(
                TeamErrorCode::TaskLimit,
                "Team task id space exhausted",
            ));
        }
        // Field validation order matches upstream's object-literal evaluation: subject,
        // description, dependencies, write scopes.
        let subject = required_text(&request.subject, "subject", MAX_SUBJECT_CHARS)?;
        let description =
            required_text(&request.description, "description", MAX_DESCRIPTION_CHARS)?;
        let blocked_by = self.dependencies(&request.blocked_by, None)?;
        let write_scopes = normalize_write_scopes(&request.write_scopes)?;
        let task = TeamTaskSnapshot {
            id,
            revision: 1,
            subject,
            description,
            status: TeamTaskStatus::Pending,
            owner_id: None,
            owner_name: None,
            blocked_by,
            write_scopes,
        };
        self.commit(task)
    }

    /// Returns one task, including a deleted tombstone. Upstream `TeamTaskBoard.get`.
    ///
    /// # Errors
    ///
    /// [`TeamErrorCode::Disabled`] when Team mode is off;
    /// [`TeamErrorCode::TaskNotFound`] when no task has that id.
    pub fn get(&self, id: &TeamTaskId) -> Result<TeamTaskView, TeamError> {
        self.require_enabled()?;
        let task = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == *id)
            .ok_or_else(|| not_found(id))?;
        Ok(project_task_view(&self.state, task))
    }

    /// Lists current non-deleted tasks in creation order. Upstream `TeamTaskBoard.list`.
    ///
    /// # Errors
    ///
    /// [`TeamErrorCode::Disabled`] when Team mode is off.
    pub fn list(&self) -> Result<Vec<TeamTaskView>, TeamError> {
        self.require_enabled()?;
        Ok(self
            .state
            .tasks
            .iter()
            .filter(|task| task.status != TeamTaskStatus::Deleted)
            .map(|task| project_task_view(&self.state, task))
            .collect())
    }

    /// The filtered, paged board read of the `team_task_list` tool.
    ///
    /// Filtering happens before paging, so `nextCursor` refers to the filtered list, and a
    /// page is never silently short because rows outside the filter were counted.
    ///
    /// `owner` is compared against the *resolved* owner identity, which is why the caller
    /// resolves a name to a Session id first (upstream `subagents.resolveName`) — a
    /// comparison against the display name would list nothing for an unnamed member.
    ///
    /// # Errors
    ///
    /// [`TeamErrorCode::InvalidArgument`] for an impossible cursor/limit or a combined
    /// `owner`/`unowned` filter; [`TeamErrorCode::Disabled`] when Team mode is off.
    pub fn list_page(&self, filter: &TeamTaskFilter) -> Result<TeamTaskList, TeamError> {
        if filter.cursor > u64::from(u32::MAX) {
            return Err(TeamError::new(
                TeamErrorCode::InvalidArgument,
                "cursor must be a non-negative safe integer",
            ));
        }
        if filter.limit < 1 || filter.limit > 100 {
            return Err(TeamError::new(
                TeamErrorCode::InvalidArgument,
                "limit must be an integer from 1 through 100",
            ));
        }
        if filter.owner.is_some() && filter.unowned.is_some() {
            return Err(TeamError::new(
                TeamErrorCode::InvalidArgument,
                "owner and unowned filters cannot be combined",
            ));
        }
        let mut rows: Vec<TeamTaskView> = self
            .list()?
            .into_iter()
            .filter(|task| {
                filter.status.is_none_or(|status| task.status == status)
                    && filter
                        .owner
                        .as_ref()
                        .is_none_or(|owner| task.owner_id.as_ref() == Some(owner))
                    && filter
                        .unowned
                        .is_none_or(|unowned| task.owner_id.is_none() == unowned)
                    && filter.ready.is_none_or(|ready| task.ready == ready)
            })
            .collect();
        let cursor = usize::try_from(filter.cursor).unwrap_or(usize::MAX);
        let limit = usize::from(filter.limit);
        let total = rows.len();
        rows.drain(..cursor.min(total));
        rows.truncate(limit);
        let next = cursor.saturating_add(limit);
        Ok(TeamTaskList {
            tasks: rows,
            next_cursor: u64::try_from(next)
                .ok()
                .filter(|next| usize::try_from(*next).is_ok_and(|next| next < total)),
        })
    }

    /// Compare-and-set one authorized task transition. Upstream
    /// `TeamTaskBoard.update`.
    ///
    /// The order of the refusals is part of the contract and is transcribed from
    /// upstream: not-found, stale revision, deleted, then authorization, then the action's
    /// own preconditions. A caller that swapped authorization before the revision check
    /// would report "unauthorized" for a task that simply moved on.
    ///
    /// `members` is the root's continuable descendants, used only to validate a `reassign`
    /// target. Upstream obtains it from `ctx.subagents.listDescendants`.
    ///
    /// # Errors
    ///
    /// Every [`TeamErrorCode`] in the team domain; see [`UpdateTeamTaskRequest`].
    #[allow(
        clippy::too_many_lines,
        reason = "the action table is one match transcribed from upstream's switch; splitting it would separate the authorization check from every transition it guards"
    )]
    pub fn update(
        &mut self,
        caller: &TeamMember,
        request: &UpdateTeamTaskRequest,
        members: &[TeamMember],
    ) -> Result<TeamTaskView, TeamError> {
        self.require_enabled()?;
        let current = self
            .state
            .tasks
            .iter()
            .find(|task| task.id == request.task_id)
            .cloned()
            .ok_or_else(|| not_found(&request.task_id))?;
        if current.revision != request.expected_revision {
            return Err(stale_revision(&current, request.expected_revision));
        }
        if current.status == TeamTaskStatus::Deleted {
            return Err(TeamError::new(
                TeamErrorCode::TaskDeleted,
                format!("team task {} is deleted", current.id.as_str()),
            ));
        }
        let lead = caller.id == *self.state.root();
        let owner = current.owner_id.as_ref() == Some(&caller.id);
        let authorize_owner = || {
            if !lead && !owner {
                return Err(TeamError::new(
                    TeamErrorCode::TaskUnauthorized,
                    "task mutation requires its owner or Team Lead",
                ));
            }
            Ok(())
        };
        let next = match request.action {
            TeamTaskAction::Claim => {
                if let Some(existing) = &current.owner_id
                    && *existing != caller.id
                {
                    return Err(TeamError::new(
                        TeamErrorCode::TaskAlreadyClaimed,
                        format!(
                            "team task {} is owned by another member",
                            current.id.as_str()
                        ),
                    ));
                }
                if current.status != TeamTaskStatus::Pending || !task_ready(&self.state, &current) {
                    return Err(TeamError::new(
                        TeamErrorCode::TaskBlocked,
                        format!("team task {} is not ready to claim", current.id.as_str()),
                    ));
                }
                let mut next = without_owner(&current);
                next.status = TeamTaskStatus::InProgress;
                next.owner_id = Some(caller.id.clone());
                // Upstream omits `ownerName` when the resolved name *is* the Session id, so
                // an unnamed root does not get a redundant display name.
                if caller.label() != caller.id.as_str() {
                    next.owner_name = Some(caller.label().to_owned());
                }
                next
            }
            TeamTaskAction::Release => {
                authorize_owner()?;
                if current.status != TeamTaskStatus::InProgress {
                    return Err(invalid_transition(
                        "only an in-progress task can be released",
                    ));
                }
                let mut next = without_owner(&current);
                next.status = TeamTaskStatus::Pending;
                next
            }
            TeamTaskAction::Edit => {
                authorize_owner()?;
                if request.subject.is_none()
                    && request.description.is_none()
                    && request.write_scopes.is_none()
                {
                    return Err(TeamError::new(
                        TeamErrorCode::InvalidArgument,
                        "task edit requires subject, description, or write_scopes",
                    ));
                }
                let mut next = current.clone();
                if let Some(subject) = &request.subject {
                    next.subject = required_text(subject, "subject", MAX_SUBJECT_CHARS)?;
                }
                if let Some(description) = &request.description {
                    next.description =
                        required_text(description, "description", MAX_DESCRIPTION_CHARS)?;
                }
                if let Some(scopes) = &request.write_scopes {
                    next.write_scopes = normalize_write_scopes(scopes)?;
                }
                next
            }
            TeamTaskAction::SetDependencies => {
                authorize_owner()?;
                let Some(blocked_by) = &request.blocked_by else {
                    return Err(TeamError::new(
                        TeamErrorCode::InvalidArgument,
                        "set_dependencies requires blocked_by",
                    ));
                };
                let mut next = current.clone();
                next.blocked_by = self.dependencies(blocked_by, Some(&current.id))?;
                next
            }
            TeamTaskAction::Complete => {
                authorize_owner()?;
                if current.status != TeamTaskStatus::InProgress {
                    return Err(invalid_transition("only an in-progress task can complete"));
                }
                let mut next = current.clone();
                next.status = TeamTaskStatus::Completed;
                next
            }
            TeamTaskAction::Reopen => {
                authorize_owner()?;
                if current.status != TeamTaskStatus::Completed {
                    return Err(invalid_transition("only a completed task can reopen"));
                }
                let mut next = without_owner(&current);
                next.status = TeamTaskStatus::Pending;
                next
            }
            TeamTaskAction::Reassign => {
                if !lead {
                    return Err(TeamError::new(
                        TeamErrorCode::LeadRequired,
                        "only the Team Lead can reassign tasks",
                    ));
                }
                if current.status != TeamTaskStatus::Pending
                    && current.status != TeamTaskStatus::InProgress
                {
                    return Err(invalid_transition(
                        "only a pending or in-progress task can be reassigned",
                    ));
                }
                match &request.owner {
                    // Upstream distinguishes `undefined` (field absent) from an explicit
                    // unassign only in the tool layer, where `owner === undefined` omits the
                    // field entirely and so never reaches `reassign`. Both spellings arrive
                    // here as "unassign", because the action was already chosen.
                    None | Some(Assignee::Nobody) => {
                        let mut next = without_owner(&current);
                        next.status = TeamTaskStatus::Pending;
                        next
                    }
                    Some(Assignee::Member(target)) => {
                        if !task_ready(&self.state, &current) {
                            return Err(TeamError::new(
                                TeamErrorCode::TaskBlocked,
                                format!("team task {} is blocked", current.id.as_str()),
                            ));
                        }
                        // The root is always a valid assignee; anything else must be a
                        // continuable descendant, because a settled or diagnostic entry
                        // cannot execute the task.
                        let owner_name = if *target == *self.state.root() {
                            Some(
                                members
                                    .iter()
                                    .find(|member| member.id == *target)
                                    .map_or_else(
                                        || self.state.root().as_str().to_owned(),
                                        |member| member.label().to_owned(),
                                    ),
                            )
                        } else if let Some(member) =
                            members.iter().find(|member| member.id == *target)
                        {
                            member.name.clone()
                        } else {
                            return Err(TeamError::new(
                                TeamErrorCode::MemberNotFound,
                                format!(
                                    "agent \"{}\" is not a continuable descendant of this task board's root",
                                    target.as_str()
                                ),
                            ));
                        };
                        let mut next = without_owner(&current);
                        next.status = TeamTaskStatus::InProgress;
                        next.owner_id = Some(target.clone());
                        next.owner_name = owner_name;
                        next
                    }
                }
            }
            TeamTaskAction::Delete => {
                authorize_owner()?;
                if let Some(dependent) = self.state.tasks.iter().find(|task| {
                    task.status != TeamTaskStatus::Deleted
                        && task.id != current.id
                        && task.blocked_by.contains(&current.id)
                }) {
                    return Err(TeamError::new(
                        TeamErrorCode::HasDependents,
                        format!(
                            "team task {} still blocks {}",
                            current.id.as_str(),
                            dependent.id.as_str()
                        ),
                    ));
                }
                let mut next = current.clone();
                next.status = TeamTaskStatus::Deleted;
                next
            }
        };
        let committed = TeamTaskSnapshot {
            revision: current.revision.saturating_add(1),
            ..next
        };
        self.commit(committed)
    }

    /// Validates and de-duplicates dependency ids against the current task graph.
    ///
    /// The candidate's own edge is refused here as a cycle, before the whole-graph walk,
    /// so the message names the task rather than an anonymous cycle.
    fn dependencies(
        &self,
        values: &[TeamTaskId],
        own_id: Option<&TeamTaskId>,
    ) -> Result<Vec<TeamTaskId>, TeamError> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut result = Vec::with_capacity(values.len());
        for id in values {
            if own_id.is_some_and(|own| own == id) {
                return Err(TeamError::new(
                    TeamErrorCode::DependencyCycle,
                    "a team task cannot block itself",
                ));
            }
            if !seen.insert(id.as_str()) {
                return Err(TeamError::new(
                    TeamErrorCode::InvalidArgument,
                    format!("duplicate blocker \"{}\"", id.as_str()),
                ));
            }
            let blocker = self.state.tasks.iter().find(|task| task.id == *id);
            if blocker.is_none_or(|task| task.status == TeamTaskStatus::Deleted) {
                return Err(TeamError::new(
                    TeamErrorCode::TaskNotFound,
                    format!("blocker task \"{}\" not found", id.as_str()),
                ));
            }
            result.push(id.clone());
        }
        Ok(result)
    }

    /// Counts the non-deleted tasks, which is what the cap bounds.
    fn count_active(&self) -> usize {
        self.state
            .tasks
            .iter()
            .filter(|task| task.status != TeamTaskStatus::Deleted)
            .count()
    }

    /// Whether Team mode is on, as the precondition every command shares.
    fn require_enabled(&self) -> Result<(), TeamError> {
        if self.state.is_enabled() {
            return Ok(());
        }
        Err(TeamError::new(
            TeamErrorCode::Disabled,
            "enable Agent Teams for this Session before using the task board",
        ))
    }

    /// Validates the whole graph, commits state, then appends. Upstream `create`/`update`
    /// tail: the append is last so a refused graph never reaches the journal.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the snapshot is consumed: it is stored in both the state and the journal, so a reference would force two clones"
    )]
    fn commit(&mut self, task: TeamTaskSnapshot) -> Result<TeamTaskView, TeamError> {
        assert_task_graph_candidate(&self.state.tasks, &task)
            .map_err(TeamTaskGraphError::into_team_error)?;
        self.state
            .apply_task(task.clone())
            .map_err(|message| TeamError::new(TeamErrorCode::InvalidArgument, message))?;
        if let Err(error) = self.journal.append(&task) {
            return Err(TeamError::new(
                TeamErrorCode::PersistenceUnconfirmed,
                format!(
                    "task \"{}\" revision {} is visible, but persistence was not confirmed; read the task before retrying ({})",
                    task.id.as_str(),
                    task.revision,
                    error.reason
                ),
            ));
        }
        Ok(project_task_view(&self.state, &task))
    }
}

/// The filters `team_task_list` applies before paging.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TeamTaskFilter {
    /// Exact status filter.
    pub status: Option<TeamTaskStatus>,
    /// Exact owner identity filter.
    pub owner: Option<SessionId>,
    /// Whether to keep only unowned (`true`) or only owned (`false`) tasks.
    pub unowned: Option<bool>,
    /// Readiness filter.
    pub ready: Option<bool>,
    /// Zero-based offset.
    pub cursor: u64,
    /// Rows per page, 1 through 100.
    pub limit: u16,
}

impl TeamTaskFilter {
    /// A first page of the default size (50, upstream's `limit ?? 50`).
    #[must_use]
    pub fn first_page() -> Self {
        Self {
            limit: 50,
            ..Self::default()
        }
    }
}

/// Copies a task without either owner field. Upstream `withoutOwner`.
fn without_owner(task: &TeamTaskSnapshot) -> TeamTaskSnapshot {
    TeamTaskSnapshot {
        owner_id: None,
        owner_name: None,
        ..task.clone()
    }
}

/// Normalizes and de-duplicates write scopes, preserving first-occurrence order.
fn normalize_write_scopes(values: &[String]) -> Result<Vec<String>, TeamError> {
    let mut result: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        let scope = write_scope(value)?;
        if !result.contains(&scope) {
            result.push(scope);
        }
    }
    Ok(result)
}

/// Validates one positive deployment limit. Upstream `positiveLimit`.
fn positive_limit(name: &str, value: usize) -> Result<usize, TeamError> {
    if value < 1 {
        return Err(TeamError::new(
            TeamErrorCode::InvalidConfig,
            format!("{name} must be a positive safe integer"),
        ));
    }
    Ok(value)
}

fn not_found(id: &TeamTaskId) -> TeamError {
    TeamError::new(
        TeamErrorCode::TaskNotFound,
        format!("team task \"{}\" not found", id.as_str()),
    )
}

/// The compare-and-set refusal, which names both revisions so the caller can re-read
/// without guessing.
fn stale_revision(task: &TeamTaskSnapshot, expected: u64) -> TeamError {
    TeamError::new(
        TeamErrorCode::StaleRevision,
        format!(
            "stale team task {} revision {expected}; current revision is {}",
            task.id.as_str(),
            task.revision
        ),
    )
}

fn invalid_transition(message: &str) -> TeamError {
    TeamError::new(TeamErrorCode::InvalidTransition, message)
}

/// Configuration keys upstream removed when Team reused subagent creation and messaging.
/// Upstream `REMOVED_CONFIG_KEYS`; a deployment still setting one is refused rather than
/// silently ignored, because it would otherwise believe a limit is in force.
pub const REMOVED_CONFIG_KEYS: [&str; 3] = ["maxMembers", "maxMessageBytes", "disposalTimeoutMs"];

/// Rejects a configuration that still sets a removed key. Upstream's constructor check.
///
/// # Errors
///
/// [`TeamErrorCode::InvalidConfig`] naming the first removed key present.
pub fn reject_removed_config(keys: impl IntoIterator<Item = String>) -> Result<(), TeamError> {
    for key in keys {
        if REMOVED_CONFIG_KEYS.contains(&key.as_str()) {
            return Err(TeamError::new(
                TeamErrorCode::InvalidConfig,
                format!(
                    "agent-team: {key} is no longer supported; teammates are subagents, so subagent limits and inbox delivery apply"
                ),
            ));
        }
    }
    Ok(())
}

/// The Team configuration a board is built from. Upstream `Config`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamConfig {
    /// Non-deleted task cap; upstream defaults it to [`DEFAULT_MAX_TASKS`].
    pub max_tasks: usize,
    /// Whether a new root starts with Team mode on; upstream defaults to false.
    pub default_enabled: bool,
}

impl Default for TeamConfig {
    fn default() -> Self {
        Self {
            max_tasks: DEFAULT_MAX_TASKS,
            default_enabled: false,
        }
    }
}

impl TeamConfig {
    /// Applies the removed-key rejection and the positive-limit rule, then returns the
    /// resolved configuration.
    ///
    /// # Errors
    ///
    /// [`TeamErrorCode::InvalidConfig`] for a removed key or a zero cap.
    pub fn resolve(
        max_tasks: Option<usize>,
        default_enabled: Option<bool>,
        supplied_keys: impl IntoIterator<Item = String>,
    ) -> Result<Self, TeamError> {
        reject_removed_config(supplied_keys)?;
        Ok(Self {
            max_tasks: positive_limit("maxTasks", max_tasks.unwrap_or(DEFAULT_MAX_TASKS))?,
            default_enabled: default_enabled.unwrap_or(false),
        })
    }
}

/// Renders the task board as a numbered list, for a host that wants a text summary.
///
/// Not an upstream symbol: upstream renders in the browser. Provided because a text
/// digest is the natural way to show a board in this crate's non-browser surfaces, and
/// because it makes the derived fields (`ready`, owner, warnings) inspectable.
#[must_use]
pub fn render_board(tasks: &[TeamTaskView]) -> String {
    if tasks.is_empty() {
        return "(no tasks)".to_owned();
    }
    let mut out = String::new();
    for task in tasks {
        let owner = task
            .owner_name
            .as_deref()
            .or(task.owner_id.as_ref().map(Id::as_str))
            .unwrap_or("-");
        let _ = writeln!(
            out,
            "{} [{}] rev {} {} owner={owner}{}",
            task.id.as_str(),
            task.status.as_str(),
            task.revision,
            task.subject,
            if task.ready { " ready" } else { "" }
        );
        for blocker in &task.blocked_by {
            let _ = writeln!(out, "  blocked by {}", blocker.as_str());
        }
        for warning in &task.write_scope_warnings {
            let _ = writeln!(out, "  {warning}");
        }
    }
    out
}
