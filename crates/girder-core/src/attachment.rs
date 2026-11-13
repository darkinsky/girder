//! Attachment admission: validating uploads before anything is stored.
//!
//! Upstream's `attachment` split into a policy package and a local backend. The policy half is what
//! this module carries, and it is the half whose correctness matters: a store that trusts a declared
//! media type will happily hold a ZIP renamed to `.png`, and whatever later renders it may execute
//! rather than display.
//!
//! # Why the bytes are sniffed
//!
//! A caller's `mediaType` is a declaration, not evidence. Every check here derives the type from the
//! magic bytes and refuses when it disagrees with the declaration. This is not paranoia about
//! malicious callers — a browser reports the type from the file extension, so a legitimate upload of
//! a `.jpg` that is actually a PNG arrives mislabelled.
//!
//! # Why canonical base64
//!
//! Base64 has several spellings for the same bytes: padding may vary, and a decoder may accept
//! trailing garbage. Two uploads of identical content that encode differently are two different
//! stored blobs, which defeats deduplication and lets a caller smuggle bytes past a length check.
//! The re-encode comparison rejects every non-canonical form.
//!
//! # Bounds are aggregate, not just per-item
//!
//! A per-image limit alone lets a caller upload twenty images that are each legal and together
//! exhaust memory. The batch is bounded by count and by total bytes, checked before any of it is
//! stored so a refused batch leaves nothing behind.

use std::collections::BTreeSet;

use crate::scope::BoxFut;

/// The image media types this build accepts.
///
/// A closed set rather than a pattern: each type here has a decoder behind it, and a type without a
/// decoder is a stored blob nothing can display.
pub const ACCEPTED_IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

/// The default cap on one decoded image.
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

/// The default cap on one upload batch, by count.
pub const DEFAULT_MAX_IMAGES_PER_BATCH: usize = 20;

/// The default cap on one upload batch, by total decoded bytes.
pub const DEFAULT_MAX_BATCH_BYTES: u64 = 80 * 1024 * 1024;

/// One durable reference to a stored image.
///
/// An opaque id rather than a path or a URL: a path ties the reference to one host's filesystem, and
/// a URL is a bearer token by accident — anyone who sees it can fetch the image.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageRef {
    /// The storage identifier.
    pub id: String,
    /// The media type verified from the bytes.
    pub media_type: String,
    /// Exact decoded length.
    pub bytes: u64,
    /// The display name the caller supplied, never interpreted as a path.
    pub name: Option<String>,
}

/// An upload as it arrives on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedImage {
    /// Base64-encoded content.
    pub data: String,
    /// The caller's declared media type.
    pub media_type: String,
    /// Optional display name.
    pub name: Option<String>,
}

/// A decoded image ready to store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedImage {
    /// The raw bytes.
    pub data: Vec<u8>,
    /// The media type, verified from the bytes.
    pub media_type: String,
    /// The display name, if any.
    pub name: Option<String>,
}

/// A failure the attachment layer reports.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AttachmentError {
    /// The payload is not canonical base64.
    #[error("upload is not canonical base64")]
    NotCanonicalBase64,

    /// The payload is not base64 at all.
    #[error("upload is not valid base64")]
    InvalidBase64,

    /// The declared media type is not accepted.
    #[error("`{declared}` is not an accepted image type")]
    UnsupportedMediaType {
        /// What the caller declared.
        declared: String,
    },

    /// The bytes are not the type the caller declared.
    #[error("`{declared}` was declared but the bytes are {detected}")]
    MediaTypeMismatch {
        /// What the caller declared.
        declared: String,
        /// What the magic bytes say.
        detected: String,
    },

    /// The bytes are not a recognised image at all.
    #[error("the payload is not a recognised image")]
    NotAnImage,

    /// One image exceeds the size cap.
    #[error("image is {bytes} bytes, above the {limit}-byte limit")]
    ImageTooLarge {
        /// Its size.
        bytes: u64,
        /// The cap.
        limit: u64,
    },

    /// The batch has too many images.
    #[error("batch holds {count} images, above the {limit}-image limit")]
    TooManyImages {
        /// How many were sent.
        count: usize,
        /// The cap.
        limit: usize,
    },

    /// The batch's total decoded size exceeds the cap.
    #[error("batch totals {bytes} bytes, above the {limit}-byte limit")]
    BatchTooLarge {
        /// Total size.
        bytes: u64,
        /// The cap.
        limit: u64,
    },

    /// A storage id cannot be used as a path segment.
    #[error("`{id}` is not a usable storage id")]
    InvalidStorageId {
        /// The offending id.
        id: String,
    },
}

/// The bounds one batch is admitted under.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Cap on one decoded image.
    pub max_image_bytes: u64,
    /// Cap on batch count.
    pub max_images: usize,
    /// Cap on batch total bytes.
    pub max_batch_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            max_images: DEFAULT_MAX_IMAGES_PER_BATCH,
            max_batch_bytes: DEFAULT_MAX_BATCH_BYTES,
        }
    }
}

