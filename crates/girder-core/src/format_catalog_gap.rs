//! The Session **format catalog**: the inventory that answers "which version reads which codec,
//! and which edge carries it forward".
//!
//! Upstream splits this over two packages. `@deepseek-ai/dsh-session-format` supplies
//! `createSessionFormatCatalog`, the generic assembler; `@deepseek-ai/dsh-session-format-catalog`
//! supplies the *first-party* inventory that the assembler is called with, and nothing else. The
//! catalog package is 153 lines and holds no logic of its own — it is a build-static declaration
//! that five physical codecs and four adjacent edges exist, plus two derived variants of that
//! declaration. Everything this module implements is the assembler the declaration is fed to.
//!
//! # What upstream's catalog does
//!
//! `createSessionFormatCatalog(options)` compiles a declaration into four operations, and it
//! **validates the declaration before it will compile it**. That validation is the part with no
//! local equivalent, and it is deliberately exhaustive:
//!
//! | check | upstream error | why a build fails rather than degrades |
//! |---|---|---|
//! | two codecs claim one version | `Session format codec v{N} is duplicated` | the dispatch table would silently keep the last one |
//! | a version `0..=current` has no codec | `Session format codec v{N} is missing` | a log in that generation becomes unreadable with no diagnostic |
//! | a codec above `current` | `Session format codec v{N} is newer than current v{M}` | a build would claim to read a generation it does not write |
//! | two edges leave one version | `Session migration v{A}->v{B} is duplicated` | `plan()` slices a dense array by version, so a duplicate corrupts the slice |
//! | two edges share a name | `Session migration name "X" is duplicated` | refusals name the edge, so a duplicate makes a refusal ambiguous |
//! | an edge is not `v{N}->v{N+1}` | `X must declare adjacent v{N}->v{N+1}` | non-adjacent composition is not what the chain planner implements |
//! | an edge leaves a version `0..current` | `Session migration v{N}->v{N+1} is missing` | a gap fails every read across it |
//! | an edge leaves `v{N}` for `N >= current` | `Session migration from v{N} does not lead to current v{M}` | an edge above the writer is unreachable by construction |
//!
//! The last of those is thrown as `SessionFormatUnsupportedMigrationError`, not
//! `SessionFormatError`: a hole in the *released* edge set means "this build cannot read that
//! artifact", which is a different answer from "the catalog is malformed". This module keeps that
//! distinction in [`CatalogError::is_unsupported`].
//!
//! Two further operations depend on the validated shape:
//!
//! - **header-only classification** (`readHeader`) answers `current`, `migration-required`,
//!   `unsupported`, or `malformed` from the header alone, never opening an event body. Listing a
//!   directory of sessions needs exactly that; `current` vs `migration-required` is what a list
//!   view renders as "needs upgrade".
//! - **the plan** (`chain.plan`), which is private upstream: the dense suffix of edges from the
//!   stored version to the current one. It can be a slice *because* the chain was proven complete
//!   and adjacent.
//!
//! The catalog package then instantiates that assembler twice over the same codec set:
//! `sessionFormatCatalogOptions` (`generated.ts`) stops at v4, and `historicalSessionFormatCatalog`
//! (`historical.ts`) stops at **v3** so that collecting a child's prerequisites cannot recursively
//! open a current session. The two differ only in `currentVersion`, the codec list, the edge list,
//! and the restorers — which is why "what is current" is a field, not a constant.
//!
//! # How this crate organises the same thing today
//!
//! [`crate::session_migration`] has the version axis and the edge set, as `const fn`s:
//!
//! - [`FormatVersion`] — `V0..=V4`, [`FormatVersion::ALL`], [`FormatVersion::from_number`],
//!   [`FormatVersion::next`], [`FormatVersion::as_number`], and `CURRENT = V4`.
//! - [`Migration::edges`] — the same four adjacent edges, as a literal `[Self; 4]`.
//! - [`Migration::is_supported`] / [`Migration::unsupported_reason`] — whether the *row* migrator
//!   can perform an edge, and, when it cannot, the exact information the edge needs instead.
//!
//! That is genuinely close to upstream, and the honest reading of the gap report ("`format_catalog`
//! grep = 0; no catalog/codec registry") is that the **declaration** exists and the **assembler**
//! does not. Concretely, what has no local equivalent:
//!
//! 1. **Nothing cross-checks the inventory.** [`FormatVersion::ALL`] is a five-element literal and
//!    [`Migration::edges`] is a four-element literal. Nothing proves they agree, and no test could
//!    fail if a generation were added to one and not the other — the two would simply disagree at
//!    runtime, with `from_number` accepting a version the chain cannot leave. Upstream cannot ship
//!    that state: module initialisation throws.
//! 2. **There is no version → reader mapping.** `detect()` classifies a *row*; nothing maps a
//!    *version* to the physical codec that reads it. That mapping exists only as control flow
//!    inside `Migrator::upgrade`, so it cannot be listed, compared, or counted.
//! 3. **There is no plan, and therefore no range query.** `Migrator::upgrade` performs exactly one
//!    edge, and the caller must loop with [`FormatVersion::next`]. `Migration::edges` returns all
//!    four regardless of where you are, so "which edges must run to bring v1 to current" is a
//!    question the types cannot express, let alone answer.
//! 4. **There is no header-only classification.** `session_extra_gap::assert_version` accepts
//!    *exactly* current and returns one refusal shape. It cannot say "v2, migration required", and
//!    it does not separate a **malformed** header (not an object, no version, a fractional or
//!    unsafe version, a missing `isSeeded`) from an **unsupported** one (a newer build). Those are
//!    different user-facing outcomes: the first is a corrupt file, the second is an upgrade prompt.
//! 5. **There is no build variant.** `FormatVersion::CURRENT` is a single `const`, so "read as far
//!    as v3 and stop" — upstream's `historicalSessionFormatCatalog`, and the reason it exists — is
//!    not expressible.
//!
//! # What this module adds, and what it deliberately does not
//!
//! [`FormatCatalog`] is the assembler: [`FormatCatalog::build`] validates a declaration exactly as
//! upstream does, and the resulting value answers [`FormatCatalog::codec`] (version → reader),
//! [`FormatCatalog::plan`] (the dense edge suffix — the range query), [`FormatCatalog::route`]
//! (whether that suffix is actually traversable by this build), [`FormatCatalog::classify`], and
//! [`FormatCatalog::read_header`] (header-only classification).
//!
//! It is **pure data plus queries**. It reads no file, decodes no row, migrates no header, and owns
//! no I/O. Physical decoding is the codec's job — upstream's `SessionFormatCodec` trait — and header
//! and row transformation is the edge's, which is why [`HeaderRead`] reports the version and the
//! plan rather than a header it would have to invent. The one thing it adds beyond upstream's
//! private plan is [`Route`]: upstream's chain is complete by construction, so it never has to ask
//! whether the path it planned can be walked. This build's is not — `v1 → v2` has no implementation
//! — so a caller needs to know that *before* it starts a migration loop that will fail halfway.
//!
//! [`FormatCatalog::released`] is the local inventory, built from [`FormatVersion`] and
//! [`Migration::edges`] so that any disagreement between them becomes a [`CatalogError`] instead of
//! a runtime surprise, and [`FormatCatalog::check_capabilities`] proves the declared [`EdgeKind`] of
//! each edge matches what [`Migration::is_supported`] reports.
//!
//! # Reference: the local inventory
//!
//! ```text
//! version  codec                          edge into it                 kind
//! v0       releasedV0SessionFormatCodec   —                            —
//! v1       releasedV1SessionFormatCodec   v0->v1  session-format-v0-to-v1   row
//! v2       releasedV2SessionFormatCodec   v1->v2  session-format-v1-to-v2   missing
//! v3       releasedV3SessionFormatCodec   v2->v3  session-format-v2-to-v3   artifact
//! v4       releasedV4SessionFormatCodec   v3->v4  session-format-v3-to-v4   row
//! ```

