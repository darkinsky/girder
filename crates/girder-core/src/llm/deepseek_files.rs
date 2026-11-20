//! `DeepSeek Files`: the decisions an upload makes before any byte leaves the process.
//!
//! Allowing `doc_markdown` at module scope: this file names `DeepSeek`, `Files`, `Messages`, and
//! `GET` as API nouns in nearly every paragraph, and quoting each one would make the prose about
//! punctuation instead of about behaviour. The two peer modules that document the same route do the
//! same.
#![allow(
    clippy::doc_markdown,
    reason = "provider and endpoint names are the subject of nearly every paragraph"
)]
// The provider's own published limits and the index format are part of this module's surface, not
// of its control flow: a driver that reports remaining quota or reads a beta header compares them,
// and this module deliberately does not perform either. `tests/deepseek_files_gap.rs` pins every
// number, so an unread constant here is still a checked one.
#![allow(
    dead_code,
    reason = "published provider limits and the upload-index format are the driver's to read"
)]
//!
//! Upstream path: `packages/llm/llm-deepseek/src/{files-api,file-store,upload-index,request-files,
//! request-pricing,images,listing,listed-catalog}.ts`.
//!
//! # What upstream has, measured
//!
//! `packages/llm/llm-deepseek` is 3,699 lines across 29 files. Six of them — `files-api.ts` (262),
//! `file-store.ts` (336), `upload-index.ts` (224), `request-files.ts` (158), `request-pricing.ts`
//! (111), `images.ts` (111) — are the Files API: turning a durable image reference into a provider
//! `file_id` that a Messages request can cite. Two more — `listing.ts` (269) and `listed-catalog.ts`
//! (186) — are the negotiated catalog that decides which models a listed `/models` answer may offer.
//! Together that is 1,657 lines, 45% of the route, and this build had none of it: the crate root's
//! `deepseek_serialize_gap` already emits `{"type":"file","file_id":…}` when a caller supplies one,
//! but nothing ever produced one.
//!
//! # What is transport, and what is the decision before transport
//!
//! Almost half of those 1,657 lines are HTTP: multipart encoding, redirect refusal, status-to-code
//! mapping, RFC 3339 decoding of `created_at`. None of that is portable in any interesting sense —
//! [`girder_core::llm::deepseek`] already owns a `reqwest` client, and a second one here would be a
//! second policy for timeouts and proxies. The other half is the part that can be wrong quietly, and
//! it is entirely pure:
//!
//! | Decision | Upstream symbol | Here |
//! | --- | --- | --- |
//! | Which endpoint root Files live beneath | `messagesApiRoot` | [`messages_api_root`] |
//! | Which credential namespace one upload belongs to | `deepSeekFileScope` | [`file_scope`] |
//! | What one cached upload is called on disk | `formatVersion: 3` records | [`UploadIndex`] |
//! | Whether a cached id is still reusable | `reusable` | [`UploadRecord::reusable`] |
//! | Which record wins a concurrent commit | `commit` | [`UploadIndex::commit`] |
//! | Which remote files a quota retry may delete | `reclaimOldestOwned` | [`reclaim_page`] |
//! | Whether a failure is a quota failure | `isFilesQuotaError` | [`is_quota_error`] |
//! | Whether a request may retry a stale id, and once | `RequestFiles.retry` | [`RequestFiles::retry`] |
//! | Which image gets which file id, at which occurrence | `prepareFileIds` | [`prepare_file_ids`] |
//! | What one image costs before it is sent | `deepSeekImageRequestPricing` | [`image_request_pricing`] |
//! | Which described list entries may be offered, and why not | `resolveListedCatalog` | [`resolve_listed_catalog`] |
//!
//! # Why the upload plan is a value and not a task
//!
//! Upstream's `ensureUploaded` is a method that performs I/O, and every rule that matters sits inside
//! it: check the cache, refuse an oversized image, upload, compare the response's byte count against
//! what was sent, commit, and — on a quota failure — delete the oldest harness-owned files and try
//! exactly once more. A port that copied the shape would put all of that behind a `reqwest` call and
//! none of it would be testable. [`plan_upload`] therefore returns an [`UploadPlan`]: a closed set of
//! instructions (`Reuse`, `Upload`, `ReclaimThenUpload`, `Refuse`) that a transport driver executes
//! mechanically. [`plan_upload_failure`] is the mirror image, and together the two contain every
//! decision `ensureUploadedOnce` makes, with the socket removed.
//!
//! # Why the listing gate is here
//!
//! `listing.ts` and `listed-catalog.ts` are the other half of "the provider told us something and we
//! must decide what it means". A `GET /models` answer carries a client requirement, declared image
//! limits, and a badge, and this build has to decide which of those rows it may offer *before* it
//! dispatches anything. That decision is a fold over a decoded value, so [`resolve_listed_catalog`]
//! takes a [`ModelListing`] and returns a [`ListedCatalog`]; [`parse_model_listing`] is the decoder
//! in front of it. Wire decoding is here rather than in `deepseek_serialize_gap` because a listing
//! is not a Messages request: it is the negotiation that decides whether there will be one.
//!
//! # Deliberate deviations
//!
//! - **A local [`ClientVersion`], not `crate::boot::install::SemVer`.** `boot_gap` is a private crate module, and
//!   this module is also compiled by path into an integration test, where `crate::` names the test
//!   crate. Re-deriving the precedence order here is the price of that; it is the same algorithm and
//!   `tests/deepseek_files_gap.rs` pins it against the cases `boot_gap` also covers.
//! - **No `Intl.Segmenter`.** Upstream bounds a badge text at 12 *graphemes*; this build counts
//!   `char`s, which differs only for combining sequences and flag pairs. A badge is decoration; a
//!   mis-counted one is trimmed rather than mis-rendered, and the deviation cannot hide a model.
//! - **No filesystem in [`UploadIndex`].** Upstream locks, reads, rewrites and fsyncs one JSON file.
//!   The *content* rules — the accepted shape, the duplicate rejection, the expiry fold on commit —
//!   are the part with the interesting failure modes, so they live here as pure functions over an
//!   [`UploadIndex`]; persistence is [`UploadIndex::to_json`] and [`UploadIndex::from_json`], and the
//!   caller that owns a lock uses them. `file_lock.rs` and `atomic_write.rs` already provide the two
//!   primitives a driver needs.
//! - **Override *validation* is not re-run.** Upstream checks each surviving model against its own
//!   options parser and reports a rejected override; that parser lives in `config.ts` and refuses a
//!   model whose modalities it cannot support. [`layer_override`] normalizes the one invariant that
//!   matters here — a text-only result drops the image limits it inherited — and leaves validation
//!   to the layer that owns the options.
//! - **`prepareFileIds` is a plan, not a loop with awaits.** It yields an ordered
//!   [`FileIdRequest`] list, each carrying the occurrence positions upstream builds for diagnostics.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value;

use girder_core::session::content_projection::{
    FileAttachmentRef, ImageAttachmentAccess, ImageAttachmentRef, ImageBlock, ImageRequestBudget,
    Representation, file_handle_text as file_reference_text, offloaded_image_text,
    project_images_for_text_model, project_offloaded_images, quoted, request_image_handle_text,
    required_image_offload, text_only_image_text,
};
use girder_core::storage::persistence_win32::sha256_hex;
use girder_core::{ContentBlock, Message, parse_instant};
use girder_core::{ImageDimensions as Dimensions, image_tokens, request_dimensions};

// ---------------------------------------------------------------------------------------------
// Constants — upstream `files-api.ts`, `file-store.ts`, `defaults.ts`, `request-pricing.ts`
// ---------------------------------------------------------------------------------------------

/// Minimum provider-supported file lifetime, in seconds — upstream `MIN_FILE_EXPIRY_SECONDS`.
pub const MIN_FILE_EXPIRY_SECONDS: u64 = 3_600;
/// Maximum provider-supported file lifetime, in seconds — upstream `MAX_FILE_EXPIRY_SECONDS`.
pub const MAX_FILE_EXPIRY_SECONDS: u64 = 2_592_000;
/// Largest upload the Files API accepts, in bytes — upstream `MAX_FILE_UPLOAD_BYTES`.
pub const MAX_FILE_UPLOAD_BYTES: u64 = 128 * 1024 * 1024;
/// Per-key file-count quota — upstream `MAX_STORED_FILE_COUNT`.
///
/// Declared but unread by this module: the quota is the provider's to enforce, and the recovery
/// policy here deletes rather than counts. It stays because the driver that reports remaining quota
/// needs the same number, and a second literal in that driver is how the two drift apart.
pub const MAX_STORED_FILE_COUNT: u64 = 10_000;
/// Per-key storage quota, in bytes — upstream `MAX_STORED_FILE_BYTES`.
pub const MAX_STORED_FILE_BYTES: u64 = 25 * 1024 * 1024 * 1024;
/// Per-request-image limit the file store enforces, including `file_id` references — upstream
/// `MAX_IMAGE_BYTES`.
pub const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
/// Filename prefix that marks one remote file as harness-owned — upstream `OWNED_FILE_PREFIX`.
pub const OWNED_FILE_PREFIX: &str = "dsh-";
/// Required beta opt-in for Messages file operations — upstream `MESSAGES_FILES_BETA`.
pub const MESSAGES_FILES_BETA: &str = "files-api-2025-04-14";
/// Default upload lifetime, in seconds — upstream `DEFAULT_FILE_EXPIRY_SECONDS`.
pub const DEFAULT_FILE_EXPIRY_SECONDS: u64 = 7 * 24 * 60 * 60;
/// Default proactive refresh window, in seconds — upstream `DEFAULT_FILE_REFRESH_MARGIN_SECONDS`.
pub const DEFAULT_FILE_REFRESH_MARGIN_SECONDS: u64 = 60 * 60;
/// Default number of oldest harness-owned files removed on quota recovery — upstream
/// `DEFAULT_FILE_QUOTA_CLEANUP_BATCH`.
pub const DEFAULT_FILE_QUOTA_CLEANUP_BATCH: u32 = 100;
/// Default deadline for resolving one request image through the Files API — upstream
/// `DEFAULT_FILES_API_TIMEOUT_MS`.
pub const DEFAULT_FILES_API_TIMEOUT_MS: u64 = 60_000;

/// Default bound on accumulated file-referenced image bytes per request — upstream
/// `DEFAULT_MAX_REQUEST_FILES_BYTES`.
pub const DEFAULT_MAX_REQUEST_FILES_BYTES: u64 = 128 * 1024 * 1024;
/// Default bound on accumulated inline base64 image payload — upstream
/// `DEFAULT_MAX_INLINE_REQUEST_IMAGE_BYTES`.
pub const DEFAULT_MAX_INLINE_REQUEST_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// Provider request image-count limit — upstream `DEFAULT_MAX_IMAGES_PER_REQUEST`.
pub const DEFAULT_MAX_IMAGES_PER_REQUEST: usize = 600;
/// Deterministic raw-byte removal step — upstream `DEFAULT_IMAGE_OFFLOAD_BYTE_QUANTUM`.
pub const DEFAULT_IMAGE_OFFLOAD_BYTE_QUANTUM: u64 = 64 * 1024 * 1024;
/// Deterministic base64-byte removal step — upstream `DEFAULT_INLINE_IMAGE_OFFLOAD_BYTE_QUANTUM`.
pub const DEFAULT_INLINE_IMAGE_OFFLOAD_BYTE_QUANTUM: u64 = 10 * 1024 * 1024;
/// Deterministic image-count removal step — upstream `DEFAULT_IMAGE_OFFLOAD_COUNT_QUANTUM`.
pub const DEFAULT_IMAGE_OFFLOAD_COUNT_QUANTUM: usize = 20;
/// Total-pixel budget matching provider low-detail image input — upstream
/// `DEFAULT_LOW_DETAIL_IMAGE_PIXEL_BUDGET`.
pub const DEFAULT_LOW_DETAIL_IMAGE_PIXEL_BUDGET: u64 = 512 * 512;
/// Encoded-byte target for one deterministic model-request image — upstream
/// `DEFAULT_REQUEST_IMAGE_MAX_BYTES`.
pub const DEFAULT_REQUEST_IMAGE_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// Provider per-side limit once a request carries 15 or more images — upstream
/// `REQUEST_IMAGE_MAX_DIMENSION`.
pub const REQUEST_IMAGE_MAX_DIMENSION: u64 = 4_096;
/// Longest badge text this build displays — upstream `MAX_BADGE_TEXT_LENGTH`.
pub const MAX_BADGE_TEXT_LENGTH: usize = 12;

/// The index format this build reads and writes — upstream `formatVersion`.
pub const UPLOAD_INDEX_FORMAT_VERSION: u32 = 3;

/// The version segment every Files route lives beneath unless the configured base already names it.
const API_VERSION_SEGMENT: &str = "/v1";

// ---------------------------------------------------------------------------------------------
// Identities
// ---------------------------------------------------------------------------------------------

/// An opaque identifier returned by the Files API — upstream `DeepSeekFileId`.
///
/// A newtype rather than `String` so a request body cannot accidentally pass an attachment digest
/// where the provider expects a file id. Upstream brands the string at type level; a struct is the
/// same guarantee with a place to hang the wire rules.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(String);

impl FileId {
    /// Brands a non-empty provider identifier.
    ///
    /// Returns `None` for the empty string and for a value carrying whitespace, which is the set of
    /// strings that would make a later `DELETE /files/{id}` address a different resource than the
    /// one this id named.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Option<Self> {
        let id = id.into();
        if id.is_empty() || id.chars().any(char::is_whitespace) {
            return None;
        }
        Some(Self(id))
    }

    /// The identifier as the wire carries it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A non-secret digest naming one endpoint and credential namespace — upstream `DeepSeekFileScope`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileScope(String);

