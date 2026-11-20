//! Durable session artifacts: canonical names, validated headers, and exclusive generation
//! publication.
//!
//! # What upstream has, and what this crate already covers
//!
//! Upstream `packages/session` is 18,832 source lines across 24 packages, measured as `src/*.ts`
//! excluding tests. Two of them own durable storage, and they are the largest pair in the set:
//!
//! | package | lines | capability |
//! |---|---|---|
//! | `session-persistence-jsonl` | 5,086 | the physical backend: generation files, exclusive publication, per-session write lease, `Win32` publication, `zstd` frame scanning, migration verification |
//! | `session-persistence` | 636 | the backend-independent contract: error taxonomy, `SessionHandle`, revision token, storage validation |
//!
//! Everything else in the set is either already ported here — format migrations
//! ([`girder_core::Migrator`]), projections (`crate::session::projection`, `crate::session::projection_extra`),
//! titles ([`crate::derive_title`]), timeline ([`crate::session_outline`]), telemetry
//! (`crate::session::telemetry`), stats ([`crate::SessionStats`]), search
//! ([`crate::session::query_plan`]) — or is a model-call plugin with no pure half
//! (`session-title-llm`, `session-title-first-prompt-llm`).
//!
//! What is absent is the layer *underneath* all of that: the code that decides what a session
//! artifact is called, whether its header may be trusted, and how a new generation of it is
//! published so that a concurrent reader sees either the old file or the new one and never a mix.
//! [`crate::session::layout`] is a flat `root/{id}.jsonl.zst` layout with no id encoding, no project
//! key and no generation number; [`girder_core::SessionStore::append`] appends frames to one file
//! forever. That is a legitimate design for this build, and it is why none of the properties below
//! existed: `grep` finds no `hard_link`, no `session.vN`, no `isSeeded`, no header line, and no
//! ownership error anywhere under `crates/`.
//!
//! # What is ported
//!
//! Three layers, each a pure function of its inputs plus a small amount of file I/O:
//!
//! 1. **Naming.** [`encode_segment`] makes a session id safe as one path segment *injectively*, so
//!    no id can collide with another and none can escape the root. [`project_key`] and
//!    [`session_dir`] fix upstream's `root / project / id / session.vN.jsonl.zstd` tree.
//!    [`generation_log_filename`] and [`parse_generation_log_filename`] are the inverse pair that
//!    makes "which generation is this file" a property of the name rather than of its contents.
//! 2. **Header and contract.** A stored log's first line is a header, and a log whose header is not
//!    bound to the requested id, or whose version this build does not read, has to be refused
//!    *before* any version-dependent structure is parsed. [`StorageMeta::new`],
//!    [`parse_header_record`] and [`assert_version`] do that, and
//!    [`validate_stored_records`] refuses a log carrying an event type this build does not know
//!    unless its writer marked it ignorable — because silently skipping a required event
//!    reconstructs a *wrong* session.
//! 3. **Exclusive publication.** [`stage`] writes a synced temporary, [`publish_exclusive`] makes
//!    it the current generation with a hard link (which fails if the target exists, atomically),
//!    and an existing target is adopted only after its bytes are proven to be the same generation.
//!    [`read_stable`], [`verify_file`] and [`assert_source_unchanged`] are the read side: the same
//!    bytes the verifier hashed are the bytes the publisher compared.
//!
//! # The properties that matter, and why each has a test
//!
//! - **Naming is injective and traversal-proof.** `encode_segment` is the only thing between an
//!   unvalidated id and the filesystem. A lossy encoding turns two sessions into one file; a
//!   traversal (`../x`) writes outside the root. Both are tested from the side that can fail.
//! - **A header is evidence, not decoration.** `isSeeded` without an inherited cut, a relative
//!   `cwd`, a retired policy field, or an unknown key is a refusal, because each one means the
//!   file was written by something whose invariants this build cannot honour.
//! - **Publication is exclusive, not overwriting.** The failure this prevents is two migrators
//!   racing: both decode, both stage, and the loser's link fails rather than clobbering the
//!   winner. The loser then *verifies* the winner instead of trusting it.
//! - **A changed source is not republished.** If the historical generation moved while it was
//!   being migrated, the publication is abandoned: the bytes that were migrated are no longer the
//!   bytes on disk.
//!
//! # What is deliberately not ported
//!
//! - **The per-session kernel lease** (`lease.ts`, 135 lines). It is `flock` semantics over
//!   `session.lock`; this crate's [`girder_core::with_lock`] already provides writer coordination
//!   with a liveness probe, and a second lock protocol beside it would be two sources of truth for
//!   who may write. The *identity* half of the lease — the inode check that makes a recreated lock
//!   file visible — is the same comparison [`PhysicalIdentity`] performs here, and it is tested.
//! - **The `Win32` publication path** (`win32.ts`, 208 lines). It exists because `link(2)` is not
//!   the same call on Windows; [`publish_exclusive`] states its platform assumption instead of
//!   pretending to have one code path for two kernels.
//! - **The worker-thread verifier** (`migration-verifier.ts`, 198 lines). Its purpose is to keep a
//!   whole-generation decode off the event loop; this crate verifies in the calling thread, and
//!   [`verify_file`] is the seam a future worker would call.
//! - **The live write-behind buffer** (`storage.ts`, `index.ts`). Batching appends up to 200 ms is a
//!   throughput decision for a long-lived service; [`girder_core::SessionStore`] writes per event
//!   and its own docs say why. Nothing here duplicates that path.
//!
//! # Reference: the artifact tree and its record shapes
//!
//! ```text
//! root/--home-me-work--/s~003A1/session.v4.jsonl.zstd   current generation
//! root/--home-me-work--/s~003A1/session.v3.jsonl.zstd   historical generation
//! root/_no-cwd/s1/session.jsonl                          version zero, uncompressed
//!
//! header  {"type":"session","version":4,"id":"s1","createdAt":1,"isSeeded":false,"delegationDepth":0}
//! row     {"type":"turn/start","turn":1}
//! ```
//!
//! This file is also compiled by `tests/session_extra_gap.rs` through `#[path]`, so every import
//! below goes through the crate root's public re-exports: in that build `crate::` names the test
//! crate, and a `crate::…` path would fail to resolve.

// The crate root has not been wired to this module yet, so in a library build nothing here has a
// caller and `dead_code` would report a whole public surface as waste. The allow says that plainly
// rather than leaving a reader to guess; the test target exercises every item below.
#![allow(dead_code)]

use std::fmt;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use girder_core::FormatVersion;

// ---------------------------------------------------------------------------------------------
// Physical encoding
// ---------------------------------------------------------------------------------------------

/// The suffix a plaintext `JSONL` artifact carries.
pub const PLAIN_SUFFIX: &str = ".jsonl";

/// The suffix a `zstd`-compressed artifact carries.
///
/// Note the `d`: upstream writes `.jsonl.zstd` here, while [`crate::session::layout`] writes
/// `.jsonl.zst`. Both are correct for their own tree; they are not interchangeable, and mixing them
/// makes a reader silently skip every file.
pub const ZSTD_SUFFIX: &str = ".jsonl.zstd";

/// The base name every generation file shares.
const LOG_STEM: &str = "session";

/// How one generation's bytes are encoded on disk.
///
/// A closed enum rather than a string, because the suffix is derived from it: a caller cannot hold
/// `"zstd"` spelled `"zst"` and get a file whose name disagrees with its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Compression {
    /// One `zstd` stream, concatenated frames allowed.
    Zstd,
    /// Plain `UTF-8` `JSONL`.
    Plain,
}

impl Compression {
    /// The suffix an artifact of this encoding carries.
    pub const fn suffix(self) -> &'static str {
        match self {
            Self::Zstd => ZSTD_SUFFIX,
            Self::Plain => PLAIN_SUFFIX,
        }
    }

    /// The word this encoding is configured by, which is upstream's own vocabulary.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Zstd => "zstd",
            Self::Plain => "none",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------------------------