use std::ops::RangeInclusive;

use girder_core::{FormatVersion, Migration};
use serde_json::{Map, Value};

/// The largest integer JavaScript represents exactly, which is the bound upstream enforces with
/// `Number.isSafeInteger`.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// [`MAX_SAFE_INTEGER`] as a float, written out so no lossy cast is needed to compare against it.
const MAX_SAFE_F64: f64 = 9_007_199_254_740_991.0;

/// The generation upstream's `historicalSessionFormatCatalog` stops at.
///
/// Upstream builds a second catalog over the same codecs whose `currentVersion` is 3, so a read
/// that must collect a child's migration prerequisites cannot recursively open a current session.
const HISTORICAL_CURRENT: u64 = 3;

/// One physical reader, registered for exactly one format version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecEntry {
    /// The version this codec reads.
    pub version: u64,
    /// Stable registration identity — upstream's exported codec name.
    pub name: &'static str,
}

impl CodecEntry {
    /// Registers a reader for `version`.
    #[must_use]
    pub const fn new(version: u64, name: &'static str) -> Self {
        Self { version, name }
    }
}

/// What this build can do with one adjacent edge.
///
/// Upstream encodes this by *which package is linked*: the catalog's `migrations` array holds the
/// real stage objects, so an edge is as capable as the code that produced it. A rewritten build
/// links modules for the same purpose, but the capability is then implicit in whether some other
/// module happens to implement it — hence an explicit kind here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    /// Every row converts on its own — [`crate::session_migration::Migrator::upgrade`].
    Row,
    /// The edge needs the whole artifact — [`crate::migration_steps_gap`].
    Artifact,
    /// No module in this build performs the edge, so a log that needs it cannot be read.
    Missing,
}