impl FileScope {
    /// The digest, as 64 lowercase hex characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FileScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------------------------
// Endpoint and namespace
// ---------------------------------------------------------------------------------------------

/// Resolves the API root without duplicating an explicit provider version path — upstream
/// `messagesApiRoot`.
///
/// A base whose *path* already ends in `/v1` is left alone; every other base gets `/v1` appended.
/// Trailing slashes are dropped first, so `https://x/api///` and `https://x/api` are the same root,
/// which is what makes two spellings of one deployment share one upload cache.
#[must_use]
pub fn messages_api_root(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if path_of(base).ends_with(API_VERSION_SEGMENT) {
        base.to_owned()
    } else {
        format!("{base}{API_VERSION_SEGMENT}")
    }
}

/// The path component of an absolute URL, or the whole string when it carries no authority.
fn path_of(url: &str) -> &str {
    let Some((_, rest)) = url.split_once("://") else {
        return url;
    };
    match rest.find('/') {
        Some(index) => &rest[index..],
        None => "",
    }
}

/// Derives the non-secret namespace one upload belongs to — upstream `deepSeekFileScope`.
///
/// The digest binds the endpoint and the credential together, so switching API keys starts a fresh
/// cache instead of reusing file ids the new key cannot read. The credential is only ever an input
/// to the hash; nothing derived from it is reversible, and the result is safe to persist.
///
/// `credentials` is the caller's serialized authentication headers; [`scope_credentials`] builds the
/// spelling upstream passes, which is what makes two orderings of the same headers one namespace.
#[must_use]
pub fn file_scope(base_url: &str, credentials: &str) -> FileScope {
    let root = messages_api_root(base_url);
    let mut input = Vec::with_capacity(root.len() + credentials.len() + 1);
    input.extend_from_slice(root.as_bytes());
    input.push(0);
    input.extend_from_slice(credentials.as_bytes());
    FileScope(sha256_hex(&input))
}

/// Serializes authentication headers the way the namespace digest consumes them — upstream
/// `JSON.stringify(Object.entries(headers).sort(...))`.
///
/// `[[name,value],…]` in byte order of the name. Sorted so two maps that differ only in insertion
/// order produce one namespace; byte order rather than `localeCompare` because the credential set is
/// ASCII header names and a locale-sensitive order would make the namespace depend on the host.
#[must_use]
pub fn scope_credentials(headers: &BTreeMap<String, String>) -> String {
    let entries: Vec<String> = headers
        .iter()
        .map(|(name, value)| format!("[{},{}]", quoted(name), quoted(value)))
        .collect();
    format!("[{}]", entries.join(","))
}

// ---------------------------------------------------------------------------------------------
// Upload index — upstream `upload-index.ts`
// ---------------------------------------------------------------------------------------------

/// One durable remote upload mapping — upstream `DeepSeekUploadRecord`.
///
/// Unix times are milliseconds, as upstream's are after it multiplies the provider's seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadRecord {
    /// Endpoint and credential namespace the remote id is readable under.
    pub scope: FileScope,
    /// Provider-independent normalized attachment the request version was derived from.
    pub attachment_id: String,
    /// Complete request transformation identity, including route budgets and encoder parameters.
    pub variant_id: String,
    /// The provider identifier.
    pub file_id: FileId,
    /// Encoded size reported by the provider.
    pub bytes: u64,
    /// Upload creation time, in Unix milliseconds.
    pub created_at: u64,
    /// Reuse deadline, in Unix milliseconds.
    pub expires_at: u64,
}

impl UploadRecord {
    /// Whether this mapping has more than `refresh_margin_ms` of lifetime left at `now`.
    ///
    /// The comparison is strict: at exactly the margin the mapping is *not* reusable, so a request
    /// that would expire mid-flight refreshes instead. Upstream's `expiresAt - now > marginMs`.
    #[must_use]
    pub fn reusable(&self, now: u64, refresh_margin_ms: u64) -> bool {
        self.expires_at.saturating_sub(now) > refresh_margin_ms
    }

    /// The key this record occupies in the index: namespace plus variant.
    ///
    /// The attachment id is *not* part of the key. Two request variants of one attachment are
    /// separate uploads, and a variant can be derived from a re-encoded copy whose attachment id
    /// changed — keying on the variant is what keeps the mapping valid across that.
    #[must_use]
    pub fn key(&self) -> (String, String) {
        (self.scope.0.clone(), self.variant_id.clone())
    }
}

/// The provider filename one upload is given — upstream `filename`.
///
/// `dsh-<16 hex of attachment>-<8 hex of variant>.<ext>`. The prefix is what later identifies
/// harness-owned files on a quota recovery sweep, so a deployment that changed it would be unable to
/// reclaim its own files. `media_type` selects the extension and must be one of the four supported
/// image types; anything else yields `None` rather than a `.bin` the provider would reject.
#[must_use]
pub fn upload_filename(attachment_id: &str, variant_id: &str, media_type: &str) -> Option<String> {
    let extension = match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpeg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => return None,
    };
    Some(format!(
        "{OWNED_FILE_PREFIX}{}-{}.{extension}",
        hex_window(attachment_id, 16),
        hex_window(variant_id, 8)
    ))
}

/// The first `count` characters of a `sha256:<hex>` identity, after the prefix.
fn hex_window(identity: &str, count: usize) -> String {
    let rest = identity.strip_prefix("sha256:").unwrap_or(identity);
    rest.chars().take(count).collect()
}

/// Why a persisted index was refused — upstream's `InvalidUploadIndexError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexError {
    /// The text was not JSON.
    NotJson,
    /// The document was JSON but not the object shape this format defines.
    NotAnObject,
    /// `formatVersion` was absent or named a version this build does not read.
    UnsupportedFormat {
        /// The version the document declared, when it declared a readable one.
        declared: Option<u64>,
    },
    /// `records` was absent or not an array.
    RecordsNotAnArray,
    /// One record was not an object, or one of its fields had the wrong shape.
    InvalidRecord {
        /// Index of the offending record.
        index: usize,
    },
    /// Two records occupied the same namespace and variant.
    DuplicateMapping {
        /// Index of the second record with that key.
        index: usize,
    },
}

impl fmt::Display for IndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJson => formatter.write_str("llm-deepseek: upload index is not valid JSON"),
            Self::NotAnObject => formatter.write_str("llm-deepseek: upload index is not an object"),
            Self::UnsupportedFormat { declared } => match declared {
                Some(version) => write!(
                    formatter,
                    "llm-deepseek: unsupported upload index format version {version}"
                ),
                None => formatter.write_str("llm-deepseek: unsupported upload index format"),
            },
            Self::RecordsNotAnArray => {
                formatter.write_str("llm-deepseek: upload index has no records array")
            }
            Self::InvalidRecord { index } => {
                write!(
                    formatter,
                    "llm-deepseek: upload index record {index} is invalid"
                )
            }
            Self::DuplicateMapping { index } => write!(
                formatter,
                "llm-deepseek: upload index record {index} duplicates another mapping"
            ),
        }
    }
}

impl std::error::Error for IndexError {}

/// The in-memory form of the durable upload index — upstream `DeepSeekUploadIndex`.
///
/// Upstream is a class over one owner-private JSON file, and every method re-reads the file under a
/// lock. The file is not the interesting part: what matters is that a corrupt or older document
/// reads as *empty* rather than failing a request, that a commit keeps a concurrently installed
/// reusable winner, that expired records are dropped rather than resurrected, and that a removal
/// names an exact generation so a successor installed meanwhile survives. Those are all folds over a
/// record set, so they live here and the caller supplies the lock.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UploadIndex {
    records: Vec<UploadRecord>,
}

/// The outcome of one commit — upstream `UploadIndexCommit`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitOutcome {
    /// The mapping the caller should use, whether or not it wrote it.
    pub record: UploadRecord,
    /// Whether `record` is the candidate this commit was offered.
    ///
    /// `false` means another process published a reusable mapping first. The caller must then delete
    /// the file it just uploaded: the winner is durable, and the duplicate only spends quota.
    pub accepted: bool,
}

impl UploadIndex {
    /// An index holding `records`, with duplicates rejected.
    ///
    /// # Errors
    ///
    /// [`IndexError::DuplicateMapping`] naming the second record that occupied a namespace and
    /// variant already taken. Upstream rejects the whole document, and so does this: a set that
    /// contains two answers to one question is not a cache, it is a coin flip.
    pub fn new(records: Vec<UploadRecord>) -> Result<Self, IndexError> {
        let mut keys: Vec<(String, String)> = Vec::with_capacity(records.len());
        for (index, record) in records.iter().enumerate() {
            let key = record.key();
            if keys.contains(&key) {
                return Err(IndexError::DuplicateMapping { index });
            }
            keys.push(key);
        }
        Ok(Self { records })
    }

    /// An empty index. What a missing or corrupt file reads as.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The records, in stored order.
    #[must_use]
    pub fn records(&self) -> &[UploadRecord] {
        &self.records
    }

    /// How many mappings the index holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the index holds no mapping.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Reads one reusable mapping — upstream `get`.
    ///
    /// A mapping below the refresh margin reads as absent rather than as stale, because a caller that
    /// acted on it would ship a `file_id` the provider is about to forget.
    #[must_use]
    pub fn get(
        &self,
        scope: &FileScope,
        variant_id: &str,
        now: u64,
        refresh_margin_ms: u64,
    ) -> Option<&UploadRecord> {
        self.records.iter().find(|record| {
            record.scope == *scope
                && record.variant_id == variant_id
                && record.reusable(now, refresh_margin_ms)
        })
    }

    /// Publishes a completed upload unless a reusable mapping already exists — upstream `commit`.
    ///
    /// Two effects happen together, and the order matters:
    ///
    /// 1. Every mapping that is *not* reusable at `now` is dropped. Expired records are what make an
    ///    index grow without bound, and offering one as a winner would be worse than growing.
    /// 2. The candidate replaces any non-reusable record with the same key and is appended. A
    ///    *reusable* record with the same key wins, and the candidate is reported as not accepted.
    #[must_use]
    pub fn commit(
        &mut self,
        candidate: UploadRecord,
        now: u64,
        refresh_margin_ms: u64,
    ) -> CommitOutcome {
        if let Some(existing) = self.records.iter().find(|record| {
            record.key() == candidate.key() && record.reusable(now, refresh_margin_ms)
        }) {
            let record = existing.clone();
            return CommitOutcome {
                record,
                accepted: false,
            };
        }
        let key = candidate.key();
        self.records
            .retain(|record| record.reusable(now, refresh_margin_ms) && record.key() != key);
        self.records.push(candidate.clone());
        CommitOutcome {
            record: candidate,
            accepted: true,
        }
    }

    /// Removes exact generations in one update — upstream `remove`.
    ///
    /// `(variant_id, file_id)` pairs, not variants, so a successor installed for the same variant
    /// while a request was in flight survives: only the id the request actually used is invalidated.
    /// Pairs absent from the index are ignored, which makes a repeated invalidation harmless.
    pub fn remove(&mut self, scope: &FileScope, generations: &[(String, FileId)]) {
        self.records.retain(|record| {
            record.scope != *scope
                || !generations.iter().any(|(variant_id, file_id)| {
                    record.variant_id == *variant_id && record.file_id == *file_id
                })
        });
    }

    /// Removes every mapping for one namespace — upstream `clear`.
    pub fn clear(&mut self, scope: &FileScope) {
        self.records.retain(|record| record.scope != *scope);
    }

    /// Decodes a persisted index — upstream `parseIndex`.
    ///
    /// This returns the error rather than swallowing it, so a driver can log *why* a cache was
    /// discarded. Every caller in upstream's `load` maps `ENOENT` and `InvalidUploadIndexError` to an
    /// empty index and rethrows anything else; [`Self::from_json_or_empty`] is that policy.
    ///
    /// # Errors
    ///
    /// Any [`IndexError`], including a duplicate mapping.
    pub fn from_json(text: &str) -> Result<Self, IndexError> {
        let value: Value = serde_json::from_str(text).map_err(|_| IndexError::NotJson)?;
        let Some(document) = value.as_object() else {
            return Err(IndexError::NotAnObject);
        };
        let declared = document.get("formatVersion").and_then(Value::as_u64);
        if declared != Some(u64::from(UPLOAD_INDEX_FORMAT_VERSION)) {
            return Err(IndexError::UnsupportedFormat { declared });
        }
        let Some(rows) = document.get("records").and_then(Value::as_array) else {
            return Err(IndexError::RecordsNotAnArray);
        };
        let mut records = Vec::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            records.push(decode_record(row).ok_or(IndexError::InvalidRecord { index })?);
        }
        Self::new(records)
    }

    /// Decodes a persisted index, treating anything unreadable as empty — upstream `load`.
    ///
    /// A cache that cannot be read is not a cache: failing a request over a damaged local file would
    /// be a worse outcome than re-uploading once, and the next commit repairs the document.
    #[must_use]
    pub fn from_json_or_empty(text: &str) -> Self {
        Self::from_json(text).unwrap_or_default()
    }

    /// Renders the index the way upstream writes it: two-space indented JSON with a trailing newline.
    #[must_use]
    pub fn to_json(&self) -> String {
        let records: Vec<Value> = self.records.iter().map(encode_record).collect();
        let document = serde_json::json!({
            "formatVersion": UPLOAD_INDEX_FORMAT_VERSION,
            "records": records,
        });
        let mut text = serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_owned());
        text.push('\n');
        text
    }
}

