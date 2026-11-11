//! Tests for `session.uploadAttachment`, the write half of the attachment surface.
//!
//! The method is reached through the crate's re-export, which is the path a client's host uses. The
//! module spells its own paths through `girder_core::` so that this file works from outside the crate.
//!
//! Six properties are why these tests exist:
//!
//! - an upload comes back with an id, or the caller has nothing to read later
//! - that id works through `session.attachment`, which is the only proof the two halves meet
//! - bad base64 is refused, or the store holds blobs nobody can decode
//! - a declared type that disagrees with the bytes is refused, or a ZIP renamed `.png` is stored
//! - an image past the size cap is refused, or one upload can exhaust the process
//! - two uploads get different ids, or the second reference silently renames the first image
//!
//! Isolation: every test builds its own store and registry, and writes its fixture PNG into a
//! scratch directory named from the process id and a counter, so tests running in parallel in one
//! process cannot read each other's fixture.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use girder_core::{
    AttachmentLimits, MemoryStore, RemoteCall, RemoteError, RemoteRegistry, encode_base64,
    register_attachment_method, register_attachment_upload_method,
};

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// A scratch directory unique to this process and this call.
fn scratch(name: &str) -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "girder-attachment-rpc-{}-{sequence}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// The smallest byte sequence the sniffer accepts as a PNG: signature plus the IHDR chunk header.
fn png_bytes() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(b"\0\0\0\rIHDR");
    bytes
}

/// Writes the fixture PNG into `dir` and reads it back.
///
/// Written to disk rather than built in memory so the bytes an upload carries are the bytes a real
/// caller read from a file, and so a failure to encode is visible here rather than inside the method.
fn png_fixture(dir: &std::path::Path) -> Vec<u8> {
    let path = dir.join("shot.png");
    std::fs::write(&path, png_bytes()).expect("write fixture");
    std::fs::read(&path).expect("read fixture")
}

/// A registry serving both the upload method and the existing read method over one store.
fn registry(store: &Arc<MemoryStore>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    register_attachment_upload_method(&mut registry, store);
    register_attachment_method(&mut registry, store);
    registry
}

fn upload_args(bytes: &[u8], media_type: &str, name: Option<&str>) -> serde_json::Value {
    match name {
        Some(name) => serde_json::json!({
            "data": encode_base64(bytes),
            "mediaType": media_type,
            "name": name,
        }),
        None => serde_json::json!({
            "data": encode_base64(bytes),
            "mediaType": media_type,
        }),
    }
}

/// Calls `session.uploadAttachment` and fails the test if it refuses.
async fn upload(
    registry: &RemoteRegistry,
    bytes: &[u8],
    media_type: &str,
    name: Option<&str>,
) -> serde_json::Value {
    registry
        .get("session", "uploadAttachment")
        .expect("upload method is registered")(call(upload_args(bytes, media_type, name)))
    .await
    .expect("upload is admitted")
}

/// Calls `session.uploadAttachment` and returns the refusal.
async fn upload_error(registry: &RemoteRegistry, args: serde_json::Value) -> RemoteError {
    registry
        .get("session", "uploadAttachment")
        .expect("upload method is registered")(call(args))
    .await
    .expect_err("upload is refused")
}

// ---- the happy path ---------------------------------------------------------------------------