/// One adjacent step of the migration chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    /// The generation the edge reads.
    pub from: u64,
    /// The generation the edge writes.
    pub to: u64,
    /// The edge's identity, so a refusal can name it. Upstream's package name verbatim.
    pub name: &'static str,
    /// What this build can do with it.
    pub kind: EdgeKind,
}

impl Edge {
    /// An edge the row migrator performs.
    #[must_use]
    pub const fn row(from: u64, to: u64, name: &'static str) -> Self {
        Self {
            from,
            to,
            name,
            kind: EdgeKind::Row,
        }
    }

    /// An edge that needs the whole artifact.
    #[must_use]
    pub const fn artifact(from: u64, to: u64, name: &'static str) -> Self {
        Self {
            from,
            to,
            name,
            kind: EdgeKind::Artifact,
        }
    }

    /// An edge nothing in this build performs yet.
    #[must_use]
    pub const fn missing(from: u64, to: u64, name: &'static str) -> Self {
        Self {
            from,
            to,
            name,
            kind: EdgeKind::Missing,
        }
    }

    /// Whether a log that arrives at [`Self::from`] can be carried past this edge.
    #[must_use]
    pub const fn is_traversable(self) -> bool {
        !matches!(self.kind, EdgeKind::Missing)
    }
}

/// Why a version → reader declaration was refused.
///
/// Every variant but [`Self::CapabilityDrift`], [`Self::UnrepresentableVersion`], and
/// [`Self::CurrentVersionMismatch`] is a message upstream throws from
/// `createSessionFormatCatalog` or `createSessionFormatChain`; those three are the checks this
/// build needs *because* it has a second copy of the inventory in [`FormatVersion`] and
/// [`Migration`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    /// Two codecs claim one version.
    #[error("Session format codec v{version} is duplicated")]
    DuplicatedCodec {
        /// The contested version.
        version: u64,
    },

    /// A version between zero and current has no codec, so a log in it is unreadable.
    #[error("Session format codec v{version} is missing")]
    MissingCodec {
        /// The version with no reader.
        version: u64,
    },

    /// A codec reads a generation above the one this build writes.
    #[error("Session format codec v{version} is newer than current v{current}")]
    CodecNewerThanCurrent {
        /// The unrepresentable codec's version.
        version: u64,
        /// The generation this build writes.
        current: u64,
    },

    /// Two edges leave one version, which would corrupt the dense plan slice.
    #[error("Session migration v{from}->v{to} is duplicated")]
    DuplicatedMigration {
        /// The contested source generation.
        from: u64,
        /// The contested target generation.
        to: u64,
    },

    /// Two edges share an identity, so a refusal naming it would be ambiguous.
    #[error("Session migration name {name:?} is duplicated")]
    DuplicatedMigrationName {
        /// The repeated identity.
        name: String,
    },

    /// An edge claims to do more than one generation in a step.
    #[error("{name} must declare adjacent v{from}->v{want}")]
    NonAdjacentMigration {
        /// The edge's identity.
        name: String,
        /// The generation it reads.
        from: u64,
        /// The generation it claims to write.
        to: u64,
        /// The only target an adjacent edge from `from` may name.
        want: u64,
    },

    /// An edge with no identity cannot be named in a refusal.
    #[error("Session migration name must be a non-empty string")]
    EmptyMigrationName,

    /// A version below current has no edge, so every read across the hole fails.
    #[error("Session migration v{from}->v{to} is missing")]
    MissingMigration {
        /// The generation with no way out.
        from: u64,
        /// The generation the edge should have written.
        to: u64,
    },

    /// An edge leaves the writer's own generation, where nothing can consume it.
    #[error("Session migration from v{from} does not lead to current v{current}")]
    MigrationBeyondCurrent {
        /// The edge's source generation.
        from: u64,
        /// The generation this build writes.
        current: u64,
    },

    /// A stored log is newer than this build.
    #[error("stored Session uses newer format v{from}; this build writes v{current}")]
    NewerThanBuild {
        /// The generation the log declares.
        from: u64,
        /// The generation this build writes.
        current: u64,
    },

    /// The catalog and [`FormatVersion`] disagree about which generations exist.
    #[error("Session format v{version} has no FormatVersion variant")]
    UnrepresentableVersion {
        /// The version [`FormatVersion::from_number`] cannot name.
        version: u64,
    },

    /// The catalog and [`FormatVersion::CURRENT`] disagree about which generation is current.
    #[error("catalog writes v{catalog}, but FormatVersion::CURRENT is v{build}")]
    CurrentVersionMismatch {
        /// The catalog's declared current version.
        catalog: u64,
        /// The build's current version.
        build: u64,
    },

    /// An edge's declared capability and the row migrator disagree about it.
    #[error("edge v{from}->v{to} is declared {declared:?}, but {reason}")]
    CapabilityDrift {
        /// The edge's source generation.
        from: u64,
        /// The edge's target generation.
        to: u64,
        /// What the catalog declared.
        declared: EdgeKind,
        /// What the row migrator reports instead.
        reason: &'static str,
    },
}