/// A non-negative safe integer, as the index stores them.
fn safe_integer(value: Option<&Value>) -> Option<u64> {
    let number = value?.as_f64()?;
    if number < 0.0 || number.fract() != 0.0 || number > 9_007_199_254_740_991.0 {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the value is a non-negative integer below 2^53, checked above"
    )]
    Some(number as u64)
}

/// Whether `value` is exactly `sha256:` followed by 64 lowercase hex digits.
fn is_sha256_identity(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    })
}

fn decode_record(row: &Value) -> Option<UploadRecord> {
    let object = row.as_object()?;
    let scope = object.get("scope")?.as_str()?;
    if scope.len() != 64
        || !scope
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return None;
    }
    let attachment_id = object.get("attachmentId")?.as_str()?;
    let variant_id = object.get("variantId")?.as_str()?;
    if !is_sha256_identity(attachment_id) || !is_sha256_identity(variant_id) {
        return None;
    }
    let file_id = FileId::new(object.get("fileId")?.as_str()?)?;
    Some(UploadRecord {
        scope: FileScope(scope.to_owned()),
        attachment_id: attachment_id.to_owned(),
        variant_id: variant_id.to_owned(),
        file_id,
        bytes: safe_integer(object.get("bytes"))?,
        created_at: safe_integer(object.get("createdAt"))?,
        expires_at: safe_integer(object.get("expiresAt"))?,
    })
}

fn encode_record(record: &UploadRecord) -> Value {
    serde_json::json!({
        "scope": record.scope.0,
        "attachmentId": record.attachment_id,
        "variantId": record.variant_id,
        "fileId": record.file_id.0,
        "bytes": record.bytes,
        "createdAt": record.created_at,
        "expiresAt": record.expires_at,
    })
}

// ---------------------------------------------------------------------------------------------
// Files API failures — upstream `files-api.ts`
// ---------------------------------------------------------------------------------------------

/// The failure class one HTTP status maps to — upstream's `DeepSeekFilesError` code.
///
/// The status is retained because the recovery policy reads it, and because the *class* is what a
/// caller's retry policy keys on rather than the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilesFailureCode {
    /// `401` or `403`: the credential is not accepted.
    Auth,
    /// `429`: the account is rate-limited or over quota.
    RateLimit,
    /// Any `5xx`: the provider failed, not the request.
    Server,
    /// Any other status: the request itself was refused.
    FilesApi,
}

impl FilesFailureCode {
    /// The code one HTTP status maps to. Upstream's exact ladder, in order.
    #[must_use]
    pub fn of(status: u16) -> Self {
        match status {
            401 | 403 => Self::Auth,
            429 => Self::RateLimit,
            status if status >= 500 => Self::Server,
            _ => Self::FilesApi,
        }
    }

    /// The build's stable error code for this class.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "AUTH",
            Self::RateLimit => "RATE_LIMIT",
            Self::Server => "SERVER",
            Self::FilesApi => "FILES_API",
        }
    }
}

/// One Files API failure, carrying the status and the provider's own detail — upstream
/// `DeepSeekFilesError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilesError {
    /// User-readable provider failure.
    pub message: String,
    /// HTTP status the Files API answered.
    pub status: u16,
    /// Provider error fields joined for classification, never for display.
    pub detail: String,
}

impl FilesError {
    /// The failure class of this error's status.
    #[must_use]
    pub fn code(&self) -> FilesFailureCode {
        FilesFailureCode::of(self.status)
    }
}

impl fmt::Display for FilesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FilesError {}

/// Joins one provider error object's classification fields — upstream `providerErrorDetail`.
///
/// `code`, then `type`, then `message`, space-separated, and only the string-valued ones. Upstream
/// ignores anything outside `error`, which is what keeps a request body that merely *mentions* a file
/// id from being read as a stale-id rejection. Returns `(message, detail)`.
#[must_use]
pub fn provider_error_detail(body: &Value) -> (Option<String>, String) {
    let Some(error) = body.get("error").and_then(Value::as_object) else {
        return (None, String::new());
    };
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let detail = ["code", "type", "message"]
        .iter()
        .filter_map(|field| error.get(*field).and_then(Value::as_str))
        .collect::<Vec<&str>>()
        .join(" ");
    (message, detail)
}

/// Whether an upload failure reports a provider storage or file-count quota — upstream
/// `isFilesQuotaError`.
///
/// The phrases are upstream's, including `stored files` and `file count`, because the provider words
/// the same condition differently by endpoint. The *status* is not consulted: a `429` whose detail
/// names no quota is a rate limit, and deleting files would not help it.
#[must_use]
pub fn is_quota_error(error: &FilesError) -> bool {
    let detail = error.detail.to_lowercase();
    [
        "quota",
        "storage",
        "stored files",
        "file count",
        "too many files",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

// ---------------------------------------------------------------------------------------------
// Request-file recovery — upstream `request-files.ts`
// ---------------------------------------------------------------------------------------------

/// Why one occurrence of a request image could not be resolved — upstream `FileResolutionFailure`.
///
/// Distinct from a [`FilesError`] because the caller's response differs: a per-image failure means
/// "fall back to inline for the whole request", never "retry this image".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileResolutionFailure {
    /// What the upload path reported.
    pub cause: String,
}

impl FileResolutionFailure {
    /// Wraps one upload failure's display text.
    #[must_use]
    pub fn new(cause: impl Into<String>) -> Self {
        Self {
            cause: cause.into(),
        }
    }
}

impl fmt::Display for FileResolutionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeepSeek Files API could not resolve a request image.")
    }
}

impl std::error::Error for FileResolutionFailure {}

/// Whether a provider detail names a file id that no longer exists — upstream
/// `providerRejectedFileId`.
///
/// Two clauses that must *both* hold: the detail must mention a file, and it must say the file is
/// gone or invalid. Requiring both is what keeps `"invalid request"` — which names no file — from
/// triggering a delete-and-retry of perfectly good ids.
#[must_use]
pub fn provider_rejected_file_id(detail: &str) -> bool {
    let lowered = detail.to_lowercase();
    let file = mentions_file(&lowered);
    let missing = [
        "expired",
        "not found",
        "not_found",
        "not-found",
        "deleted",
        "does not exist",
        "do not exist",
        "not created under this account",
        "not created under your account",
    ]
    .iter()
    .any(|needle| lowered.contains(needle));
    file && (missing || invalid_file_id_phrase(&lowered))
}

/// Whether a lowercased detail mentions a file at all, in any of the provider's spellings.
///
/// `file`, `file_id`, `file-id`, `file id`, `file_api`, `file_not_found` all count; `filename` and
/// `profile` do not, because the character after the word decides.
fn mentions_file(lowered: &str) -> bool {
    let mut search = lowered;
    while let Some(offset) = search.find("file") {
        let after = &search[offset + 4..];
        if after.chars().next().is_none_or(|c| !c.is_alphanumeric()) {
            return true;
        }
        search = after;
    }
    false
}

/// Whether a lowercased detail says a file id is invalid, allowing up to 20 characters between the
/// two words — upstream's `invalid.{0,20}file[_ -]?(id|api)` and its mirror.
fn invalid_file_id_phrase(lowered: &str) -> bool {
    let names_file_id = |window: &str| {
        [
            "file_id", "file-id", "file id", "file_api", "file api", "file",
        ]
        .iter()
        .any(|needle| window.contains(needle))
    };
    let mut search = lowered;
    while let Some(offset) = search.find("invalid") {
        let tail = &search[offset + "invalid".len()..];
        let window: String = tail.chars().take(20).collect();
        if names_file_id(&window) {
            return true;
        }
        search = tail;
    }
    let mut search = lowered;
    while let Some(offset) = search.find("file") {
        let tail = &search[offset + "file".len()..];
        let window: String = tail.chars().take(20).collect();
        if window.contains("invalid") {
            return true;
        }
        search = tail;
    }
    false
}

/// Whether `detail` names `file_id` as a whole word — upstream `detailNamesFileId`.
///
/// A word boundary is a character that is not a letter, digit, `_`, or `-`, so `file-a` does not
/// match inside `file-abc`. This is what lets a provider that names one bad id invalidate exactly
/// that id instead of every id the request used.
#[must_use]
pub fn detail_names_file_id(detail: &str, file_id: &str) -> bool {
    if file_id.is_empty() {
        return false;
    }
    let boundary = |c: char| !(c.is_alphanumeric() || c == '_' || c == '-');
    for (index, _) in detail.match_indices(file_id) {
        let before = detail[..index].chars().next_back();
        let after = detail[index + file_id.len()..].chars().next();
        if before.is_none_or(boundary) && after.is_none_or(boundary) {
            return true;
        }
    }
    false
}

/// One request image the wire cited, with the occurrence position upstream uses for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsedRequestFile {
    /// The prepared request version's transformation identity.
    pub variant_id: String,
    /// The id the request actually sent for it.
    pub file_id: FileId,
    /// The durable reference, which is what a rejection message describes.
    pub reference: ImageAttachmentRef,
    /// 1-based message index in the serialized request.
    pub message: usize,
    /// 1-based image index within that message.
    pub image: usize,
}

/// The set of file ids one HTTP attempt cited, and the one allowed stale-id retry — upstream
/// `RequestFiles`.
///
/// The rules are small and every one of them has been the cause of a real bug somewhere:
///
/// - [`Self::retry`] does nothing unless the provider detail names a *stale* id. A `400` for any
///   other reason must not delete a working mapping.
/// - Its invalidation set is every id the attempt used, unless the detail names specific ids — then
///   exactly those. Invalidating the named ids is the difference between re-uploading one image and
///   re-uploading all of them.
/// - It answers `Some` at most once per request. A provider that kept rejecting a file id would
///   otherwise produce an unbounded loop.
/// - [`Self::begin_attempt`] resets the occurrence list, because the ids a retry cites are the ones
///   the *retry* resolved, not the ones the failed attempt did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestFiles {
    used: Vec<UsedRequestFile>,
    retried: bool,
}

impl RequestFiles {
    /// A request state with no occurrences recorded.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Resets occurrence tracking before serializing the next attempt.
    pub fn begin_attempt(&mut self) {
        self.used.clear();
    }

    /// Records one resolved occurrence, returning its id so a caller can use it immediately.
    pub fn record(&mut self, file: UsedRequestFile) -> FileId {
        let id = file.file_id.clone();
        self.used.push(file);
        id
    }

    /// What this attempt has cited.
    #[must_use]
    pub fn used(&self) -> &[UsedRequestFile] {
        &self.used
    }

    /// Whether a stale-id retry has already been spent.
    #[must_use]
    pub fn retried(&self) -> bool {
        self.retried
    }

    /// Classifies one rejected response and reports whether to serialize and dispatch again —
    /// upstream `retry`.
    ///
    /// The invalidation set is returned rather than performed, because the caller owns the index and
    /// its lock. `None` means "do not retry"; `Some(generations)` means "invalidate exactly these,
    /// then retry", and is answered at most once per request.
    pub fn retry(&mut self, detail: &str) -> Option<Vec<(String, FileId)>> {
        if self.used.is_empty() || !provider_rejected_file_id(detail) {
            return None;
        }
        let generations = stale_generations(&self.used, detail);
        if self.retried {
            return None;
        }
        self.retried = true;
        Some(generations)
    }

    /// The diagnostic a rejected request carries — upstream `errorMessage`.
    ///
    /// A `400` whose detail reports an unusable image is almost always the provider refusing bytes
    /// the harness itself normalized, so the message names the exact uploads and their geometries.
    /// Without the positions the user sees "invalid image" and has no way to tell which of twelve
    /// screenshots it was.
    #[must_use]
    pub fn error_message(&self, status: u16, message: &str, detail: &str) -> String {
        if status == 400 && !self.used.is_empty() && provider_rejected_normalized_image(detail) {
            normalized_image_diagnostic(&self.used, message, detail)
        } else {
            message.to_owned()
        }
    }
}

/// Upstream `staleMappings`: the ids a rejection named, or every id the attempt used.
///
/// Deduplicated by `(variant, file_id)` first, because one shared variant can serve several
/// occurrences and invalidating the same pair twice is harmless but confusing in a log.
fn stale_generations(used: &[UsedRequestFile], detail: &str) -> Vec<(String, FileId)> {
    let mut unique: Vec<(String, FileId)> = Vec::new();
    for file in used {
        let key = (file.variant_id.clone(), file.file_id.clone());
        if !unique.contains(&key) {
            unique.push(key);
        }
    }
    let named: Vec<(String, FileId)> = unique
        .iter()
        .filter(|(_, file_id)| detail_names_file_id(detail, file_id.as_str()))
        .cloned()
        .collect();
    if named.is_empty() { unique } else { named }
}

/// Whether a provider detail says the *normalized image itself* is unusable — upstream
/// `providerRejectedNormalizedImage`.
#[must_use]
pub fn provider_rejected_normalized_image(detail: &str) -> bool {
    let lowered = detail.to_lowercase();
    let reason_before = [
        "unsupported",
        "invalid",
        "cannot read",
        "failed to decode",
        "failed to process",
    ]
    .iter()
    .any(|reason| window_before(&lowered, reason, "image", 40));
    let image_before = ["unsupported", "invalid", "cannot be decoded"]
        .iter()
        .any(|reason| window_after(&lowered, "image", reason, 40));
    reason_before || image_before
}

/// Whether `reason` appears at most `max` characters before `subject`.
fn window_before(haystack: &str, reason: &str, subject: &str, max: usize) -> bool {
    let mut search = haystack;
    while let Some(offset) = search.find(reason) {
        let tail = &search[offset + reason.len()..];
        if tail.chars().take(max).collect::<String>().contains(subject) {
            return true;
        }
        search = tail;
    }
    false
}

