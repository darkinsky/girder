//! An image in the shape a provider expects to receive it.
//!
//! `attachment.rs` answers a different question: may this upload be stored, and what handle stands
//! for it later? Its answer is an opaque reference — the bytes stay in the store and the
//! conversation carries an id. A provider-facing image is the opposite shape. There is no reference
//! to resolve on the far side of the wire, so the bytes themselves have to travel, base64-encoded,
//! inside the request body.
//!
//! That difference is why this module exists rather than a method on the attachment store: one is a
//! lifetime question about stored bytes, the other is a wire-format question about bytes leaving the
//! process. Today every `ContentBlock::Image` reaches a provider as a text placeholder, which is a
//! placeholder precisely because nothing produced this wire shape yet.
//!
//! # Why the type comes from the bytes
//!
//! A file name is chosen by whoever created the file, and a client that says `photo.png` is stating
//! an intention, not a fact. Renaming a GIF to `.png` is one keystroke, and the provider's decoder
//! reads the header rather than the name, so a payload built from the name alone would be rejected
//! upstream with an error that points at the user's list of attachments instead of the file itself.
//! [`sniff_type`] therefore reads magic numbers, and the extension is never an input to it.
//!
//! # Why the supported list is closed
//!
//! The four types in [`SUPPORTED_TYPES`] are the intersection of what the providers accept. Anything
//! else is refused here, at the edge, because the alternative is a request that is well-formed on
//! this side and rejected on the other — after the bytes have been paid for in tokens and latency.
//! A `.svg` is the case that matters: it is text, it can carry script, and it is not on the list.
//!
//! # Why encoding is stricter than decoding
//!
//! [`encode`] always writes standard, padded base64, because that is the only spelling that is
//! unambiguous on the wire. [`decode`] additionally accepts the unpadded spelling, because some
//! clients strip the trailing `=` and a payload that is otherwise perfect should not fail over four
//! bytes of padding. What decoding does not accept is the URL-safe alphabet, padding in the middle,
//! or non-zero trailing bits: each of those decodes to the same bytes under a lenient parser and to
//! different bytes under a strict one, and a silent disagreement about what the image *is* is worse
//! than an error saying the payload is malformed.
//!
//! # Why the size cap lives here
//!
//! Base64 inflates a payload by a third, so a limit checked after encoding is a limit on the wrong
//! quantity. The cap is checked against the raw bytes on the way out and against the length implied
//! by the encoding on the way in — the second check runs before anything is allocated, so a hostile
//! length cannot force a buffer larger than the cap.

use girder_core::{decode_base64_canonical, encode_base64};

/// The media types this module will carry.
///
/// Closed by design. The values are spelled exactly as the providers spell them, because both
/// providers match the string literally and neither treats the type as a hint.
pub const SUPPORTED_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The largest image, in bytes, that may cross this boundary.
///
/// Measured before base64 encoding, since that is the quantity a caller can reason about — it is
/// the size of the file they have. Five mebibytes is the per-image ceiling the providers publish,
/// and keeping one copy of it here stops each call site from picking its own.
pub const MAX_BYTES: usize = 5 * 1024 * 1024;

/// One image ready to be embedded in a request.
///
/// The two fields are the wire fields, not a comfortable Rust shape: `data` is base64 text rather
/// than bytes because re-encoding it at the point of use is the step that gets skipped under time
/// pressure, and skipping it sends a JSON string containing raw binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePayload {
    /// The media type, one of [`SUPPORTED_TYPES`].
    pub media_type: String,
    /// Standard base64, padded.
    pub data: String,
}