impl CatalogError {
    /// Whether the error means "this artifact cannot be read here" rather than "the catalog is
    /// broken".
    ///
    /// Upstream draws the same line with two classes: `SessionFormatUnsupportedMigrationError`
    /// versus `SessionFormatError`. It matters because the first is a property of the *log* — the
    /// user should upgrade or convert elsewhere — while the second is a property of the *build*.
    #[must_use]
    pub const fn is_unsupported(&self) -> bool {
        matches!(
            self,
            Self::MissingMigration { .. }
                | Self::NewerThanBuild { .. }
                | Self::MigrationBeyondCurrent { .. }
        )
    }
}

/// What a stored version number means to this build, judged without reading a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionClass {
    /// The generation the installed writer emits; nothing to do.
    Current,
    /// Older than current, and this catalog has a codec for it.
    MigrationRequired,
    /// Written by a newer harness, which this build must not pretend to understand.
    NewerThanBuild {
        /// The generation the log declares.
        stored: u64,
        /// The generation this build writes.
        current: u64,
    },
}

/// The edges a stored version must cross, and where this build runs out of capability.
///
/// Upstream's chain is complete by construction, so it can return a bare slice and trust that
/// walking it succeeds. This build's is not: `v1 → v2` has no implementation, so a caller that
/// starts a migration loop from v1 walks one edge and then fails. Knowing that in advance is the
/// point of this type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    from: u64,
    target: u64,
    edges: Vec<Edge>,
    blocked_at: Option<usize>,
}

impl Route {
    /// The generation the route starts at.
    #[must_use]
    pub const fn source(&self) -> u64 {
        self.from
    }

    /// The generation the route aims at — the catalog's current version.
    #[must_use]
    pub const fn target(&self) -> u64 {
        self.target
    }

    /// The edges in order, whether or not this build can perform them.
    #[must_use]
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// Whether every edge on the route is implemented.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.blocked_at.is_none()
    }

    /// Index into [`Self::edges`] of the first edge nothing implements.
    #[must_use]
    pub const fn blocked_at(&self) -> Option<usize> {
        self.blocked_at
    }

    /// The first edge nothing implements.
    #[must_use]
    pub fn blocked_edge(&self) -> Option<Edge> {
        self.blocked_at
            .and_then(|index| self.edges.get(index))
            .copied()
    }

    /// The newest generation a log starting at [`Self::from`] can actually reach here.
    ///
    /// Equal to [`Self::target`] for a complete route, and to the blocked edge's source generation
    /// otherwise.
    #[must_use]
    pub fn reachable_version(&self) -> u64 {
        if self.is_complete() {
            return self.target;
        }
        self.blocked_edge().map_or(self.target, |edge| edge.from)
    }
}