/// Whether `reason` appears at most `max` characters after `subject`.
fn window_after(haystack: &str, subject: &str, reason: &str, max: usize) -> bool {
    let mut search = haystack;
    while let Some(offset) = search.find(subject) {
        let tail = &search[offset + subject.len()..];
        if tail.chars().take(max).collect::<String>().contains(reason) {
            return true;
        }
        search = tail;
    }
    false
}

/// The facts one cited image contributes to a rejection message — upstream `normalizedImageFacts`.
///
/// Upstream reads these from the prepared `RequestImageAttachment`; this build's durable
/// [`ImageAttachmentRef`] has the same facts, which are the ones the message needs.
fn normalized_image_facts(reference: &ImageAttachmentRef, file: &UsedRequestFile) -> String {
    let name = match &reference.name {
        Some(name) => quoted(name),
        None => reference.attachment_id.clone(),
    };
    format!(
        "{name} at message {}, image {} ({}, 8-bit sRGB, {}x{})",
        file.message, file.image, reference.media_type, reference.width, reference.height
    )
}

/// Upstream `normalizedImageDiagnostic`.
fn normalized_image_diagnostic(
    used: &[UsedRequestFile],
    provider_message: &str,
    provider_detail: &str,
) -> String {
    let exact = used
        .iter()
        .find(|file| detail_names_file_id(provider_detail, file.file_id.as_str()))
        .or(if used.len() == 1 { used.first() } else { None });
    if let Some(target) = exact {
        let facts = normalized_image_facts(&target.reference, target);
        return format!(
            "DeepSeek rejected normalized image {facts}: {provider_message}. The provider rejected \
             bytes already normalized by the harness; PNG, JPEG, WebP, and GIF remain supported \
             input formats."
        );
    }
    let mut candidates: Vec<String> = Vec::new();
    for file in used {
        let facts = normalized_image_facts(&file.reference, file);
        if !candidates.contains(&facts) {
            candidates.push(facts);
        }
    }
    format!(
        "DeepSeek rejected a normalized request image: {provider_message}. Candidate images: {}. \
         The provider rejected bytes already normalized by the harness; PNG, JPEG, WebP, and GIF \
         remain supported input formats.",
        candidates.join("; ")
    )
}

// ---------------------------------------------------------------------------------------------
// Upload planning — upstream `file-store.ts` `ensureUploadedOnce`
// ---------------------------------------------------------------------------------------------

/// The expiry and recovery policy one deployment resolved from its configuration — upstream
/// `DeepSeekFilePolicy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilePolicy {
    /// Lifetime requested for each upload, in seconds.
    pub expires_after_seconds: u64,
    /// Remaining lifetime below which an indexed id is refreshed, in seconds.
    pub refresh_margin_seconds: u64,
    /// Maximum number of oldest harness-owned files a quota recovery deletes.
    pub quota_cleanup_batch: u32,
}

impl Default for FilePolicy {
    fn default() -> Self {
        Self {
            expires_after_seconds: DEFAULT_FILE_EXPIRY_SECONDS,
            refresh_margin_seconds: DEFAULT_FILE_REFRESH_MARGIN_SECONDS,
            quota_cleanup_batch: DEFAULT_FILE_QUOTA_CLEANUP_BATCH,
        }
    }
}

impl FilePolicy {
    /// The refresh margin in milliseconds, which is the unit every timestamp here uses.
    #[must_use]
    pub fn refresh_margin_ms(&self) -> u64 {
        self.refresh_margin_seconds.saturating_mul(1_000)
    }

    /// Whether the expiry is one the provider accepts.
    ///
    /// Upstream's ladder is `3600 ..= 2592000` and integral. A deployment that configured anything
    /// else would have every upload refused by the provider *after* the bytes were sent, so the
    /// refusal happens here instead.
    #[must_use]
    pub fn expiry_is_supported(&self) -> bool {
        (MIN_FILE_EXPIRY_SECONDS..=MAX_FILE_EXPIRY_SECONDS).contains(&self.expires_after_seconds)
    }
}

/// One prepared request image, as the upload decision sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestImage {
    /// Provider-independent normalized attachment the version was derived from.
    pub attachment_id: String,
    /// Complete request transformation identity.
    pub variant_id: String,
    /// Encoded size the provider will be asked to store.
    pub bytes: u64,
    /// One of the four supported image types.
    pub media_type: String,
}

/// Why an image never reaches the Files API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The prepared image exceeds [`MAX_IMAGE_BYTES`].
    ImageTooLarge {
        /// Its size.
        bytes: u64,
    },
    /// The configured lifetime is outside what the provider accepts.
    UnsupportedExpiry {
        /// The configured lifetime.
        seconds: u64,
    },
    /// The media type is not one the Files API accepts.
    UnsupportedMediaType {
        /// The configured media type.
        media_type: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ImageTooLarge { bytes } => write!(
                formatter,
                "DeepSeek image is {bytes} bytes, above the 32 MiB per-image limit."
            ),
            Self::UnsupportedExpiry { seconds } => write!(
                formatter,
                "DeepSeek file expiry must be between {MIN_FILE_EXPIRY_SECONDS} and \
                 {MAX_FILE_EXPIRY_SECONDS} seconds, not {seconds}."
            ),
            Self::UnsupportedMediaType { media_type } => write!(
                formatter,
                "DeepSeek Files accepts PNG, JPEG, WebP, and GIF, not {media_type}."
            ),
        }
    }
}

/// What a driver should do for one request image — the decision half of `ensureUploadedOnce`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadPlan {
    /// A reusable mapping already exists; read no bytes and send nothing.
    Reuse(UploadRecord),
    /// Upload the prepared bytes under this filename and lifetime.
    Upload {
        /// Provider filename, carrying the harness-owned prefix.
        filename: String,
        /// Requested lifetime, in seconds.
        expires_after_seconds: u64,
    },
    /// Delete up to `count` oldest harness-owned files, then upload once more.
    ///
    /// The original quota error is returned unchanged when the cleanup deletes nothing, because a
    /// quota error with no reclaimable file is the honest answer and re-uploading fails the same way.
    ReclaimThenUpload {
        /// How many oldest harness-owned files to delete.
        count: u32,
        /// Provider filename for the retried upload.
        filename: String,
        /// Requested lifetime, in seconds, for the retried upload.
        expires_after_seconds: u64,
    },
    /// The image cannot be uploaded at all, before any transport happens.
    Refuse(Refusal),
}

/// The record a completed upload produces — upstream's inline object literal in `upload`.
///
/// Provider `created_at` is in seconds and everything in the index is in milliseconds; this is the
/// only place that multiplication happens, so an expiry computed from the *provider's* creation time
/// (not the local clock) is what a later reuse test compares against.
#[must_use]
pub fn completed_upload(
    image: &RequestImage,
    scope: &FileScope,
    file_id: FileId,
    bytes: u64,
    created_at_seconds: u64,
    expires_after_seconds: u64,
) -> UploadRecord {
    let created_at = created_at_seconds.saturating_mul(1_000);
    UploadRecord {
        scope: scope.clone(),
        attachment_id: image.attachment_id.clone(),
        variant_id: image.variant_id.clone(),
        file_id,
        bytes,
        created_at,
        expires_at: created_at.saturating_add(expires_after_seconds.saturating_mul(1_000)),
    }
}

/// Decides what to do with one request image — the pure half of `ensureUploadedOnce`.
///
/// The order of the checks is upstream's and each position is load-bearing:
///
/// 1. **Cache first.** A reusable mapping means the oversized-image check never runs, so shrinking
///    [`MAX_IMAGE_BYTES`] does not invalidate uploads that already exist.
/// 2. **Size second.** Refusing before transport is the point of a size limit; a provider refusal
///    would spend the upload first.
/// 3. **Expiry third**, for the same reason.
/// 4. **Media type**, because the filename extension is derived from it and the provider keys on it.
#[must_use]
pub fn plan_upload(
    image: &RequestImage,
    index: &UploadIndex,
    scope: &FileScope,
    policy: &FilePolicy,
    now: u64,
) -> UploadPlan {
    if let Some(record) = index.get(scope, &image.variant_id, now, policy.refresh_margin_ms()) {
        return UploadPlan::Reuse(record.clone());
    }
    if image.bytes > MAX_IMAGE_BYTES {
        return UploadPlan::Refuse(Refusal::ImageTooLarge { bytes: image.bytes });
    }
    if !policy.expiry_is_supported() {
        return UploadPlan::Refuse(Refusal::UnsupportedExpiry {
            seconds: policy.expires_after_seconds,
        });
    }
    let Some(filename) =
        upload_filename(&image.attachment_id, &image.variant_id, &image.media_type)
    else {
        return UploadPlan::Refuse(Refusal::UnsupportedMediaType {
            media_type: image.media_type.clone(),
        });
    };
    UploadPlan::Upload {
        filename,
        expires_after_seconds: policy.expires_after_seconds,
    }
}

/// What to do after an upload attempt failed — the recovery half of `ensureUploadedOnce`.
///
/// Exactly one cleanup is ever attempted, and only for a quota failure: any other failure is the
/// caller's to report, and a second cleanup would delete files for a problem deleting cannot fix.
///
/// `already_attempted` is the caller's record of whether this image has already retried; upstream
/// expresses the same thing by calling `upload` twice in one branch and never looping.
#[must_use]
pub fn plan_upload_failure(
    error: &FilesError,
    image: &RequestImage,
    policy: &FilePolicy,
    already_attempted: bool,
) -> Option<UploadPlan> {
    if already_attempted || !is_quota_error(error) {
        return None;
    }
    let filename = upload_filename(&image.attachment_id, &image.variant_id, &image.media_type)?;
    Some(UploadPlan::ReclaimThenUpload {
        count: policy.quota_cleanup_batch,
        filename,
        expires_after_seconds: policy.expires_after_seconds,
    })
}

/// Whether a completed upload's byte count agrees with what was submitted — upstream's
/// `remote.bytes !== version.data.byteLength`.
///
/// A provider that stored a different number of bytes stored a different image. Committing such a
/// mapping would attach a silently wrong picture to every later request that reuses the variant, so
/// the upload is rejected instead and the mismatch is reported as an invalid response.
#[must_use]
pub fn upload_bytes_agree(submitted: u64, reported: u64) -> bool {
    submitted == reported
}

// ---------------------------------------------------------------------------------------------
// Quota reclamation — upstream `reclaimOldestOwned`
// ---------------------------------------------------------------------------------------------

/// One page of `GET /files` — upstream `DeepSeekFilePage`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilePage {
    /// The page's files, in provider order.
    pub data: Vec<ProviderFile>,
    /// Cursor of the first row, when the provider sent one.
    pub first_id: Option<FileId>,
    /// Cursor of the last row, when the provider sent one.
    pub last_id: Option<FileId>,
    /// Whether another page follows.
    pub has_more: bool,
}

/// Where one page leaves a reclamation sweep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageStop {
    /// Another page follows; nothing may be deleted yet.
    Continue,
    /// The provider reported no further page; the plan is final.
    Exhausted,
    /// The page carried no `last_id`, so there is no cursor to ask for.
    MissingCursor,
    /// The page's `last_id` repeated the cursor that produced it, so the next request would loop.
    RepeatedCursor,
}

/// What one reclamation sweep should do, given the pages seen so far — upstream's fold in
/// `reclaimOldestOwned`.
///
/// Upstream cannot ask the API for ascending order, so it must see *every* page before deleting: the
/// oldest owned file can be on the last page. Two consequences follow, and both are represented here
/// rather than left to the caller:
///
/// - **Delete only after the last page.** A sweep that deleted per page would delete the newest
///   files first, which is the wrong order for a cache.
/// - **Stop on a missing or repeated cursor.** Otherwise a provider that answers `has_more: true`
///   without a cursor makes the sweep unbounded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimPlan {
    /// The files to delete, oldest first. Empty while [`PageStop::Continue`].
    pub deletes: Vec<FileId>,
    /// Where this page leaves the sweep.
    pub stopped: PageStop,
    /// The cursor the next page request should carry, or `None` when the sweep is complete.
    pub next_cursor: Option<FileId>,
}

/// Folds one page into a reclamation sweep — the pure form of `reclaimOldestOwned`'s loop body.
///
/// `candidates` accumulates owned files across pages and is pruned to `count` after every fold, so
/// the sweep's memory is bounded by the batch rather than by the account. `cursor` is the `after_id`
/// that produced `page`.
#[must_use]
pub fn reclaim_page(
    candidates: &mut Vec<ProviderFile>,
    page: &FilePage,
    cursor: Option<&FileId>,
    count: u32,
) -> ReclaimPlan {
    for file in &page.data {
        if file.is_owned() {
            candidates.push(file.clone());
        }
    }
    // Provider order is descending by creation; a stable sort by ascending creation keeps the oldest
    // candidates across every page.
    candidates.sort_by_key(|file| file.created_at_seconds);
    candidates.truncate(usize::try_from(count).unwrap_or(usize::MAX));
    let finished = |stopped: PageStop| ReclaimPlan {
        deletes: candidates.iter().map(|file| file.id.clone()).collect(),
        stopped,
        next_cursor: None,
    };
    if !page.has_more {
        return finished(PageStop::Exhausted);
    }
    let Some(last) = page.last_id.clone() else {
        return finished(PageStop::MissingCursor);
    };
    if cursor == Some(&last) {
        return finished(PageStop::RepeatedCursor);
    }
    ReclaimPlan {
        deletes: Vec::new(),
        stopped: PageStop::Continue,
        next_cursor: Some(last),
    }
}

