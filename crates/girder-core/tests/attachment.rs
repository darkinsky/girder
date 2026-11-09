//! Tests for attachment admission (M3 of the v2 plan).
//!
//! Two properties carry the correctness: base64 must be canonical, or the same bytes have several
//! spellings; and the media type must come from the bytes, or a store holds a ZIP named `.png`.

use girder_core::{
    AttachmentError, AttachmentLimits, DecodedImage, EncodedImage, MemoryStore, admit_batch,
    admit_image, decode_base64_canonical, detect_media_type,
};

fn png_bytes() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(b"\0\0\0\rIHDR");
    bytes
}

fn jpeg_bytes() -> Vec<u8> {
    vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, b'J', b'F', b'I', b'F']
}

fn encode(bytes: &[u8]) -> String {
    // Reuse the crate's encoder rather than carrying a second base64 table that could drift from
    // the one under test.
    girder_core::encode_base64(bytes)
}

fn image(bytes: &[u8], media_type: &str) -> EncodedImage {
    EncodedImage {
        data: encode(bytes),
        media_type: media_type.to_owned(),
        name: None,
    }
}

// ---- media type detection ---------------------------------------------------------------------

#[test]
fn png_and_jpeg_are_recognised_from_their_magic_bytes() {
    assert_eq!(detect_media_type(&png_bytes()), Some("image/png"));
    assert_eq!(detect_media_type(&jpeg_bytes()), Some("image/jpeg"));
}

#[test]
fn gif_and_webp_are_recognised() {
    assert_eq!(detect_media_type(b"GIF89a\x01\x00"), Some("image/gif"));
    assert_eq!(detect_media_type(b"GIF87a\x01\x00"), Some("image/gif"));

    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&[0, 0, 0, 0]);
    webp.extend_from_slice(b"WEBP");
    assert_eq!(detect_media_type(&webp), Some("image/webp"));
}

#[test]
fn an_unrecognised_payload_is_refused_rather_than_guessed() {
    // Storing a blob whose type nobody knows means whatever reads it later has to guess too.
    assert_eq!(detect_media_type(b"PK\x03\x04zip"), None);
    assert_eq!(detect_media_type(b"not an image"), None);
    assert_eq!(detect_media_type(&[]), None);
}

// ---- base64 canonical form --------------------------------------------------------------------

#[test]
fn canonical_base64_round_trips() {
    let bytes = png_bytes();
    let encoded = encode(&bytes);

    assert_eq!(decode_base64_canonical(&encoded).unwrap(), bytes);
}

#[test]
fn padding_variants_are_refused() {
    // Base64 has several spellings for the same bytes, and two spellings are two stored blobs.
    let bytes = b"a";
    let canonical = encode(bytes);
    assert_eq!(canonical, "YQ==");

    // Without padding.
    assert!(decode_base64_canonical("YQ").is_err());
    // Wrong padding count.
    assert!(decode_base64_canonical("YQ=").is_err());
}

#[test]
fn a_non_canonical_encoding_of_valid_bytes_is_refused() {
    // Some decoders accept these; accepting them would let a length check be bypassed.
    assert!(matches!(
        decode_base64_canonical("YQ==="),
        Err(AttachmentError::NotCanonicalBase64 | AttachmentError::InvalidBase64)
    ));
}

#[test]
fn an_invalid_character_is_refused() {
    assert!(matches!(
        decode_base64_canonical("Y Q = ="),
        Err(AttachmentError::InvalidBase64)
    ));
}

#[test]
fn empty_payloads_decode_to_nothing() {
    assert_eq!(decode_base64_canonical("").unwrap(), Vec::<u8>::new());
}

// ---- admission ---------------------------------------------------------------------------------

#[test]
fn a_valid_png_is_admitted() {
    let admitted = admit_image(
        &image(&png_bytes(), "image/png"),
        &AttachmentLimits::default(),
    )
    .expect("admitted");

    assert_eq!(admitted.media_type, "image/png");
    assert_eq!(admitted.data, png_bytes());
}

#[test]
fn a_declared_type_that_the_bytes_contradict_is_refused() {
    // The property this module exists for: a browser reports the type from the extension, so a
    // legitimate upload can arrive mislabelled, and a store that trusts it holds a ZIP as an image.
    let error = admit_image(
        &image(&jpeg_bytes(), "image/png"),
        &AttachmentLimits::default(),
    )
    .expect_err("mismatch");

    let AttachmentError::MediaTypeMismatch { declared, detected } = error else {
        panic!("expected MediaTypeMismatch, got {error:?}");
    };
    assert_eq!(declared, "image/png");
    assert_eq!(detected, "image/jpeg");
}

#[test]
fn an_unsupported_type_is_refused_before_decoding() {
    let error = admit_image(
        &image(&png_bytes(), "image/tiff"),
        &AttachmentLimits::default(),
    )
    .expect_err("unsupported");

    assert!(matches!(
        error,
        AttachmentError::UnsupportedMediaType { .. }
    ));
}