/// Why an image could not be turned into a payload, or a payload back into an image.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImageError {
    /// The type is not one the providers accept.
    ///
    /// The value is the type as the caller declared it, so the message quotes what was actually
    /// passed rather than a normalised form the caller would not recognise.
    #[error("unsupported image media type: {got}")]
    UnsupportedType {
        /// The declared type.
        got: String,
    },
    /// The base64 text does not decode.
    ///
    /// `detail` names the specific defect instead of repeating "invalid" — a payload that is the
    /// wrong length and one that contains a stray character need different fixes.
    #[error("invalid base64 image payload: {detail}")]
    InvalidBase64 {
        /// What was wrong with it.
        detail: String,
    },
    /// The image is larger than [`MAX_BYTES`].
    #[error("image is {bytes} bytes, over the {max}-byte limit")]
    TooLarge {
        /// The size that was measured.
        bytes: usize,
        /// The ceiling it exceeded.
        max: usize,
    },
}

/// Encodes raw image bytes into a payload.
///
/// The declared type is normalised — case and parameters are dropped — because `IMAGE/PNG` and
/// `image/png; name=cat` name the same type, and refusing either over its spelling would fail a
/// request that is actually correct. The normalised value is what ends up on the wire.
///
/// The bytes are not sniffed here. The declared type is the caller's statement about bytes they
/// already hold, and the callers that have it are the ones that ran [`sniff_type`] while admitting
/// the upload. Re-sniffing would turn a disagreement into an error at a layer with no way to
/// explain it; a caller that wants the check can compare [`sniff_type`] with the type it passed.
///
/// # Errors
///
/// Returns [`ImageError::UnsupportedType`] when the type is not in [`SUPPORTED_TYPES`], and
/// [`ImageError::TooLarge`] when the bytes exceed [`MAX_BYTES`] — the size is checked before
/// encoding, so an oversized input is never turned into a larger string first.
pub fn encode(media_type: &str, bytes: &[u8]) -> Result<ImagePayload, ImageError> {
    let canonical = canonical_media_type(media_type);
    if !SUPPORTED_TYPES.contains(&canonical.as_str()) {
        return Err(ImageError::UnsupportedType {
            got: media_type.trim().to_owned(),
        });
    }
    if bytes.len() > MAX_BYTES {
        return Err(ImageError::TooLarge {
            bytes: bytes.len(),
            max: MAX_BYTES,
        });
    }
    Ok(ImagePayload {
        media_type: canonical,
        data: encode_base64(bytes),
    })
}

/// Decodes a payload back into raw bytes.
///
/// Padding is optional in the input, and whitespace — including the line breaks some clients insert
/// — is ignored, since neither changes which bytes the text stands for. Everything else about the
/// spelling is required.
///
/// # Errors
///
/// Returns [`ImageError::UnsupportedType`] when the payload declares a type outside
/// [`SUPPORTED_TYPES`], [`ImageError::InvalidBase64`] when the text is not standard base64, and
/// [`ImageError::TooLarge`] when the encoding implies more than [`MAX_BYTES`] — the last one is
/// decided from the text length alone, before a buffer is allocated.
pub fn decode(payload: &ImagePayload) -> Result<Vec<u8>, ImageError> {
    let canonical = canonical_media_type(&payload.media_type);
    if !SUPPORTED_TYPES.contains(&canonical.as_str()) {
        return Err(ImageError::UnsupportedType {
            got: payload.media_type.clone(),
        });
    }

    let unpadded = strip_padding(&payload.data)?;
    let size = decoded_len(unpadded.len()).ok_or_else(|| ImageError::InvalidBase64 {
        detail: "length is not a whole number of base64 quanta".to_owned(),
    })?;
    if size > MAX_BYTES {
        return Err(ImageError::TooLarge {
            bytes: size,
            max: MAX_BYTES,
        });
    }

    let mut padded = unpadded;
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    // The shared decoder re-encodes what it produced and compares, which is what rejects non-zero
    // trailing bits: dropping them decodes fine but does not re-encode to the same text.
    decode_base64_canonical(&padded).map_err(|error| ImageError::InvalidBase64 {
        detail: error.to_string(),
    })
}

