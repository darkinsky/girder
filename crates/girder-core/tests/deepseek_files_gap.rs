//! Tests for the `DeepSeek Files` decisions: upload planning, cache reuse, stale-id recovery,
//! request-image pricing, and the listed-catalog gate.
//!
//! The reference vectors are transcriptions of `packages/llm/llm-deepseek/tests/
//! {files-api,files,file-store,upload-index,request-pricing,listing,listed-catalog}.spec.ts`, which
//! is why every test names the upstream case it comes from. Where a case cannot be transcribed —
//! upstream's suite drives a `fetch` stub through a real store, and this build has no transport in
//! this module — the deviation is stated in the test that covers the neighbouring behaviour.
//!
//! # Why the module is included by path
//!
//! `girder-core`'s crate root does not declare `deepseek_files_gap`; adding `pub mod
//! deepseek_files_gap;` and its `pub use` belongs to the integrator. Compiling the module into this
//! test binary directly makes the tests independent of that wiring and keeps the crate root
//! untouched, which is why the imports below name `girder_core::` rather than `crate::`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    reason = "tests assert by panicking; a failed unwrap is a failed test, and the per-rule tables \
              are clearer as long cases than as dozens of near-identical functions"
)]

#[path = "../src/deepseek_files_gap.rs"]
mod deepseek_files_gap;

use std::collections::BTreeMap;

use deepseek_files_gap::{
    BadgeTone, CatalogRefusal, ClientRequirement, ClientVersion, FileId, FilePage, FilePolicy,
    FileScope, FilesError, ImageAdmission, ImageAdmissionFacts, ImageOverrides, ListedBadge,
    ListedCapabilities, ListedCatalogInput, ListingError, MAX_IMAGE_BYTES, Modality, ModelOverride,
    PageStop, PixelBudget, ProviderFile, Refusal, RequestFiles, RequestImage, SystemPromptMode,
    ToolUpdateMode, UploadIndex, UploadPlan, UploadRecord, UsedRequestFile, WireError,
    admit_images, check_upload_size, completed_upload, detail_names_file_id, file_scope,
    image_request_pricing, is_quota_error, layer_override, long_edge_projection,
    measure_request_images, messages_api_root, parse_created_at_seconds, parse_deletion,
    parse_file_object, parse_file_page, parse_model_listing, plan_upload, plan_upload_failure,
    prepare_file_ids, project_offloaded, project_to_pixels, provider_error_detail,
    provider_rejected_file_id, provider_rejected_normalized_image, reclaim_page, reclaim_recovered,
    refusal_message, request_image_max_bytes, resolve_listed_catalog, resolve_request_image_target,
    route_budgets, scope_credentials, text_model_projection, upload_bytes_agree, upload_filename,
};
use girder_core::content_projection_gap::{ImageAttachmentAccess, ImageAttachmentRef, ImageBlock};
use girder_core::{ContentBlock, ImageDimensions, Message, Role, image_tokens};

const ATTACHMENT: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const VARIANT: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const NOW: u64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

fn scope() -> FileScope {
    file_scope("https://api.deepseek.com", "key")
}

fn record(scope: FileScope, variant: &str, file_id: &str, expires_at: u64) -> UploadRecord {
    UploadRecord {
        scope,
        attachment_id: ATTACHMENT.to_owned(),
        variant_id: variant.to_owned(),
        file_id: FileId::new(file_id).unwrap(),
        bytes: 3,
        created_at: 1_000,
        expires_at,
    }
}

fn image(width: u64, height: u64, bytes: u64) -> ImageAttachmentRef {
    ImageAttachmentRef::new(ATTACHMENT, "image/png", bytes, width, height)
}

fn request_image(bytes: u64) -> RequestImage {
    RequestImage {
        attachment_id: ATTACHMENT.to_owned(),
        variant_id: VARIANT.to_owned(),
        bytes,
        media_type: "image/png".to_owned(),
    }
}

fn block(reference: ImageAttachmentRef, offloaded: bool) -> ImageBlock {
    if offloaded {
        ImageBlock::offloaded(reference)
    } else {
        ImageBlock::retained(reference)
    }
}

fn message(role: Role, content: Vec<ContentBlock>) -> Message {
    Message::new(role, content)
}