/// Whether a quota recovery deleted enough for a retry to be worth making.
///
/// Zero means "retrying the upload would fail the same way", which is why upstream rethrows the
/// original quota error rather than the retry's. Kept as a function so the branch has a name.
#[must_use]
pub fn reclaim_recovered(deleted: usize) -> bool {
    deleted > 0
}

// ---------------------------------------------------------------------------------------------
// Request image preparation — upstream `images.ts` + `request-pricing.ts`
// ---------------------------------------------------------------------------------------------

/// Projects every occurrence the surface already offloaded into its placeholder text — upstream
/// `prepareImages`'s first step.
///
/// Deliberately runs *before* the vision check: a request whose images are all logged as offloaded
/// needs neither an attachment service nor a vision model, and upstream's test asserts exactly that
/// with a text-only model and no store. Projecting after the check would reject a request that has
/// nothing left to send.
#[must_use]
pub fn project_offloaded(
    messages: &[Message],
    resolve: &dyn Fn(&str) -> Option<ImageAttachmentRef>,
    access: &dyn Fn(&ImageAttachmentRef) -> Option<ImageAttachmentAccess>,
) -> Vec<Message> {
    project_offloaded_images(
        messages,
        &|attachment: &str| resolve(attachment).map(ImageBlock::offloaded),
        &|reference| offloaded_image_text(reference, access(reference).as_ref()),
    )
}

/// Whether an image request may proceed at all — the two admission checks in `prepareImages`.
///
/// Roles are checked as well as modality because Messages accepts images only in `user` turns and
/// `tool_result` content; an image in an assistant turn is a request the provider will reject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageAdmission {
    /// The request may carry images.
    Admitted,
    /// It carries none, so nothing else matters.
    NoImages,
    /// Images are present and the model cannot see.
    ModelIsTextOnly,
    /// Images are present and there is no attachment service to read them.
    NoAttachmentService,
    /// An image sits in a role Messages does not accept images in.
    UnsupportedRole,
}

/// Which precondition one request fails, or that it fails none — the facts `prepareImages`'s
/// guards consult.
///
/// Modeled as a closed set rather than a bag of booleans: the guards are a *ladder*, and a caller
/// that could describe "text-only model, no store, image in an assistant turn" would have to decide
/// for itself which reason to report. The ladder is the answer, so the input is the ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageAdmissionFacts {
    /// The request carries no image, so nothing else matters.
    NoImages,
    /// The request carries an image and the model cannot see.
    ModelIsTextOnly,
    /// The request carries an image and there is no attachment service to read it.
    NoAttachmentService,
    /// The request carries an image in a role Messages does not accept images in.
    UnsupportedRole,
    /// Every precondition holds.
    Admissible,
}

/// Answers whether one request's images may be sent — upstream `prepareImages`'s guards.
#[must_use]
pub fn admit_images(facts: ImageAdmissionFacts) -> ImageAdmission {
    match facts {
        ImageAdmissionFacts::NoImages => ImageAdmission::NoImages,
        ImageAdmissionFacts::ModelIsTextOnly => ImageAdmission::ModelIsTextOnly,
        ImageAdmissionFacts::NoAttachmentService => ImageAdmission::NoAttachmentService,
        ImageAdmissionFacts::UnsupportedRole => ImageAdmission::UnsupportedRole,
        ImageAdmissionFacts::Admissible => ImageAdmission::Admitted,
    }
}

/// The byte/count budgets one request faces on each of its two paths.
///
/// The two differ in every quantity: the Files path counts raw bytes against a large budget, the
/// inline fallback counts base64 against a much smaller one. Upstream builds them from one
/// connection with a `representation` switch; this is that switch as data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteBudgets {
    /// Bits accepted when images are cited as file ids.
    pub files: ImageRequestBudget,
    /// Bits accepted when images are inlined as base64.
    pub inline: ImageRequestBudget,
}

/// Builds both budgets from one connection snapshot's limits.
#[must_use]
pub fn route_budgets(
    max_request_files_bytes: u64,
    max_inline_request_image_bytes: u64,
    max_images_per_request: usize,
    image_offload_byte_quantum: u64,
    inline_image_offload_byte_quantum: u64,
    image_offload_count_quantum: usize,
) -> RouteBudgets {
    RouteBudgets {
        files: ImageRequestBudget {
            representation: Representation::Raw,
            max_bytes: Some(max_request_files_bytes),
            max_images: Some(max_images_per_request),
            byte_quantum: Some(image_offload_byte_quantum),
            count_quantum: Some(image_offload_count_quantum),
        },
        inline: ImageRequestBudget {
            representation: Representation::Base64,
            max_bytes: Some(max_inline_request_image_bytes),
            max_images: Some(max_images_per_request),
            byte_quantum: Some(inline_image_offload_byte_quantum),
            count_quantum: Some(image_offload_count_quantum),
        },
    }
}

/// How many more leading occurrences the surface must offload before a request fits — upstream
/// `assertImagesFit`'s count, for one representation.
///
/// `version_bytes` reports the *prepared* size, not the source's, for the same reason upstream takes
/// it as a callback: the budget is about what the wire carries.
#[must_use]
pub fn required_offload(
    messages: &[Message],
    budget: &ImageRequestBudget,
    version_bytes: &dyn Fn(&ImageAttachmentRef) -> u64,
) -> usize {
    required_image_offload(messages, budget, &|block| version_bytes(&block.attachment))
}

/// What one retained image occurrence costs a route — upstream `LlmImageRequestPrice`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePrice {
    /// Vision tokens the occurrence costs. Zero for a text-only route or an offloaded occurrence.
    pub visual_tokens: u64,
    /// The handle or placeholder text the model sees in its place.
    pub text: String,
}

/// One image limit a route may declare over the provider's published defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImageOverrides {
    /// Explicit pixel budget, or [`PixelBudget::Low`] for the published low-detail preset.
    pub pixel_budget: Option<PixelBudget>,
    /// Encoded-byte target for the prepared image.
    pub max_bytes: Option<u64>,
}

/// The pixel budget one route declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelBudget {
    /// The published low-detail preset: [`DEFAULT_LOW_DETAIL_IMAGE_PIXEL_BUDGET`] total pixels.
    Low,
    /// An explicit total-pixel cap.
    Pixels(u64),
}

/// The geometry and byte target one route applies to one source image — upstream
/// `ImageRequestTarget`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageRequestTarget {
    /// Encoded width the request carries.
    pub width: u64,
    /// Encoded height the request carries.
    pub height: u64,
    /// Encoded-byte target the encoder aims at.
    pub max_bytes: u64,
}

/// The encoded-byte target one route applies to every request image — upstream
/// `resolveRequestImageMaxBytes`.
#[must_use]
pub fn request_image_max_bytes(overrides: &ImageOverrides) -> u64 {
    overrides
        .max_bytes
        .unwrap_or(DEFAULT_REQUEST_IMAGE_MAX_BYTES)
}

/// Resolves the geometry one route sends for one source image — upstream
/// `resolveRequestImageTarget`.
///
/// Three stages, in upstream's order, and the order changes the answer:
///
/// 1. The published token grid, unless the route declares a pixel budget — in which case the
///    aspect-preserving projection to that budget, with no enlargement.
/// 2. The provider's per-side limit of [`REQUEST_IMAGE_MAX_DIMENSION`], applied to *whatever* stage
///    1 produced. The limit exists because a request carrying 15 or more images is capped per side,
///    and applying it unconditionally is what keeps the image count from changing the projection.
/// 3. The route's encoded-byte target, which is a size for the encoder and not a geometry.
#[must_use]
pub fn resolve_request_image_target(
    source: Dimensions,
    overrides: &ImageOverrides,
) -> ImageRequestTarget {
    let (width, height) = (u64::from(source.width), u64::from(source.height));
    let projected = match overrides.pixel_budget {
        None => {
            let projected = request_dimensions(source.width, source.height);
            (u64::from(projected.width), u64::from(projected.height))
        }
        Some(PixelBudget::Low) => {
            project_to_pixels(width, height, DEFAULT_LOW_DETAIL_IMAGE_PIXEL_BUDGET)
        }
        Some(PixelBudget::Pixels(budget)) => project_to_pixels(width, height, budget),
    };
    let capped = if projected.0.max(projected.1) > REQUEST_IMAGE_MAX_DIMENSION {
        long_edge_projection(width, height, REQUEST_IMAGE_MAX_DIMENSION)
    } else {
        projected
    };
    ImageRequestTarget {
        width: capped.0,
        height: capped.1,
        max_bytes: request_image_max_bytes(overrides),
    }
}

/// Aspect-preserving projection within a total-pixel cap — upstream `requestImageDimensions`.
///
/// Inward-rounded, never enlarging, and it decrements the long axis until the product fits because
/// rounding the short edge can push the product back over the cap. That loop is the whole reason
/// this is not three lines of arithmetic.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel counts are below 2^53 and the rounded short edge is non-negative"
)]
#[must_use]
pub fn project_to_pixels(width: u64, height: u64, max_pixels: u64) -> (u64, u64) {
    if width == 0 || height == 0 || max_pixels == 0 {
        return (width, height);
    }
    // `requestImageDimensions`'s own early return: `Math.min(1, sqrt(maxPixels / area)) === 1`.
    // Testing the area instead of the scale keeps this an integer comparison.
    if width.saturating_mul(height) <= max_pixels {
        return (width, height);
    }
    let source_area = (width as f64) * (height as f64);
    let scale = (max_pixels as f64 / source_area).sqrt();
    if width >= height {
        let mut projected_width = ((width as f64) * scale).floor().max(1.0) as u64;
        let mut projected_height =
            round_half_up(projected_width as f64 * height as f64 / width as f64);
        while projected_width.saturating_mul(projected_height) > max_pixels && projected_width > 1 {
            projected_width -= 1;
            projected_height = round_half_up(projected_width as f64 * height as f64 / width as f64);
        }
        (projected_width, projected_height)
    } else {
        let mut projected_height = ((height as f64) * scale).floor().max(1.0) as u64;
        let mut projected_width =
            round_half_up(projected_height as f64 * width as f64 / height as f64);
        while projected_width.saturating_mul(projected_height) > max_pixels && projected_height > 1
        {
            projected_height -= 1;
            projected_width = round_half_up(projected_height as f64 * width as f64 / height as f64);
        }
        (projected_width, projected_height)
    }
}

/// `Math.round` for non-negative values: half away from zero, floored at 1.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the argument is a non-negative pixel count below 2^53"
)]
fn round_half_up(value: f64) -> u64 {
    (value + 0.5).floor().max(1.0) as u64
}

/// Rescales a geometry to an exact long edge — upstream `longEdgeDimensions`.
#[allow(clippy::cast_precision_loss, reason = "pixel counts are below 2^53")]
#[must_use]
pub fn long_edge_projection(width: u64, height: u64, long_edge: u64) -> (u64, u64) {
    if long_edge >= width.max(height) {
        return (width, height);
    }
    if width >= height {
        (
            long_edge,
            round_half_up(long_edge as f64 * height as f64 / width as f64),
        )
    } else {
        (
            round_half_up(long_edge as f64 * width as f64 / height as f64),
            long_edge,
        )
    }
}

/// Prices every retained occurrence of one request — upstream `deepSeekImageRequestPricing`.
///
/// `vision` decides what an occurrence costs: a text-only route substitutes deterministic text and
/// charges nothing visual, because the model never receives pixels; a vision route charges its
/// *projected request geometry*, not the source's.
///
/// `resolve_access` is consulted per occurrence, at pricing time, for the same reason the serializer
/// consults it: the handle text names a path in the current execution world, and a path that moved
/// between pricing and dispatch changes the price by its own length.
#[must_use]
pub fn image_request_pricing(
    vision: bool,
    overrides: &ImageOverrides,
    occurrences: &[ImageBlock],
    resolve_access: &dyn Fn(&ImageAttachmentRef) -> Option<ImageAttachmentAccess>,
) -> Vec<ImagePrice> {
    occurrences
        .iter()
        .map(|occurrence| {
            let reference = &occurrence.attachment;
            let access = resolve_access(reference);
            if !vision {
                return ImagePrice {
                    visual_tokens: 0,
                    text: text_only_image_text(reference),
                };
            }
            if occurrence.offloaded {
                return ImagePrice {
                    visual_tokens: 0,
                    text: offloaded_image_text(reference, access.as_ref()),
                };
            }
            let target = resolve_request_image_target(
                Dimensions::new(
                    u32::try_from(reference.width).unwrap_or(u32::MAX),
                    u32::try_from(reference.height).unwrap_or(u32::MAX),
                ),
                overrides,
            );
            let width = u32::try_from(target.width).unwrap_or(u32::MAX);
            let height = u32::try_from(target.height).unwrap_or(u32::MAX);
            ImagePrice {
                visual_tokens: image_tokens(width, height),
                text: request_image_handle_text(
                    reference,
                    (u64::from(width), u64::from(height)),
                    access.as_ref(),
                ),
            }
        })
        .collect()
}

/// What one request's images cost on each of its two paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FitReport {
    /// Occurrences that must be offloaded before the request fits. Zero means it fits.
    pub offload_images: usize,
}

impl FitReport {
    /// Whether the request fits.
    #[must_use]
    pub fn fits(self) -> bool {
        self.offload_images == 0
    }
}

/// Measures one request against both route budgets — upstream `assertImagesFit` for `raw` then
/// `base64`.
#[must_use]
pub fn measure_request_images(
    messages: &[Message],
    budgets: &RouteBudgets,
    version_bytes: &dyn Fn(&ImageAttachmentRef) -> u64,
) -> (FitReport, FitReport) {
    (
        FitReport {
            offload_images: required_offload(messages, &budgets.files, version_bytes),
        },
        FitReport {
            offload_images: required_offload(messages, &budgets.inline, version_bytes),
        },
    )
}