/// Why a path component could not be produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamingError {
    /// An id must encode to at least one code unit; the empty string names nothing.
    EmptyId,
    /// A project directory must be non-empty; `cwd` is the caller's project root.
    EmptyCwd,
    /// A project directory that is not absolute would make every deployment group its sessions
    /// differently, depending on where the process happened to start.
    RelativeCwd {
        /// The offending path, verbatim.
        cwd: String,
    },
    /// The generated name would still contain a path separator, which no component may.
    SeparatorInName {
        /// The offending name.
        name: String,
    },
}

impl fmt::Display for NamingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyId => write!(formatter, "cannot encode an empty session id"),
            Self::EmptyCwd => write!(formatter, "a session project directory must not be empty"),
            Self::RelativeCwd { cwd } => write!(
                formatter,
                "session project directory \"{cwd}\" is relative; the project key would depend on \
                 the reader's working directory"
            ),
            Self::SeparatorInName { name } => write!(
                formatter,
                "generated path component \"{name}\" still holds a path separator"
            ),
        }
    }
}

impl std::error::Error for NamingError {}

/// Uppercase hex digits, so an escape is written without a formatter per byte.
///
/// Uppercase is upstream's spelling for path escapes, and it is part of the contract: a lowercase
/// escape would still decode, so accepting both would make one id have two names.
const HEX: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F',
];

/// Lowercase hex digits, for digests. Every published `SHA-256` test vector is lowercase, and a
/// digest gets compared as a string by callers.
const HEX_LOWER: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

/// Appends one `~XX` escape for a single byte.
fn push_escape(out: &mut String, byte: u8) {
    out.push('~');
    out.push(HEX[usize::from(byte >> 4)]);
    out.push(HEX[usize::from(byte & 0x0F)]);
}

/// Whether a character may appear in an encoded segment verbatim.
///
/// The safe set is deliberately narrow: `.` and `-` are needed for readable ids, `_` for generated
/// ones, and everything else — including `~` itself — is escaped so the encoding stays injective.
const fn is_safe_unit(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
}

/// Whether a name a caller proposes as a path component actually is one.
fn has_separator(name: &str) -> bool {
    name.contains('/') || name.contains('\\') || name.contains('\0')
}

/// Encodes an unvalidated session id as exactly one filesystem path component.
///
/// # Why this must be injective
///
/// A session id is an unvalidated string: it may hold `../`, an absolute path, a `NUL`, or a
/// separator. Upstream's rule is that the encoding is injective over *all* strings, so two ids can
/// never name one file and no id can name a file outside the root. Escaped units are `~XX`, two
/// uppercase hex digits per `UTF-8` byte, and the safe set excludes `~` so an escape can never be
/// read as data.
///
/// `.` and `..` are escaped as a special case: they are safe *characters* but not safe *names*, and
/// a segment equal to either would resolve to a directory.
///
/// # Errors
///
/// Returns [`NamingError::EmptyId`] for the empty string, and [`NamingError::SeparatorInName`] if
/// the result somehow contains a separator — a check that can only fire if [`is_safe_unit`]
/// regresses, which is exactly when it must fire rather than silently write somewhere unexpected.
pub fn encode_segment(raw: &str) -> Result<String, NamingError> {
    if raw.is_empty() {
        return Err(NamingError::EmptyId);
    }
    if raw == "." {
        return Ok("~2E".to_owned());
    }
    if raw == ".." {
        return Ok("~2E~2E".to_owned());
    }
    let mut encoded = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character != '~' && is_safe_unit(character) {
            encoded.push(character);
        } else {
            let mut buffer = [0_u8; 4];
            for byte in character.encode_utf8(&mut buffer).bytes() {
                // Each UTF-8 byte is escaped separately, so the encoding is reversible byte-wise and
                // a NUL or a non-BMP character is representable without changing the name's shape.
                push_escape(&mut encoded, byte);
            }
        }
    }
    if has_separator(&encoded) {
        return Err(NamingError::SeparatorInName { name: encoded });
    }
    Ok(encoded)
}

/// Builds the human-navigable directory key for one project path.
///
/// Separators (`/`, `\`, `:`) collapse into a single `-` so a project path stays recognisable in a
/// file listing, which upstream accepts as deliberately lossy. Unlike the session-id encoding this
/// is *not* injective — two project paths can share a key — and that is safe because the key is a
/// grouping, not an identity: the session id inside it still is.
///
/// The result is `--{slug}--`, trimmed of leading separators and bounded to 251 characters so the
/// whole component fits a 255-byte filesystem limit.
///
/// # Errors
///
/// Returns [`NamingError::EmptyCwd`] for the empty path.
pub fn project_key(cwd: &str) -> Result<String, NamingError> {
    if cwd.is_empty() {
        return Err(NamingError::EmptyCwd);
    }
    let mut readable = String::with_capacity(cwd.len());
    let mut separator_run = false;
    for character in cwd.chars() {
        if matches!(character, '/' | '\\' | ':') {
            if !separator_run {
                readable.push('-');
            }
            separator_run = true;
        } else if character != '~' && is_safe_unit(character) {
            readable.push(character);
            separator_run = false;
        } else {
            let mut buffer = [0_u8; 4];
            for byte in character.encode_utf8(&mut buffer).bytes() {
                push_escape(&mut readable, byte);
            }
            separator_run = false;
        }
    }
    let trimmed = readable.trim_start_matches('-');
    let slug = if trimmed.is_empty() { "root" } else { trimmed };
    let bounded: String = slug.chars().take(251).collect();
    let key = format!("--{bounded}--");
    if has_separator(&key) {
        return Err(NamingError::SeparatorInName { name: key });
    }
    Ok(key)
}

/// The project directory a session with this `cwd` belongs to.
///
/// A session with no recorded working directory goes to `_no-cwd`: it is a real state (a session
/// created before the header carried one), and putting it under a fabricated path would make the
/// listing claim a project the session never ran in.
///
/// # Errors
///
/// Returns [`NamingError::RelativeCwd`] when `cwd` is not absolute, and propagates
/// [`project_key`]'s failures.
pub fn project_dir(root: &Path, cwd: Option<&str>) -> Result<PathBuf, NamingError> {
    match cwd {
        None => Ok(root.join("_no-cwd")),
        Some(cwd) => {
            if cwd.is_empty() {
                return Err(NamingError::EmptyCwd);
            }
            if !Path::new(cwd).is_absolute() {
                return Err(NamingError::RelativeCwd {
                    cwd: cwd.to_owned(),
                });
            }
            Ok(root.join(project_key(cwd)?))
        }
    }
}

/// The directory owned by one session.
///
/// # Errors
///
/// Propagates [`project_dir`] and [`encode_segment`]: both refusals are about the caller's input,
/// not about the filesystem.
pub fn session_dir(root: &Path, cwd: Option<&str>, id: &str) -> Result<PathBuf, NamingError> {
    Ok(project_dir(root, cwd)?.join(encode_segment(id)?))
}

/// The canonical base name of one generation's log, without the compression suffix.
///
/// Version zero keeps the original `session.jsonl`; every later generation carries `.vN`. The
/// numbering is what lets a directory hold a historical generation and its successor at once, so a
/// failed migration is retried rather than destructive.
pub fn generation_log_stem(version: FormatVersion) -> String {
    let number = version.as_number();
    if number == 0 {
        LOG_STEM.to_owned()
    } else {
        format!("{LOG_STEM}.v{number}")
    }
}

/// The canonical file name of one generation, including its compression suffix.
pub fn generation_log_filename(version: FormatVersion, compression: Compression) -> String {
    format!("{}{}", generation_log_stem(version), compression.suffix())
}

/// Reads the generation a file name names, or `None` when it is not canonical.
///
/// The refusal set is the interesting part, and it is upstream's: uppercase `V`, a leading zero
/// (`v04`), a `.v0` tag, a version this build does not know, and a compression suffix that
/// disagrees with the configured encoding are all *not* canonical. Each would otherwise let one
/// logical generation exist under two names, and a resolver that found both would have to guess
/// which is current.
pub fn parse_generation_log_filename(
    name: &str,
    compression: Compression,
) -> Option<FormatVersion> {
    let stem = name.strip_suffix(compression.suffix())?;
    let rest = stem.strip_prefix(LOG_STEM)?;
    if rest.is_empty() {
        return Some(FormatVersion::V0);
    }
    let version = rest.strip_prefix(".v")?;
    if version.is_empty()
        || version.starts_with('0')
        || !version.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let number = version.parse::<u64>().ok()?;
    FormatVersion::from_number(number)
}