#[test]
fn a_payload_that_is_not_an_image_is_refused() {
    let error = admit_image(
        &image(b"PK\x03\x04", "image/png"),
        &AttachmentLimits::default(),
    )
    .expect_err("not an image");

    assert!(matches!(error, AttachmentError::NotAnImage));
}

#[test]
fn an_image_above_the_limit_is_refused() {
    let limits = AttachmentLimits {
        max_image_bytes: 4,
        ..AttachmentLimits::default()
    };

    let error = admit_image(&image(&png_bytes(), "image/png"), &limits).expect_err("too large");

    let AttachmentError::ImageTooLarge { bytes, limit } = error else {
        panic!("expected ImageTooLarge, got {error:?}");
    };
    assert_eq!(limit, 4);
    assert!(bytes > 4);
}

// ---- batches -----------------------------------------------------------------------------------

#[test]
fn a_batch_is_admitted_in_order() {
    let images = vec![
        image(&png_bytes(), "image/png"),
        image(&jpeg_bytes(), "image/jpeg"),
    ];

    let admitted = admit_batch(&images, &AttachmentLimits::default()).expect("admitted");

    assert_eq!(admitted.len(), 2);
    assert_eq!(admitted[0].media_type, "image/png");
    assert_eq!(admitted[1].media_type, "image/jpeg");
}

#[test]
fn too_many_images_are_refused_before_any_is_decoded() {
    let limits = AttachmentLimits {
        max_images: 1,
        ..AttachmentLimits::default()
    };
    let images = vec![
        image(&png_bytes(), "image/png"),
        image(&jpeg_bytes(), "image/jpeg"),
    ];

    let error = admit_batch(&images, &limits).expect_err("too many");

    let AttachmentError::TooManyImages { count, limit } = error else {
        panic!("expected TooManyImages, got {error:?}");
    };
    assert_eq!((count, limit), (2, 1));
}

#[test]
fn a_batch_above_the_total_budget_is_refused() {
    // A per-image limit alone lets twenty legal images together exhaust memory.
    let limits = AttachmentLimits {
        max_image_bytes: 1000,
        max_images: 10,
        max_batch_bytes: 20,
    };
    let images = vec![
        image(&png_bytes(), "image/png"),
        image(&png_bytes(), "image/png"),
    ];

    let error = admit_batch(&images, &limits).expect_err("batch too large");
    assert!(matches!(error, AttachmentError::BatchTooLarge { .. }));
}

#[test]
fn one_bad_member_refuses_the_whole_batch() {
    // An upload is refused wholesale rather than reported in full: the caller's next step is to
    // re-encode the file, not to fix several fields.
    let images = vec![
        image(&png_bytes(), "image/png"),
        image(&png_bytes(), "image/jpeg"),
    ];

    let error = admit_batch(&images, &AttachmentLimits::default()).expect_err("one is mislabelled");
    assert!(matches!(error, AttachmentError::MediaTypeMismatch { .. }));
}

// ---- storage -----------------------------------------------------------------------------------

#[test]
fn a_stored_image_can_be_read_back() {
    let store = MemoryStore::new();
    let admitted = admit_image(
        &image(&png_bytes(), "image/png"),
        &AttachmentLimits::default(),
    )
    .unwrap();

    let reference = store.put("img-1", admitted).expect("stored");

    assert_eq!(reference.id, "img-1");
    assert_eq!(reference.media_type, "image/png");
    assert_eq!(store.get("img-1").unwrap().data, png_bytes());
}

#[test]
fn re_storing_the_same_id_converges() {
    // A re-upload of identical content should not accumulate copies.
    let store = MemoryStore::new();
    let limits = AttachmentLimits::default();
    let first = admit_image(&image(&png_bytes(), "image/png"), &limits).unwrap();
    let second = admit_image(&image(&png_bytes(), "image/png"), &limits).unwrap();

    store.put("img-1", first).unwrap();
    store.put("img-1", second).unwrap();

    assert_eq!(store.ids().len(), 1);
}

#[test]
fn an_unsafe_storage_id_is_refused() {
    // The id becomes a path segment in a file-backed store.
    let store = MemoryStore::new();
    let image = DecodedImage {
        data: vec![1],
        media_type: "image/png".to_owned(),
        name: None,
    };

    assert!(store.put("../escape", image.clone()).is_none());
    assert!(store.put("a/b", image.clone()).is_none());
    assert!(store.put("", image).is_none());
}

#[test]
fn a_name_is_kept_but_never_interpreted_as_a_path() {
    let mut encoded = image(&png_bytes(), "image/png");
    encoded.name = Some("../../etc/passwd".to_owned());

    let admitted = admit_image(&encoded, &AttachmentLimits::default()).expect("admitted");

    assert_eq!(admitted.name.as_deref(), Some("../../etc/passwd"));
    // The name is only ever a display string; the storage key is separate and validated.
    let store = MemoryStore::new();
    assert!(store.put("safe-id", admitted).is_some());
}