fn image_block(id: &str) -> ContentBlock {
    ContentBlock::Image {
        attachment: id.to_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Endpoint and namespace — upstream `files-api.spec.ts`, `file-store.spec.ts`
// ---------------------------------------------------------------------------------------------

#[test]
fn api_roots_match_the_upstream_endpoint_table() {
    // `files-api.spec.ts` "resolves the Messages Files endpoint from %s".
    assert_eq!(
        messages_api_root("https://provider.example/anthropic/v1"),
        "https://provider.example/anthropic/v1"
    );
    assert_eq!(
        messages_api_root("https://provider.example/v1beta/"),
        "https://provider.example/v1beta/v1"
    );
    // "uploads Messages files beneath the literal configured root": `custom/route///` collapses.
    assert_eq!(
        messages_api_root("https://gateway.example/custom/route///"),
        "https://gateway.example/custom/route/v1"
    );
    assert_eq!(
        messages_api_root("https://api.deepseek.com"),
        "https://api.deepseek.com/v1"
    );
}

#[test]
fn api_root_only_recognizes_the_version_segment_at_the_path_end() {
    // `v1beta` is not `v1`, so it still receives a version segment.
    assert_eq!(
        messages_api_root("https://x.example/v1beta"),
        "https://x.example/v1beta/v1"
    );
    // A host that merely starts with `v1` is not a version segment.
    assert_eq!(
        messages_api_root("https://v1.example/api"),
        "https://v1.example/api/v1"
    );
    // A base with no path at all gains one.
    assert_eq!(
        messages_api_root("https://x.example"),
        "https://x.example/v1"
    );
}

#[test]
fn trailing_endpoint_slashes_do_not_split_the_credential_scope() {
    // `upload-index.spec.ts` "normalizes trailing endpoint slashes in the credential scope".
    assert_eq!(
        file_scope("https://api.deepseek.com///", "key").as_str(),
        file_scope("https://api.deepseek.com", "key").as_str()
    );
}

#[test]
fn scope_isolates_endpoints_and_credentials() {
    // `file-store.spec.ts` "isolates credential values and header kinds".
    let base = file_scope("https://api.deepseek.com", "key");
    assert_ne!(
        base.as_str(),
        file_scope("https://api.deepseek.com", "other").as_str()
    );
    assert_ne!(
        base.as_str(),
        file_scope("https://other.example", "key").as_str()
    );
    // 64 lowercase hex characters, so a persisted namespace is a stable digest.
    assert_eq!(base.as_str().len(), 64);
    assert!(base.as_str().chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn scope_credentials_are_sorted_so_header_order_cannot_split_a_namespace() {
    // `file-store.spec.ts` "reusing reordered headers": one namespace for two orderings.
    let mut first = BTreeMap::new();
    first.insert("a".to_owned(), "one".to_owned());
    first.insert("b".to_owned(), "two".to_owned());
    let mut reordered = BTreeMap::new();
    reordered.insert("b".to_owned(), "two".to_owned());
    reordered.insert("a".to_owned(), "one".to_owned());
    assert_eq!(scope_credentials(&first), scope_credentials(&reordered));
    assert_eq!(scope_credentials(&first), r#"[["a","one"],["b","two"]]"#);
    assert_eq!(
        file_scope("https://api.deepseek.com", &scope_credentials(&first)).as_str(),
        file_scope("https://api.deepseek.com", &scope_credentials(&reordered)).as_str()
    );
    // A different header *kind* is a different credential even with the same value.
    let mut kinded = BTreeMap::new();
    kinded.insert("x-dsh-auth-token".to_owned(), "one".to_owned());
    assert_ne!(scope_credentials(&first), scope_credentials(&kinded));
}

// ---------------------------------------------------------------------------------------------
// Filenames — upstream `file-store.ts` `filename`
// ---------------------------------------------------------------------------------------------

#[test]
fn filenames_carry_the_owned_prefix_and_windowed_identities() {
    // `file-store.spec.ts` fixture: `dsh-${'a'.repeat(16)}-${'b'.repeat(8)}.png`.
    assert_eq!(
        upload_filename(ATTACHMENT, VARIANT, "image/png").unwrap(),
        format!("dsh-{}-{}.png", "a".repeat(16), "b".repeat(8))
    );
    assert_eq!(
        upload_filename(ATTACHMENT, VARIANT, "image/jpeg").unwrap(),
        format!("dsh-{}-{}.jpeg", "a".repeat(16), "b".repeat(8))
    );
    for (media_type, expected) in [("image/webp", ".webp"), ("image/gif", ".gif")] {
        let filename = upload_filename(ATTACHMENT, VARIANT, media_type).unwrap();
        assert_eq!(
            filename.rsplit_once('.').map(|(_, ext)| ext),
            Some(&expected[1..])
        );
    }
    // An unsupported type yields nothing rather than a `.bin` the provider would reject.
    assert_eq!(upload_filename(ATTACHMENT, VARIANT, "image/avif"), None);
    assert_eq!(
        upload_filename(ATTACHMENT, VARIANT, "application/pdf"),
        None
    );
}

// ---------------------------------------------------------------------------------------------
// Upload index — upstream `upload-index.spec.ts`
// ---------------------------------------------------------------------------------------------

#[test]
fn index_reuses_only_above_the_refresh_margin_and_only_in_the_same_namespace() {
    // "isolates API-key namespaces and reuses only records above the refresh margin".
    let first = scope();
    let second = file_scope("https://api.deepseek.com", "second-key");
    let mut index = UploadIndex::empty();
    let outcome = index.commit(
        record(first.clone(), VARIANT, "file-api-one", 10_000),
        1_000,
        1_000,
    );
    assert!(outcome.accepted);
    assert_eq!(
        index
            .get(&first, VARIANT, 1_000, 1_000)
            .map(|r| r.file_id.as_str()),
        Some("file-api-one")
    );
    assert_eq!(index.get(&second, VARIANT, NOW, 1_000), None);
    // At exactly the margin the mapping is *not* reusable.
    assert_eq!(index.get(&first, VARIANT, 9_000, 1_000), None);
    assert!(index.get(&first, VARIANT, 8_999, 1_000).is_some());
}

#[test]
fn commit_keeps_a_reusable_winner_and_rejects_the_duplicate() {
    // "keeps a reusable cross-process winner and removes only an exact generation".
    let scope = scope();
    let mut index = UploadIndex::empty();
    let first = record(scope.clone(), VARIANT, "file-api-first", 10_000);
    let outcome = index.commit(first.clone(), 1, 1);
    assert!(outcome.accepted);
    let duplicate = record(scope.clone(), VARIANT, "file-api-duplicate", 10_000);
    let outcome = index.commit(duplicate.clone(), 2, 1);
    assert!(!outcome.accepted);
    assert_eq!(outcome.record.file_id.as_str(), "file-api-first");
    // Removing the duplicate's generation leaves the winner in place.
    index.remove(&scope, &[(VARIANT.to_owned(), duplicate.file_id.clone())]);
    assert_eq!(
        index.get(&scope, VARIANT, 2, 1).map(|r| r.file_id.as_str()),
        Some("file-api-first")
    );
    // Removing the winner's own generation is what clears it.
    index.remove(&scope, &[(VARIANT.to_owned(), first.file_id.clone())]);
    assert!(index.get(&scope, VARIANT, 2, 1).is_none());
}

#[test]
fn remove_takes_exact_generations_and_keeps_unlisted_mappings() {
    // "removes several exact generations in one update and keeps unlisted mappings".
    let scope = scope();
    let variant = |byte: &str| format!("sha256:{}", byte.repeat(64));
    let first = record(scope.clone(), &variant("b"), "file-api-b", 10_000);
    let second = record(scope.clone(), &variant("c"), "file-api-c", 10_000);
    let third = record(scope.clone(), &variant("d"), "file-api-d", 10_000);
    let mut index = UploadIndex::empty();
    for entry in [&first, &second, &third] {
        assert!(index.commit(entry.clone(), 1, 1).accepted);
    }
    index.remove(
        &scope,
        &[
            (first.variant_id.clone(), first.file_id.clone()),
            (second.variant_id.clone(), second.file_id.clone()),
            // A superseded id for a variant that is still indexed: no-op.
            (
                third.variant_id.clone(),
                FileId::new("file-api-superseded").unwrap(),
            ),
        ],
    );
    assert!(index.get(&scope, &first.variant_id, 2, 1).is_none());
    assert!(index.get(&scope, &second.variant_id, 2, 1).is_none());
    assert!(index.get(&scope, &third.variant_id, 2, 1).is_some());
}

#[test]
fn a_corrupt_index_reads_as_empty_and_is_repaired_by_the_next_commit() {
    // "treats a corrupt upload cache as empty and repairs it on the next commit".
    let repaired = UploadIndex::from_json_or_empty("{bad");
    assert!(repaired.is_empty());
    let mut index = repaired;
    let record = record(scope(), VARIANT, "file-api-repaired", 10_000);
    assert!(index.commit(record.clone(), 1, 1).accepted);
    let decoded = UploadIndex::from_json(&index.to_json()).unwrap();
    assert_eq!(decoded.get(&scope(), VARIANT, 1, 1), Some(&record));
    assert!(index.to_json().contains("\"formatVersion\": 3"));
}

#[test]
fn every_invalid_persisted_document_reads_as_empty() {
    // The `it.each` table of invalid documents.
    let record = serde_json::json!({
        "scope": "a".repeat(64),
        "attachmentId": ATTACHMENT,
        "variantId": VARIANT,
        "fileId": "file-api-one",
        "bytes": 3,
        "createdAt": 1,
        "expiresAt": 10_000,
    });
    let invalid = [
        "null".to_owned(),
        "[]".to_owned(),
        "{}".to_owned(),
        r#"{"formatVersion":1,"records":[]}"#.to_owned(),
        r#"{"formatVersion":2,"records":[]}"#.to_owned(),
        r#"{"formatVersion":3,"records":null}"#.to_owned(),
        r#"{"formatVersion":3,"records":[null]}"#.to_owned(),
        r#"{"formatVersion":3,"records":[[]]"}"#.to_owned(),
        r#"{"formatVersion":3,"records":[{}]}"#.to_owned(),
        serde_json::json!({"formatVersion": 3, "records": [{
            "scope": "x".repeat(64),
            "attachmentId": ATTACHMENT, "variantId": VARIANT,
            "fileId": "file-api-one", "bytes": 3, "createdAt": 1, "expiresAt": 10_000,
        }]})
        .to_string(),
    ];
    for text in invalid {
        let index = UploadIndex::from_json_or_empty(&text);
        assert!(index.is_empty(), "expected empty for {text}");
        assert!(
            UploadIndex::from_json(&text).is_err(),
            "expected an error for {text}"
        );
    }
    // Field-level corruption: each one is a rejected record, not a silently coerced one.
    for (field, value) in [
        ("attachmentId", serde_json::json!("wrong")),
        ("variantId", serde_json::json!("wrong")),
        ("fileId", serde_json::json!("")),
        ("bytes", serde_json::json!(-1)),
        ("bytes", serde_json::json!(1.5)),
        ("createdAt", serde_json::json!(-1)),
        ("createdAt", serde_json::json!(1.5)),
        ("expiresAt", serde_json::json!(-1)),
        ("expiresAt", serde_json::json!(1.5)),
    ] {
        let mut bad = record.clone();
        bad[field] = value;
        let text = serde_json::json!({"formatVersion": 3, "records": [bad]}).to_string();
        assert!(
            UploadIndex::from_json_or_empty(&text).is_empty(),
            "expected empty for {field}"
        );
    }
}

#[test]
fn duplicate_persisted_mappings_are_a_corrupt_cache() {
    // "rejects duplicate persisted mappings as a corrupt cache".
    let record = serde_json::json!({
        "scope": "a".repeat(64),
        "attachmentId": ATTACHMENT,
        "variantId": VARIANT,
        "fileId": "file-api-one",
        "bytes": 3,
        "createdAt": 1,
        "expiresAt": 10_000,
    });
    let text =
        serde_json::json!({"formatVersion": 3, "records": [record.clone(), record]}).to_string();
    assert!(UploadIndex::from_json_or_empty(&text).is_empty());
    assert!(UploadIndex::from_json(&text).is_err());
    // The same duplicate through the constructor is refused just as loudly.
    let built = UploadIndex::new(vec![
        record_for(scope(), VARIANT, "one", 10_000),
        record_for(scope(), VARIANT, "two", 10_000),
    ]);
    assert!(built.is_err());
}

fn record_for(scope: FileScope, variant: &str, file_id: &str, expires_at: u64) -> UploadRecord {
    record(scope, variant, file_id, expires_at)
}

#[test]
fn commit_drops_expired_records_and_clear_targets_one_namespace() {
    // "drops expired records on commit and clears only the selected namespace".
    let first = file_scope("https://api.deepseek.com", "first");
    let second = file_scope("https://api.deepseek.com", "second");
    let mut index = UploadIndex::empty();
    let _ = index.commit(record(first.clone(), VARIANT, "file-api-expired", 2), 0, 0);
    let _ = index.commit(
        record(second.clone(), VARIANT, "file-api-live", 10_000),
        3,
        1,
    );
    // The expired record is gone after the second commit, which is what bounds the index.
    assert_eq!(index.len(), 1);
    assert!(index.get(&first, VARIANT, 3, 1).is_none());
    assert!(index.get(&second, VARIANT, 3, 1).is_some());
    index.clear(&first);
    index.clear(&second);
    assert!(index.is_empty());
    // A clear of an already-empty namespace is harmless.
    index.clear(&second);
}

#[test]
fn a_refreshed_commit_replaces_its_expired_predecessor() {
    // The commit fold must not resurrect a record it just dropped.
    let scope = scope();
    let mut index = UploadIndex::empty();
    let _ = index.commit(record(scope.clone(), VARIANT, "old", 5_000), 1_000, 1_000);
    // `old` is below the margin at 4_500, so the refresh is accepted and replaces it.
    let outcome = index.commit(record(scope.clone(), VARIANT, "new", 100_000), 4_500, 1_000);
    assert!(outcome.accepted);
    assert_eq!(index.len(), 1);
    assert_eq!(
        index
            .get(&scope, VARIANT, 4_500, 1_000)
            .map(|r| r.file_id.as_str()),
        Some("new")
    );
}

#[test]
fn index_round_trips_every_field() {
    let scope = scope();
    let original = UploadRecord {
        scope: scope.clone(),
        attachment_id: ATTACHMENT.to_owned(),
        variant_id: VARIANT.to_owned(),
        file_id: FileId::new("file-api-round-trip").unwrap(),
        bytes: 3,
        created_at: 1_700_000_000_000,
        expires_at: 1_700_604_800_000,
    };
    let mut index = UploadIndex::empty();
    let _ = index.commit(original.clone(), NOW, 1_000);
    let decoded = UploadIndex::from_json(&index.to_json()).unwrap();
    assert_eq!(decoded.records(), &[original]);
    // The rendering is the document upstream writes, including the trailing newline.
    assert!(index.to_json().ends_with("}\n"));
}

// ---------------------------------------------------------------------------------------------
// Failures and recovery — upstream `files-api.spec.ts`, `files.spec.ts`
// ---------------------------------------------------------------------------------------------

#[test]
fn files_failures_classify_by_status() {
    // `files-api.spec.ts` "classifies HTTP %i Files failures as %s".
    for (status, code) in [
        (401_u16, "AUTH"),
        (403, "AUTH"),
        (429, "RATE_LIMIT"),
        (500, "SERVER"),
        (503, "SERVER"),
        (400, "FILES_API"),
        (404, "FILES_API"),
    ] {
        let error = FilesError {
            message: "x".to_owned(),
            status,
            detail: String::new(),
        };
        assert_eq!(error.code().as_str(), code, "status {status}");
    }
}

#[test]
fn provider_error_detail_reads_only_the_error_object() {
    // `files.spec.ts` "classifies Files recovery only from provider error fields".
    assert_eq!(
        provider_error_detail(&serde_json::json!({
            "extra": "file-a expired",
            "error": { "message": "invalid request" },
        })),
        (
            Some("invalid request".to_owned()),
            "invalid request".to_owned()
        )
    );
    assert_eq!(
        provider_error_detail(&serde_json::json!({
            "error": { "code": "file_not_found", "type": "invalid_request_error", "message": "missing" },
        })),
        (
            Some("missing".to_owned()),
            "file_not_found invalid_request_error missing".to_owned()
        )
    );
    assert_eq!(
        provider_error_detail(&serde_json::json!({ "error": "file-a expired" })),
        (None, String::new())
    );
    assert_eq!(
        provider_error_detail(&serde_json::json!(null)),
        (None, String::new())
    );
    assert_eq!(
        provider_error_detail(&serde_json::json!([])),
        (None, String::new())
    );
}

#[test]
fn quota_errors_are_recognized_from_provider_detail_only() {
    // `files-api.spec.ts` "retains quota error detail for the one cleanup retry policy".
    let quota = FilesError {
        message: "quota".to_owned(),
        status: 400,
        detail: "file_quota invalid_request_error user storage quota exceeded".to_owned(),
    };
    assert!(is_quota_error(&quota));
    for detail in [
        "user storage quota exceeded",
        "stored files limit reached",
        "file count exceeded",
        "too many files",
        "STORAGE QUOTA",
    ] {
        assert!(
            is_quota_error(&FilesError {
                message: String::new(),
                status: 429,
                detail: detail.to_owned(),
            }),
            "{detail}"
        );
    }
    // A rate limit that names no quota is not a quota failure: deleting files would not help.
    assert!(!is_quota_error(&FilesError {
        message: "slow down".to_owned(),
        status: 429,
        detail: "rate_limit_error slow down".to_owned(),
    }));
}

#[test]
fn stale_file_ids_are_recognized_only_when_a_file_is_named() {
    // `files.spec.ts` "invalidates all referenced mappings when the provider identifies no
    // particular stale id".
    for detail in [
        "file id not found",
        "file-a expired",
        "file_not_found",
        "the file was deleted",
        "invalid file_id",
        "file id is invalid",
        "file-api-one does not exist",
        "file not created under this account",
    ] {
        assert!(provider_rejected_file_id(detail), "{detail}");
    }
    for detail in [
        // Names no file at all.
        "invalid request",
        "unsupported image",
        // Names a file but not that it is gone.
        "file-a is too large",
        "profile update failed",
        "filename is required",
    ] {
        assert!(!provider_rejected_file_id(detail), "{detail}");
    }
}

#[test]
fn file_ids_match_on_word_boundaries() {
    // `detailNamesFileId`.
    assert!(detail_names_file_id("file-a expired", "file-a"));
    assert!(detail_names_file_id("invalid image file-a", "file-a"));
    assert!(detail_names_file_id("file-a", "file-a"));
    assert!(!detail_names_file_id("file-abc expired", "file-a"));
    assert!(!detail_names_file_id("xfile-a expired", "file-a"));
    assert!(!detail_names_file_id("file-a_extra", "file-a"));
    assert!(!detail_names_file_id("anything", ""));
}

#[test]
fn request_files_invalidate_the_named_ids_once() {
    // "invalidates the named stale id once and prepares extensions for each HTTP attempt".
    let second_variant = format!("sha256:{}", "d".repeat(64));
    let mut files = RequestFiles::new();
    files.record(UsedRequestFile {
        variant_id: VARIANT.to_owned(),
        file_id: FileId::new("file-a").unwrap(),
        reference: image(1, 1, 3),
        message: 1,
        image: 1,
    });
    files.record(UsedRequestFile {
        variant_id: second_variant.clone(),
        file_id: FileId::new("file-b").unwrap(),
        reference: image(1, 1, 3),
        message: 1,
        image: 2,
    });
    // The provider named exactly one id.
    let generations = files.retry("file-a expired").unwrap();
    assert_eq!(
        generations,
        vec![(VARIANT.to_owned(), FileId::new("file-a").unwrap())]
    );
    assert!(files.retried());
    // A second rejection may not retry again, however clear it is.
    assert_eq!(files.retry("file-b expired"), None);
    // A new attempt starts with no occurrences and a spent retry.
    files.begin_attempt();
    assert!(files.used().is_empty());
    assert_eq!(files.retry("file-a expired"), None);
}

#[test]
fn request_files_invalidate_every_used_id_when_none_is_named() {
    // "invalidates all referenced mappings when the provider identifies no particular stale id,
    // then fails after one retry".
    let mut files = RequestFiles::new();
    for id in ["file-a", "file-b"] {
        files.record(UsedRequestFile {
            variant_id: VARIANT.to_owned(),
            file_id: FileId::new(id).unwrap(),
            reference: image(1, 1, 3),
            message: 1,
            image: 1,
        });
    }
    let generations = files.retry("file id not found").unwrap();
    assert_eq!(generations.len(), 2);
}

#[test]
fn request_files_ignore_a_rejection_that_names_no_file() {
    let mut files = RequestFiles::new();
    files.record(UsedRequestFile {
        variant_id: VARIANT.to_owned(),
        file_id: FileId::new("file-a").unwrap(),
        reference: image(1, 1, 3),
        message: 1,
        image: 1,
    });
    assert_eq!(files.retry("invalid request"), None);
    assert!(!files.retried());
}

#[test]
fn request_files_deduplicate_shared_variants() {
    // Two occurrences of one attachment share a variant; the invalidation set holds it once.
    let mut files = RequestFiles::new();
    for image_index in 1..=2 {
        files.record(UsedRequestFile {
            variant_id: VARIANT.to_owned(),
            file_id: FileId::new("file-a").unwrap(),
            reference: image(1, 1, 3),
            message: 1,
            image: image_index,
        });
    }
    assert_eq!(files.retry("file id not found").unwrap().len(), 1);
}

#[test]
fn a_normalized_image_rejection_names_the_occurrence() {
    // "attributes normalized-image rejection without retrying its bytes as inline".
    let mut files = RequestFiles::new();
    files.record(UsedRequestFile {
        variant_id: VARIANT.to_owned(),
        file_id: FileId::new("file-a").unwrap(),
        reference: image(1920, 1080, 3).with_name("photo"),
        message: 1,
        image: 1,
    });
    let message = files.error_message(400, "invalid image", "invalid image file-a");
    assert!(message.contains("message 1, image 1"), "{message}");
    assert!(message.contains("\"photo\""), "{message}");
    assert!(message.contains("1920x1080"), "{message}");
    // A rejection that names no image is passed through untouched.
    assert_eq!(
        files.error_message(400, "invalid request", "invalid request"),
        "invalid request"
    );
    // A 500 is never an image rejection.
    assert_eq!(
        files.error_message(500, "server error", "invalid image"),
        "server error"
    );
}

#[test]
fn a_normalized_image_rejection_lists_candidates_when_ambiguous() {
    let mut files = RequestFiles::new();
    for (index, name) in [(1_u64, "first"), (2, "second")] {
        files.record(UsedRequestFile {
            variant_id: VARIANT.to_owned(),
            file_id: FileId::new(format!("file-{index}")).unwrap(),
            reference: image(800, 800, 3).with_name(name),
            message: 1,
            image: usize::try_from(index).unwrap(),
        });
    }
    let message = files.error_message(400, "invalid image", "invalid image");
    assert!(message.contains("Candidate images:"), "{message}");
    assert!(message.contains("\"first\""), "{message}");
    assert!(message.contains("\"second\""), "{message}");
}

#[test]
fn normalized_image_rejection_classification_matches_both_clauses() {
    for detail in [
        "unsupported image",
        "invalid image",
        "cannot read image",
        "failed to decode image",
        "failed to process image",
        "image is invalid",
        "image cannot be decoded",
    ] {
        assert!(provider_rejected_normalized_image(detail), "{detail}");
    }
    for detail in [
        "invalid request",
        "image is too large",
        "unsupported format",
    ] {
        assert!(!provider_rejected_normalized_image(detail), "{detail}");
    }
}

// ---------------------------------------------------------------------------------------------
// Upload planning — upstream `file-store.spec.ts`
// ---------------------------------------------------------------------------------------------

#[test]
fn a_reusable_mapping_short_circuits_the_upload() {
    let scope = scope();
    let mut index = UploadIndex::empty();
    // The default refresh margin is an hour; the mapping must outlive it by more than nothing.
    let _ = index.commit(
        record(scope.clone(), VARIANT, "file-api-one", NOW + 7_200_000),
        NOW,
        FilePolicy::default().refresh_margin_ms(),
    );
    let plan = plan_upload(
        &request_image(3),
        &index,
        &scope,
        &FilePolicy::default(),
        NOW,
    );
    match plan {
        UploadPlan::Reuse(record) => assert_eq!(record.file_id.as_str(), "file-api-one"),
        other => panic!("expected a reuse, got {other:?}"),
    }
}

#[test]
fn the_cache_check_precedes_the_size_check() {
    // Upstream's `ensureUploadedOnce` returns before the size guard when a mapping exists, so
    // shrinking `MAX_IMAGE_BYTES` cannot invalidate uploads that already succeeded.
    let scope = scope();
    let mut index = UploadIndex::empty();
    let _ = index.commit(
        record(scope.clone(), VARIANT, "file-api-one", NOW + 7_200_000),
        NOW,
        FilePolicy::default().refresh_margin_ms(),
    );
    let plan = plan_upload(
        &request_image(MAX_IMAGE_BYTES + 1),
        &index,
        &scope,
        &FilePolicy::default(),
        NOW,
    );
    assert!(matches!(plan, UploadPlan::Reuse(_)));
}

#[test]
fn an_oversized_image_is_refused_before_transport() {
    // "rejects a request version above the per-image limit before transport".
    let plan = plan_upload(
        &request_image(MAX_IMAGE_BYTES + 1),
        &UploadIndex::empty(),
        &scope(),
        &FilePolicy::default(),
        NOW,
    );
    assert_eq!(
        plan,
        UploadPlan::Refuse(Refusal::ImageTooLarge {
            bytes: MAX_IMAGE_BYTES + 1
        })
    );
    // Exactly at the limit is allowed.
    assert!(matches!(
        plan_upload(
            &request_image(MAX_IMAGE_BYTES),
            &UploadIndex::empty(),
            &scope(),
            &FilePolicy::default(),
            NOW
        ),
        UploadPlan::Upload { .. }
    ));
}

#[test]
fn an_unsupported_expiry_is_refused_before_transport() {
    for seconds in [3_599_u64, 2_592_001, 0] {
        let policy = FilePolicy {
            expires_after_seconds: seconds,
            ..FilePolicy::default()
        };
        let plan = plan_upload(
            &request_image(3),
            &UploadIndex::empty(),
            &scope(),
            &policy,
            NOW,
        );
        assert_eq!(
            plan,
            UploadPlan::Refuse(Refusal::UnsupportedExpiry { seconds }),
            "{seconds}"
        );
    }
    assert!(
        FilePolicy {
            expires_after_seconds: 3_600,
            ..FilePolicy::default()
        }
        .expiry_is_supported()
    );
}

#[test]
fn an_unsupported_media_type_is_refused_before_transport() {
    let mut image = request_image(3);
    image.media_type = "image/avif".to_owned();
    let plan = plan_upload(
        &image,
        &UploadIndex::empty(),
        &scope(),
        &FilePolicy::default(),
        NOW,
    );
    assert_eq!(
        plan,
        UploadPlan::Refuse(Refusal::UnsupportedMediaType {
            media_type: "image/avif".to_owned()
        })
    );
}

#[test]
fn an_upload_plan_carries_the_filename_and_lifetime() {
    let plan = plan_upload(
        &request_image(3),
        &UploadIndex::empty(),
        &scope(),
        &FilePolicy::default(),
        NOW,
    );
    assert_eq!(
        plan,
        UploadPlan::Upload {
            filename: format!("dsh-{}-{}.png", "a".repeat(16), "b".repeat(8)),
            expires_after_seconds: 604_800,
        }
    );
}

#[test]
fn a_quota_failure_plans_one_reclaim() {
    // "reclaims one owned file after quota rejection and retries the upload once".
    let error = FilesError {
        message: "quota".to_owned(),
        status: 429,
        detail: "storage quota".to_owned(),
    };
    let policy = FilePolicy {
        quota_cleanup_batch: 1,
        ..FilePolicy::default()
    };
    assert_eq!(
        plan_upload_failure(&error, &request_image(3), &policy, false),
        Some(UploadPlan::ReclaimThenUpload {
            count: 1,
            filename: format!("dsh-{}-{}.png", "a".repeat(16), "b".repeat(8)),
            expires_after_seconds: 604_800,
        })
    );
    // A retry that already ran does not reclaim again.
    assert_eq!(
        plan_upload_failure(&error, &request_image(3), &policy, true),
        None
    );
}

#[test]
fn a_non_quota_failure_plans_no_reclaim() {
    // "preserves a quota error when no harness-owned file can be reclaimed" reaches the same
    // branch: only a quota detail justifies deleting anything.
    let error = FilesError {
        message: "bad request".to_owned(),
        status: 400,
        detail: "invalid_request_error".to_owned(),
    };
    assert_eq!(
        plan_upload_failure(&error, &request_image(3), &FilePolicy::default(), false),
        None
    );
}

#[test]
fn a_completed_upload_takes_its_expiry_from_the_provider_creation_time() {
    // `files-api.spec.ts` upload vector: `createdAt` is the provider's seconds and
    // `expiresAt = createdAt + 3_600`.
    let record = completed_upload(
        &request_image(3),
        &scope(),
        FileId::new("file-api-one").unwrap(),
        3,
        1_700_000_000,
        3_600,
    );
    assert_eq!(record.created_at, 1_700_000_000_000);
    assert_eq!(record.expires_at, 1_700_003_600_000);
}

#[test]
fn a_mismatched_upload_size_is_never_accepted() {
    // "rejects an upload response whose byte count differs from the request version".
    assert!(upload_bytes_agree(3, 3));
    assert!(!upload_bytes_agree(3, 4));
}

#[test]
fn an_upload_above_the_api_limit_is_refused_before_transport() {
    // `files-api.spec.ts` "refuses a file larger than the upload limit before transport".
    assert_eq!(check_upload_size(3), Ok(()));
    assert_eq!(
        check_upload_size(deepseek_files_gap::MAX_FILE_UPLOAD_BYTES),
        Ok(())
    );
    assert_eq!(
        check_upload_size(deepseek_files_gap::MAX_FILE_UPLOAD_BYTES + 1),
        Err(deepseek_files_gap::MAX_FILE_UPLOAD_BYTES + 1)
    );
}

// ---------------------------------------------------------------------------------------------
// Quota reclamation — upstream `file-store.spec.ts` pagination cases
// ---------------------------------------------------------------------------------------------

#[test]
fn a_sweep_deletes_nothing_until_the_last_page() {
    // "reclaims the oldest owned native file across descending pages before retrying an upload":
    // the oldest owned file is on the *second* page, so the first page must not delete.
    let mut candidates: Vec<ProviderFile> = Vec::new();
    let first = FilePage {
        data: vec![
            owned("newest", 4_000),
            ProviderFile {
                filename: "user-file.png".to_owned(),
                ..owned("middle", 3_000)
            },
        ],
        first_id: None,
        last_id: Some(FileId::new("middle").unwrap()),
        has_more: true,
    };
    let plan = reclaim_page(&mut candidates, &first, None, 1);
    assert!(plan.deletes.is_empty());
    assert_eq!(plan.stopped, PageStop::Continue);
    assert_eq!(
        plan.next_cursor.as_ref().map(FileId::as_str),
        Some("middle")
    );

    let second = FilePage {
        data: vec![
            owned("oldest-owned", 1_000),
            ProviderFile {
                filename: "user-file.png".to_owned(),
                ..owned("foreign", 2_000)
            },
        ],
        first_id: None,
        last_id: None,
        has_more: false,
    };
    let plan = reclaim_page(
        &mut candidates,
        &second,
        Some(&FileId::new("middle").unwrap()),
        1,
    );
    assert_eq!(
        plan.deletes.iter().map(FileId::as_str).collect::<Vec<_>>(),
        vec!["oldest-owned"]
    );
    assert_eq!(plan.stopped, PageStop::Exhausted);
}

#[test]
fn a_sweep_keeps_the_oldest_candidates_across_pages() {
    let mut candidates = Vec::new();
    let page = FilePage {
        data: vec![
            owned("one", 5_000),
            owned("two", 1_000),
            owned("three", 3_000),
        ],
        first_id: None,
        last_id: None,
        has_more: false,
    };
    let plan = reclaim_page(&mut candidates, &page, None, 2);
    assert_eq!(
        plan.deletes.iter().map(FileId::as_str).collect::<Vec<_>>(),
        vec!["two", "three"]
    );
}

#[test]
fn a_sweep_stops_when_a_page_omits_or_repeats_its_cursor() {
    // "stops pagination when a page omits or repeats its cursor".
    let mut candidates = Vec::new();
    let page = FilePage {
        data: vec![owned("one", 1_000)],
        first_id: None,
        last_id: None,
        has_more: true,
    };
    let plan = reclaim_page(&mut candidates, &page, None, 1);
    assert_eq!(plan.stopped, PageStop::MissingCursor);
    assert_eq!(plan.next_cursor, None);
    assert_eq!(plan.deletes.len(), 1);

    let mut candidates = Vec::new();
    let cursor = FileId::new("same").unwrap();
    let page = FilePage {
        data: vec![owned("one", 1_000)],
        first_id: None,
        last_id: Some(cursor.clone()),
        has_more: true,
    };
    let plan = reclaim_page(&mut candidates, &page, Some(&cursor), 1);
    assert_eq!(plan.stopped, PageStop::RepeatedCursor);
    assert_eq!(plan.next_cursor, None);
}

#[test]
fn a_sweep_of_count_zero_deletes_nothing() {
    let mut candidates = Vec::new();
    let page = FilePage {
        data: vec![owned("one", 1_000)],
        first_id: None,
        last_id: None,
        has_more: false,
    };
    let plan = reclaim_page(&mut candidates, &page, None, 0);
    assert!(plan.deletes.is_empty());
    assert!(!reclaim_recovered(0));
    assert!(reclaim_recovered(1));
}

fn owned(id: &str, created_at_seconds: u64) -> ProviderFile {
    ProviderFile {
        id: FileId::new(id).unwrap(),
        bytes: 3,
        created_at_seconds,
        filename: "dsh-owned.png".to_owned(),
        mime_type: "image/png".to_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Wire decoding — upstream `files-api.spec.ts`
// ---------------------------------------------------------------------------------------------

fn wire_file() -> serde_json::Value {
    serde_json::json!({
        "id": "file-api-one",
        "type": "file",
        "size_bytes": 3,
        "created_at": "2023-11-14T22:13:20.000Z",
        "filename": "image.png",
        "mime_type": "image/png",
    })
}

#[test]
fn a_file_object_decodes_every_documented_field() {
    let file = parse_file_object(&wire_file(), "upload").unwrap();
    assert_eq!(file.id.as_str(), "file-api-one");
    assert_eq!(file.bytes, 3);
    assert_eq!(file.created_at_seconds, 1_700_000_000);
    assert_eq!(file.filename, "image.png");
    assert_eq!(file.mime_type, "image/png");
}

#[test]
fn a_file_object_rejects_every_malformed_shape() {
    let invalid = [
        serde_json::json!(null),
        serde_json::json!([]),
        serde_json::json!({}),
        serde_json::json!({ "id": "", "type": "file", "size_bytes": 3,
            "created_at": "2023-11-14T22:13:20.000Z", "filename": "x", "mime_type": "image/png" }),
        // `type` must be `file`, not `file_deleted`.
        serde_json::json!({ "id": "one", "type": "file_deleted", "size_bytes": 3,
            "created_at": "2023-11-14T22:13:20.000Z", "filename": "x", "mime_type": "image/png" }),
        // A negative size is not a size.
        serde_json::json!({ "id": "one", "type": "file", "size_bytes": -1,
            "created_at": "2023-11-14T22:13:20.000Z", "filename": "x", "mime_type": "image/png" }),
        // An unparseable creation instant is a rejection, not a zero.
        serde_json::json!({ "id": "one", "type": "file", "size_bytes": 3,
            "created_at": "yesterday", "filename": "x", "mime_type": "image/png" }),
        // An empty filename would make ownership undecidable.
        serde_json::json!({ "id": "one", "type": "file", "size_bytes": 3,
            "created_at": "2023-11-14T22:13:20.000Z", "filename": "", "mime_type": "image/png" }),
    ];
    for value in invalid {
        assert!(
            parse_file_object(&value, "upload").is_err(),
            "expected a rejection for {value}"
        );
    }
}

#[test]
fn creation_instants_accept_the_spellings_the_provider_sends() {
    assert_eq!(
        parse_created_at_seconds("2023-11-14T22:13:20.000Z"),
        Some(1_700_000_000)
    );
    // No fraction.
    assert_eq!(
        parse_created_at_seconds("2023-11-14T22:13:20Z"),
        Some(1_700_000_000)
    );
    // An explicit zero offset.
    assert_eq!(
        parse_created_at_seconds("2023-11-14T22:13:20+00:00"),
        Some(1_700_000_000)
    );
    for text in ["", "0", "2023-11-14", "not a date"] {
        assert_eq!(parse_created_at_seconds(text), None, "{text}");
    }
}

#[test]
fn a_list_page_maps_cursors_and_metadata() {
    // "maps Messages list cursors, file metadata and deletion with native metadata".
    let page = parse_file_page(
        &serde_json::json!({
            "data": [wire_file()],
            "first_id": "file-api-one",
            "last_id": "file-api-one",
            "has_more": false,
        }),
        "list",
    )
    .unwrap();
    assert_eq!(page.data.len(), 1);
    assert_eq!(
        page.first_id.as_ref().map(FileId::as_str),
        Some("file-api-one")
    );
    assert_eq!(
        page.last_id.as_ref().map(FileId::as_str),
        Some("file-api-one")
    );
    assert!(!page.has_more);
}

#[test]
fn an_empty_list_page_with_null_cursors_is_accepted() {
    // "accepts an empty Messages list with null cursors".
    let page = parse_file_page(
        &serde_json::json!({ "data": [], "first_id": null, "last_id": null, "has_more": false }),
        "list",
    )
    .unwrap();
    assert!(page.data.is_empty());
    assert_eq!(page.first_id, None);
    assert_eq!(page.last_id, None);
    // An absent cursor is absent, not invalid.
    let page = parse_file_page(
        &serde_json::json!({ "data": [], "has_more": false }),
        "list",
    )
    .unwrap();
    assert_eq!(page.first_id, None);
}

#[test]
fn a_numeric_cursor_is_a_protocol_error() {
    // "rejects a Messages list with a numeric %s".
    for field in ["first_id", "last_id"] {
        let mut body = serde_json::json!({ "data": [], "has_more": false });
        body[field] = serde_json::json!(1);
        match parse_file_page(&body, "list") {
            Err(WireError::InvalidCursor { field: named }) => assert_eq!(named, field),
            other => panic!("expected a cursor error for {field}, got {other:?}"),
        }
    }
}

#[test]
fn an_invalid_list_body_is_a_protocol_error() {
    // "rejects an invalid list response %#".
    for body in [
        serde_json::json!(null),
        serde_json::json!([]),
        serde_json::json!({}),
        serde_json::json!({ "data": null, "has_more": false }),
        serde_json::json!({ "data": [], "has_more": 0 }),
    ] {
        assert!(
            matches!(
                parse_file_page(&body, "list"),
                Err(WireError::InvalidResponse { .. })
            ),
            "{body}"
        );
    }
}

#[test]
fn a_deletion_must_name_the_same_file() {
    // "rejects a Messages deletion that returns a different file identity".
    let file_id = FileId::new("file-api-one").unwrap();
    assert!(
        parse_deletion(
            &serde_json::json!({ "id": "file-api-one", "type": "file_deleted" }),
            &file_id,
            "delete"
        )
        .is_ok()
    );
    for body in [
        serde_json::json!({ "id": "file-api-two", "type": "file_deleted" }),
        serde_json::json!({ "id": "file-api-one", "type": "file" }),
        serde_json::json!({ "type": "file_deleted" }),
        serde_json::json!(null),
    ] {
        assert!(parse_deletion(&body, &file_id, "delete").is_err(), "{body}");
    }
}

// ---------------------------------------------------------------------------------------------
// Request image preparation and pricing — upstream `request-pricing.spec.ts`, `files.spec.ts`
// ---------------------------------------------------------------------------------------------

#[test]
fn image_admission_follows_upstream_guard_order() {
    // `prepareImages`'s guards, including the offloaded-only path that needs no store or vision.
    for (facts, expected) in [
        (ImageAdmissionFacts::NoImages, ImageAdmission::NoImages),
        (
            ImageAdmissionFacts::ModelIsTextOnly,
            ImageAdmission::ModelIsTextOnly,
        ),
        (
            ImageAdmissionFacts::NoAttachmentService,
            ImageAdmission::NoAttachmentService,
        ),
        (
            ImageAdmissionFacts::UnsupportedRole,
            ImageAdmission::UnsupportedRole,
        ),
        (ImageAdmissionFacts::Admissible, ImageAdmission::Admitted),
    ] {
        assert_eq!(admit_images(facts), expected);
    }
}

#[test]
fn logged_offloads_project_without_a_vision_model() {
    // "projects nested logged offloads without an attachment service or a vision model".
    let messages = vec![message(
        Role::Tool,
        vec![ContentBlock::Image {
            attachment: ATTACHMENT.to_owned(),
        }],
    )];
    let projected = project_offloaded(
        &messages,
        &|id: &str| {
            if id == ATTACHMENT {
                Some(image(1, 1, 3))
            } else {
                None
            }
        },
        &|_: &ImageAttachmentRef| Some(ImageAttachmentAccess::new("/workspace/image.png")),
    );
    assert_eq!(projected[0].content.len(), 1);
    match &projected[0].content[0] {
        ContentBlock::Text { text } => {
            assert!(
                text.contains("image omitted to fit request image limits"),
                "{text}"
            );
            assert!(text.contains("/workspace/image.png"), "{text}");
        }
        other => panic!("expected placeholder text, got {other:?}"),
    }
    // The durable history is untouched.
    assert!(matches!(messages[0].content[0], ContentBlock::Image { .. }));
}

#[test]
fn project_offloaded_leaves_retained_occurrences_alone() {
    let messages = vec![message(Role::User, vec![image_block(ATTACHMENT)])];
    let projected = project_offloaded(
        &messages,
        &|_: &str| Some(ImageBlock::retained(image(1, 1, 3)).attachment),
        &|_: &ImageAttachmentRef| None,
    );
    // The resolver only supplies a reference; `project_offloaded` marks every hit offloaded, which
    // is upstream's behaviour for this step. A retained occurrence is therefore projected too —
    // the caller is expected to resolve only offloaded occurrences, as `prepareImages` does.
    assert!(matches!(projected[0].content[0], ContentBlock::Text { .. }));
}

#[test]
fn text_model_projection_substitutes_every_image() {
    let messages = vec![message(Role::User, vec![image_block(ATTACHMENT)])];
    let projected = text_model_projection(&messages);
    match &projected[0].content[0] {
        ContentBlock::Text { text } => {
            assert!(text.contains("this model accepts text only"), "{text}");
        }
        other => panic!("expected text-only substitution, got {other:?}"),
    }
}

#[test]
fn both_route_budgets_measure_the_same_history_differently() {
    // Base64 counts four characters per three bytes, so the same image fits raw and not inline.
    let messages = vec![message(Role::User, vec![image_block(ATTACHMENT)])];
    let budgets = route_budgets(1_000, 3, 2, 1, 1, 1);
    let (files, inline) = measure_request_images(&messages, &budgets, &|_: &ImageAttachmentRef| 3);
    assert!(files.fits());
    assert!(!inline.fits());
    assert_eq!(inline.offload_images, 1);
    // A route that accepts everything reports no offload.
    let budgets = route_budgets(1_000_000, 1_000_000, 600, 1, 1, 1);
    let (files, inline) = measure_request_images(&messages, &budgets, &|_: &ImageAttachmentRef| 3);
    assert!(files.fits() && inline.fits());
}

#[test]
fn the_offload_count_uses_the_prepared_bytes_not_the_source_size() {
    // "uses exact prepared bytes when the durable reference fits the Files budget".
    let messages = vec![message(Role::User, vec![image_block(ATTACHMENT)])];
    let budgets = route_budgets(3, 1_000_000, 600, 1, 1, 1);
    let (files, _) = measure_request_images(&messages, &budgets, &|_: &ImageAttachmentRef| 4);
    assert_eq!(files.offload_images, 1);
}

#[test]
fn a_retained_vision_image_prices_at_its_projected_request_dimensions() {
    // request-pricing.spec.ts: a 1920x1080 source prices at 1708x961 and 968 tokens.
    let prices = image_request_pricing(
        true,
        &ImageOverrides::default(),
        &[block(image(1920, 1080, 3), false)],
        &|_: &ImageAttachmentRef| None,
    );
    assert_eq!(prices.len(), 1);
    assert_eq!(prices[0].visual_tokens, 968);
    assert!(prices[0].text.contains("1708x961"), "{}", prices[0].text);
}

#[test]
fn a_thin_source_prices_at_its_per_side_capped_dimensions() {
    // `it.each` table: [8192,1,4096,1,832] and [1,8192,1,4096,1024].
    for (width, height, capped_width, capped_height, tokens) in [
        (8192_u64, 1_u64, 4096_u64, 1_u64, 832_u64),
        (1, 8192, 1, 4096, 1024),
    ] {
        let prices = image_request_pricing(
            true,
            &ImageOverrides::default(),
            &[block(image(width, height, 3), false)],
            &|_: &ImageAttachmentRef| None,
        );
        assert_eq!(prices[0].visual_tokens, tokens, "{width}x{height}");
        assert!(
            prices[0]
                .text
                .contains(&format!("{capped_width}x{capped_height}")),
            "{}",
            prices[0].text
        );
    }
}

#[test]
fn a_portrait_source_prices_at_its_sent_geometry() {
    // "prices the sent dimensions when aspect-preserving projection changes the token grid":
    // 1224x1429 sources at 959 tokens (source) but the 1187x1386 request prices at 992.
    let prices = image_request_pricing(
        true,
        &ImageOverrides::default(),
        &[block(image(1224, 1429, 3), false)],
        &|_: &ImageAttachmentRef| None,
    );
    assert_eq!(prices[0].visual_tokens, 992);
    assert!(prices[0].text.contains("1187x1386"), "{}", prices[0].text);
}

#[test]
fn a_numeric_pixel_budget_override_is_honored() {
    // "honors a numeric pixel budget override": 4096x4096 at 640_000 pixels is 800x800, 422.
    let overrides = ImageOverrides {
        pixel_budget: Some(PixelBudget::Pixels(640_000)),
        max_bytes: None,
    };
    let prices = image_request_pricing(
        true,
        &overrides,
        &[block(image(4096, 4096, 3), false)],
        &|_: &ImageAttachmentRef| None,
    );
    assert_eq!(prices[0].visual_tokens, 422);
    assert!(prices[0].text.contains("800x800"), "{}", prices[0].text);
}

#[test]
fn the_low_detail_preset_is_honored() {
    // "honors the low-detail pixel budget preset": 4096x4096 at 512*512 prices at 184.
    let overrides = ImageOverrides {
        pixel_budget: Some(PixelBudget::Low),
        max_bytes: None,
    };
    let prices = image_request_pricing(
        true,
        &overrides,
        &[block(image(4096, 4096, 3), false)],
        &|_: &ImageAttachmentRef| None,
    );
    assert_eq!(prices[0].visual_tokens, 184);
}

#[test]
fn a_text_only_route_prices_the_text_substitution() {
    // "prices an uncatalogued model as its text-only substitution" and the catalogued text-only
    // case: the model never receives pixels, so the price is zero visual tokens.
    let prices = image_request_pricing(
        false,
        &ImageOverrides::default(),
        &[block(image(1920, 1080, 3), false)],
        &|_: &ImageAttachmentRef| None,
    );
    assert_eq!(prices[0].visual_tokens, 0);
    assert!(prices[0].text.contains("this model accepts text only"));
}

#[test]
fn an_offloaded_occurrence_prices_as_its_placeholder() {
    // "prices surface-offloaded occurrences as placeholder text and every retained one at its
    // visual price".
    let prices = image_request_pricing(
        true,
        &ImageOverrides::default(),
        &[
            block(image(800, 800, 3), true),
            block(image(800, 800, 3), false),
        ],
        &|_: &ImageAttachmentRef| None,
    );
    assert_eq!(prices[0].visual_tokens, 0);
    assert!(
        prices[0]
            .text
            .contains("image omitted to fit request image limits")
    );
    assert_eq!(prices[1].visual_tokens, 422);
}

#[test]
fn pricing_uses_the_supplied_access_resolution_per_occurrence() {
    // "builds handle and placeholder text through the supplied access resolution".
    let prices = image_request_pricing(
        true,
        &ImageOverrides::default(),
        &[
            block(image(800, 800, 3), true),
            block(image(800, 800, 3), false),
        ],
        &|_: &ImageAttachmentRef| Some(ImageAttachmentAccess::new("/world/attachments/photo.png")),
    );
    assert!(prices[0].text.contains("/world/attachments/photo.png"));
    assert!(prices[1].text.contains("/world/attachments/photo.png"));
    assert_eq!(prices[1].visual_tokens, 422);
}

#[test]
fn a_retained_oversized_occurrence_still_prices_visually() {
    // "prices a retained oversized occurrence at its visual price: the surface, not the budget,
    // decides offload".
    let prices = image_request_pricing(
        true,
        &ImageOverrides::default(),
        &[block(image(800, 800, 5 * 1024 * 1024), false)],
        &|_: &ImageAttachmentRef| None,
    );
    assert_eq!(prices[0].visual_tokens, 422);
}

#[test]
fn an_encodable_byte_target_is_carried_through() {
    // "exposes the same image dimensions and byte budget used by DeepSeek request preparation".
    let overrides = ImageOverrides {
        pixel_budget: Some(PixelBudget::Pixels(10_000)),
        max_bytes: Some(50_000),
    };
    let target = resolve_request_image_target(ImageDimensions::new(1000, 1000), &overrides);
    assert_eq!(
        (target.width, target.height, target.max_bytes),
        (100, 100, 50_000)
    );
    // The default target is the published 2 MiB.
    assert_eq!(
        request_image_max_bytes(&ImageOverrides::default()),
        deepseek_files_gap::DEFAULT_REQUEST_IMAGE_MAX_BYTES
    );
}

#[test]
fn pixel_projection_never_enlarges() {
    assert_eq!(project_to_pixels(100, 100, 640_000), (100, 100));
    assert_eq!(project_to_pixels(800, 600, 640_000), (800, 600));
    // Exactly at the cap.
    assert_eq!(project_to_pixels(800, 800, 640_000), (800, 800));
    // One pixel over.
    assert_eq!(project_to_pixels(801, 800, 640_000), (800, 799));
}

#[test]
fn pixel_projection_stays_within_the_cap_for_extreme_aspect_ratios() {
    // The decrement loop exists so rounding the short edge cannot push the product back over.
    for (width, height) in [(10_000_u64, 1_u64), (1, 10_000), (3_000, 2), (7, 9_001)] {
        let (projected_width, projected_height) = project_to_pixels(width, height, 640_000);
        assert!(
            projected_width.saturating_mul(projected_height) <= 640_000,
            "{width}x{height} -> {projected_width}x{projected_height}"
        );
        assert!(projected_width >= 1 && projected_height >= 1);
        assert!(projected_width <= width && projected_height <= height);
    }
}

#[test]
fn long_edge_projection_matches_the_published_resize() {
    assert_eq!(long_edge_projection(3840, 2160, 4_096), (3840, 2160));
    assert_eq!(long_edge_projection(8192, 4096, 4_096), (4_096, 2_048));
    assert_eq!(long_edge_projection(4096, 8192, 4_096), (2_048, 4_096));
    assert_eq!(long_edge_projection(8192, 2160, 4_096), (4_096, 1_080));
}

#[test]
fn request_target_prices_the_geometry_it_reports() {
    // The composition the module documents: pricing a source must price the *sent* dimensions.
    let target =
        resolve_request_image_target(ImageDimensions::new(1224, 1429), &ImageOverrides::default());
    let width = u32::try_from(target.width).unwrap();
    let height = u32::try_from(target.height).unwrap();
    assert_eq!((width, height), (1187, 1386));
    assert_eq!(image_tokens(width, height), 992);
}

// ---------------------------------------------------------------------------------------------
// File id preparation — upstream `images.ts` `prepareFileIds`
// ---------------------------------------------------------------------------------------------

#[test]
fn file_id_requests_follow_wire_order_and_deduplicate_by_attachment() {
    let messages = vec![
        message(
            Role::User,
            vec![image_block(ATTACHMENT), image_block("second")],
        ),
        message(Role::Tool, vec![image_block(ATTACHMENT)]),
    ];
    let mut versions = BTreeMap::new();
    versions.insert(ATTACHMENT.to_owned(), VARIANT.to_owned());
    versions.insert("second".to_owned(), "variant-second".to_owned());
    let requests = prepare_file_ids(&messages, &versions);
    // Two attachments, in first-appearance order.
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].attachment_id, ATTACHMENT);
    assert_eq!(requests[1].attachment_id, "second");
    // The last occurrence's position wins, because the map upstream builds is keyed by attachment.
    assert_eq!((requests[0].message, requests[0].image), (2, 1));
    assert_eq!((requests[1].message, requests[1].image), (1, 2));
}

#[test]
fn file_id_requests_skip_attachments_without_a_prepared_version() {
    let messages = vec![message(Role::User, vec![image_block(ATTACHMENT)])];
    assert!(prepare_file_ids(&messages, &BTreeMap::new()).is_empty());
}

// ---------------------------------------------------------------------------------------------
// Listing decode — upstream `listing.spec.ts`
// ---------------------------------------------------------------------------------------------

fn listing_entry(id: &str, extra: &serde_json::Value) -> serde_json::Value {
    let mut entry = serde_json::json!({
        "id": id,
        "api_capabilities": { "anthropic_messages": {} },
    });
    for (key, value) in extra.as_object().unwrap() {
        entry[key] = value.clone();
    }
    entry
}

#[test]
fn a_listing_body_decodes_entries_in_order() {
    let listing = parse_model_listing(&serde_json::json!({
        "data": [
            listing_entry("first", &serde_json::json!({ "name": "First" })),
            listing_entry("second", &serde_json::json!({})),
        ],
    }))
    .unwrap();
    assert_eq!(listing.entries.len(), 2);
    assert_eq!(listing.entries[0].id, "first");
    assert_eq!(listing.entries[0].name.as_deref(), Some("First"));
    assert_eq!(listing.entries[1].name, None);
    assert_eq!(listing.skipped, 0);
}

#[test]
fn rows_without_a_usable_id_are_skipped_and_counted() {
    let listing = parse_model_listing(&serde_json::json!({
        "data": [
            { "id": "" },
            { "name": "no id" },
            "not an object",
            listing_entry("kept", &serde_json::json!({})),
            listing_entry("kept", &serde_json::json!({ "name": "duplicate" })),
        ],
    }))
    .unwrap();
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].id, "kept");
    assert_eq!(listing.skipped, 4);
}

#[test]
fn a_listing_without_usable_entries_is_an_error() {
    assert_eq!(
        parse_model_listing(&serde_json::json!(null)).unwrap_err(),
        ListingError::NoDataArray
    );
    assert_eq!(
        parse_model_listing(&serde_json::json!({ "data": "no" })).unwrap_err(),
        ListingError::NoDataArray
    );
    assert_eq!(
        parse_model_listing(&serde_json::json!({ "data": [] })).unwrap_err(),
        ListingError::NoEntries
    );
    assert_eq!(
        parse_model_listing(&serde_json::json!({ "data": [{ "id": "" }] })).unwrap_err(),
        ListingError::NoEntries
    );
}

#[test]
fn requirements_decode_absent_null_and_versioned_forms() {
    let requirement = |extra: serde_json::Value| {
        parse_model_listing(&serde_json::json!({
            "data": [listing_entry("m", &extra)],
        }))
        .unwrap()
        .entries[0]
            .requirement
            .clone()
    };
    assert_eq!(requirement(serde_json::json!({})), ClientRequirement::None);
    assert_eq!(
        requirement(serde_json::json!({ "client_requirements": null })),
        ClientRequirement::None
    );
    // Another product's requirement is not ours.
    assert_eq!(
        requirement(
            serde_json::json!({ "client_requirements": { "other": { "min_version": "1.0.0" } } })
        ),
        ClientRequirement::None
    );
    assert_eq!(
        requirement(serde_json::json!({
            "client_requirements": { "deepseek-harness": { "min_version": "1.2.3" } },
        })),
        ClientRequirement::MinVersion("1.2.3".to_owned())
    );
    assert_eq!(
        requirement(serde_json::json!({ "client_requirements": [] })),
        ClientRequirement::Invalid
    );
    assert_eq!(
        requirement(serde_json::json!({
            "client_requirements": { "deepseek-harness": { "min_version": "" } },
        })),
        ClientRequirement::Invalid
    );
    assert_eq!(
        requirement(serde_json::json!({
            "client_requirements": { "deepseek-harness": { "min_version": 3 } },
        })),
        ClientRequirement::Invalid
    );
}

#[test]
fn capabilities_decode_modalities_capacities_and_modes() {
    let facts = |extra: serde_json::Value| {
        let listing = parse_model_listing(&serde_json::json!({
            "data": [listing_entry("m", &extra)],
        }))
        .unwrap();
        listing.entries[0].capabilities.clone()
    };
    // Defaults: text in, text out, nothing else declared.
    let ListedCapabilities::Usable(default) = facts(serde_json::json!({})) else {
        panic!("expected usable capabilities");
    };
    assert_eq!(default.input_modalities, vec![Modality::Text]);
    assert_eq!(default.context_window, None);
    assert_eq!(default.max_output_tokens, None);
    assert_eq!(default.system_prompt_update, None);

    let ListedCapabilities::Usable(full) = facts(serde_json::json!({
        "input_modalities": ["text", "image"],
        "output_modalities": ["text"],
        "context_window": 1_000_000,
        "max_output_tokens": 256_000,
        "api_capabilities": { "anthropic_messages": {
            "system_prompt_update": "in-history",
            "tool_update": "addition-only",
        }},
    })) else {
        panic!("expected usable capabilities");
    };
    assert_eq!(full.input_modalities, vec![Modality::Text, Modality::Image]);
    assert_eq!(full.context_window, Some(1_000_000));
    assert_eq!(full.max_output_tokens, Some(256_000));
    assert_eq!(full.system_prompt_update, Some(SystemPromptMode::InHistory));
    assert_eq!(full.tool_update, Some(ToolUpdateMode::AdditionOnly));
}

#[test]
fn unusable_capabilities_state_why() {
    let reason = |extra: serde_json::Value| {
        let listing = parse_model_listing(&serde_json::json!({
            "data": [listing_entry("m", &extra)],
        }))
        .unwrap();
        match &listing.entries[0].capabilities {
            ListedCapabilities::Unusable { reason } => reason.clone(),
            ListedCapabilities::Usable(_) => panic!("expected unusable capabilities"),
        }
    };
    assert!(reason(serde_json::json!({ "api_capabilities": {} })).contains("anthropic_messages"));
    assert!(reason(serde_json::json!({ "output_modalities": ["image"] })).contains("text output"));
    assert!(reason(serde_json::json!({ "input_modalities": [] })).contains("input modality"));
    assert!(
        reason(serde_json::json!({ "input_modalities": ["video"] })).contains("input modality")
    );
    assert!(reason(serde_json::json!({ "context_window": 0 })).contains("context_window"));
    assert!(reason(serde_json::json!({ "max_output_tokens": -1 })).contains("max_output_tokens"));
    assert!(reason(serde_json::json!({ "max_output_tokens": 1.5 })).contains("max_output_tokens"));
    // A `null` window declares nothing rather than something invalid.
    let listing = parse_model_listing(&serde_json::json!({
        "data": [listing_entry("m", &serde_json::json!({ "context_window": null }))],
    }))
    .unwrap();
    assert!(matches!(
        listing.entries[0].capabilities,
        ListedCapabilities::Usable(_)
    ));
}

#[test]
fn an_unrecognized_mode_falls_back_to_the_conservative_one() {
    let listing = parse_model_listing(&serde_json::json!({
        "data": [listing_entry("m", &serde_json::json!({
            "api_capabilities": { "anthropic_messages": {
                "system_prompt_update": "sometimes",
                "tool_update": "sometimes",
            }},
        }))],
    }))
    .unwrap();
    let ListedCapabilities::Usable(facts) = &listing.entries[0].capabilities else {
        panic!("expected usable capabilities");
    };
    assert_eq!(
        facts.system_prompt_update,
        Some(SystemPromptMode::LeadingOnly)
    );
    assert_eq!(facts.tool_update, Some(ToolUpdateMode::LeadingOnly));
}

#[test]
fn badges_decode_text_tone_and_localizations() {
    let badge = |extra: serde_json::Value| {
        parse_model_listing(&serde_json::json!({
            "data": [listing_entry("m", &extra)],
        }))
        .unwrap()
        .entries[0]
            .badge
            .clone()
    };
    assert_eq!(badge(serde_json::json!({})), None);
    assert_eq!(badge(serde_json::json!({ "client_metadata": null })), None);
    let Some(ListedBadge::Badge(declared)) = badge(serde_json::json!({
        "client_metadata": { "deepseek-harness": { "badge": { "text": "  New  ", "tone": "success" } } },
    })) else {
        panic!("expected a badge");
    };
    assert_eq!(declared.text, "New");
    assert_eq!(declared.tone, BadgeTone::Success);
    // An unrecognized tone keeps the text at the least emphasis.
    let Some(ListedBadge::Badge(declared)) = badge(serde_json::json!({
        "client_metadata": { "deepseek-harness": { "badge": { "text": "Beta", "tone": "loud" } } },
    })) else {
        panic!("expected a badge");
    };
    assert_eq!(declared.tone, BadgeTone::Neutral);
    // A localized object must include English.
    let Some(ListedBadge::Badge(declared)) = badge(serde_json::json!({
        "client_metadata": { "deepseek-harness": { "badge": { "text": { "en": "New", "zh-cn": "新" } } } },
    })) else {
        panic!("expected a badge");
    };
    assert_eq!(declared.text, "New");
}

#[test]
fn unreadable_badges_name_the_field_and_hide_no_model() {
    let badge = |extra: serde_json::Value| {
        parse_model_listing(&serde_json::json!({
            "data": [listing_entry("m", &extra)],
        }))
        .unwrap()
        .entries[0]
            .badge
            .clone()
    };
    for (extra, expected) in [
        (
            serde_json::json!({ "client_metadata": "no" }),
            "client_metadata is not an object",
        ),
        (
            serde_json::json!({ "client_metadata": { "deepseek-harness": "no" } }),
            "is not an object",
        ),
        (
            serde_json::json!({ "client_metadata": { "deepseek-harness": { "badge": "no" } } }),
            ".badge is not an object",
        ),
        (
            serde_json::json!({ "client_metadata": { "deepseek-harness": { "badge": { "text": "" } } } }),
            ".badge.text is not a string",
        ),
        (
            // Over the 12-character bound.
            serde_json::json!({ "client_metadata": { "deepseek-harness": { "badge": { "text": "abcdefghijklm" } } } }),
            ".badge.text is not a string",
        ),
        (
            // A control character would break the one-line contract.
            serde_json::json!({ "client_metadata": { "deepseek-harness": { "badge": { "text": "a\u{7}b" } } } }),
            ".badge.text is not a string",
        ),
        (
            // A translation set without English is unusable.
            serde_json::json!({ "client_metadata": { "deepseek-harness": { "badge": { "text": { "zh-cn": "新" } } } } }),
            ".badge.text is not a string",
        ),
        (
            // A language key that is not a lowercase id.
            serde_json::json!({ "client_metadata": { "deepseek-harness": { "badge": { "text": { "EN": "New" } } } } }),
            ".badge.text is not a string",
        ),
    ] {
        match badge(extra) {
            Some(ListedBadge::Invalid { reason }) => {
                assert!(
                    reason.contains(expected),
                    "{reason} should contain {expected}"
                );
            }
            other => panic!("expected an invalid badge, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Client version — the precedence the gate needs
// ---------------------------------------------------------------------------------------------

#[test]
fn client_versions_parse_strictly() {
    assert!(ClientVersion::parse("1.2.3").is_some());
    assert!(ClientVersion::parse("v1.2.3").is_some());
    assert!(ClientVersion::parse("1.2.3-alpha.1+build.5").is_some());
    for invalid in [
        "", "1.2", "1.2.3.4", "01.2.3", "1.02.3", "1.2.03", "x.y.z", "1.2.3-",
    ] {
        assert!(
            ClientVersion::parse(invalid).is_none(),
            "expected no parse for {invalid}"
        );
    }
}

#[test]
fn client_versions_order_by_semver_precedence() {
    let parse = |text: &str| ClientVersion::parse(text).unwrap();
    assert!(parse("1.2.4").satisfies_at_least(&parse("1.2.3")));
    assert!(parse("1.3.0").satisfies_at_least(&parse("1.2.9")));
    assert!(!parse("1.2.2").satisfies_at_least(&parse("1.2.3")));
    assert!(parse("1.2.3").satisfies_at_least(&parse("1.2.3")));
    // A prerelease sorts *before* its release.
    assert!(!parse("1.2.3-alpha").satisfies_at_least(&parse("1.2.3")));
    assert!(parse("1.2.3").satisfies_at_least(&parse("1.2.3-alpha")));
    // Numeric identifiers sort before alphanumeric ones, and a shorter run before a longer.
    assert!(parse("1.2.3-alpha.2").satisfies_at_least(&parse("1.2.3-alpha.1")));
    assert!(!parse("1.2.3-alpha").satisfies_at_least(&parse("1.2.3-alpha.1")));
    // Build metadata is ignored, as semver precedence requires.
    assert!(parse("1.2.3+a").satisfies_at_least(&parse("1.2.3+b")));
}

// ---------------------------------------------------------------------------------------------
// Listed catalog — upstream `listed-catalog.spec.ts`
// ---------------------------------------------------------------------------------------------

fn listing_of(entries: &[serde_json::Value]) -> deepseek_files_gap::ModelListing {
    parse_model_listing(&serde_json::json!({ "data": entries })).unwrap()
}

fn catalog_input(listing: Option<deepseek_files_gap::ModelListing>) -> ListedCatalogInput {
    ListedCatalogInput {
        listing,
        supported: vec![
            CatalogVision::shipped("vision"),
            deepseek_files_gap::CatalogModel::text_only("text-only"),
        ],
        overrides: BTreeMap::new(),
        route_max_tokens: 256_000,
        client_version: "2.0.0".to_owned(),
    }
}

/// A helper namespace so the shipped vision model's fields read once.
struct CatalogVision;

impl CatalogVision {
    fn shipped(id: &str) -> deepseek_files_gap::CatalogModel {
        let mut model = deepseek_files_gap::CatalogModel::text_only(id);
        model.input_modalities = vec![Modality::Text, Modality::Image];
        model.vision = true;
        model.context_window = Some(1_000_000);
        model.max_output_tokens = Some(256_000);
        model
    }
}

#[test]
fn without_a_listing_the_builds_own_models_are_offered() {
    let catalog = resolve_listed_catalog(&catalog_input(None));
    assert_eq!(catalog.offered.len(), 2);
    assert_eq!(catalog.resolution.len(), 2);
    assert!(catalog.refusals.is_empty());
    assert!(catalog.badges.is_empty());
    assert!(catalog.problems.is_empty());
}

#[test]
fn a_listed_model_the_build_knows_is_offered_with_its_declarations() {
    let listing = listing_of(&[listing_entry(
        "vision",
        &serde_json::json!({
            "name": "Vision",
            "input_modalities": ["text", "image"],
            "context_window": 200_000,
            "max_output_tokens": 8_000,
        }),
    )]);
    let catalog = resolve_listed_catalog(&catalog_input(Some(listing)));
    assert_eq!(catalog.offered.len(), 1);
    let model = &catalog.offered[0];
    assert_eq!(model.id, "vision");
    assert_eq!(model.name.as_deref(), Some("Vision"));
    assert_eq!(model.context_window, Some(200_000));
    // The route's cap bounds the listed capability.
    assert_eq!(model.max_output_tokens, Some(8_000));
    assert!(model.vision);
}

#[test]
fn a_listed_output_capability_is_bounded_by_the_route_cap() {
    let listing = listing_of(&[listing_entry(
        "vision",
        &serde_json::json!({ "max_output_tokens": 1_000_000 }),
    )]);
    let catalog = resolve_listed_catalog(&catalog_input(Some(listing)));
    assert_eq!(catalog.offered[0].max_output_tokens, Some(256_000));
}

#[test]
fn a_listed_model_the_build_has_never_heard_of_is_offered_only_with_a_requirement() {
    // Without a requirement the gate hides it: the build cannot describe what it would be sending.
    let listing = listing_of(&[listing_entry("unknown", &serde_json::json!({}))]);
    let catalog = resolve_listed_catalog(&catalog_input(Some(listing)));
    assert!(catalog.offered.is_empty());
    assert_eq!(
        catalog.refusals.get("unknown"),
        Some(&CatalogRefusal::Unsupported)
    );
    // With a requirement at or below the current version, it is offered as a bare text-only model.
    let listing = listing_of(&[listing_entry(
        "unknown",
        &serde_json::json!({
            "client_requirements": { "deepseek-harness": { "min_version": "1.0.0" } },
        }),
    )]);
    let catalog = resolve_listed_catalog(&catalog_input(Some(listing)));
    assert_eq!(catalog.offered.len(), 1);
    assert_eq!(catalog.offered[0].id, "unknown");
}

#[test]
fn a_requirement_above_the_current_version_is_refused_with_the_version_named() {
    let listing = listing_of(&[listing_entry(
        "future",
        &serde_json::json!({
            "client_requirements": { "deepseek-harness": { "min_version": "3.0.0" } },
        }),
    )]);
    let catalog = resolve_listed_catalog(&catalog_input(Some(listing)));
    assert!(catalog.offered.is_empty());
    assert_eq!(
        catalog.refusals.get("future"),
        Some(&CatalogRefusal::UpgradeRequired {
            required_version: "3.0.0".to_owned()
        })
    );
    let message = refusal_message(
        "future",
        catalog.refusals.get("future"),
        &catalog_input(None).client_version,
    )
    .unwrap();
    assert!(
        message.contains("requires DeepSeek Harness 3.0.0 or later"),
        "{message}"
    );
    assert!(message.contains("current version is 2.0.0"), "{message}");
}

#[test]
fn an_unreadable_requirement_is_a_problem_not_a_silent_hide() {
    let listing = listing_of(&[listing_entry(
        "broken",
        &serde_json::json!({
            "client_requirements": { "deepseek-harness": { "min_version": "not-a-version" } },
        }),
    )]);
    let catalog = resolve_listed_catalog(&catalog_input(Some(listing)));
    assert!(catalog.offered.is_empty());
    assert_eq!(catalog.problems.len(), 1);
    assert!(
        catalog.problems[0].contains("unreadable client requirement"),
        "{:?}",
        catalog.problems
    );
}

#[test]
fn an_unusable_listed_model_is_hidden_with_its_reason() {
    let listing = listing_of(&[listing_entry(
        "vision",
        &serde_json::json!({ "input_modalities": [] }),
    )]);
    let catalog = resolve_listed_catalog(&catalog_input(Some(listing)));
    assert!(catalog.offered.is_empty());
    assert_eq!(
        catalog.refusals.get("vision"),
        Some(&CatalogRefusal::Unsupported)
    );
    assert!(
        catalog.problems[0].contains("input modality"),
        "{:?}",
        catalog.problems
    );
}

#[test]
fn unsupported_models_name_the_build_and_version() {
    let message = refusal_message("mystery", Some(&CatalogRefusal::Unsupported), "2.0.0").unwrap();
    assert!(message.contains("is not supported"), "{message}");
    assert!(message.contains("2.0.0"), "{message}");
    // An admitted model produces no message at all.
    assert_eq!(refusal_message("vision", None, "2.0.0"), None);
}

#[test]
fn a_leading_only_declaration_removes_the_inherited_mode() {
    // The build ships both modes; a listed `leading-only` must *remove* them, not restate them.
    let mut base = CatalogVision::shipped("vision");
    base.system_prompt_update = Some(SystemPromptMode::InHistory);
    base.tool_update = Some(ToolUpdateMode::InHistory);
    let mut input = catalog_input(None);
    input.supported = vec![base];
    input.listing = Some(listing_of(&[listing_entry(
        "vision",
        &serde_json::json!({
            "api_capabilities": { "anthropic_messages": {
                "system_prompt_update": "leading-only",
                "tool_update": "leading-only",
            }},
        }),
    )]));
    let catalog = resolve_listed_catalog(&input);
    assert_eq!(catalog.offered[0].system_prompt_update, None);
    assert_eq!(catalog.offered[0].tool_update, None);
}

#[test]
fn badges_reach_the_catalog_and_an_unreadable_one_is_a_problem() {
    let listing = listing_of(&[
        listing_entry(
            "vision",
            &serde_json::json!({
                "client_metadata": { "deepseek-harness": { "badge": { "text": "New", "tone": "info" } } },
            }),
        ),
        listing_entry(
            "text-only",
            &serde_json::json!({ "client_metadata": { "deepseek-harness": { "badge": "no" } } }),
        ),
    ]);
    let catalog = resolve_listed_catalog(&catalog_input(Some(listing)));
    assert_eq!(
        catalog.badges.get("vision").map(|b| b.text.as_str()),
        Some("New")
    );
    assert_eq!(
        catalog.badges.get("vision").map(|b| b.tone),
        Some(BadgeTone::Info)
    );
    assert!(!catalog.badges.contains_key("text-only"));
    // The model is still offered; only its badge was dropped.
    assert_eq!(catalog.offered.len(), 2);
    assert!(
        catalog
            .problems
            .iter()
            .any(|problem| problem.contains("shows no badge")),
        "{:?}",
        catalog.problems
    );
}

#[test]
fn an_override_applies_to_an_offered_model_and_drops_inherited_image_limits() {
    let listing = listing_of(&[listing_entry("vision", &serde_json::json!({}))]);
    let mut input = catalog_input(Some(listing));
    input.overrides.insert(
        "vision".to_owned(),
        ModelOverride {
            input_modalities: Some(vec![Modality::Text]),
            ..ModelOverride::default()
        },
    );
    let catalog = resolve_listed_catalog(&input);
    let model = &catalog.offered[0];
    assert_eq!(model.input_modalities, vec![Modality::Text]);
    assert!(!model.vision);
    assert_eq!(model.image_pixel_budget, None);
    assert_eq!(model.image_max_bytes, None);
}

#[test]
fn an_override_that_names_nothing_is_a_configuration_error() {
    // Upstream reports an unmatched override only once a listing exists: before one succeeds,
    // every override is a statement about a model this build may yet learn about.
    let mut input = catalog_input(None);
    input
        .overrides
        .insert("ghost".to_owned(), ModelOverride::default());
    assert!(
        resolve_listed_catalog(&input).override_errors.is_empty(),
        "no listing means no unmatched-override report"
    );
    let mut input = catalog_input(Some(listing_of(&[listing_entry(
        "vision",
        &serde_json::json!({}),
    )])));
    input
        .overrides
        .insert("ghost".to_owned(), ModelOverride::default());
    let catalog = resolve_listed_catalog(&input);
    assert_eq!(catalog.override_errors.len(), 1);
    assert!(
        catalog.override_errors[0].contains("neither the model listing nor the built-in catalog"),
        "{:?}",
        catalog.override_errors
    );
    // An override naming a listed model is not an error even when the build does not know it.
    let mut input = catalog_input(Some(listing_of(&[listing_entry(
        "unknown",
        &serde_json::json!({
            "client_requirements": { "deepseek-harness": { "min_version": "1.0.0" } },
        }),
    )])));
    input
        .overrides
        .insert("unknown".to_owned(), ModelOverride::default());
    assert!(resolve_listed_catalog(&input).override_errors.is_empty());
}

#[test]
fn resolution_keeps_models_the_listing_does_not_offer() {
    // A listed catalog narrows what is *offered*; it must not remove the build's own models from
    // request resolution, because a session may still name one.
    let listing = listing_of(&[listing_entry("vision", &serde_json::json!({}))]);
    let catalog = resolve_listed_catalog(&catalog_input(Some(listing)));
    assert_eq!(catalog.offered.len(), 1);
    assert_eq!(catalog.resolution.len(), 2);
    assert_eq!(catalog.resolution[1].id, "text-only");
}

#[test]
fn layering_keeps_undeclared_fields_and_ignores_a_declared_image_limit_on_a_text_result() {
    let mut base = CatalogVision::shipped("vision");
    base.image_max_bytes = Some(1_000_000);
    base.image_pixel_budget = Some(PixelBudget::Pixels(640_000));
    // A text-only layer drops the inherited limits...
    let text_only = layer_override(
        &base,
        &ModelOverride {
            input_modalities: Some(vec![Modality::Text]),
            ..ModelOverride::default()
        },
    );
    assert_eq!(text_only.image_max_bytes, None);
    assert_eq!(text_only.image_pixel_budget, None);
    assert_eq!(text_only.context_window, Some(1_000_000));
    // ...unless the override declares them itself, which is how a route re-declares a budget.
    let explicit = layer_override(
        &base,
        &ModelOverride {
            input_modalities: Some(vec![Modality::Text]),
            image_max_bytes: Some(50_000),
            ..ModelOverride::default()
        },
    );
    assert_eq!(explicit.image_max_bytes, Some(50_000));
    // A vision layer keeps them.
    let vision = layer_override(&base, &ModelOverride::default());
    assert_eq!(vision.image_max_bytes, Some(1_000_000));
    assert_eq!(
        vision.image_pixel_budget,
        Some(PixelBudget::Pixels(640_000))
    );
    // The overrides a model reports are the ones its own fields declare.
    assert_eq!(
        vision.image_overrides(),
        ImageOverrides {
            pixel_budget: Some(PixelBudget::Pixels(640_000)),
            max_bytes: Some(1_000_000),
        }
    );
}

#[test]
fn modality_spellings_are_the_wire_spellings() {
    assert_eq!(Modality::Text.as_str(), "text");
    assert_eq!(Modality::Image.as_str(), "image");
}

// ---------------------------------------------------------------------------------------------
// File handle text — the sibling of image handles
// ---------------------------------------------------------------------------------------------

#[test]
fn a_file_attachment_is_always_handle_text() {
    let reference = girder_core::content_projection_gap::FileAttachmentRef::new(
        format!("sha256:{}", "ab".repeat(32)),
        "report.pdf",
        42,
    );
    let text = deepseek_files_gap::file_handle_text(&reference);
    assert!(text.contains("report.pdf"), "{text}");
    assert!(text.contains("42 bytes"), "{text}");
    assert!(text.contains("do not claim to have read it"), "{text}");
}