#[tokio::test]
async fn an_upload_returns_a_reference() {
    let dir = scratch("reference");
    let bytes = png_fixture(&dir);
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    let value = upload(&registry, &bytes, "image/png", Some("shot.png")).await;

    let id = value["attachmentId"].as_str().expect("an id");
    assert!(!id.is_empty());
    assert_eq!(value["mediaType"], "image/png");
    assert_eq!(value["bytes"], bytes.len());
    assert_eq!(value["name"], "shot.png");
    assert!(
        store.ids().contains(id),
        "the id the caller was given must name a stored image"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_uploaded_image_reads_back_through_the_read_method() {
    // The end-to-end proof: the id this method mints is the id `session.attachment` accepts, and the
    // bytes come back unchanged. Either half working alone would still leave uploads unreadable.
    let dir = scratch("roundtrip");
    let bytes = png_fixture(&dir);
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    let uploaded = upload(&registry, &bytes, "image/png", Some("shot.png")).await;
    let id = uploaded["attachmentId"].as_str().expect("an id").to_owned();

    let read = registry
        .get("session", "attachment")
        .expect("read method is registered")(call(
        serde_json::json!({"attachment": {"attachmentId": id}}),
    ))
    .await
    .expect("read back");

    assert_eq!(read["attachment"]["attachmentId"], id);
    assert_eq!(read["attachment"]["mediaType"], "image/png");
    assert_eq!(read["attachment"]["bytes"], bytes.len());
    assert_eq!(read["attachment"]["name"], "shot.png");

    let encoded = read["data"].as_str().expect("base64 data");
    assert_eq!(
        girder_core::decode_base64_canonical(encoded).expect("canonical"),
        bytes
    );
    // The stored bytes are the file's bytes, so a caller can hand them to a decoder without a
    // re-encode step that might change them.
    assert_eq!(encoded, encode_base64(&bytes));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_nameless_upload_reports_no_name() {
    let dir = scratch("nameless");
    let bytes = png_fixture(&dir);
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    let value = upload(&registry, &bytes, "image/png", None).await;

    // `null` rather than `""`: a client that distinguishes "no filename" from "empty filename" would
    // otherwise render a blank label.
    assert!(value["name"].is_null(), "{value}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_blank_name_is_treated_as_absent() {
    let dir = scratch("blank-name");
    let bytes = png_fixture(&dir);
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    let value = upload(&registry, &bytes, "image/png", Some("   ")).await;

    assert!(value["name"].is_null(), "{value}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn two_uploads_are_not_merged_into_one_id() {
    // Identical bytes must stay two uploads: a content-addressed id would let the second upload
    // silently stand for the first, so an old reference would start meaning something new.
    let dir = scratch("distinct");
    let bytes = png_fixture(&dir);
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    let first = upload(&registry, &bytes, "image/png", Some("shot.png")).await;
    let second = upload(&registry, &bytes, "image/png", Some("shot.png")).await;

    let first_id = first["attachmentId"].as_str().expect("an id");
    let second_id = second["attachmentId"].as_str().expect("an id");
    assert_ne!(first_id, second_id);
    assert!(store.get(first_id).is_some());
    assert!(store.get(second_id).is_some());
    assert_eq!(store.ids().len(), 2);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- refusals ---------------------------------------------------------------------------------

#[tokio::test]
async fn invalid_base64_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    for data in ["not base64!!", "AAAAA", "===="] {
        let error = upload_error(
            &registry,
            serde_json::json!({"data": data, "mediaType": "image/png"}),
        )
        .await;
        let RemoteError::InvalidArguments {
            method, message, ..
        } = error
        else {
            panic!("expected InvalidArguments for {data:?}");
        };
        assert_eq!(method, "uploadAttachment");
        assert!(message.contains("not an acceptable image"), "{message}");
    }
    assert!(
        store.ids().is_empty(),
        "a refused upload must leave nothing behind"
    );
}

#[tokio::test]
async fn a_non_canonical_encoding_is_refused() {
    // `AB==` decodes to the same byte as `AA==`, so accepting it would give one image two spellings
    // and defeat the length and deduplication reasoning admission relies on.
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    let error = upload_error(
        &registry,
        serde_json::json!({"data": "AB==", "mediaType": "image/png"}),
    )
    .await;

    let RemoteError::InvalidArguments { message, .. } = error else {
        panic!("expected InvalidArguments");
    };
    assert!(message.contains("canonical"), "{message}");
    assert!(store.ids().is_empty());
}

#[tokio::test]
async fn a_media_type_that_disagrees_with_the_bytes_is_refused() {
    // The declared type is checked against the magic bytes, not trusted: a `.jpg` that is really a PNG
    // arrives mislabelled from real browsers, and a ZIP named `.png` arrives from an attacker.
    let dir = scratch("mismatch");
    let bytes = png_fixture(&dir);
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    let error = upload_error(&registry, upload_args(&bytes, "image/jpeg", None)).await;

    let RemoteError::InvalidArguments { message, .. } = error else {
        panic!("expected InvalidArguments");
    };
    assert!(message.contains("image/jpeg"), "{message}");
    assert!(message.contains("image/png"), "{message}");
    assert!(store.ids().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unaccepted_media_type_is_refused() {
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    // A type with no decoder behind it is a stored blob nothing can display.
    let error = upload_error(
        &registry,
        serde_json::json!({"data": encode_base64(b"PK\x03\x04zip"), "mediaType": "application/zip"}),
    )
    .await;

    let RemoteError::InvalidArguments { message, .. } = error else {
        panic!("expected InvalidArguments");
    };
    assert!(message.contains("application/zip"), "{message}");
    assert!(store.ids().is_empty());
}

#[tokio::test]
async fn bytes_that_are_not_an_image_are_refused() {
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    let error = upload_error(
        &registry,
        serde_json::json!({"data": encode_base64(b"PK\x03\x04zip"), "mediaType": "image/png"}),
    )
    .await;

    let RemoteError::InvalidArguments { message, .. } = error else {
        panic!("expected InvalidArguments");
    };
    assert!(message.contains("not a recognised image"), "{message}");
}

#[tokio::test]
async fn an_image_past_the_size_cap_is_refused() {
    let dir = scratch("oversized");
    let mut bytes = png_fixture(&dir);
    // Still a PNG by its signature, so only the size check can be what refuses it.
    bytes.resize(
        usize::try_from(AttachmentLimits::default().max_image_bytes).expect("fits") + 1,
        0,
    );
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    let error = upload_error(&registry, upload_args(&bytes, "image/png", None)).await;

    let RemoteError::InvalidArguments { message, .. } = error else {
        panic!("expected InvalidArguments");
    };
    assert!(message.contains("limit"), "{message}");
    assert!(store.ids().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_payload_larger_than_the_encoded_bound_is_refused_without_decoding() {
    // The cap is applied to the encoded length as well: base64 of an over-limit file is refused before
    // the decoder allocates the bytes it would produce.
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);
    // One byte past what the largest admissible image could encode to: 4 * ceil(n / 3) for n equal to
    // the byte cap, plus a little slack so only the pre-decode bound can refuse it.
    let max_bytes = usize::try_from(AttachmentLimits::default().max_image_bytes).expect("fits");
    let encoded = "A".repeat((max_bytes / 3) * 4 + 8);

    let error = upload_error(
        &registry,
        serde_json::json!({"data": encoded, "mediaType": "image/png"}),
    )
    .await;

    let RemoteError::InvalidArguments { message, .. } = error else {
        panic!("expected InvalidArguments");
    };
    assert!(message.contains("limit"), "{message}");
    assert!(store.ids().is_empty());
}

#[tokio::test]
async fn malformed_arguments_are_refused() {
    let store = Arc::new(MemoryStore::new());
    let registry = registry(&store);

    for args in [
        serde_json::json!({}),
        serde_json::json!({"mediaType": "image/png"}),
        serde_json::json!({"data": "AAAA"}),
        serde_json::json!({"data": 1, "mediaType": "image/png"}),
        serde_json::json!({"data": "AAAA", "mediaType": 2}),
        serde_json::json!({"data": "AAAA", "mediaType": "image/png", "name": 3}),
    ] {
        let error = upload_error(&registry, args).await;
        assert!(
            matches!(error, RemoteError::InvalidArguments { .. }),
            "expected InvalidArguments, got {error:?}"
        );
    }

    assert!(store.ids().is_empty());
}