/// One resolution the request must perform, in wire order — upstream `prepareFileIds`'s loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileIdRequest {
    /// The prepared request version to resolve.
    pub variant_id: String,
    /// Durable reference the version was prepared from.
    pub attachment_id: String,
    /// 1-based message index the occurrence sits at.
    pub message: usize,
    /// 1-based image index within that message.
    pub image: usize,
}

/// The ordered resolutions one request needs, deduplicated by attachment — upstream
/// `prepareFileIds`.
///
/// Two properties matter. First, order is wire order: a caller that resolves while serializing must
/// cite ids in the order they appear, or two occurrences of one attachment would upload twice.
/// Second, the *last* recorded position for each attachment wins, which is why the dedup keeps the
/// first insertion but refreshes its position — upstream's `ids.set` overwrites, so the map holds the
/// last occurrence while the ordering is the first's.
#[must_use]
pub fn prepare_file_ids(
    messages: &[Message],
    versions: &BTreeMap<String, String>,
) -> Vec<FileIdRequest> {
    let mut ordered: Vec<FileIdRequest> = Vec::new();
    let mut index_of: BTreeMap<String, usize> = BTreeMap::new();
    for (message_index, message) in messages.iter().enumerate() {
        let mut image = 0_usize;
        for block in &message.content {
            let ContentBlock::Image { attachment } = block else {
                continue;
            };
            image += 1;
            let Some(variant_id) = versions.get(attachment) else {
                continue;
            };
            let position = FileIdRequest {
                variant_id: variant_id.clone(),
                attachment_id: attachment.clone(),
                message: message_index + 1,
                image,
            };
            if let Some(existing) = index_of.get(attachment) {
                ordered[*existing] = position;
            } else {
                index_of.insert(attachment.clone(), ordered.len());
                ordered.push(position);
            }
        }
    }
    ordered
}

/// Text for a *file* attachment a request cites, which is always handle text — never bytes.
///
/// Included because the Files work is the reason a caller holds both an image and a file reference
/// and would otherwise be tempted to treat them alike.
#[must_use]
pub fn file_handle_text(reference: &FileAttachmentRef) -> String {
    file_reference_text(reference, None)
}

/// The text-only substitution applied to every image of a request a text model will see.
#[must_use]
pub fn text_model_projection(messages: &[Message]) -> Vec<Message> {
    project_images_for_text_model(messages)
}

// ---------------------------------------------------------------------------------------------
// Wire decoding — upstream `files-api.ts` response validation
// ---------------------------------------------------------------------------------------------

/// A validated provider file object — upstream `DeepSeekFileObject`.
///
/// One type for both the wire and the reclamation policy: the sweep reads only the id, creation time
/// and filename, and a second three-field mirror of the same object would be a place for the two
/// copies to disagree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderFile {
    /// Provider identifier.
    pub id: FileId,
    /// Stored size.
    pub bytes: u64,
    /// Creation time, floored to Unix seconds.
    pub created_at_seconds: u64,
    /// Stored filename.
    pub filename: String,
    /// Stored media type.
    pub mime_type: String,
}

impl ProviderFile {
    /// Whether the harness uploaded this file — upstream's `filename.startsWith('dsh-')`.
    #[must_use]
    pub fn is_owned(&self) -> bool {
        self.filename.starts_with(OWNED_FILE_PREFIX)
    }
}

/// Why one Files response could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    /// The response was not the shape the operation documents.
    InvalidResponse {
        /// Which operation was being decoded.
        operation: String,
    },
    /// A list page had a cursor that was neither absent, `null`, nor a string.
    InvalidCursor {
        /// Which cursor field.
        field: String,
    },
}

impl fmt::Display for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidResponse { operation } => write!(
                formatter,
                "DeepSeek Files API returned an invalid {operation} response."
            ),
            Self::InvalidCursor { field } => {
                write!(
                    formatter,
                    "DeepSeek Files API returned a non-string {field}."
                )
            }
        }
    }
}

impl std::error::Error for WireError {}

/// Decodes one `created_at` into Unix seconds — upstream's
/// `Math.floor(Date.parse(wire.created_at) / 1_000)`.
///
/// `Date.parse` accepts every RFC 3339 spelling including a missing fraction and a numeric offset;
/// this build's [`parse_instant`] accepts exactly `YYYY-MM-DDTHH:MM:SS.mmmZ`. The two normalizations
/// below cover the spellings a provider actually sends, and anything else is a rejection rather than
/// a guess — a `created_at` that silently reads as zero would put an expiry in 1970 and make every
/// mapping unreusable.
#[must_use]
pub fn parse_created_at_seconds(text: &str) -> Option<u64> {
    if let Ok(milliseconds) = parse_instant(text) {
        return whole_seconds(milliseconds);
    }
    // `…:SSZ` — add the zero fraction the strict parser requires.
    if let Some(prefix) = text.strip_suffix('Z')
        && prefix.len() == 19
    {
        let normalized = format!("{prefix}.000Z");
        if let Ok(milliseconds) = parse_instant(&normalized) {
            return whole_seconds(milliseconds);
        }
    }
    // `…:SS+00:00` — the same instant written with an explicit zero offset.
    if let Some(prefix) = text.strip_suffix("+00:00")
        && prefix.len() == 19
    {
        let normalized = format!("{prefix}.000Z");
        if let Ok(milliseconds) = parse_instant(&normalized) {
            return whole_seconds(milliseconds);
        }
    }
    None
}

/// Whole seconds from the millisecond instant `parse_instant` reports, floored as `Date.parse`
/// division floors. A pre-epoch instant is not a value the Files API can return.
fn whole_seconds(milliseconds: i64) -> Option<u64> {
    u64::try_from(milliseconds / 1_000).ok()
}

/// Decodes one `file` object from an upload, list, or retrieve response — upstream
/// `parseFileObject`.
///
/// Every field is validated because a malformed object silently committed to the index would attach
/// the wrong remote file to every later request.
///
/// # Errors
///
/// [`WireError::InvalidResponse`] when any field is missing or has the wrong shape, including a
/// `created_at` that is not a parseable instant.
pub fn parse_file_object(value: &Value, operation: &str) -> Result<ProviderFile, WireError> {
    let invalid = || WireError::InvalidResponse {
        operation: operation.to_owned(),
    };
    let object = value.as_object().ok_or_else(invalid)?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .and_then(FileId::new)
        .ok_or_else(invalid)?;
    if object.get("type").and_then(Value::as_str) != Some("file") {
        return Err(invalid());
    }
    let mime_type = object
        .get("mime_type")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let bytes = object
        .get("size_bytes")
        .and_then(Value::as_u64)
        .ok_or_else(invalid)?;
    let filename = object
        .get("filename")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(invalid)?;
    let created_at_seconds = object
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(parse_created_at_seconds)
        .ok_or_else(invalid)?;
    Ok(ProviderFile {
        id,
        bytes,
        created_at_seconds,
        filename: filename.to_owned(),
        mime_type: mime_type.to_owned(),
    })
}

/// Decodes one `GET /files` page — upstream `list`'s body handling.
///
/// A `null` cursor is *absent*, not invalid: the provider sends `first_id: null` on an empty page,
/// and treating that as a protocol error would fail an empty namespace. A *non-string, non-null*
/// cursor is a protocol error, because guessing would paginate from the wrong place.
///
/// # Errors
///
/// [`WireError::InvalidResponse`] for a body missing `data` or `has_more`, and
/// [`WireError::InvalidCursor`] for a numeric or object-valued cursor.
pub fn parse_file_page(value: &Value, operation: &str) -> Result<FilePage, WireError> {
    let invalid = || WireError::InvalidResponse {
        operation: operation.to_owned(),
    };
    let object = value.as_object().ok_or_else(invalid)?;
    let rows = object
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let has_more = object
        .get("has_more")
        .and_then(Value::as_bool)
        .ok_or_else(invalid)?;
    let cursor = |field: &str| -> Result<Option<FileId>, WireError> {
        match object.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(FileId::new(text.as_str())),
            Some(_) => Err(WireError::InvalidCursor {
                field: field.to_owned(),
            }),
        }
    };
    let first_id = cursor("first_id")?;
    let last_id = cursor("last_id")?;
    let mut data = Vec::with_capacity(rows.len());
    for row in rows {
        data.push(parse_file_object(row, operation)?);
    }
    Ok(FilePage {
        data,
        first_id,
        last_id,
        has_more,
    })
}

/// Decodes a `DELETE /files/{id}` acknowledgement — upstream `delete`'s validation.
///
/// The body must name the same id and carry `type: "file_deleted"`. Accepting any 2xx instead would
/// let a provider that deleted a *different* file pass unnoticed, and the index would keep a mapping
/// to an id that is gone.
///
/// # Errors
///
/// [`WireError::InvalidResponse`] when the body disagrees.
pub fn parse_deletion(value: &Value, file_id: &FileId, operation: &str) -> Result<(), WireError> {
    let matches = value.as_object().is_some_and(|object| {
        object.get("id").and_then(Value::as_str) == Some(file_id.as_str())
            && object.get("type").and_then(Value::as_str) == Some("file_deleted")
    });
    if matches {
        Ok(())
    } else {
        Err(WireError::InvalidResponse {
            operation: operation.to_owned(),
        })
    }
}

/// Refuses an upload above [`MAX_FILE_UPLOAD_BYTES`] before transport — upstream's explicit check.
///
/// # Errors
///
/// Returns the submitted size when it exceeds the limit.
pub fn check_upload_size(bytes: u64) -> Result<(), u64> {
    if bytes > MAX_FILE_UPLOAD_BYTES {
        Err(bytes)
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Client version — the semver precedence the listing gate needs
// ---------------------------------------------------------------------------------------------

/// A semantic version, ordered by semver precedence — the subset of `node-semver` the listing gate
/// needs (`valid()` and `gte()`).
///
/// `crate::boot::install::SemVer` already implements this, but that module is private to the crate and this one
/// is compiled by path into an integration test, where `crate::` names the test crate. Equality here
/// follows *precedence*, so build metadata is ignored, exactly as `semver.eq` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientVersion {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<PrereleaseIdentifier>,
}

/// One prerelease identifier, which compares numerically when it is a number.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PrereleaseIdentifier {
    Numeric(u64),
    Alpha(String),
}

impl ClientVersion {
    /// Parses `major.minor.patch`, with optional prerelease and build metadata.
    ///
    /// Returns `None` for anything else, including a numeric field with a leading zero — the same
    /// strictness `semver.valid` applies, because a lenient parse would let a requirement compare
    /// against a version that never existed.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let (text, build) = match text.split_once('+') {
            Some((core, build)) => (core, Some(build)),
            None => (text, None),
        };
        if let Some(build) = build
            && !identifiers_ok(build)
        {
            return None;
        }
        let (core, prerelease) = match text.split_once('-') {
            Some((core, prerelease)) => (core, Some(prerelease)),
            None => (text, None),
        };
        let mut fields = core.split('.');
        let major = numeric_field(fields.next()?)?;
        let minor = numeric_field(fields.next()?)?;
        let patch = numeric_field(fields.next()?)?;
        if fields.next().is_some() {
            return None;
        }
        let prerelease = match prerelease {
            None => Vec::new(),
            Some(text) => {
                if !identifiers_ok(text) {
                    return None;
                }
                text.split('.')
                    .map(|identifier| {
                        if identifier.bytes().all(|byte| byte.is_ascii_digit()) {
                            if identifier.len() > 1 && identifier.starts_with('0') {
                                return None;
                            }
                            identifier.parse().ok().map(PrereleaseIdentifier::Numeric)
                        } else {
                            Some(PrereleaseIdentifier::Alpha(identifier.to_owned()))
                        }
                    })
                    .collect::<Option<Vec<_>>>()?
            }
        };
        Some(Self {
            major,
            minor,
            patch,
            prerelease,
        })
    }

    /// Whether this version is at or above `required` — `semver.gte(self, required)`.
    #[must_use]
    pub fn satisfies_at_least(&self, required: &Self) -> bool {
        self.cmp(required) != std::cmp::Ordering::Less
    }
}

/// Whether a dotted identifier list is non-empty and free of characters semver excludes.
fn identifiers_ok(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// Parse a numeric field, refusing a leading zero (`01`).
fn numeric_field(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    text.parse().ok()
}

impl Ord for ClientVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| compare_prerelease(&self.prerelease, &other.prerelease))
    }
}