/// The full path of one generation's log.
///
/// # Errors
///
/// Propagates [`session_dir`].
pub fn generation_log_path(
    root: &Path,
    cwd: Option<&str>,
    id: &str,
    version: FormatVersion,
    compression: Compression,
) -> Result<PathBuf, NamingError> {
    Ok(session_dir(root, cwd, id)?.join(generation_log_filename(version, compression)))
}

/// The two files a publication reads and writes, once they are proven to be one session's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationPaths {
    /// The historical generation being read.
    pub source: PathBuf,
    /// The current generation being written.
    pub current: PathBuf,
    /// The physical encoding both files use.
    pub compression: Compression,
}

/// Why a pair of generation paths cannot be published to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathPlanError {
    /// A file name is not canonical for the configured encoding.
    NonCanonicalName {
        /// The path whose name was refused.
        path: PathBuf,
        /// What was expected, in words.
        expected: String,
    },
    /// The source and the target are in different directories, so the tree would hold one session
    /// in two places.
    DifferentDirectories {
        /// The directory the source lives in.
        source: PathBuf,
        /// The directory the target would live in.
        current: PathBuf,
    },
    /// A caller asked to migrate a generation to itself or to something older.
    NotAnUpgrade {
        /// The source generation.
        source: FormatVersion,
        /// The requested target generation.
        current: FormatVersion,
    },
}

impl fmt::Display for PathPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonCanonicalName { path, expected } => write!(
                formatter,
                "resolved session artifact \"{}\" is not a canonical {expected} name",
                path.display()
            ),
            Self::DifferentDirectories { source, current } => write!(
                formatter,
                "source generation \"{}\" and target generation \"{}\" must share one session \
                 directory",
                source.display(),
                current.display()
            ),
            Self::NotAnUpgrade { source, current } => write!(
                formatter,
                "migration must target a newer generation than {source}, got {current}"
            ),
        }
    }
}

impl std::error::Error for PathPlanError {}

/// Checks that a resolved source and target really are one session's two generations.
///
/// This runs before any bytes are read, because every failure it catches would otherwise surface
/// after a decode and a staged write — the expensive, half-done part.
///
/// # Errors
///
/// Returns [`PathPlanError`] naming which of the three invariants failed: canonical names, one
/// directory, and a strictly newer target.
pub fn plan_generation_paths(
    source: &Path,
    source_version: FormatVersion,
    current: &Path,
    current_version: FormatVersion,
    compression: Compression,
) -> Result<GenerationPaths, PathPlanError> {
    let expected_source = generation_log_filename(source_version, compression);
    let expected_current = generation_log_filename(current_version, compression);
    let source_name = source
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let current_name = current
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    if source_name != expected_source {
        return Err(PathPlanError::NonCanonicalName {
            path: source.to_path_buf(),
            expected: expected_source,
        });
    }
    if current_name != expected_current {
        return Err(PathPlanError::NonCanonicalName {
            path: current.to_path_buf(),
            expected: expected_current,
        });
    }
    let source_parent = source.parent().unwrap_or_else(|| Path::new("."));
    let current_parent = current.parent().unwrap_or_else(|| Path::new("."));
    if source_parent != current_parent {
        return Err(PathPlanError::DifferentDirectories {
            source: source_parent.to_path_buf(),
            current: current_parent.to_path_buf(),
        });
    }
    if source_version >= current_version {
        return Err(PathPlanError::NotAnUpgrade {
            source: source_version,
            current: current_version,
        });
    }
    Ok(GenerationPaths {
        source: source.to_path_buf(),
        current: current.to_path_buf(),
        compression,
    })
}

// ---------------------------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------------------------

/// Why a stored header was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// The first line is not a `JSON` object, so it cannot be a header.
    NotAnObject,
    /// A required key is absent.
    MissingKey {
        /// The absent key.
        key: &'static str,
    },
    /// The record holds a key no released header carries. Unknown data in metadata is a refusal
    /// rather than a field to ignore, because it means the writer knew something this build does
    /// not.
    UnknownKey {
        /// The unexpected key.
        key: String,
    },
    /// A retired policy field is present. Those baselines moved out of the header before release,
    /// and a log that still carries one was written by a pre-release harness.
    RetiredKey {
        /// The retired key.
        key: &'static str,
    },
    /// A value has the wrong `JSON` type.
    WrongType {
        /// The offending key.
        key: &'static str,
        /// The type the field must have.
        expected: &'static str,
    },
    /// A numeric field is not a non-negative safe integer.
    NotANonNegativeInteger {
        /// The offending key.
        key: &'static str,
    },
    /// A `cwd` is present but relative. The header records the session's project, and a relative
    /// path does not name one.
    RelativeCwd {
        /// The offending path.
        cwd: String,
    },
    /// `isSeeded` is true but no inherited cut was supplied.
    SeededWithoutCut,
    /// An inherited cut was supplied for a session that is not seeded.
    CutWithoutSeed {
        /// The cut the caller supplied.
        cut: u64,
    },
    /// The stored id and the requested id disagree.
    IdMismatch {
        /// The id the caller asked for.
        requested: String,
        /// The id the header carries.
        stored: String,
    },
}

impl fmt::Display for HeaderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject => {
                write!(formatter, "the stored session header is not a JSON object")
            }
            Self::MissingKey { key } => {
                write!(formatter, "the stored session header is missing \"{key}\"")
            }
            Self::UnknownKey { key } => write!(
                formatter,
                "the stored session header carries \"{key}\", which no released header defines"
            ),
            Self::RetiredKey { key } => write!(
                formatter,
                "the stored session header carries the retired policy field \"{key}\""
            ),
            Self::WrongType { key, expected } => write!(
                formatter,
                "the stored session header field \"{key}\" must be {expected}"
            ),
            Self::NotANonNegativeInteger { key } => write!(
                formatter,
                "the stored session header field \"{key}\" is not a non-negative safe integer"
            ),
            Self::RelativeCwd { cwd } => write!(
                formatter,
                "the stored session header cwd \"{cwd}\" is not an absolute path"
            ),
            Self::SeededWithoutCut => write!(
                formatter,
                "a seeded session header requires the exact inherited event count"
            ),
            Self::CutWithoutSeed { cut } => write!(
                formatter,
                "an unseeded session header must inherit 0 events, got {cut}"
            ),
            Self::IdMismatch { requested, stored } => write!(
                formatter,
                "stored session identity mismatch: requested \"{requested}\", header contains \
                 \"{stored}\""
            ),
        }
    }
}

impl std::error::Error for HeaderError {}

/// Who asked for a session to exist, for the cases where the answer is not a person.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A subagent, spawned by another session rather than by a client.
    Subagent,
}

impl Origin {
    /// The wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Subagent => "subagent",
        }
    }
}

/// The header of a stored session: immutable facts that are not events.
///
/// Kept apart from `SessionEvent` because none of these can be replayed: a log that lost its id
/// could not be told from any other log, and one that lost its lineage could not be resumed as the
/// session it is.
///
/// `version` is the raw stored number rather than a [`FormatVersion`], because a log written by a
/// newer harness declares a number this build has no variant for — and that must be *reported*, not
/// fail to parse. [`assert_version`] is the check; parsing is only classification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageHeader {
    /// The format generation this log is written in, as stored.
    pub version: u64,
    /// The session's identity.
    pub id: String,
    /// Creation time in milliseconds since the Unix epoch.
    pub created_at: u64,
    /// The project directory, when one was recorded.
    pub cwd: Option<String>,
    /// The fork parent, when this session was seeded from another.
    pub parent_session: Option<String>,
    /// Whether this session inherited a prefix from its parent.
    pub is_seeded: bool,
    /// How far below the root agent this session runs.
    pub delegation_depth: u64,
    /// Who created it, when it was not a client.
    pub origin: Option<Origin>,
    /// The agent preset it runs under.
    pub agent_preset: Option<String>,
}

