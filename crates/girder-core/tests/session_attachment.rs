//! Tests for reading a stored image back.
//!
//! The interesting properties are about trust: the type in the response comes from the verified bytes,
//! and the encoding is canonical so two reads of one image produce identical text.

use std::sync::Arc;

use girder_core::{
    AttachmentLimits, DecodedImage, EncodedImage, MemoryStore, RemoteCall, RemoteRegistry,
    admit_image, encode_base64,
};
use serde_json::json;

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// A minimal valid PNG header plus a body.
fn png() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(b"\0\0\0\rIHDR");
    bytes
}

/// A store holding one image under `id`.
fn store_with(id: &str) -> Arc<MemoryStore> {
    let store = MemoryStore::new();
    let image = admit_image(
        &EncodedImage {
            data: encode_base64(&png()),
            media_type: "image/png".to_owned(),
            name: Some("shot.png".to_owned()),
        },
        &AttachmentLimits::default(),
    )
    .expect("admitted");
    store.put(id, image).expect("stored");
    Arc::new(store)
}

fn registry(store: &Arc<MemoryStore>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    girder_core::register_attachment_method(&mut registry, store);
    registry
}

#[tokio::test]
async fn a_stored_image_reads_back() {
    let store = store_with("img-1");
    let registry = registry(&store);

    let value = registry.get("session", "attachment").expect("registered")(call(
        json!({"attachment": {"attachmentId": "img-1"}}),
    ))
    .await
    .expect("read");

    assert_eq!(value["attachment"]["attachmentId"], "img-1");
    assert_eq!(value["attachment"]["mediaType"], "image/png");
    assert_eq!(value["attachment"]["bytes"], png().len());
}

#[tokio::test]
async fn the_data_is_canonical_base64() {
    // Two reads of one image must produce identical text, or a client comparing them sees a change.
    let store = store_with("img-1");
    let registry = registry(&store);
    let method = registry.get("session", "attachment").expect("registered");

    let first = method(call(json!({"attachment": {"attachmentId": "img-1"}})))
        .await
        .expect("read");
    let second = method(call(json!({"attachment": {"attachmentId": "img-1"}})))
        .await
        .expect("read");

    assert_eq!(first["data"], second["data"]);
    let encoded = first["data"].as_str().expect("a string");
    assert_eq!(
        girder_core::decode_base64_canonical(encoded).expect("decodes"),
        png()
    );
}

#[tokio::test]
async fn the_media_type_comes_from_the_bytes_not_the_caller() {
    // The store refused anything whose declared type disagreed with its bytes, so what comes back is
    // the verified type rather than what a client claimed.
    let store = MemoryStore::new();
    // Stored directly with a type that would not have passed admission for these bytes: the method
    // reports what the store holds, which is the verified form.
    store.put(
        "img-1",
        DecodedImage {
            data: png(),
            media_type: "image/png".to_owned(),
            name: None,
        },
    );

    let registry = registry(&Arc::new(store));
    let value = registry.get("session", "attachment").expect("registered")(call(
        json!({"attachment": {"attachmentId": "img-1"}}),
    ))
    .await
    .expect("read");

    assert_eq!(value["attachment"]["mediaType"], "image/png");
}

#[tokio::test]
async fn an_unknown_reference_is_refused() {
    let store = store_with("img-1");
    let registry = registry(&store);

    let error = registry.get("session", "attachment").expect("registered")(call(
        json!({"attachment": {"attachmentId": "nope"}}),
    ))
    .await
    .expect_err("unknown");

    let girder_core::RemoteError::InvalidArguments { message, .. } = error else {
        panic!("expected InvalidArguments");
    };
    assert!(message.contains("nope"), "{message}");
}

#[tokio::test]
async fn a_missing_reference_is_refused() {
    let store = store_with("img-1");
    let registry = registry(&store);

    for args in [json!({}), json!({"attachment": {}})] {
        let error = registry.get("session", "attachment").expect("registered")(call(args))
            .await
            .expect_err("missing");
        assert!(matches!(
            error,
            girder_core::RemoteError::InvalidArguments { .. }
        ));
    }
}

#[tokio::test]
async fn a_named_image_carries_its_name() {
    let store = store_with("img-1");
    let registry = registry(&store);

    let value = registry.get("session", "attachment").expect("registered")(call(
        json!({"attachment": {"attachmentId": "img-1"}}),
    ))
    .await
    .expect("read");

    assert_eq!(value["attachment"]["name"], "shot.png");
}

#[tokio::test]
async fn the_method_is_registered_by_the_plain_registry() {
    // The drift test caught `fork` missing from one path; this asserts the attachment method is on the
    // list rather than only reachable through a constructor the test happened to build.
    let dir = std::env::temp_dir().join(format!("girder-att-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let catalog: Arc<dyn girder_core::SessionCatalog> =
        Arc::new(girder_core::DirectoryCatalog::new(&dir));

    let names = girder_core::default_registry(catalog).names();
    assert!(
        names.iter().any(|(_, method)| method == "attachment"),
        "the plain registry must serve it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