impl PartialOrd for ClientVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Compare prerelease identifiers: an absent prerelease sorts *after* a present one, numeric
/// identifiers compare numerically and sort before alphanumeric ones, and a shorter run sorts before
/// a longer one that starts with it.
fn compare_prerelease(
    left: &[PrereleaseIdentifier],
    right: &[PrereleaseIdentifier],
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (left.is_empty(), right.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for (left, right) in left.iter().zip(right.iter()) {
        let ordering = match (left, right) {
            (PrereleaseIdentifier::Numeric(left), PrereleaseIdentifier::Numeric(right)) => {
                left.cmp(right)
            }
            (PrereleaseIdentifier::Numeric(_), PrereleaseIdentifier::Alpha(_)) => Ordering::Less,
            (PrereleaseIdentifier::Alpha(_), PrereleaseIdentifier::Numeric(_)) => Ordering::Greater,
            (PrereleaseIdentifier::Alpha(left), PrereleaseIdentifier::Alpha(right)) => {
                left.cmp(right)
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

// ---------------------------------------------------------------------------------------------
// Model listing — upstream `listing.ts`
// ---------------------------------------------------------------------------------------------

/// The product token listing entries key their requirements and badges under.
///
/// Upstream reads this from the build's `APP_IDENTITY`; it appears in diagnostics verbatim, which is
/// why it is a constant here rather than a parameter on every call.
pub const LISTING_PRODUCT: &str = "deepseek-harness";

/// The requirement one listing entry declares under this client's product token — upstream
/// `ClientRequirement`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientRequirement {
    /// The entry declares none, so availability is decided by whether this build knows the model.
    None,
    /// The entry requires a minimum client version.
    MinVersion(String),
    /// The entry's requirement is unreadable.
    Invalid,
}

/// A badge tone — upstream `LlmModelBadgeTone`, in increasing emphasis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeTone {
    /// No emphasis.
    Neutral,
    /// Informational.
    Info,
    /// Positive.
    Success,
    /// Cautionary.
    Warning,
}

impl BadgeTone {
    /// The tone one wire value names; an unrecognized or absent tone reads as [`Self::Neutral`],
    /// which is the least emphasis rather than an error.
    #[must_use]
    pub fn of(value: Option<&str>) -> Self {
        match value {
            Some("info") => Self::Info,
            Some("success") => Self::Success,
            Some("warning") => Self::Warning,
            _ => Self::Neutral,
        }
    }
}

/// One selector badge — upstream `LlmModelBadge`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelBadge {
    /// Display text, at most [`MAX_BADGE_TEXT_LENGTH`] characters on one line.
    pub text: String,
    /// Its emphasis.
    pub tone: BadgeTone,
}

/// A badge one listing entry declares, or why it cannot be shown — upstream `ListedBadge`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListedBadge {
    /// A displayable badge.
    Badge(ModelBadge),
    /// The declaration is unreadable; the entry is still offered, without a badge.
    Invalid {
        /// Why, in the terms the diagnostic uses.
        reason: String,
    },
}

/// Accepted request modalities — upstream `ModelModality`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Modality {
    /// Text.
    Text,
    /// Images.
    Image,
}

impl Modality {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
        }
    }
}

/// The Messages system-prompt update mode a route uses — upstream `SystemPromptUpdate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemPromptMode {
    /// A later system prompt becomes a turn in the history.
    InHistory,
    /// Only the leading prompt is used.
    LeadingOnly,
}

/// The Messages tool-update mode a route uses — upstream `ToolUpdate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolUpdateMode {
    /// Tool changes ride the history.
    InHistory,
    /// Mid-conversation additions are allowed.
    AdditionOnly,
    /// Only the leading tool list is used.
    LeadingOnly,
}

/// Capabilities a usable listing entry declares — upstream `ListedFacts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedFacts {
    /// Accepted request modalities this build sends, in listing order.
    pub input_modalities: Vec<Modality>,
    /// Combined request and response context, when declared.
    pub context_window: Option<u64>,
    /// Output capability, when declared.
    pub max_output_tokens: Option<u64>,
    /// Messages system-prompt mode, when declared.
    pub system_prompt_update: Option<SystemPromptMode>,
    /// Messages tool-update mode, when declared.
    pub tool_update: Option<ToolUpdateMode>,
}

/// Declared capabilities, or why the Messages transport cannot drive the model — upstream
/// `ListedCapabilities`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListedCapabilities {
    /// The entry can be offered.
    Usable(ListedFacts),
    /// It cannot, for this reason.
    Unusable {
        /// Why, in the terms the diagnostic uses.
        reason: String,
    },
}

/// One decoded listing entry — upstream `ListingEntry`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingEntry {
    /// Wire model id.
    pub id: String,
    /// Display name, when the entry declares a non-empty one.
    pub name: Option<String>,
    /// Requirement declared under this client's product token.
    pub requirement: ClientRequirement,
    /// Declared capabilities.
    pub capabilities: ListedCapabilities,
    /// Selector badge, when the entry declares a displayable one.
    pub badge: Option<ListedBadge>,
}

/// A decoded listing with at least one entry — upstream `ModelListing`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelListing {
    /// Entries in listing order.
    pub entries: Vec<ListingEntry>,
    /// Rows without a usable id, and repeated ids.
    pub skipped: usize,
}

/// Why a listing body could not be decoded — upstream's `INVALID_RESPONSE` throws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListingError {
    /// The body had no `data` array.
    NoDataArray,
    /// Every row lacked a usable id.
    NoEntries,
}

impl fmt::Display for ListingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDataArray => {
                formatter.write_str("DeepSeek model listing has no \"data\" array")
            }
            Self::NoEntries => {
                formatter.write_str("DeepSeek model listing contains no model entry")
            }
        }
    }
}

impl std::error::Error for ListingError {}

/// Decodes one `/models` body, entry by entry — upstream `parseModelListing`.
///
/// # Errors
///
/// [`ListingError::NoDataArray`] when the body is not the object shape the endpoint documents, and
/// [`ListingError::NoEntries`] when no row carries a usable id. Upstream throws in both cases rather
/// than serving an empty catalog: an empty listing is indistinguishable from a deployment with no
/// models, and hiding that would be worse than failing the refresh.
pub fn parse_model_listing(body: &Value) -> Result<ModelListing, ListingError> {
    let data = body
        .as_object()
        .and_then(|object| object.get("data"))
        .and_then(Value::as_array)
        .ok_or(ListingError::NoDataArray)?;
    let mut entries: Vec<ListingEntry> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for row in data {
        let Some(object) = row.as_object() else {
            continue;
        };
        let Some(id) = object.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned);
        entries.push(ListingEntry {
            id: id.to_owned(),
            name,
            requirement: requirement_of(object),
            capabilities: capabilities_of(object),
            badge: badge_of(object),
        });
    }
    if entries.is_empty() {
        return Err(ListingError::NoEntries);
    }
    Ok(ModelListing {
        skipped: data.len().saturating_sub(entries.len()),
        entries,
    })
}

/// The client requirement one entry declares — upstream `requirementOf`.
fn requirement_of(entry: &serde_json::Map<String, Value>) -> ClientRequirement {
    let requirements = match entry.get("client_requirements") {
        None | Some(Value::Null) => return ClientRequirement::None,
        Some(Value::Object(object)) => object,
        Some(_) => return ClientRequirement::Invalid,
    };
    let own = match requirements.get(LISTING_PRODUCT) {
        None | Some(Value::Null) => return ClientRequirement::None,
        Some(Value::Object(object)) => object,
        Some(_) => return ClientRequirement::Invalid,
    };
    match own.get("min_version") {
        None | Some(Value::Null) => ClientRequirement::None,
        Some(Value::String(version)) if !version.is_empty() => {
            ClientRequirement::MinVersion(version.clone())
        }
        Some(_) => ClientRequirement::Invalid,
    }
}

/// The modalities this build sends, in the order the entry lists them — upstream `knownModalities`.
fn known_modalities(value: &Value) -> Option<Vec<Modality>> {
    let rows = value.as_array()?;
    let mut modalities: Vec<Modality> = Vec::new();
    for row in rows {
        let spelling = row.as_str()?;
        let modality = match spelling {
            "text" => Modality::Text,
            "image" => Modality::Image,
            _ => continue,
        };
        if !modalities.contains(&modality) {
            modalities.push(modality);
        }
    }
    Some(modalities)
}

/// A declared capacity: `None` when absent, `Err(())` when it is not a positive safe integer.
fn capacity(value: Option<&Value>) -> Result<Option<u64>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(number) if number > 0 => Ok(Some(number)),
            _ => Err(()),
        },
    }
}

/// The capabilities one entry declares — upstream `capabilitiesOf`.
fn capabilities_of(entry: &serde_json::Map<String, Value>) -> ListedCapabilities {
    let messages = entry
        .get("api_capabilities")
        .and_then(Value::as_object)
        .and_then(|capabilities| capabilities.get("anthropic_messages"))
        .and_then(Value::as_object);
    let Some(messages) = messages else {
        return ListedCapabilities::Unusable {
            reason: "declares no api_capabilities.anthropic_messages".to_owned(),
        };
    };
    let outputs = match entry.get("output_modalities") {
        None | Some(Value::Null) => Some(vec![Modality::Text]),
        Some(value) => known_modalities(value),
    };
    if !outputs.is_some_and(|outputs| outputs.contains(&Modality::Text)) {
        return ListedCapabilities::Unusable {
            reason: "declares no text output".to_owned(),
        };
    }
    let inputs = match entry.get("input_modalities") {
        None | Some(Value::Null) => Some(vec![Modality::Text]),
        Some(value) => known_modalities(value),
    };
    let Some(inputs) = inputs.filter(|inputs| !inputs.is_empty()) else {
        return ListedCapabilities::Unusable {
            reason: "declares no input modality DeepSeek Harness supports".to_owned(),
        };
    };
    let (Ok(context_window), Ok(max_output_tokens)) = (
        capacity(entry.get("context_window")),
        capacity(entry.get("max_output_tokens")),
    ) else {
        return ListedCapabilities::Unusable {
            reason: "declares an invalid context_window or max_output_tokens".to_owned(),
        };
    };
    // A `null` mode declares nothing, like an absent field, so a serializer that writes empty values
    // as `null` keeps this build's knowledge instead of dropping to the conservative mode.
    let system_prompt_update = match declared_mode(messages, "system_prompt_update") {
        Some("in-history") => Some(SystemPromptMode::InHistory),
        Some(_) => Some(SystemPromptMode::LeadingOnly),
        None => None,
    };
    let tool_update = match declared_mode(messages, "tool_update") {
        Some("in-history") => Some(ToolUpdateMode::InHistory),
        Some("addition-only") => Some(ToolUpdateMode::AdditionOnly),
        Some(_) => Some(ToolUpdateMode::LeadingOnly),
        None => None,
    };
    ListedCapabilities::Usable(ListedFacts {
        input_modalities: inputs,
        context_window,
        max_output_tokens,
        system_prompt_update,
        tool_update,
    })
}

/// One mode spelled as a string, or `None` when it is absent, `null`, or not a string.
///
/// A non-string declares nothing rather than declaring something unreadable, because the mode only
/// ever *narrows* what the serializer may do and an unreadable value cannot be acted on.
fn declared_mode<'a>(messages: &'a serde_json::Map<String, Value>, field: &str) -> Option<&'a str> {
    match messages.get(field) {
        Some(Value::String(mode)) => Some(mode.as_str()),
        _ => None,
    }
}

/// The badge one entry declares — upstream `badgeOf`.
fn badge_of(entry: &serde_json::Map<String, Value>) -> Option<ListedBadge> {
    let metadata = match entry.get("client_metadata") {
        None | Some(Value::Null) => None,
        Some(Value::Object(object)) => Some(object),
        Some(_) => {
            return Some(ListedBadge::Invalid {
                reason: "client_metadata is not an object".to_owned(),
            });
        }
    };
    let own = match metadata.map(|metadata| metadata.get(LISTING_PRODUCT)) {
        None | Some(None | Some(Value::Null)) => None,
        Some(Some(Value::Object(object))) => Some(object),
        Some(Some(_)) => {
            return Some(ListedBadge::Invalid {
                reason: format!("client_metadata[\"{LISTING_PRODUCT}\"] is not an object"),
            });
        }
    };
    let declared = match own.map(|own| own.get("badge")) {
        None | Some(None | Some(Value::Null)) => return None,
        Some(Some(Value::Object(object))) => object,
        Some(Some(_)) => {
            return Some(ListedBadge::Invalid {
                reason: format!("client_metadata[\"{LISTING_PRODUCT}\"].badge is not an object"),
            });
        }
    };
    let Some(text) = badge_text(declared.get("text")) else {
        return Some(ListedBadge::Invalid {
            reason: format!(
                "client_metadata[\"{LISTING_PRODUCT}\"].badge.text is not a string or an object of \
                 lowercase language ids including \"en\", each text 1 to \
                 {MAX_BADGE_TEXT_LENGTH} characters on one line"
            ),
        });
    };
    Some(ListedBadge::Badge(ModelBadge {
        text,
        tone: BadgeTone::of(declared.get("tone").and_then(Value::as_str)),
    }))
}

/// One displayable badge text: trimmed, one line, bounded — upstream `badgeText`, extended to the
/// localized-object form `localizedBadgeText` accepts.
///
/// The object form must carry an `en` variant and every key must be a lowercase language id; a
/// translation set without English is unusable because this build cannot guarantee a reader speaks
/// any of the languages it does have.
fn badge_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => single_badge_text(text),
        Value::Object(variants) => {
            let mut has_english = false;
            for (language, variant) in variants {
                let text = variant.as_str()?;
                if !is_language_id(language) || single_badge_text(text).is_none() {
                    return None;
                }
                if language == "en" {
                    has_english = true;
                }
            }
            if !has_english {
                return None;
            }
            variants.get("en")?.as_str().and_then(single_badge_text)
        }
        _ => None,
    }
}