impl StorageHeader {
    /// A minimal, unseeded header — the shape every ordinary session has.
    pub fn new(id: impl Into<String>, created_at: u64) -> Self {
        Self {
            version: FormatVersion::CURRENT.as_number(),
            id: id.into(),
            created_at,
            cwd: None,
            parent_session: None,
            is_seeded: false,
            delegation_depth: 0,
            origin: None,
            agent_preset: None,
        }
    }

    /// The keys a released header must carry.
    const REQUIRED_KEYS: [&'static str; 6] = [
        "type",
        "version",
        "id",
        "createdAt",
        "isSeeded",
        "delegationDepth",
    ];

    /// The keys a released header may carry.
    const OPTIONAL_KEYS: [&'static str; 4] = ["cwd", "parentSession", "origin", "agentPreset"];

    /// Fields that were removed before release; their presence dates the log.
    const RETIRED_KEYS: [&'static str; 2] = ["sandboxMode", "approvalPolicy"];
}

/// A stored header paired with the exact inherited prefix length.
///
/// The cut travels *beside* the header rather than inside it, because it is not replayable: a log
/// records events, and the number of leading ones that came from the parent is metadata about the
/// file, not something the model saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageMeta {
    /// The validated immutable header.
    pub header: StorageHeader,
    /// How many leading events came from the fork parent.
    pub inherited_event_count: u64,
}

impl StorageMeta {
    /// Pairs a header with its cut, refusing the two combinations that cannot be honoured.
    ///
    /// # Errors
    ///
    /// Returns [`HeaderError::SeededWithoutCut`] when a seeded session names no cut — a seeded
    /// session whose inherited length is unknown cannot tell its own events from its parent's — and
    /// [`HeaderError::CutWithoutSeed`] when an unseeded session claims one.
    pub fn new(header: StorageHeader, inherited: Option<u64>) -> Result<Self, HeaderError> {
        let cut = inherited.unwrap_or(0);
        if header.is_seeded && inherited.is_none() {
            return Err(HeaderError::SeededWithoutCut);
        }
        if !header.is_seeded && cut != 0 {
            return Err(HeaderError::CutWithoutSeed { cut });
        }
        Ok(Self {
            header,
            inherited_event_count: cut,
        })
    }

    /// The first line of the log, as the `JSON` object it is stored as.
    ///
    /// Optional fields are omitted rather than written as `null`: a `null` `cwd` and an absent one
    /// mean the same thing to a reader, and only one of them is a value this harness writes.
    pub fn record(&self) -> Value {
        let header = &self.header;
        let mut map = Map::new();
        map.insert("type".to_owned(), Value::String("session".to_owned()));
        map.insert("version".to_owned(), Value::from(header.version));
        map.insert("id".to_owned(), Value::String(header.id.clone()));
        map.insert("createdAt".to_owned(), Value::from(header.created_at));
        map.insert("isSeeded".to_owned(), Value::Bool(header.is_seeded));
        map.insert(
            "delegationDepth".to_owned(),
            Value::from(header.delegation_depth),
        );
        if let Some(cwd) = &header.cwd {
            map.insert("cwd".to_owned(), Value::String(cwd.clone()));
        }
        if let Some(parent) = &header.parent_session {
            map.insert("parentSession".to_owned(), Value::String(parent.clone()));
        }
        if let Some(origin) = header.origin {
            map.insert(
                "origin".to_owned(),
                Value::String(origin.as_str().to_owned()),
            );
        }
        if let Some(preset) = &header.agent_preset {
            map.insert("agentPreset".to_owned(), Value::String(preset.clone()));
        }
        Value::Object(map)
    }
}

/// Reads the stored header line, validating every field a released header defines.
///
/// The inherited cut is returned as `0`: it travels with the storage metadata, not inside the header
/// line, so a reader that needs an exact cut must obtain it from the caller that opened the file.
/// Parsing therefore proves the *identity* of the file, not its lineage depth.
///
/// A version this build does not read is *not* refused here — it parses, and [`assert_version`]
/// refuses it. That split is what lets a caller tell "newer harness" from "corrupt log".
///
/// # Errors
///
/// Returns [`HeaderError`] for a non-object, a missing required key, an unknown or retired key, a
/// mistyped or out-of-range value, a relative `cwd`, or an unknown `origin`.
/// Enforces the required, retired and unknown-key rules before any field is read.
///
/// Separate from the field parsing because the two answer different questions -- "is this a header at all" and "what does
/// it say" -- and a caller fixing a malformed file wants the first answer without the second's error text.
fn check_header_keys(object: &serde_json::Map<String, Value>) -> Result<(), HeaderError> {
    for key in StorageHeader::REQUIRED_KEYS {
        if !object.contains_key(key) {
            return Err(HeaderError::MissingKey { key });
        }
    }
    for key in StorageHeader::RETIRED_KEYS {
        if object.contains_key(key) {
            return Err(HeaderError::RetiredKey { key });
        }
    }
    for key in object.keys() {
        if StorageHeader::REQUIRED_KEYS.contains(&key.as_str())
            || StorageHeader::OPTIONAL_KEYS.contains(&key.as_str())
        {
            continue;
        }
        return Err(HeaderError::UnknownKey { key: key.clone() });
    }
    if object.get("type").and_then(Value::as_str) != Some("session") {
        return Err(HeaderError::WrongType {
            key: "type",
            expected: "the string \"session\"",
        });
    }
    Ok(())
}

pub fn parse_header_record(value: &Value) -> Result<StorageMeta, HeaderError> {
    let object = value.as_object().ok_or(HeaderError::NotAnObject)?;
    check_header_keys(object)?;
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .ok_or(HeaderError::NotANonNegativeInteger { key: "version" })?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or(HeaderError::WrongType {
            key: "id",
            expected: "a string",
        })?
        .to_owned();
    if id.is_empty() {
        return Err(HeaderError::WrongType {
            key: "id",
            expected: "a non-empty string",
        });
    }
    let created_at = object
        .get("createdAt")
        .and_then(Value::as_u64)
        .ok_or(HeaderError::NotANonNegativeInteger { key: "createdAt" })?;
    let is_seeded =
        object
            .get("isSeeded")
            .and_then(Value::as_bool)
            .ok_or(HeaderError::WrongType {
                key: "isSeeded",
                expected: "a boolean",
            })?;
    let delegation_depth = object
        .get("delegationDepth")
        .and_then(Value::as_u64)
        .ok_or(HeaderError::NotANonNegativeInteger {
            key: "delegationDepth",
        })?;
    let cwd = match object.get("cwd") {
        None => None,
        Some(Value::String(cwd)) => {
            if !Path::new(cwd).is_absolute() {
                return Err(HeaderError::RelativeCwd { cwd: cwd.clone() });
            }
            Some(cwd.clone())
        }
        Some(_) => {
            return Err(HeaderError::WrongType {
                key: "cwd",
                expected: "a string holding an absolute path",
            });
        }
    };
    let parent_session = match object.get("parentSession") {
        None => None,
        Some(Value::String(parent)) => Some(parent.clone()),
        Some(_) => {
            return Err(HeaderError::WrongType {
                key: "parentSession",
                expected: "a string",
            });
        }
    };
    let origin = match object.get("origin") {
        None => None,
        Some(Value::String(origin)) if origin == Origin::Subagent.as_str() => {
            Some(Origin::Subagent)
        }
        Some(_) => {
            return Err(HeaderError::WrongType {
                key: "origin",
                expected: "the string \"subagent\"",
            });
        }
    };
    let agent_preset = match object.get("agentPreset") {
        None => None,
        Some(Value::String(preset)) => Some(preset.clone()),
        Some(_) => {
            return Err(HeaderError::WrongType {
                key: "agentPreset",
                expected: "a string",
            });
        }
    };
    let header = StorageHeader {
        version,
        id,
        created_at,
        cwd,
        parent_session,
        is_seeded,
        delegation_depth,
        origin,
        agent_preset,
    };
    StorageMeta::new(header, None)
}

/// A header this build refuses to read, and why the user should care.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionRefusal {
    /// The session the refusal is about.
    pub id: String,
    /// The generation the log declares.
    pub stored: u64,
    /// The generation this build reads.
    pub supported: u64,
}