/// What a header alone says about a stored log.
///
/// The shape is upstream's `SessionFormatHeaderReadResult`, with one deliberate difference: instead
/// of a migrated header, [`Self::MigrationRequired`] carries the [`Route`] the caller must walk.
/// This module owns no header transformation, so returning a header it did not migrate would be a
/// plausible-looking wrong answer; returning the plan is what the caller actually needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderRead {
    /// The header declares the generation this build writes.
    Current {
        /// The generation the header declares.
        version: u64,
    },
    /// The header declares an older generation, and every edge to current is implemented.
    MigrationRequired {
        /// The generation the header declares.
        stored: u64,
        /// The generation this build writes.
        target: u64,
        /// The edges that carry it forward.
        route: Route,
    },
    /// The header is readable but this build cannot honour it.
    Unsupported {
        /// The generation the header declares, when it was readable at all.
        stored: Option<u64>,
        /// The generation this build writes.
        target: u64,
        /// Why, in terms the user can act on.
        reason: String,
    },
    /// The header is not a valid session header.
    Malformed {
        /// The generation the header declares, when it was readable at all.
        stored: Option<u64>,
        /// The generation this build writes.
        target: u64,
        /// Which rule it broke.
        reason: String,
    },
}

impl HeaderRead {
    /// Whether this build can open the log the header describes without further work.
    #[must_use]
    pub const fn is_current(&self) -> bool {
        matches!(self, Self::Current { .. })
    }

    /// Whether the log can be opened by walking [`Self::MigrationRequired`]'s route.
    #[must_use]
    pub const fn is_migration_required(&self) -> bool {
        matches!(self, Self::MigrationRequired { .. })
    }

    /// Whether the log cannot be opened here at all.
    #[must_use]
    pub const fn is_refused(&self) -> bool {
        matches!(self, Self::Unsupported { .. } | Self::Malformed { .. })
    }

    /// The generation the header declares, when it declared a readable one.
    #[must_use]
    pub const fn stored_version(&self) -> Option<u64> {
        match self {
            Self::Current { version } => Some(*version),
            Self::MigrationRequired { stored, .. } => Some(*stored),
            Self::Unsupported { stored, .. } | Self::Malformed { stored, .. } => *stored,
        }
    }

    /// The user-facing sentence, for the two refused variants.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Current { .. } | Self::MigrationRequired { .. } => None,
            Self::Unsupported { reason, .. } | Self::Malformed { reason, .. } => Some(reason),
        }
    }
}

/// A validated version → reader inventory and its adjacent-edge chain.
///
/// Constructed by [`Self::build`], which refuses every declaration upstream refuses; once built the
/// chain is dense, adjacent, and complete up to [`Self::current`], so the plan is a slice and a
/// lookup cannot miss.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatCatalog {
    current: u64,
    codecs: Vec<CodecEntry>,
    edges: Vec<Edge>,
}

impl FormatCatalog {
    /// Validates a declaration and compiles it into a catalog.
    ///
    /// The checks and their order follow `createSessionFormatCatalog` followed by
    /// `createSessionFormatChain`: codecs first (duplicated, missing, newer than current), then
    /// edges (identity, adjacency, duplication, completeness, reachability).
    ///
    /// # Errors
    ///
    /// Returns the [`CatalogError`] naming the first violated rule. A
    /// [`CatalogError::MissingMigration`] is flagged by [`CatalogError::is_unsupported`] as a
    /// property of the artifact rather than of the declaration.
    pub fn build(
        current: u64,
        codecs: &[CodecEntry],
        edges: &[Edge],
    ) -> Result<Self, CatalogError> {
        let mut codecs = codecs.to_vec();
        codecs.sort_by_key(|codec| codec.version);
        for pair in codecs.windows(2) {
            let [left, right] = pair else { continue };
            if left.version != right.version {
                continue;
            }
            return Err(CatalogError::DuplicatedCodec {
                version: left.version,
            });
        }
        for version in 0..=current {
            if !codecs.iter().any(|codec| codec.version == version) {
                return Err(CatalogError::MissingCodec { version });
            }
        }
        if let Some(codec) = codecs.iter().find(|codec| codec.version > current) {
            return Err(CatalogError::CodecNewerThanCurrent {
                version: codec.version,
                current,
            });
        }

        let mut edges = edges.to_vec();
        edges.sort_by_key(|edge| edge.from);
        for edge in &edges {
            if edge.name.is_empty() {
                return Err(CatalogError::EmptyMigrationName);
            }
            let want = edge.from.checked_add(1).unwrap_or(edge.to);
            if edge.to != want {
                return Err(CatalogError::NonAdjacentMigration {
                    name: edge.name.to_owned(),
                    from: edge.from,
                    to: edge.to,
                    want,
                });
            }
        }
        for pair in edges.windows(2) {
            let [left, right] = pair else { continue };
            if left.from != right.from {
                continue;
            }
            return Err(CatalogError::DuplicatedMigration {
                from: left.from,
                to: left.to,
            });
        }
        for (index, edge) in edges.iter().enumerate() {
            if edges
                .iter()
                .take(index)
                .any(|earlier| earlier.name == edge.name)
            {
                return Err(CatalogError::DuplicatedMigrationName {
                    name: edge.name.to_owned(),
                });
            }
        }
        for version in 0..current {
            if !edges.iter().any(|edge| edge.from == version) {
                return Err(CatalogError::MissingMigration {
                    from: version,
                    to: version.saturating_add(1),
                });
            }
        }
        if let Some(edge) = edges.iter().find(|edge| edge.from >= current) {
            return Err(CatalogError::MigrationBeyondCurrent {
                from: edge.from,
                current,
            });
        }

        Ok(Self {
            current,
            codecs,
            edges,
        })
    }