/// Identifies an image from its first bytes.
///
/// Returns `None` for anything unrecognised, which is a refusal rather than a guess: a caller that
/// got `None` has no type to declare, and the alternative — defaulting to `image/png` — produces a
/// request the provider rejects for the wrong reason.
///
/// The extension is deliberately not an argument. See the module docs for why.
#[must_use]
pub fn sniff_type(bytes: &[u8]) -> Option<&'static str> {
    // PNG: 89 50 4E 47. The remaining four bytes of the signature are a fixed CRLF/EOF pattern that
    // no other format begins with, so they add no discrimination — and demanding them would reject
    // a stream that is truncated after a valid header.
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some("image/png");
    }
    // JPEG: FF D8 FF opens every variant, from baseline JFIF to EXIF-carrying camera output.
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    // GIF: "GIF8", which covers both the 87a and 89a revisions.
    if bytes.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    // WebP: "RIFF", a little-endian chunk size, then "WEBP". The fourcc at offset 8 is what
    // separates it from the other RIFF containers (WAV, AVI), which are audio and video.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Reduces a declared type to the form the providers compare against.
///
/// Parameters are dropped because they describe transfer details, not the type's identity, and a
/// caller forwarding a header such as `image/png; charset=binary` means PNG.
fn canonical_media_type(media_type: &str) -> String {
    let base = media_type.split(';').next().unwrap_or(media_type);
    base.trim().to_ascii_lowercase()
}

/// Returns the text without padding, having verified that the padding it had was legal.
///
/// The padding policy is stated once, here: at most two `=`, only at the end, and exactly the number
/// the preceding length requires. Accepting any other count would mean accepting text whose decoded
/// length is ambiguous, which is what leaves two implementations disagreeing about the same bytes.
fn strip_padding(data: &str) -> Result<String, ImageError> {
    if !data.is_ascii() {
        return Err(ImageError::InvalidBase64 {
            detail: "payload contains non-ASCII characters".to_owned(),
        });
    }

    // Whitespace is dropped first: a wrapped payload is still the same payload, and the wrapping is
    // inserted by transports that never intended to change the value.
    let compact: String = data
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();

    let unpadded = compact.trim_end_matches('=');
    if unpadded.contains('=') {
        return Err(ImageError::InvalidBase64 {
            detail: "padding appears before the end of the payload".to_owned(),
        });
    }
    let padding = compact.len() - unpadded.len();
    if padding > 2 {
        return Err(ImageError::InvalidBase64 {
            detail: "more than two padding characters".to_owned(),
        });
    }
    if padding > 0 && compact.len() % 4 != 0 {
        return Err(ImageError::InvalidBase64 {
            detail: "a padded payload must be a whole number of four-character quanta".to_owned(),
        });
    }
    // Two leftover characters encode one byte, three encode two; a single one cannot encode anything,
    // so it is a truncated payload rather than an unpadded one.
    if unpadded.len() % 4 == 1 {
        return Err(ImageError::InvalidBase64 {
            detail: "payload ends with a single leftover character".to_owned(),
        });
    }
    if padding > 0 {
        let required = match unpadded.len() % 4 {
            2 => 2,
            3 => 1,
            _ => 0,
        };
        if padding != required {
            return Err(ImageError::InvalidBase64 {
                detail: format!("{padding} padding characters where {required} are required"),
            });
        }
    }
    Ok(unpadded.to_owned())
}

/// The number of bytes an unpadded base64 text stands for.
///
/// `None` for a length that cannot occur, which is how the one-character case is caught before any
/// decoding is attempted.
#[must_use]
const fn decoded_len(unpadded_len: usize) -> Option<usize> {
    let quanta = unpadded_len / 4;
    match unpadded_len % 4 {
        0 => Some(quanta * 3),
        2 => Some(quanta * 3 + 1),
        3 => Some(quanta * 3 + 2),
        _ => None,
    }
}