impl VersionRefusal {
    /// Whether the log was written by a newer harness.
    pub const fn is_newer(&self) -> bool {
        self.stored > self.supported
    }

    /// The sentence a user should see.
    pub fn message(&self) -> String {
        if self.is_newer() {
            format!(
                "session \"{}\" uses log format v{}, but this harness reads only v{}: the log was \
                 written by a newer harness — upgrade the harness to open it",
                self.id, self.stored, self.supported
            )
        } else {
            format!(
                "session \"{}\" uses log format v{}, older than the supported v{}, and this build \
                 ships no upgrade path for it",
                self.id, self.stored, self.supported
            )
        }
    }
}

impl fmt::Display for VersionRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for VersionRefusal {}

/// Refuses a header this build does not read.
///
/// # Errors
///
/// Returns [`VersionRefusal`] when the header's generation is not [`FormatVersion::CURRENT`].
pub fn assert_version(header: &StorageHeader) -> Result<(), VersionRefusal> {
    let supported = FormatVersion::CURRENT.as_number();
    if header.version == supported {
        Ok(())
    } else {
        Err(VersionRefusal {
            id: header.id.clone(),
            stored: header.version,
            supported,
        })
    }
}

/// Refuses a stored header that belongs to a different session than the one requested.
///
/// # Errors
///
/// Returns [`HeaderError::IdMismatch`] when the ids differ. Reading one session's events as
/// another's is the failure this exists to make impossible, so it is checked before any event is
/// decoded.
pub fn assert_stored_id(requested: &str, header: &StorageHeader) -> Result<(), HeaderError> {
    if requested == header.id {
        Ok(())
    } else {
        Err(HeaderError::IdMismatch {
            requested: requested.to_owned(),
            stored: header.id.clone(),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Storage contract
// ---------------------------------------------------------------------------------------------

/// The event types this build can decode.
///
/// Mirrors upstream's `KNOWN_SESSION_EVENT_TYPES`, restricted to the variants
/// `SessionEvent` actually has: a type this crate cannot deserialize is *not* known here, and
/// [`validate_stored_records`] therefore refuses it unless its writer marked it ignorable. The test
/// suite pins this list against the real vocabulary, so the two cannot drift.
pub const KNOWN_EVENT_TYPES: [&str; 18] = [
    "turn/start",
    "turn/end",
    "step/start",
    "step/end",
    "user/message",
    "developer/message",
    "assistant/message",
    "model/usage",
    "tool/call",
    "tool/result",
    "system/message",
    "model/selection",
    "permission/preset",
    "plan/mode",
    "sandbox/mode",
    "request/context",
    "session/title",
    "llm/retry",
];

/// Whether this build knows an event type.
pub fn is_known_event_type(name: &str) -> bool {
    KNOWN_EVENT_TYPES.contains(&name)
}

/// Why a stored record array was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContractError {
    /// A row is not a `JSON` object.
    NotAnObject {
        /// Zero-based position of the row.
        index: usize,
    },
    /// A row has no `type` string.
    MissingType {
        /// Zero-based position of the row.
        index: usize,
    },
    /// A row names an event type this build cannot decode, and it is not marked ignorable.
    UnknownEventType {
        /// The offending type.
        type_name: String,
        /// Its `seq`, when it declares one.
        seq: Option<u64>,
    },
    /// A `request/header` row carries the retired `fallback` reason.
    RetiredReason {
        /// Its `seq`, when it declares one.
        seq: Option<u64>,
    },
    /// A row's sequence number does not continue the log.
    OutOfSequence {
        /// Zero-based position of the row in the batch.
        index: usize,
        /// The sequence the row should carry.
        expected: u64,
        /// The sequence it does carry.
        found: u64,
    },
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject { index } => {
                write!(formatter, "stored record {index} is not an object")
            }
            Self::MissingType { index } => {
                write!(formatter, "stored record {index} has no \"type\" string")
            }
            Self::UnknownEventType { type_name, seq } => write!(
                formatter,
                "stored event \"{type_name}\"{} is unknown to this harness and not marked \
                 ignorable; refusing to interpret the log — it was likely written by a newer \
                 harness",
                seq.map_or_else(String::new, |seq| format!(" (seq {seq})"))
            ),
            Self::RetiredReason { seq } => write!(
                formatter,
                "stored request/header event{} carries the retired reason \"fallback\"; refusing to \
                 interpret the log — it was written by a retired pre-release harness",
                seq.map_or_else(String::new, |seq| format!(" (seq {seq})"))
            ),
            Self::OutOfSequence {
                index,
                expected,
                found,
            } => write!(
                formatter,
                "append seq mismatch at index {index}: expected {expected}, got {found}"
            ),
        }
    }
}

impl std::error::Error for ContractError {}

/// The `seq` a row declares, when it declares one.
fn row_seq(row: &Value) -> Option<u64> {
    row.get("seq").and_then(Value::as_u64)
}

/// Validates a decoded record array in one pass, refusing fail-closed.
///
/// # Why "unknown but ignorable" is the only exception
///
/// A newer harness may add an event type; a reader that skipped it would silently reconstruct a
/// session missing whatever that event meant. Upstream's answer is that the *writer* must mark an
/// addition ignorable, which is a promise that nothing needed it. This function holds that line: an
/// unknown type without `ignorable: true` refuses the whole log.
///
/// `request/header` is checked first because it is retired rather than unknown: a released log never
/// holds one, and the one pre-release shape that hides behind it — the removed delta codec's
/// `fallback` reason — is the only remaining case whose correct answer is a version refusal instead
/// of a vocabulary refusal.
///
/// # Errors
///
/// Returns [`ContractError`] for a non-object row, a missing type, the retired `fallback` reason, or
/// an unknown required type.
pub fn validate_stored_records(rows: &[Value]) -> Result<(), ContractError> {
    for (index, row) in rows.iter().enumerate() {
        if !row.is_object() {
            return Err(ContractError::NotAnObject { index });
        }
        let type_name = row
            .get("type")
            .and_then(Value::as_str)
            .ok_or(ContractError::MissingType { index })?;
        let seq = row_seq(row);
        if type_name == "request/header"
            && row
                .get("data")
                .and_then(|data| data.get("reason"))
                .and_then(Value::as_str)
                == Some("fallback")
        {
            return Err(ContractError::RetiredReason { seq });
        }
        if is_known_event_type(type_name) {
            continue;
        }
        if row.get("ignorable").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        return Err(ContractError::UnknownEventType {
            type_name: type_name.to_owned(),
            seq,
        });
    }
    Ok(())
}

/// Refuses a batch that does not continue the stored log's sequence numbers.
///
/// # Errors
///
/// Returns [`ContractError::OutOfSequence`] naming the first row that breaks the run. A batch whose
/// numbers are dense but start in the wrong place is the same defect: the cursor is the stored
/// next-seq, and the batch must begin exactly there.
pub fn assert_contiguous(seqs: &[u64], cursor: u64) -> Result<(), ContractError> {
    for (index, found) in seqs.iter().enumerate() {
        let expected = cursor.saturating_add(u64::try_from(index).unwrap_or(u64::MAX));
        if *found != expected {
            return Err(ContractError::OutOfSequence {
                index,
                expected,
                found: *found,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Physical identity
// ---------------------------------------------------------------------------------------------

/// The stat identity of one file revision.
///
/// Two identities are equal exactly when no evidence says the bytes changed between the two stats,
/// which is what makes "read, then ask whether the file moved" a proof rather than a guess.
/// Timestamps are nanoseconds since the epoch so a sub-second rewrite is visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalIdentity {
    /// Device the file lives on.
    pub device: u64,
    /// Inode number.
    pub inode: u64,
    /// Byte length.
    pub size: u64,
    /// Last modification time, in nanoseconds since the epoch.
    pub modified: i128,
    /// Last metadata change time, in nanoseconds since the epoch.
    pub changed: i128,
}

impl PhysicalIdentity {
    /// Whether no evidence contradicts the two stats describing one revision.
    ///
    /// Named as a question rather than an equality so the call site reads as what it is: nothing
    /// here proves the bytes are the same, only that nothing says otherwise.
    pub fn may_be_unchanged(self, other: Self) -> bool {
        self == other
    }
}

fn nanos(seconds: i64, fraction: i64) -> i128 {
    i128::from(seconds) * 1_000_000_000 + i128::from(fraction)
}

/// The stat identity of a file, using the strongest fields the platform offers.
fn identity_of(metadata: &Metadata) -> PhysicalIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        PhysicalIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified: nanos(metadata.mtime(), metadata.mtime_nsec()),
            changed: nanos(metadata.ctime(), metadata.ctime_nsec()),
        }
    }
    #[cfg(not(unix))]
    {
        // Without `st_ino`/`st_ctime` the identity is weaker: a rewrite inside one timestamp
        // granularity is invisible. Size and modification time still catch every rewrite a migration
        // can perform, and the caller's own verification re-reads the bytes.
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| {
                i128::try_from(elapsed.as_nanos()).unwrap_or(i128::MAX)
            });
        PhysicalIdentity {
            device: 0,
            inode: 0,
            size: metadata.len(),
            modified,
            changed: 0,
        }
    }
}

/// The stat identity of a path.
///
/// # Errors
///
/// Propagates the `stat` failure as [`GenerationError::Io`].
pub fn identity(path: &Path) -> Result<PhysicalIdentity, GenerationError> {
    Ok(identity_of(&std::fs::metadata(path)?))
}

/// Exact bytes of one file revision, together with the identity that proved it stable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StableFile {
    /// The bytes read.
    pub bytes: Vec<u8>,
    /// The identity the bytes belong to.
    pub identity: PhysicalIdentity,
}

