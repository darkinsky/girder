//! The upload half of `session.attachment`: the write method the read method never had.
//!
//! [`crate::attachment`] carries admission (sniff, bounds, canonical base64) and an in-memory store,
//! but nothing turned an encoded upload into a stored image with an id. That left `session.attachment`
//! able to read an image no caller could ever have put there. This module is the missing entrance:
//! it decodes, admits, stores, and answers with the reference the read method takes.
//!
//! # Why the id is minted here
//!
//! [`MemoryStore::put`] takes the id from its caller, so the caller decides identity. Content hashing
//! would deduplicate identical bytes, which sounds attractive until two uploads of one screenshot
//! collapse into one id and a later overwrite silently changes what an earlier reference means. This
//! method instead mints a fresh id per call, so a reference always names one upload.
//!
//! # Why refusal is `InvalidArguments`
//!
//! The neighbouring `session.attachment` reports a bad or unknown reference as
//! [`RemoteError::InvalidArguments`], and a decoded-but-unusable upload is the same class of problem:
//! the caller's input was wrong and re-encoding the file is the fix. `Failed` would read as "the
//! server broke", which would send a client into a retry loop that can never succeed.
//!
//! # Why the paths go through `girder_core::`
//!
//! The crate aliases itself as `girder_core`, and the integration test includes this file by path
//! because wiring the module into `lib.rs` belongs to a different change. Spelling the paths through
//! the crate root keeps one copy of them working from inside the crate and from the test crate.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use girder_core::{
    AttachmentLimits, EncodedImage, MemoryStore, RemoteError, RemoteRegistry, admit_image,
};

/// The namespace the method is served under, matching the read method.
const NAMESPACE: &str = "session";

/// The method name a client calls.
const METHOD: &str = "uploadAttachment";

/// Disambiguates two ids minted in the same millisecond.
///
/// A counter rather than randomness: it needs no dependency, it cannot repeat within the process that
/// owns the store, and the ids stay short enough to read in a log.
static NEXT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Registers `session.uploadAttachment`.
///
/// The store is captured rather than passed per call: a caller chooser of destination could store an
/// image where the read method is not looking, and the upload would look lost.
pub fn register_attachment_upload_method(registry: &mut RemoteRegistry, store: &Arc<MemoryStore>) {
    let store = Arc::clone(store);
    registry.register(NAMESPACE, METHOD, move |call| {
        let store = Arc::clone(&store);
        Box::pin(async move {
            let data = required_str(&call.args, "data")?;
            let media_type = required_str(&call.args, "mediaType")?;
            let name = optional_name(&call.args)?;

            let limits = AttachmentLimits::default();
            // Checked before decoding, not after. `admit_image` bounds the decoded size, but by then a
            // hostile `data` has already been expanded into memory; the encoded length is a cheap
            // upper bound on what decoding would allocate. The bound is the same limit, so this only
            // rejects payloads that admission would have refused anyway.
            let max_encoded = u64::try_from(data.len()).unwrap_or(u64::MAX);
            let encoded_limit = (limits.max_image_bytes / 3)
                .saturating_mul(4)
                .saturating_add(4);
            if max_encoded > encoded_limit {
                return Err(invalid(format!(
                    "`data` encodes more than the {}-byte image limit",
                    limits.max_image_bytes
                )));
            }

            let encoded = EncodedImage {
                data: data.to_owned(),
                media_type: media_type.to_owned(),
                name,
            };
            // Admission is the security boundary: it derives the type from the magic bytes and refuses
            // a declaration that disagrees, so a ZIP renamed `.png` never reaches the store.
            let decoded = admit_image(&encoded, &limits)
                .map_err(|error| invalid(format!("`data` is not an acceptable image: {error}")))?;

            let id = allocate_id();
            let Some(reference) = store.put(&id, decoded) else {
                // Unreachable: every id `allocate_id` builds is alphanumeric with `-` separators, and
                // `storage_key` accepts exactly that. Reported rather than asserted so a future change
                // to the id format surfaces as a clear refusal instead of a panic in a request.
                return Err(invalid(format!("`{id}` is not a usable storage id")));
            };

            Ok(json!({
                "attachmentId": reference.id,
                // The verified type, not the caller's declaration: admission already refused a
                // mismatch, so echoing the claim would hide which type the bytes actually are.
                "mediaType": reference.media_type,
                "bytes": reference.bytes,
                // Absent names come back as `null`, matching the read method rather than inventing an
                // empty string that a client would render as a blank filename.
                "name": reference.name,
            }))
        })
    });
}

/// Mints a storage id for one upload.
///
/// A timestamp plus a per-process counter, hex-encoded. The timestamp keeps ids roughly ordered so a
/// log reads chronologically; the counter is what actually guarantees uniqueness inside the process
/// that owns the store, including two uploads in the same millisecond. No process id is mixed in
/// because [`MemoryStore`] is per-process — ids from two processes never meet — and a content hash is
/// rejected on purpose: identical bytes must still be distinguishable uploads, or a reference would
/// change meaning when the same image arrived twice.
fn allocate_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        // A clock set before the epoch yields zero rather than an error: the counter alone still keeps
        // ids unique, so an odd timestamp must not turn an upload into a refusal.
        .map_or(0, |elapsed| elapsed.as_millis());
    let sequence = NEXT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("att-{millis:x}-{sequence:x}")
}

/// Reads a required string field.
fn required_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, RemoteError> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("`{field}` must be a string")))
}

/// Reads the optional display name.
///
/// A blank name is treated as absent: it is what a client sends for "no filename", and storing it
/// would render as an empty label rather than nothing.
fn optional_name(args: &Value) -> Result<Option<String>, RemoteError> {
    match args.get("name") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) if name.trim().is_empty() => Ok(None),
        Some(Value::String(name)) => Ok(Some(name.clone())),
        Some(_) => Err(invalid("`name` must be a string")),
    }
}

/// Builds a refusal for this method.
fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: METHOD.to_owned(),
        message: message.into(),
    }
}