    /// The local inventory: this crate's five released codecs and four adjacent edges.
    ///
    /// Derived from [`FormatVersion`] and [`Migration::edges`] rather than restated, so any drift
    /// between the two literals becomes an error here. The declared [`EdgeKind`] of each edge is
    /// what this build actually implements: `v0 → v1` and `v3 → v4` row-at-a-time, `v2 → v3` across
    /// the whole artifact, and `v1 → v2` nowhere.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::CapabilityDrift`] when a declared kind and
    /// [`Migration::is_supported`] disagree, [`CatalogError::UnrepresentableVersion`] when the
    /// catalog names a generation [`FormatVersion`] has no variant for, and
    /// [`CatalogError::CurrentVersionMismatch`] when the two disagree about which generation is
    /// current. None of them can fire while the two literals agree.
    pub fn released() -> Result<Self, CatalogError> {
        let catalog = Self::build(
            FormatVersion::CURRENT.as_number(),
            &released_codecs(),
            &released_edges(),
        )?;
        catalog.check_capabilities()?;
        Ok(catalog)
    }

    /// Upstream's `historicalSessionFormatCatalog`: the same readers, stopping at v3.
    ///
    /// A historical read exists so that collecting a child's prerequisites cannot recursively open
    /// a current session — it is the same inventory with a lower ceiling. Against this catalog, a v4
    /// log is [`VersionClass::NewerThanBuild`], which is exactly the upstream behaviour.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Self::build`] refuses. The declaration is structurally valid, so this is
    /// the codec and edge completeness checks.
    pub fn historical() -> Result<Self, CatalogError> {
        let codecs: Vec<CodecEntry> = released_codecs()
            .into_iter()
            .filter(|codec| codec.version <= HISTORICAL_CURRENT)
            .collect();
        let edges: Vec<Edge> = released_edges()
            .into_iter()
            .filter(|edge| edge.from < HISTORICAL_CURRENT)
            .collect();
        Self::build(HISTORICAL_CURRENT, &codecs, &edges)
    }

    /// The generation this catalog writes.
    #[must_use]
    pub const fn current(&self) -> u64 {
        self.current
    }

    /// Every registered reader, oldest first.
    #[must_use]
    pub fn codecs(&self) -> &[CodecEntry] {
        &self.codecs
    }

    /// Every adjacent edge, oldest first.
    #[must_use]
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// The reader for `version`, or `None` when this catalog has none.
    #[must_use]
    pub fn codec(&self, version: u64) -> Option<&CodecEntry> {
        self.codecs
            .binary_search_by_key(&version, |entry| entry.version)
            .ok()
            .and_then(|index| self.codecs.get(index))
    }

    /// The edge leaving `version`, or `None` when it is current or unknown.
    #[must_use]
    pub fn edge(&self, version: u64) -> Option<&Edge> {
        self.edges.iter().find(|edge| edge.from == version)
    }

    /// The generations this catalog reads, as a range query.
    ///
    /// A validated catalog reads exactly `0..=current`; the range is exposed as a range rather than
    /// a slice so a caller can test membership without materialising the codec list.
    #[must_use]
    pub fn readable_versions(&self) -> RangeInclusive<u64> {
        0..=self.current
    }

    /// Whether this catalog has a reader for `version`.
    #[must_use]
    pub fn reads(&self, version: u64) -> bool {
        self.readable_versions().contains(&version)
    }

    /// Classifies a stored version number without touching a row.
    #[must_use]
    pub const fn classify(&self, version: u64) -> VersionClass {
        if version > self.current {
            VersionClass::NewerThanBuild {
                stored: version,
                current: self.current,
            }
        } else if version == self.current {
            VersionClass::Current
        } else {
            VersionClass::MigrationRequired
        }
    }