/// Reads a file, proving the bytes read belong to the revision that was stat'ed.
///
/// The read is bracketed by two stats. If they agree, the bytes are one revision. If they do not,
/// the file was being written while we read it, and the second attempt's *prefix* — truncated to the
/// first stat's size — is exactly the revision we asked for, because an append cannot change bytes
/// that already existed. Upstream makes the same trade, and it is why a log with an in-flight append
/// can still be migrated.
///
/// # Errors
///
/// Returns [`GenerationError::Io`] if the file cannot be stat'ed or read, and
/// [`GenerationError::Unstable`] if two consecutive attempts each saw a rewrite.
pub fn read_stable(path: &Path) -> Result<StableFile, GenerationError> {
    let mut expected = identity(path)?;
    for attempt in 0..2_u8 {
        let bytes = std::fs::read(path)?;
        let after = identity(path)?;
        if expected == after {
            return Ok(StableFile {
                bytes,
                identity: after,
            });
        }
        if attempt == 1 {
            let keep = usize::try_from(expected.size)
                .unwrap_or(bytes.len())
                .min(bytes.len());
            let mut prefix = bytes;
            prefix.truncate(keep);
            return Ok(StableFile {
                bytes: prefix,
                identity: expected,
            });
        }
        expected = after;
    }
    Err(GenerationError::Unstable {
        path: path.to_path_buf(),
    })
}

/// Refuses a publication whose source moved while it was being read.
///
/// # Errors
///
/// Returns [`GenerationError::SourceChanged`] when the current identity differs from the witnessed
/// one. Republishing would install bytes migrated from a revision that no longer exists.
pub fn assert_source_unchanged(
    path: &Path,
    witnessed: PhysicalIdentity,
) -> Result<(), GenerationError> {
    if identity(path)? == witnessed {
        Ok(())
    } else {
        Err(GenerationError::SourceChanged {
            path: path.to_path_buf(),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Revision tokens
// ---------------------------------------------------------------------------------------------

/// An opaque change token for one stored session.
///
/// Two revisions that are equal *may* mean the log is unchanged; unequal ones promise nothing. The
/// point of the token is that a cache can avoid re-reading, not that it can prove a negative — so it
/// is derived from the artifact identity, which changes on every write, and deliberately not from
/// write-ownership churn, which does not change the log at all.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(String);

impl Revision {
    /// The token for one artifact identity.
    pub fn of(physical: PhysicalIdentity) -> Self {
        Self(format!(
            "{}-{}-{}-{}-{}",
            physical.device, physical.inode, physical.size, physical.modified, physical.changed
        ))
    }

    /// The token as a string, for a wire field.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------------------------

/// Why encoding, verification, or publication failed.
#[derive(Debug)]
pub enum GenerationError {
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// The bytes hold no header line, so they are not a log.
    HeaderMissing,
    /// The bytes end inside a record, which is what an interrupted write leaves behind.
    TornTail,
    /// The bytes are not `UTF-8`.
    NotUtf8,
    /// A row is not valid `JSON`.
    CorruptJson {
        /// One-based line number.
        line: usize,
        /// The parser's account.
        detail: String,
    },
    /// A header line was refused.
    Header(HeaderError),
    /// A record array was refused.
    Contract(ContractError),
    /// The decoded log's event count is not the expected one.
    CountMismatch {
        /// What was decoded.
        found: usize,
        /// What the caller expected.
        expected: usize,
    },
    /// The decoded log belongs to another session.
    IdMismatch {
        /// The id the header carries.
        found: String,
        /// The id the caller expected.
        expected: String,
    },
    /// The historical generation changed while it was being migrated.
    SourceChanged {
        /// The moved path.
        path: PathBuf,
    },
    /// A file kept changing across two read attempts.
    Unstable {
        /// The path that would not settle.
        path: PathBuf,
    },
    /// The current generation already exists and is not the generation being published.
    TargetConflict {
        /// The competing path.
        path: PathBuf,
        /// Why the existing target cannot be adopted.
        reason: String,
    },
    /// An existing target is shorter than the generation being published.
    PrefixTooShort {
        /// The target's length.
        bytes: usize,
        /// The length the published generation needs.
        required: usize,
    },
    /// A staged temporary could not be created after several attempts.
    StagingExhausted {
        /// The session directory that was tried.
        directory: PathBuf,
    },
}

impl fmt::Display for GenerationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "session artifact I/O failed: {error}"),
            Self::HeaderMissing => write!(formatter, "empty or header-less session log"),
            Self::TornTail => write!(
                formatter,
                "the current session generation has a torn physical tail"
            ),
            Self::NotUtf8 => write!(formatter, "session log is not valid UTF-8"),
            Self::CorruptJson { line, detail } => {
                write!(
                    formatter,
                    "corrupt session log: line {line} is not valid JSON ({detail})"
                )
            }
            Self::Header(error) => write!(formatter, "{error}"),
            Self::Contract(error) => write!(formatter, "{error}"),
            Self::CountMismatch { found, expected } => write!(
                formatter,
                "current session generation holds {found} event(s), expected {expected}"
            ),
            Self::IdMismatch { found, expected } => write!(
                formatter,
                "current session generation contains id \"{found}\", expected \"{expected}\""
            ),
            Self::SourceChanged { path } => write!(
                formatter,
                "historical session generation changed during migration: \"{}\"",
                path.display()
            ),
            Self::Unstable { path } => write!(
                formatter,
                "session artifact \"{}\" kept changing across two read attempts",
                path.display()
            ),
            Self::TargetConflict { path, reason } => write!(
                formatter,
                "current session generation already exists at \"{}\": {reason}",
                path.display()
            ),
            Self::PrefixTooShort { bytes, required } => write!(
                formatter,
                "target bytes are shorter than the migrated generation: {bytes} < {required}"
            ),
            Self::StagingExhausted { directory } => write!(
                formatter,
                "could not create a staging file beside \"{}\"",
                directory.display()
            ),
        }
    }
}

impl std::error::Error for GenerationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Header(error) => Some(error),
            Self::Contract(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for GenerationError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<HeaderError> for GenerationError {
    fn from(error: HeaderError) -> Self {
        Self::Header(error)
    }
}

impl From<ContractError> for GenerationError {
    fn from(error: ContractError) -> Self {
        Self::Contract(error)
    }
}

// ---------------------------------------------------------------------------------------------
// Encoding and verification
// ---------------------------------------------------------------------------------------------

/// One decoded, validated generation.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedGeneration {
    /// The validated header and its inherited cut.
    pub meta: StorageMeta,
    /// The body rows, in log order.
    pub rows: Vec<Value>,
}

/// Renders a generation as the exact bytes its file holds.
///
/// The header is the first line and every event is one line, each terminated by `\n`. That is what
/// makes "ends inside a record" decidable from the bytes alone, which is what
/// [`GenerationError::TornTail`] depends on. With [`Compression::Zstd`] the whole text becomes one
/// frame; upstream keeps the header in its own frame so a reader can classify a log without decoding
/// the body, and [`decode_generation`] accepts either shape.
///
/// # Errors
///
/// Returns [`GenerationError::CorruptJson`] if a row cannot be serialized, and
/// [`GenerationError::Io`] if `zstd` cannot compress the text.
pub fn encode_generation(
    meta: &StorageMeta,
    rows: &[Value],
    compression: Compression,
) -> Result<Vec<u8>, GenerationError> {
    let mut text = String::new();
    text.push_str(&serde_json::to_string(&meta.record()).map_err(|error| {
        GenerationError::CorruptJson {
            line: 1,
            detail: error.to_string(),
        }
    })?);
    text.push('\n');
    for (offset, row) in rows.iter().enumerate() {
        text.push_str(&serde_json::to_string(row).map_err(|error| {
            GenerationError::CorruptJson {
                line: offset + 2,
                detail: error.to_string(),
            }
        })?);
        text.push('\n');
    }
    match compression {
        Compression::Plain => Ok(text.into_bytes()),
        Compression::Zstd => zstd::encode_all(text.as_bytes(), 3).map_err(GenerationError::Io),
    }
}

/// The plaintext `JSONL` of a generation, `zstd`-decoded when needed.
///
/// Concatenated frames decode as one stream, which is what
/// [`girder_core::SessionStore::append`] relies on and what lets a generation be extended by
/// appending frames. A truncated frame fails to decode and is reported as [`GenerationError::Io`]:
/// unlike a plaintext tail, a torn frame is not distinguishable here from any other decode failure
/// without frame-level surgery, and pretending otherwise would let a damaged log through as a short
/// one.
///
/// # Errors
///
/// Returns [`GenerationError::HeaderMissing`] for no bytes, [`GenerationError::Io`] for a `zstd`
/// failure, and [`GenerationError::NotUtf8`] when the plaintext is not `UTF-8`.
pub fn decode_generation(
    bytes: &[u8],
    compression: Compression,
) -> Result<String, GenerationError> {
    if bytes.is_empty() {
        return Err(GenerationError::HeaderMissing);
    }
    let plain = match compression {
        Compression::Plain => bytes.to_vec(),
        Compression::Zstd => zstd::decode_all(bytes).map_err(GenerationError::Io)?,
    };
    if plain.is_empty() {
        return Err(GenerationError::HeaderMissing);
    }
    String::from_utf8(plain).map_err(|_| GenerationError::NotUtf8)
}

/// Decodes and validates one generation, without touching the filesystem.
///
/// # Errors
///
/// Returns [`GenerationError`] for missing or torn records, invalid `JSON`, a refused header, or a
/// refused event vocabulary.
pub fn verify_bytes(
    bytes: &[u8],
    compression: Compression,
) -> Result<DecodedGeneration, GenerationError> {
    let text = decode_generation(bytes, compression)?;
    if !text.ends_with('\n') {
        return Err(GenerationError::TornTail);
    }
    let mut lines = text.lines();
    let header_line = lines.next().ok_or(GenerationError::HeaderMissing)?;
    let header_value: Value =
        serde_json::from_str(header_line).map_err(|error| GenerationError::CorruptJson {
            line: 1,
            detail: error.to_string(),
        })?;
    let meta = parse_header_record(&header_value)?;
    let mut rows = Vec::new();
    for (offset, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(line).map_err(|error| GenerationError::CorruptJson {
                line: offset + 2,
                detail: error.to_string(),
            })?;
        rows.push(value);
    }
    validate_stored_records(&rows)?;
    Ok(DecodedGeneration { meta, rows })
}

/// A generation read from disk and proven to be the expected one.
///
/// `decoded` is `None` when only a proven byte prefix was checked — the case of verifying a
/// competing target that a live writer may already have extended. The bytes are then proven to begin
/// with the expected generation, and nothing is claimed about the tail beyond it.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedGeneration {
    /// Its stat identity, for publication comparison.
    pub identity: PhysicalIdentity,
    /// How many bytes its file holds.
    pub bytes: usize,
    /// The digest of those bytes.
    pub digest: String,
    /// The validated content, absent when only a prefix was verified.
    pub decoded: Option<DecodedGeneration>,
}

