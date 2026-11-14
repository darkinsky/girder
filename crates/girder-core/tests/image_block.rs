//! Tests for provider-facing image payloads (`image_block`).
//!
//! The properties that matter are all about the boundary rather than the base64 itself: the type is
//! decided by the bytes and not by a name, the four supported types are the only ones that survive,
//! and the two directions agree about what a payload means.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/image_block.rs"]
mod image_block;

use image_block::{
    ImageError, ImagePayload, MAX_BYTES, SUPPORTED_TYPES, decode, encode, sniff_type,
};

/// A minimal PNG signature followed by an IHDR-shaped body.
fn png_bytes() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(&[0, 0, 0, 13]);
    bytes.extend_from_slice(b"IHDR");
    bytes
}

/// A JPEG opening: SOI then the first marker of an APP0 segment.
fn jpeg_bytes() -> Vec<u8> {
    vec![
        0xff, 0xd8, 0xff, 0xe0, 0, 0x10, b'J', b'F', b'I', b'F', 0, 1,
    ]
}

/// A `GIF89a` header.
fn gif_bytes() -> Vec<u8> {
    let mut bytes = b"GIF89a".to_vec();
    bytes.extend_from_slice(&[0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00]);
    bytes
}

/// A WebP header: RIFF, a chunk size, then the WEBP fourcc.
fn webp_bytes() -> Vec<u8> {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&[0x24, 0x00, 0x00, 0x00]);
    bytes.extend_from_slice(b"WEBPVP8 ");
    bytes
}

fn payload(media_type: &str, data: &str) -> ImagePayload {
    ImagePayload {
        media_type: media_type.to_owned(),
        data: data.to_owned(),
    }
}

#[test]
fn round_trips_png_bytes() {
    let bytes = png_bytes();
    let encoded = encode("image/png", &bytes).expect("png is supported");
    assert_eq!(encoded.media_type, "image/png");
    assert_eq!(decode(&encoded).expect("valid payload"), bytes);
}

#[test]
fn round_trips_every_supported_type() {
    let cases = [
        ("image/png", png_bytes()),
        ("image/jpeg", jpeg_bytes()),
        ("image/gif", gif_bytes()),
        ("image/webp", webp_bytes()),
    ];
    for (media_type, bytes) in cases {
        let encoded = encode(media_type, &bytes).expect("supported type encodes");
        assert_eq!(encoded.media_type, media_type);
        assert_eq!(sniff_type(&bytes), Some(media_type), "{media_type} magic");
        assert_eq!(
            decode(&encoded).expect("decodes"),
            bytes,
            "{media_type} round trip"
        );
    }
}

#[test]
fn encoding_is_standard_padded_base64() {
    // One, two, and three bytes cover the three padding lengths: `==`, `=`, and none.
    assert_eq!(encode("image/png", b"f").expect("encodes").data, "Zg==");
    assert_eq!(encode("image/png", b"fo").expect("encodes").data, "Zm8=");
    assert_eq!(encode("image/png", b"foo").expect("encodes").data, "Zm9v");
}

#[test]
fn encoding_never_uses_the_url_safe_alphabet() {
    let data = encode("image/png", &[0xfb, 0xff, 0xbf])
        .expect("encodes")
        .data;
    assert_eq!(data, "+/+/");
    assert!(!data.contains('-'));
    assert!(!data.contains('_'));
}

#[test]
fn sniffs_png_from_magic_bytes() {
    assert_eq!(sniff_type(&png_bytes()), Some("image/png"));
}

#[test]
fn sniffs_jpeg_from_magic_bytes() {
    assert_eq!(sniff_type(&jpeg_bytes()), Some("image/jpeg"));
}

#[test]
fn sniffs_gif_from_magic_bytes() {
    assert_eq!(sniff_type(&gif_bytes()), Some("image/gif"));
    assert_eq!(
        sniff_type(b"GIF87a\x01\x00\x01\x00"),
        Some("image/gif"),
        "the earlier revision is the same type"
    );
}

#[test]
fn sniffs_webp_from_magic_bytes() {
    assert_eq!(sniff_type(&webp_bytes()), Some("image/webp"));
}

#[test]
fn sniff_ignores_the_extension() {
    // The whole point of a magic-number sniff: the type comes from the bytes, so a name that claims
    // otherwise carries no weight in either direction.
    let gif_bytes = gif_bytes();
    assert_eq!(sniff_type(&gif_bytes), Some("image/gif"));

    // The case a name-based check would let through: a file called .png whose bytes are not a PNG.
    assert_eq!(sniff_type(b"this is not an image"), None);
}

#[test]
fn sniff_rejects_bytes_that_match_no_magic_number() {
    assert_eq!(sniff_type(b""), None);
    assert_eq!(sniff_type(b"\x89PN"), None, "a truncated PNG signature");
    assert_eq!(sniff_type(b"\xff\xd8"), None, "a truncated JPEG signature");
    assert_eq!(sniff_type(b"GIF"), None, "a truncated GIF signature");
    assert_eq!(
        sniff_type(b"RIFF\x00\x00\x00\x00WAVE"),
        None,
        "a RIFF container that is not WebP"
    );
    assert_eq!(sniff_type(b"RIFF"), None, "a RIFF header with no fourcc");
}