/// Detects an image type from its magic bytes.
///
/// Returns `None` for anything unrecognised, which is a refusal rather than a guess: storing a blob
/// whose type nobody knows means whatever later reads it has to guess too.
#[must_use]
pub fn detect_media_type(bytes: &[u8]) -> Option<&'static str> {
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    // JPEG: FF D8 FF
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    // GIF: "GIF87a" or "GIF89a"
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // WebP: "RIFF" <size> "WEBP"
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Decodes standard base64, refusing every non-canonical spelling.
///
/// # Errors
///
/// Returns [`AttachmentError::InvalidBase64`] for a payload that does not decode and
/// [`AttachmentError::NotCanonicalBase64`] for one that decodes but re-encodes differently.
pub fn decode_base64_canonical(data: &str) -> Result<Vec<u8>, AttachmentError> {
    let decoded = base64_engine::decode(data).map_err(|_| AttachmentError::InvalidBase64)?;
    // The re-encode comparison is what rejects padding variants and trailing garbage.
    if base64_engine::encode(&decoded) != data {
        return Err(AttachmentError::NotCanonicalBase64);
    }
    Ok(decoded)
}

/// Encodes bytes as standard base64 with padding.
///
/// Exposed so a caller — or a test — produces the canonical spelling the admission check expects,
/// rather than carrying a second implementation that could drift.
#[must_use]
pub fn encode_base64(input: &[u8]) -> String {
    base64_engine::encode(input)
}

/// A minimal base64 implementation.
///
/// Vendored rather than added as a dependency: the crate needs encode and decode over the standard
/// alphabet and nothing else, and a dependency for forty lines is a dependency to audit and keep
/// current.
mod base64_engine {
    /// Why a payload did not decode.
    #[derive(Debug, PartialEq, Eq)]
    pub struct DecodeError;

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    /// Decodes standard base64.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid alphabet character, a wrong length, or bad padding.
    pub fn decode(input: &str) -> Result<Vec<u8>, DecodeError> {
        let bytes = input.as_bytes();
        if bytes.len() % 4 != 0 {
            return Err(DecodeError);
        }
        if bytes.is_empty() {
            return Ok(Vec::new());
        }

        let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
        for chunk in bytes.chunks(4) {
            let mut values = [0u8; 4];
            let mut padding = 0;
            for (index, byte) in chunk.iter().enumerate() {
                if *byte == b'=' {
                    // Padding is only legal in the last two positions.
                    if index < 2 {
                        return Err(DecodeError);
                    }
                    padding += 1;
                    values[index] = 0;
                    continue;
                }
                if padding > 0 {
                    // A non-padding character after padding is not canonical.
                    return Err(DecodeError);
                }
                values[index] = decode_char(*byte)?;
            }
            out.push((values[0] << 2) | (values[1] >> 4));
            if padding < 2 {
                out.push((values[1] << 4) | (values[2] >> 2));
            }
            if padding < 1 {
                out.push((values[2] << 6) | values[3]);
            }
        }
        Ok(out)
    }