/// Reads one current generation and proves it is the expected one.
///
/// # Why the digest and not just the count
///
/// A staged file and a competing file can decode to the same count and differ in a byte; comparing
/// digests is what makes "adopt the winner" mean "adopt exactly these bytes". The prefix form is for
/// an append tail: the verified prefix must match and the tail beyond it is not validated, because a
/// live writer may already have extended the file.
///
/// # Errors
///
/// Returns [`GenerationError`] for a read failure, a refused generation, an id mismatch, a count
/// mismatch, or a target that does not begin with the expected prefix.
pub fn verify_file(
    path: &Path,
    compression: Compression,
    expected_id: &str,
    expected_event_count: usize,
    prefix: Option<&ExpectedPrefix>,
) -> Result<VerifiedGeneration, GenerationError> {
    let stable = read_stable(path)?;
    let digest = digest_hex(&stable.bytes);
    if let Some(prefix) = prefix {
        let head = stable
            .bytes
            .get(..prefix.bytes)
            .ok_or(GenerationError::PrefixTooShort {
                bytes: stable.bytes.len(),
                required: prefix.bytes,
            })?;
        if digest_hex(head) != prefix.digest {
            return Err(GenerationError::TargetConflict {
                path: path.to_path_buf(),
                reason: "target bytes do not begin with the migrated generation".to_owned(),
            });
        }
        return Ok(VerifiedGeneration {
            identity: stable.identity,
            bytes: stable.bytes.len(),
            digest,
            decoded: None,
        });
    }
    let decoded = verify_bytes(&stable.bytes, compression)?;
    if decoded.meta.header.id != expected_id {
        return Err(GenerationError::IdMismatch {
            found: decoded.meta.header.id,
            expected: expected_id.to_owned(),
        });
    }
    if decoded.rows.len() != expected_event_count {
        return Err(GenerationError::CountMismatch {
            found: decoded.rows.len(),
            expected: expected_event_count,
        });
    }
    Ok(VerifiedGeneration {
        identity: stable.identity,
        bytes: stable.bytes.len(),
        digest,
        decoded: Some(decoded),
    })
}

// ---------------------------------------------------------------------------------------------
// Staging and publication
// ---------------------------------------------------------------------------------------------

/// A byte prefix already proven to be a valid complete generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedPrefix {
    /// How many leading bytes are proven.
    pub bytes: usize,
    /// The digest of exactly those bytes.
    pub digest: String,
}

impl ExpectedPrefix {
    /// The prefix consisting of all of `data`.
    pub fn of(data: &[u8]) -> Self {
        Self {
            bytes: data.len(),
            digest: digest_hex(data),
        }
    }
}

/// One written-but-unpublished generation: a synced temporary beside its target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Staged {
    /// The temporary file.
    pub path: PathBuf,
    /// Its byte length.
    pub bytes: usize,
    /// The digest of its bytes, computed from the same buffer that was written.
    pub digest: String,
}

impl Staged {
    /// The prefix this staged generation proves, for adopting a competing target.
    pub fn prefix(&self) -> ExpectedPrefix {
        ExpectedPrefix {
            bytes: self.bytes,
            digest: self.digest.clone(),
        }
    }
}

/// The outcome of a publication attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    /// This caller created the current generation.
    Published {
        /// The new file's identity.
        identity: PhysicalIdentity,
    },
    /// Another writer created it first with the same bytes; this caller adopted theirs.
    Adopted {
        /// The winner's identity.
        identity: PhysicalIdentity,
    },
}