    /// The dense suffix of edges a log at `from` must cross.
    ///
    /// This is upstream's private `chain.plan`. It is a slice because the chain was proven adjacent
    /// and complete, so the edge leaving `v{n}` is at index `n`.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NewerThanBuild`] when `from` is above [`Self::current`] — a plan
    /// would have to invent edges rather than select them.
    pub fn plan(&self, from: u64) -> Result<&[Edge], CatalogError> {
        if from > self.current {
            return Err(CatalogError::NewerThanBuild {
                from,
                current: self.current,
            });
        }
        let start = usize::try_from(from).map_err(|_| CatalogError::NewerThanBuild {
            from,
            current: self.current,
        })?;
        self.edges.get(start..).ok_or(CatalogError::NewerThanBuild {
            from,
            current: self.current,
        })
    }

    /// The plan, annotated with whether this build can walk it.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::plan`].
    pub fn route(&self, from: u64) -> Result<Route, CatalogError> {
        let edges = self.plan(from)?.to_vec();
        let blocked_at = edges.iter().position(|edge| !edge.is_traversable());
        Ok(Route {
            from,
            target: self.current,
            edges,
            blocked_at,
        })
    }

    /// Classifies a header without reading an event body.
    ///
    /// Mirrors `readHeader`'s four outcomes and its order: the version is inspected first, a
    /// version above current is *unsupported* before any shape check, then the header's shape is
    /// validated, and only then is the route planned. The difference from upstream is that this
    /// build's route can be *blocked*: a v1 header classifies as
    /// [`HeaderRead::Unsupported`] rather than `migration-required`, because no edge from v1 is
    /// implemented. Reporting it as migration-required would start a loop that fails one edge later.
    ///
    /// The `header` must already be decoded: physical decoding is the codec's job, which this
    /// module by design does not do.
    #[must_use]
    pub fn read_header(&self, header: &Value) -> HeaderRead {
        let Some(object) = header.as_object() else {
            return HeaderRead::Malformed {
                stored: None,
                target: self.current,
                reason: "Session header must be a JSON object".to_owned(),
            };
        };
        let Some(stored) = object.get("version").and_then(safe_integer) else {
            return HeaderRead::Malformed {
                stored: None,
                target: self.current,
                reason: "Session format version must be a non-negative safe integer".to_owned(),
            };
        };
        if stored > self.current {
            return HeaderRead::Unsupported {
                stored: Some(stored),
                target: self.current,
                reason: format!(
                    "stored Session uses newer format v{stored}; this build writes v{}",
                    self.current
                ),
            };
        }
        if let Some(reason) = header_shape_refusal(object) {
            return HeaderRead::Malformed {
                stored: Some(stored),
                target: self.current,
                reason,
            };
        }
        if stored == self.current {
            return HeaderRead::Current { version: stored };
        }
        let Ok(route) = self.route(stored) else {
            return HeaderRead::Unsupported {
                stored: Some(stored),
                target: self.current,
                reason: format!(
                    "stored Session uses format v{stored}; this build cannot plan a migration"
                ),
            };
        };
        if route.is_complete() {
            HeaderRead::MigrationRequired {
                stored,
                target: self.current,
                route,
            }
        } else {
            HeaderRead::Unsupported {
                stored: Some(stored),
                target: self.current,
                reason: blocked_reason(&route),
            }
        }
    }

    /// Proves the declaration agrees with the rest of this build.
    ///
    /// Three independent statements about the inventory live in three places — this catalog's
    /// `current`, [`FormatVersion`]'s variants and `CURRENT`, and [`Migration`]'s edge table — and
    /// nothing but this method makes them agree. It is the check upstream gets for free by having a
    /// single declaration.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::CurrentVersionMismatch`] when this catalog writes a different
    /// generation than [`FormatVersion::CURRENT`], [`CatalogError::UnrepresentableVersion`] when it
    /// names a generation [`FormatVersion::from_number`] cannot, and
    /// [`CatalogError::CapabilityDrift`] when an edge's declared [`EdgeKind`] contradicts
    /// [`Migration::is_supported`].
    pub fn check_capabilities(&self) -> Result<(), CatalogError> {
        for version in self.readable_versions() {
            if FormatVersion::from_number(version).is_none() {
                return Err(CatalogError::UnrepresentableVersion { version });
            }
        }
        let build = FormatVersion::CURRENT.as_number();
        if self.current != build {
            return Err(CatalogError::CurrentVersionMismatch {
                catalog: self.current,
                build,
            });
        }
        for edge in &self.edges {
            let (Some(from), Some(to)) = (
                FormatVersion::from_number(edge.from),
                FormatVersion::from_number(edge.to),
            ) else {
                continue;
            };
            let migrator = Migration::new(from, to);
            let reason = match edge.kind {
                EdgeKind::Row if !migrator.is_supported() => {
                    Some("the row migrator reports the edge unsupported")
                }
                EdgeKind::Artifact | EdgeKind::Missing if migrator.is_supported() => {
                    Some("the row migrator already performs the edge")
                }
                _ => None,
            };
            if let Some(reason) = reason {
                return Err(CatalogError::CapabilityDrift {
                    from: edge.from,
                    to: edge.to,
                    declared: edge.kind,
                    reason,
                });
            }
        }
        Ok(())
    }
}