/// `^[a-z]{2,8}(-[a-z0-9]{1,8})*$` — upstream `LANGUAGE_ID`.
fn is_language_id(language: &str) -> bool {
    let mut parts = language.split('-');
    let Some(primary) = parts.next() else {
        return false;
    };
    if !(2..=8).contains(&primary.len()) || !primary.chars().all(|c| c.is_ascii_lowercase()) {
        return false;
    }
    parts.all(|part| {
        (1..=8).contains(&part.len())
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

/// One badge text, or `None` when it is empty, over-long, or carries an unreadable character —
/// upstream `badgeText`.
fn single_badge_text(value: &str) -> Option<String> {
    let text = value.trim();
    if text.is_empty() || text.chars().count() > MAX_BADGE_TEXT_LENGTH {
        return None;
    }
    if text.chars().any(|character| {
        character.is_control()
            || matches!(character, '\u{2028}' | '\u{2029}')
            || is_bidi_control(character)
    }) {
        return None;
    }
    Some(text.to_owned())
}

/// The Unicode bidirectional control characters upstream's `\p{Bidi_Control}` excludes.
fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

// ---------------------------------------------------------------------------------------------
// Listed catalog — upstream `listed-catalog.ts`
// ---------------------------------------------------------------------------------------------

/// Why this build refuses one listed model before network I/O — upstream `CatalogRefusal`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogRefusal {
    /// The entry requires a newer client.
    UpgradeRequired {
        /// The version the entry requires.
        required_version: String,
    },
    /// This build cannot serve the model at all.
    Unsupported,
}

/// This build's knowledge about one model — the `DeepSeekCatalogModel` fields listing resolution
/// reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogModel {
    /// Wire model id.
    pub id: String,
    /// Display name this build ships.
    pub name: Option<String>,
    /// Accepted request modalities this build advertises.
    pub input_modalities: Vec<Modality>,
    /// Combined request and response context.
    pub context_window: Option<u64>,
    /// Per-request output cap.
    pub max_output_tokens: Option<u64>,
    /// Messages system-prompt mode.
    pub system_prompt_update: Option<SystemPromptMode>,
    /// Messages tool-update mode.
    pub tool_update: Option<ToolUpdateMode>,
    /// Pixel budget override.
    pub image_pixel_budget: Option<PixelBudget>,
    /// Encoded-byte target override.
    pub image_max_bytes: Option<u64>,
    /// Whether this model can see images — carried rather than derived, because a layered
    /// `input_modalities` can add or remove the capability independently of the other fields.
    pub vision: bool,
}

impl CatalogModel {
    /// A text-only model with this id and nothing else declared.
    #[must_use]
    pub fn text_only(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            input_modalities: vec![Modality::Text],
            context_window: None,
            max_output_tokens: None,
            system_prompt_update: None,
            tool_update: None,
            image_pixel_budget: None,
            image_max_bytes: None,
            vision: false,
        }
    }

    /// The image overrides this model declares.
    #[must_use]
    pub fn image_overrides(&self) -> ImageOverrides {
        ImageOverrides {
            pixel_budget: self.image_pixel_budget,
            max_bytes: self.image_max_bytes,
        }
    }
}

/// A per-model field override — upstream `ModelOverride`.
///
/// Every field is optional and an unset one inherits the entry it layers over. A text-only result
/// drops the image limits it inherited, which is why this cannot be a plain merge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelOverride {
    /// Display name.
    pub name: Option<String>,
    /// Accepted request modalities.
    pub input_modalities: Option<Vec<Modality>>,
    /// Combined context.
    pub context_window: Option<u64>,
    /// Output cap.
    pub max_output_tokens: Option<u64>,
    /// Messages system-prompt mode.
    pub system_prompt_update: Option<SystemPromptMode>,
    /// Messages tool-update mode.
    pub tool_update: Option<ToolUpdateMode>,
    /// Pixel budget.
    pub image_pixel_budget: Option<PixelBudget>,
    /// Encoded-byte target.
    pub image_max_bytes: Option<u64>,
}

/// The inputs of one catalog resolution — upstream `ListedCatalogInput`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedCatalogInput {
    /// Entries of the last successful listing, or `None` before one succeeds.
    pub listing: Option<ModelListing>,
    /// Validated models this build supports.
    pub supported: Vec<CatalogModel>,
    /// Overrides by model id.
    pub overrides: BTreeMap<String, ModelOverride>,
    /// Route output cap that a listed output capability bounds.
    pub route_max_tokens: u64,
    /// This build's semantic version.
    pub client_version: String,
}

/// One resolved listed catalog — upstream `ListedCatalog`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListedCatalog {
    /// Selectable models in listing order, or this build's models when no listing succeeded.
    pub offered: Vec<CatalogModel>,
    /// Entries request resolution consults: offered models, then supported models nobody offered.
    pub resolution: Vec<CatalogModel>,
    /// Refused models, by id.
    pub refusals: BTreeMap<String, CatalogRefusal>,
    /// Badges of offered models, by id.
    pub badges: BTreeMap<String, ModelBadge>,
    /// Entries hidden for unreadable declarations, and offered models whose badge was dropped.
    pub problems: Vec<String>,
    /// Configuration diagnostics for the overrides.
    pub override_errors: Vec<String>,
}

/// Applies one override layer — upstream `layered`.
///
/// The image limits are dropped when the *result* is not vision-capable and the override did not
/// declare them: inheriting a pixel budget onto a text-only model would be a limit with nothing to
/// apply it to, and it would survive into a catalog a caller reads as "this route can see images".
#[must_use]
pub fn layer_override(model: &CatalogModel, layer: &ModelOverride) -> CatalogModel {
    let mut merged = model.clone();
    if let Some(name) = &layer.name {
        merged.name = Some(name.clone());
    }
    if let Some(modalities) = &layer.input_modalities {
        merged.vision = modalities.contains(&Modality::Image);
        merged.input_modalities.clone_from(modalities);
    }
    if let Some(window) = layer.context_window {
        merged.context_window = Some(window);
    }
    if let Some(max) = layer.max_output_tokens {
        merged.max_output_tokens = Some(max);
    }
    if let Some(mode) = layer.system_prompt_update {
        merged.system_prompt_update = Some(mode);
    }
    if let Some(mode) = layer.tool_update {
        merged.tool_update = Some(mode);
    }
    if let Some(budget) = layer.image_pixel_budget {
        merged.image_pixel_budget = Some(budget);
    }
    if let Some(bytes) = layer.image_max_bytes {
        merged.image_max_bytes = Some(bytes);
    }
    if !merged.vision {
        if layer.image_pixel_budget.is_none() {
            merged.image_pixel_budget = None;
        }
        if layer.image_max_bytes.is_none() {
            merged.image_max_bytes = None;
        }
    }
    merged
}

/// Layers one usable listing entry over this build's entry for the same id — upstream `listed`.
fn listed(
    base: &CatalogModel,
    entry: &ListingEntry,
    facts: &ListedFacts,
    route_max_tokens: u64,
) -> CatalogModel {
    let mut inherited = base.clone();
    // A `leading-only` declaration *removes* the inherited mode rather than restating it, so a route
    // that only supports the leading prompt does not carry an in-history flag it cannot honour.
    match facts.system_prompt_update {
        Some(SystemPromptMode::InHistory) => {
            inherited.system_prompt_update = Some(SystemPromptMode::InHistory);
        }
        Some(SystemPromptMode::LeadingOnly) => inherited.system_prompt_update = None,
        None => {}
    }
    match facts.tool_update {
        Some(ToolUpdateMode::LeadingOnly) | None => {
            if facts.tool_update.is_some() {
                inherited.tool_update = None;
            }
        }
        Some(mode) => inherited.tool_update = Some(mode),
    }
    let layer = ModelOverride {
        name: entry.name.clone(),
        input_modalities: Some(facts.input_modalities.clone()),
        context_window: facts.context_window,
        max_output_tokens: facts.max_output_tokens.map(|max| max.min(route_max_tokens)),
        system_prompt_update: None,
        tool_update: None,
        image_pixel_budget: None,
        image_max_bytes: None,
    };
    layer_override(&inherited, &layer)
}

/// Decides whether one listing entry may be offered — upstream `gateOf`.
fn gate_of(
    entry: &ListingEntry,
    known: bool,
    client_version: &str,
    problems: &mut Vec<String>,
) -> Option<CatalogRefusal> {
    match &entry.requirement {
        ClientRequirement::None => {
            if known {
                None
            } else {
                Some(CatalogRefusal::Unsupported)
            }
        }
        ClientRequirement::MinVersion(required) => {
            let Some(required_version) = ClientVersion::parse(required) else {
                problems.push(format!(
                    "model \"{}\" declares an unreadable client requirement",
                    entry.id
                ));
                return Some(CatalogRefusal::Unsupported);
            };
            let Some(current) = ClientVersion::parse(client_version) else {
                problems.push(format!(
                    "model \"{}\" requires a client version this build cannot compare",
                    entry.id
                ));
                return Some(CatalogRefusal::Unsupported);
            };
            if current.satisfies_at_least(&required_version) {
                None
            } else {
                Some(CatalogRefusal::UpgradeRequired {
                    required_version: required.clone(),
                })
            }
        }
        ClientRequirement::Invalid => {
            problems.push(format!(
                "model \"{}\" declares an unreadable client requirement",
                entry.id
            ));
            Some(CatalogRefusal::Unsupported)
        }
    }
}

/// Resolves one route's catalog from its listing — upstream `resolveListedCatalog`.
///
/// The rules, in the order they apply:
///
/// 1. With no listing, the build's own models are offered, overridden, and that is the catalog.
/// 2. A listed entry is gated by its declared requirement: an unknown model with no requirement is
///    hidden, a requirement above the current version is refused *with the version named*, and an
///    unreadable requirement is recorded as a problem rather than silently hidden.
/// 3. An entry whose capabilities are unusable is hidden, and the reason is recorded.
/// 4. An offered entry is layered over the build's knowledge of the same id, or over a bare
///    text-only model when the build has never heard of it — a listed model this build cannot
///    describe is still real, and hiding it would contradict the listing.
/// 5. An override naming neither a listed nor a built-in model is a configuration error, because the
///    user asked about a model that exists in neither source.
#[must_use]
pub fn resolve_listed_catalog(input: &ListedCatalogInput) -> ListedCatalog {
    let mut problems: Vec<String> = Vec::new();
    let override_errors: Vec<String> = Vec::new();
    let supported: BTreeMap<&str, &CatalogModel> = input
        .supported
        .iter()
        .map(|model| (model.id.as_str(), model))
        .collect();
    let with_override = |model: &CatalogModel| -> CatalogModel {
        match input.overrides.get(&model.id) {
            None => model.clone(),
            Some(layer) => layer_override(model, layer),
        }
    };
    let Some(listing) = &input.listing else {
        let offered: Vec<CatalogModel> = input.supported.iter().map(&with_override).collect();
        return ListedCatalog {
            resolution: offered.clone(),
            offered,
            refusals: BTreeMap::new(),
            badges: BTreeMap::new(),
            problems,
            override_errors,
        };
    };
    let mut offered: Vec<CatalogModel> = Vec::new();
    let mut refusals: BTreeMap<String, CatalogRefusal> = BTreeMap::new();
    let mut badges: BTreeMap<String, ModelBadge> = BTreeMap::new();
    for entry in &listing.entries {
        let known = supported.contains_key(entry.id.as_str());
        if let Some(refusal) = gate_of(entry, known, &input.client_version, &mut problems) {
            refusals.insert(entry.id.clone(), refusal);
            continue;
        }
        let ListedCapabilities::Usable(facts) = &entry.capabilities else {
            if let ListedCapabilities::Unusable { reason } = &entry.capabilities {
                problems.push(format!("model \"{}\" {reason}", entry.id));
            }
            refusals.insert(entry.id.clone(), CatalogRefusal::Unsupported);
            continue;
        };
        let base = supported.get(entry.id.as_str()).map_or_else(
            || CatalogModel::text_only(entry.id.clone()),
            |model| (*model).clone(),
        );
        offered.push(with_override(&listed(
            &base,
            entry,
            facts,
            input.route_max_tokens,
        )));
        match &entry.badge {
            None => {}
            Some(ListedBadge::Badge(badge)) => {
                badges.insert(entry.id.clone(), badge.clone());
            }
            Some(ListedBadge::Invalid { reason }) => {
                problems.push(format!("model \"{}\" shows no badge: {reason}", entry.id));
            }
        }
    }
    let offered_ids: BTreeSet<&str> = offered.iter().map(|model| model.id.as_str()).collect();
    let unmatched = unmatched_overrides(
        &input.overrides,
        input.supported.iter().map(|model| model.id.as_str()),
        listing.entries.iter().map(|entry| entry.id.as_str()),
    );
    let mut override_errors = override_errors;
    override_errors.extend(unmatched);
    let mut resolution = offered.clone();
    for model in &input.supported {
        if !offered_ids.contains(model.id.as_str()) {
            resolution.push(with_override(model));
        }
    }
    ListedCatalog {
        offered,
        resolution,
        refusals,
        badges,
        problems,
        override_errors,
    }
}

/// The override diagnostics upstream emits for ids neither source describes.
fn unmatched_overrides<'a>(
    overrides: &BTreeMap<String, ModelOverride>,
    supported: impl Iterator<Item = &'a str>,
    listed: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    let known: BTreeSet<&str> = supported.chain(listed).collect();
    overrides
        .keys()
        .filter(|id| !known.contains(id.as_str()))
        .map(|id| {
            format!(
                "modelOverrides names \"{id}\", which neither the model listing nor the built-in \
                 catalog describes"
            )
        })
        .collect()
}

/// The message one refused listed model produces — upstream `refuseListedModel`.
///
/// A refusal is checked *before* any network I/O, so the user learns that the model needs a newer
/// client instead of getting a provider's "unknown model" after a round trip.
///
/// Returns `None` when the model may be used.
#[must_use]
pub fn refusal_message(
    model: &str,
    refusal: Option<&CatalogRefusal>,
    client_version: &str,
) -> Option<String> {
    match refusal? {
        CatalogRefusal::UpgradeRequired { required_version } => Some(format!(
            "DeepSeek model \"{model}\" requires DeepSeek Harness {required_version} or later; \
             current version is {client_version}"
        )),
        CatalogRefusal::Unsupported => Some(format!(
            "DeepSeek model \"{model}\" is not supported by DeepSeek Harness; current version is \
             {client_version}"
        )),
    }
}