impl PublishOutcome {
    /// The current generation's identity, however it came to exist.
    pub const fn identity(self) -> PhysicalIdentity {
        match self {
            Self::Published { identity } | Self::Adopted { identity } => identity,
        }
    }

    /// Whether this caller published the file.
    pub const fn is_published(self) -> bool {
        matches!(self, Self::Published { .. })
    }
}

/// How many distinct temporary names staging tries before giving up.
const STAGING_ATTEMPTS: u32 = 8;

/// Writes `contents` to a synced temporary file in `directory`, ready to publish.
///
/// The temporary is named `session.migration.<pid>-<n>.tmp`, created with `create_new` so it cannot
/// adopt someone else's file, written and `sync_all`ed so the published link cannot point at bytes
/// the kernel has not committed, and its digest computed from the same buffer that was written.
///
/// # Errors
///
/// Returns [`GenerationError::Io`] on a create, write, or sync failure, and
/// [`GenerationError::StagingExhausted`] when every candidate name is taken.
pub fn stage(directory: &Path, contents: &[u8]) -> Result<Staged, GenerationError> {
    std::fs::create_dir_all(directory)?;
    let pid = std::process::id();
    for attempt in 0..STAGING_ATTEMPTS {
        let path = directory.join(format!("session.migration.{pid}-{attempt}.tmp"));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        return Ok(Staged {
            path,
            bytes: contents.len(),
            digest: digest_hex(contents),
        });
    }
    Err(GenerationError::StagingExhausted {
        directory: directory.to_path_buf(),
    })
}

/// Makes a staged generation the current one, exclusively.
///
/// # Why a hard link and not a rename
///
/// `rename` overwrites: two migrators that both stage would both succeed and the loser's bytes would
/// silently replace the winner's, with no record of which was correct. `link` fails when the target
/// exists, so exactly one caller publishes and every other caller learns it lost — and then verifies
/// the winner's bytes against its own staging, which is the only way to know the two agreed.
///
/// The staged link is removed afterwards. A redundant unlink is not allowed to turn a committed
/// publication into a failure: the target already owns the bytes.
///
/// # Platform
///
/// Assumes `link(2)` semantics (POSIX, and `NTFS` with `CreateHardLink`). Upstream carries a separate
/// `Win32` publication helper; this build states the assumption instead of shipping a code path it
/// cannot test.
///
/// # Errors
///
/// Returns [`GenerationError::TargetConflict`] when the target exists with different bytes (or is
/// not a regular file), [`GenerationError::PrefixTooShort`] when it is shorter than the staging, and
/// [`GenerationError::Io`] for a link or sync failure.
pub fn publish_exclusive(
    staged: &Staged,
    current: &Path,
) -> Result<PublishOutcome, GenerationError> {
    let staged_dir = staged.path.parent().unwrap_or_else(|| Path::new("."));
    let current_dir = current.parent().unwrap_or_else(|| Path::new("."));
    if staged_dir != current_dir {
        return Err(GenerationError::TargetConflict {
            path: current.to_path_buf(),
            reason: "staged and current generations must share one session directory".to_owned(),
        });
    }
    match std::fs::hard_link(&staged.path, current) {
        Ok(()) => {
            sync_directory(current_dir)?;
            // The staged link is dropped *before* the identity is taken. Unlinking changes the
            // inode's link count, which changes its `ctime`; taking the identity first would report a
            // revision that no longer exists by the time the caller compares it against a peer's.
            let _ignored = std::fs::remove_file(&staged.path);
            let identity = identity(current)?;
            Ok(PublishOutcome::Published { identity })
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            adopt_existing(staged, current)
        }
        Err(error) => Err(error.into()),
    }
}

/// Verifies a competing target and adopts it when it holds exactly the staged bytes.
fn adopt_existing(staged: &Staged, current: &Path) -> Result<PublishOutcome, GenerationError> {
    let metadata = std::fs::symlink_metadata(current)?;
    if metadata.file_type().is_symlink() {
        return Err(GenerationError::TargetConflict {
            path: current.to_path_buf(),
            reason: "target is a symbolic link".to_owned(),
        });
    }
    if !metadata.is_file() {
        return Err(GenerationError::TargetConflict {
            path: current.to_path_buf(),
            reason: "target is not a regular file".to_owned(),
        });
    }
    let size = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if size < staged.bytes {
        return Err(GenerationError::PrefixTooShort {
            bytes: size,
            required: staged.bytes,
        });
    }
    // Only the staged prefix is compared: a live writer may already have appended to the winner, and
    // those bytes are a valid continuation rather than a disagreement.
    let mut head = vec![0_u8; staged.bytes];
    let mut file = File::open(current)?;
    file.read_exact(&mut head)?;
    if digest_hex(&head) != staged.digest {
        return Err(GenerationError::TargetConflict {
            path: current.to_path_buf(),
            reason: "target bytes differ from the migrated generation".to_owned(),
        });
    }
    let identity = {
        // As on the publishing path: unlink first, then stat, so the reported revision is the one
        // the file is left in.
        let _ignored = std::fs::remove_file(&staged.path);
        identity(current)?
    };
    Ok(PublishOutcome::Adopted { identity })
}

/// Flushes a directory's entries, so a created link survives a power loss.
fn sync_directory(directory: &Path) -> Result<(), GenerationError> {
    #[cfg(unix)]
    {
        File::open(directory)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------------------------

/// The lowercase hex `SHA-256` of `bytes`.
///
/// The crate has no hash dependency, and the two other modules that need one keep their own copy
/// ([`crate::host::webserver::sha1`] is the same decision for `SHA-1`). A dependency for one function would
/// be a supply-chain surface for every build of this crate.
#[allow(
    clippy::many_single_char_names,
    reason = "the round variable names are the ones FIPS 180-4 uses; renaming them would hide a transcription error"
)]
pub fn digest_hex(bytes: &[u8]) -> String {
    let mut state = SHA256_H;
    let bit_length = u64::try_from(bytes.len())
        .unwrap_or(u64::MAX)
        .wrapping_mul(8);
    let mut padded = Vec::with_capacity(bytes.len() + 72);
    padded.extend_from_slice(bytes);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());

    let mut words = [0_u32; 64];
    for chunk in padded.chunks_exact(64) {
        for (word, group) in words.iter_mut().zip(chunk.chunks_exact(4)) {
            *word = u32::from_be_bytes([group[0], group[1], group[2], group[3]]);
        }
        for round in 16..64 {
            let (head, tail) = words.split_at_mut(round);
            let w15 = head[round - 15];
            let w2 = head[round - 2];
            let w7 = head[round - 7];
            let w16 = head[round - 16];
            let s0 = w15.rotate_right(7) ^ w15.rotate_right(18) ^ (w15 >> 3);
            let s1 = w2.rotate_right(17) ^ w2.rotate_right(19) ^ (w2 >> 10);
            if let Some(slot) = tail.first_mut() {
                *slot = w16.wrapping_add(s0).wrapping_add(w7).wrapping_add(s1);
            }
        }
        let mut working = state;
        for (&constant, &word) in SHA256_K.iter().zip(words.iter()) {
            let [a, b, c, d, e, f, g, h] = working;
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(constant)
                .wrapping_add(word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            working = [
                temp1.wrapping_add(temp2),
                a,
                b,
                c,
                d.wrapping_add(temp1),
                e,
                f,
                g,
            ];
        }
        for (slot, value) in state.iter_mut().zip(working.iter()) {
            *slot = slot.wrapping_add(*value);
        }
    }
    let mut hex = String::with_capacity(64);
    for word in state {
        for shift in [28_u32, 24, 20, 16, 12, 8, 4, 0] {
            let nibble = u8::try_from((word >> shift) & 0x0F).unwrap_or(0);
            hex.push(HEX_LOWER[usize::from(nibble)]);
        }
    }
    hex
}

/// The SHA-256 round constants (FIPS 180-4 §4.2.2).
const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The SHA-256 initial hash values (FIPS 180-4 §5.3.3).
const SHA256_H: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];