/// The five released physical codecs, oldest first.
fn released_codecs() -> Vec<CodecEntry> {
    vec![
        CodecEntry::new(0, "releasedV0SessionFormatCodec"),
        CodecEntry::new(1, "releasedV1SessionFormatCodec"),
        CodecEntry::new(2, "releasedV2SessionFormatCodec"),
        CodecEntry::new(3, "releasedV3SessionFormatCodec"),
        CodecEntry::new(4, "releasedV4SessionFormatCodec"),
    ]
}

/// The four adjacent edges, as this build implements them.
///
/// The names are upstream's migration package names verbatim, so a refusal can quote an identity a
/// reader can look up.
fn released_edges() -> Vec<Edge> {
    vec![
        Edge::row(0, 1, "@deepseek-ai/dsh-session-format-v0-to-v1"),
        Edge::missing(1, 2, "@deepseek-ai/dsh-session-format-v1-to-v2"),
        Edge::artifact(2, 3, "@deepseek-ai/dsh-session-format-v2-to-v3"),
        Edge::row(3, 4, "@deepseek-ai/dsh-session-format-v3-to-v4"),
    ]
}

/// The sentence a blocked route deserves.
fn blocked_reason(route: &Route) -> String {
    route.blocked_edge().map_or_else(
        || {
            format!(
                "stored Session uses format v{}; this build cannot migrate it to v{}",
                route.source(),
                route.target()
            )
        },
        |edge| {
            format!(
                "stored Session uses format v{}, and this build cannot migrate v{} to v{}: the edge \
                 v{}->v{} ({}) is not implemented",
                route.source(),
                route.source(),
                route.target(),
                edge.from,
                edge.to,
                edge.name
            )
        },
    )
}

/// Validates the header fields upstream's `snapshotSessionFormatHeader` requires.
fn header_shape_refusal(object: &Map<String, Value>) -> Option<String> {
    if !object.get("id").is_some_and(Value::is_string) {
        return Some("Session header id must be a string".to_owned());
    }
    if object.get("createdAt").and_then(safe_integer).is_none() {
        return Some("Session header createdAt must be a non-negative safe integer".to_owned());
    }
    if !object.get("isSeeded").is_some_and(Value::is_boolean) {
        return Some("Session header isSeeded must be a boolean".to_owned());
    }
    if object
        .get("delegationDepth")
        .and_then(safe_integer)
        .is_none()
    {
        return Some(
            "Session header delegationDepth must be a non-negative safe integer".to_owned(),
        );
    }
    None
}

/// Reads a JSON number as JavaScript's `Number.isSafeInteger` would, refusing negative zero.
///
/// The negative-zero refusal is upstream's, and it is not pedantry: `-0` and `0` are the same number
/// to every arithmetic test but serialize differently, so a version round-tripped through the
/// canonical encoder would change bytes without changing meaning.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    reason = "the range and integrality are proven immediately above the cast, which is therefore \
              exact and non-negative; `fract() == 0.0` is likewise an exact test for an integral \
              value, which is what the predicate means"
)]
fn safe_integer(value: &Value) -> Option<u64> {
    let number = value.as_number()?;
    if let Some(integer) = number.as_u64() {
        return (integer <= MAX_SAFE_INTEGER).then_some(integer);
    }
    // A literal JavaScript normalises onto an integer, such as `4.0` or `4e0`, is the same number
    // as its integer spelling and must classify identically. `as_u64` rejects both.
    let real = number.as_f64()?;
    if !real.is_finite() || !(0.0..=MAX_SAFE_F64).contains(&real) || real.fract() != 0.0 {
        return None;
    }
    if real == 0.0 && real.is_sign_negative() {
        return None;
    }
    Some(real as u64)
}