#[test]
fn sniff_rejects_a_webp_fourcc_at_the_wrong_offset() {
    // "WEBP" must sit at offset 8. A file that merely contains the word is not a WebP.
    assert_eq!(sniff_type(b"RIFFWEBP\x00\x00\x00\x00"), None);
}

#[test]
fn rejects_types_outside_the_supported_list() {
    for media_type in [
        "image/svg+xml",
        "image/bmp",
        "image/tiff",
        "text/plain",
        "application/pdf",
    ] {
        let error = encode(media_type, &png_bytes()).expect_err("must be refused");
        assert_eq!(
            error,
            ImageError::UnsupportedType {
                got: media_type.to_owned()
            }
        );
    }
}

#[test]
fn rejects_an_empty_type() {
    let error = encode("", &png_bytes()).expect_err("must be refused");
    assert!(matches!(error, ImageError::UnsupportedType { .. }));
}

#[test]
fn accepts_a_type_that_differs_only_in_spelling() {
    let upper = encode("IMAGE/PNG", &png_bytes()).expect("case is not part of the type");
    assert_eq!(upper.media_type, "image/png");

    let with_parameter =
        encode("image/png; charset=binary", &png_bytes()).expect("parameter is ignored");
    assert_eq!(with_parameter.media_type, "image/png");
}

#[test]
fn rejects_an_oversized_image() {
    let bytes = vec![0u8; MAX_BYTES + 1];
    let error = encode("image/png", &bytes).expect_err("over the cap");
    assert_eq!(
        error,
        ImageError::TooLarge {
            bytes: MAX_BYTES + 1,
            max: MAX_BYTES
        }
    );
}

#[test]
fn accepts_exactly_the_maximum_size() {
    let bytes = vec![0u8; MAX_BYTES];
    let encoded = encode("image/png", &bytes).expect("the limit is inclusive");
    assert_eq!(decode(&encoded).expect("decodes").len(), MAX_BYTES);
}

#[test]
fn decode_rejects_an_oversized_payload_before_allocating() {
    // The text is never decoded: the implied length is judged first. This keeps a hostile length
    // from forcing a buffer larger than the cap.
    let data = "A".repeat(MAX_BYTES / 3 * 4 + 8);
    let error = decode(&payload("image/png", &data)).expect_err("over the cap");
    assert!(matches!(error, ImageError::TooLarge { .. }));
}

#[test]
fn accepts_unpadded_input() {
    let padded = encode("image/png", b"fo").expect("encodes");
    assert_eq!(padded.data, "Zm8=");

    let unpadded = payload("image/png", padded.data.trim_end_matches('='));
    assert_eq!(decode(&unpadded).expect("padding is optional"), b"fo");
}

#[test]
fn accepts_input_wrapped_across_lines() {
    let wrapped = payload("image/png", "Zm9v\nYmFy\n");
    assert_eq!(
        decode(&wrapped).expect("whitespace is not content"),
        b"foobar"
    );
}

#[test]
fn rejects_invalid_base64() {
    for data in ["!!!!", "Zm9v*", "Zm9v=", "Z", "Zm9vYmFy=", "====", "Zm=8"] {
        let error = decode(&payload("image/png", data)).expect_err("must be refused");
        assert!(
            matches!(error, ImageError::InvalidBase64 { .. }),
            "{data} produced {error:?}"
        );
    }
}

#[test]
fn rejects_non_ascii_input() {
    let error = decode(&payload("image/png", "Zm9vé")).expect_err("must be refused");
    assert!(matches!(error, ImageError::InvalidBase64 { .. }));
}

#[test]
fn rejects_a_declared_type_that_is_not_supported_even_when_the_data_is_valid() {
    let error = decode(&payload("image/svg+xml", "Zm9v")).expect_err("must be refused");
    assert_eq!(
        error,
        ImageError::UnsupportedType {
            got: "image/svg+xml".to_owned()
        }
    );
}

#[test]
fn round_trips_empty_data() {
    // Zero bytes is a legal encoding of zero bytes. It is also what an empty upload produces, and
    // failing on it here would make the empty case a special one for every caller.
    let encoded = encode("image/png", &[]).expect("the empty image is within every limit");
    assert_eq!(encoded.data, "");
    assert_eq!(decode(&encoded).expect("decodes"), Vec::<u8>::new());
}

#[test]
fn sniff_agrees_with_every_supported_type() {
    // A guard on the list itself: each entry must be a type the sniffer can actually produce, or the
    // two would drift and an admitted upload would have no describable type.
    let samples = [png_bytes(), jpeg_bytes(), gif_bytes(), webp_bytes()];
    for media_type in SUPPORTED_TYPES {
        assert!(
            samples
                .iter()
                .any(|bytes| sniff_type(bytes) == Some(media_type)),
            "{media_type} is supported but not sniffable"
        );
    }
}