    /// Encodes to standard base64 with padding.
    #[must_use]
    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b0 = chunk[0];
            let b1 = chunk.get(1).copied().unwrap_or(0);
            let b2 = chunk.get(2).copied().unwrap_or(0);
            out.push(ALPHABET[usize::from(b0 >> 2)] as char);
            out.push(ALPHABET[usize::from(((b0 & 0x03) << 4) | (b1 >> 4))] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[usize::from(((b1 & 0x0f) << 2) | (b2 >> 6))] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[usize::from(b2 & 0x3f)] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    fn decode_char(byte: u8) -> Result<u8, DecodeError> {
        match byte {
            b'A'..=b'Z' => Ok(byte - b'A'),
            b'a'..=b'z' => Ok(byte - b'a' + 26),
            b'0'..=b'9' => Ok(byte - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(DecodeError),
        }
    }
}

/// Admits one upload: decodes, sniffs, and checks every bound.
///
/// # Errors
///
/// Returns the first problem found. Unlike config validation, an upload is refused wholesale rather
/// than reported in full: the caller's next step is to re-encode the file, not to fix several fields.
pub fn admit_image(image: &EncodedImage, limits: &Limits) -> Result<DecodedImage, AttachmentError> {
    if !ACCEPTED_IMAGE_TYPES.contains(&image.media_type.as_str()) {
        return Err(AttachmentError::UnsupportedMediaType {
            declared: image.media_type.clone(),
        });
    }

    let data = decode_base64_canonical(&image.data)?;
    let bytes = u64::try_from(data.len()).unwrap_or(u64::MAX);
    if bytes > limits.max_image_bytes {
        return Err(AttachmentError::ImageTooLarge {
            bytes,
            limit: limits.max_image_bytes,
        });
    }

    // The declaration is checked against the bytes rather than trusted: a browser reports the type
    // from the extension, so a legitimate upload can arrive mislabelled.
    let Some(detected) = detect_media_type(&data) else {
        return Err(AttachmentError::NotAnImage);
    };
    if detected != image.media_type {
        return Err(AttachmentError::MediaTypeMismatch {
            declared: image.media_type.clone(),
            detected: detected.to_owned(),
        });
    }

    Ok(DecodedImage {
        data,
        media_type: detected.to_owned(),
        name: image.name.clone(),
    })
}

/// Admits a whole batch, checking count and total size before storing anything.
///
/// # Errors
///
/// Returns [`AttachmentError::TooManyImages`] or [`AttachmentError::BatchTooLarge`] for a refused
/// batch, and whatever [`admit_image`] reports for a bad member.
pub fn admit_batch(
    images: &[EncodedImage],
    limits: &Limits,
) -> Result<Vec<DecodedImage>, AttachmentError> {
    if images.len() > limits.max_images {
        return Err(AttachmentError::TooManyImages {
            count: images.len(),
            limit: limits.max_images,
        });
    }

    let mut decoded = Vec::with_capacity(images.len());
    let mut total: u64 = 0;
    for image in images {
        let admitted = admit_image(image, limits)?;
        total = total.saturating_add(u64::try_from(admitted.data.len()).unwrap_or(u64::MAX));
        // Checked as it accumulates, so an oversized batch is refused before the last image is
        // decoded rather than after.
        if total > limits.max_batch_bytes {
            return Err(AttachmentError::BatchTooLarge {
                bytes: total,
                limit: limits.max_batch_bytes,
            });
        }
        decoded.push(admitted);
    }
    Ok(decoded)
}

/// Names an attachment for storage.
///
/// A caller-supplied id rather than a fresh random one, so the store can be tested and so a caller
/// that already has an id does not create a second copy of the same image.
#[must_use]
pub fn storage_key(id: &str) -> Option<String> {
    // The id becomes a path segment in a file-backed store, so it is restricted to an alphabet that
    // cannot escape one. An unsafe id is refused rather than escaped: two spellings of one id would
    // be two blobs.
    let safe = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    safe.then(|| id.to_owned())
}

/// An in-memory store, for a caller that does not need durability.
#[derive(Default)]
pub struct MemoryStore {
    images: std::sync::Mutex<std::collections::BTreeMap<String, DecodedImage>>,
    /// How many images it will hold.
    ///
    /// A store with no bound is a memory leak with an API: every upload is retained for the life of the process, and
    /// nothing in the attachment path ever removes one. The bound turns an unbounded leak into a refusal a client can
    /// read.
    capacity: usize,
}

/// How many images a store holds when nobody says otherwise.
///
/// A hundred: more than a conversation plausibly references, few enough that a decoded image is a manageable fraction
/// of the process.
pub const DEFAULT_CAPACITY: usize = 100;

impl MemoryStore {
    /// Creates an empty store holding the default number of images.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Creates an empty store holding at most `capacity` images.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            images: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            capacity: capacity.max(1),
        }
    }

    /// How many it will hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many it holds now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.images
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether it holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.images
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Stores one decoded image, returning its reference.
    ///
    /// Storing the same id twice replaces it, so a re-upload of identical content converges rather
    /// than accumulating.
    pub fn put(&self, id: &str, image: DecodedImage) -> Option<ImageRef> {
        let key = storage_key(id)?;
        let reference = ImageRef {
            id: key.clone(),
            media_type: image.media_type.clone(),
            bytes: u64::try_from(image.data.len()).unwrap_or(u64::MAX),
            name: image.name.clone(),
        };
        let mut images = self
            .images
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Replacing an id that is already stored is always allowed: the bound is on distinct images, and a re-upload of
        // the same content converging rather than accumulating is the behaviour the id is for.
        if !images.contains_key(&key) && images.len() >= self.capacity {
            return None;
        }
        images.insert(key, image);
        Some(reference)
    }

    /// Reads one image back.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<DecodedImage> {
        self.images
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// The ids currently held.
    #[must_use]
    pub fn ids(&self) -> BTreeSet<String> {
        self.images
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
}

/// Stores an admitted batch, returning references in caller order.
///
/// # Errors
///
/// Returns [`AttachmentError::InvalidStorageId`] when an id cannot be used as a storage key.
pub fn store_batch(
    store: &MemoryStore,
    ids: &[&str],
    images: Vec<DecodedImage>,
) -> Result<Vec<ImageRef>, AttachmentError> {
    let mut refs = Vec::with_capacity(images.len());
    for (id, image) in ids.iter().zip(images) {
        let reference = store
            .put(id, image)
            .ok_or_else(|| AttachmentError::InvalidStorageId {
                id: (*id).to_owned(),
            })?;
        refs.push(reference);
    }
    Ok(refs)
}

/// The seam a durable store implements.
pub trait AttachmentStore: Send + Sync {
    /// Saves a decoded image under `id`.
    fn save(&self, id: &str, image: DecodedImage) -> BoxFut<Option<ImageRef>>;
    /// Loads an image by id.
    fn load(&self, id: &str) -> BoxFut<Option<DecodedImage>>;
}
