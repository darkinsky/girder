//! A portable session bundle: the machine-readable counterpart to `session.export`.
//!
//! `session.export` writes a transcript for a *reader*. Its Markdown form drops system prompts,
//! model selections, usage, and retries because they would drown the conversation, and it can cut
//! the body at a byte budget because a human reading a long transcript tolerates a truncation
//! notice. Its JSON form keeps the events but wraps them in a response envelope with a `truncated`
//! flag and no header saying what it is.
//!
//! Neither can carry a session into another instance. Restoring a session needs every event, all of
//! them, plus enough header to know whether the reader understands the shape before it parses a
//! thousand objects and half-applies them. That is this module: the bundle is a *transport* format,
//! not a display format.
//!
//! # Why the envelope has a version
//!
//! The event vocabulary grows. A future bundle may carry a variant this build has never heard of,
//! and an importer that found an unknown `type` tag could only guess: refuse the whole bundle, or
//! skip the event and silently restore a session with a hole. A version field makes that a decision
//! instead of a guess — a bundle numbered above [`VERSION`] is refused before any event is touched,
//! so the failure names the format rather than one arbitrary event.
//!
//! # Why import validates every event
//!
//! Checking only that `events` is an array of objects proves nothing about whether the session can
//! be used. `{"type":"user/message"}` is valid JSON and an invalid event: the missing `message`
//! field would surface as a deserialization error on the first turn that replayed it, long after
//! the import reported success and the caller deleted its source. So every event is parsed into a
//! [`SessionEvent`] during import, and a bundle holding an event this build cannot reconstruct is
//! rejected whole rather than partially accepted.
//!
//! # Why errors carry the index
//!
//! A bundle is a thousand events in one line of code at the call site. "The bundle is invalid" is
//! not an actionable report: the caller cannot tell a corrupt transfer from a vocabulary mismatch,
//! and cannot find the one event that is wrong. [`BundleError::InvalidEvent`] names its position so
//! a reader can go straight to it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::SessionEvent;

/// The format version this build writes and accepts.
///
/// Bump it when the envelope or the event vocabulary changes incompatibly, and teach the importer
/// to read the previous version in the same change. A reader accepts only the versions it knows;
/// see [`import`].
pub const VERSION: u32 = 1;

/// A portable session package.
///
/// Kept as `serde_json::Value` for the events rather than `Vec<SessionEvent>` so that import can
/// report *which* event failed. Deserializing straight into the typed vector would collapse a bad
/// event into one opaque syntax error with no position, which is the report this module exists to
/// avoid. The events are validated into [`SessionEvent`] by [`import`] before a caller sees them;
/// the untyped form is the intermediate representation, not the contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bundle {
    /// The format version, so a future reader knows what it is looking at.
    pub version: u32,
    /// When the bundle was exported, in milliseconds since the Unix epoch.
    ///
    /// Carried for provenance only: import never consults it, so a bundle written by a machine with
    /// a skewed clock still restores.
    pub exported_at: u64,
    /// The session title, when the source had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The events, in log order.
    ///
    /// Order is the content: the log is the session, and a bundle that sorted or deduplicated its
    /// events would restore a conversation that never happened.
    pub events: Vec<Value>,
}

/// Why a bundle could not be exported or imported.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BundleError {
    /// The input was not valid JSON, or not a JSON object of the expected shape.
    #[error("the bundle is not valid JSON for a session package: {detail}")]
    InvalidJson {
        /// What the parser objected to, kept verbatim so the caller can see the position.
        detail: String,
    },

    /// The bundle's format version is not one this build reads.
    #[error("bundle format version {found} is not supported; this build reads version {supported}")]
    UnsupportedVersion {
        /// The version the bundle declares.
        found: u32,
        /// The version this build writes and reads.
        supported: u32,
    },

    /// One event in the bundle could not be reconstructed as a [`SessionEvent`].
    #[error("event {index} in the bundle is not a valid session event: {detail}")]
    InvalidEvent {
        /// Zero-based position within [`Bundle::events`], so the offending entry is findable.
        index: usize,
        /// The deserializer's complaint, which usually names the missing or mistyped field.
        detail: String,
    },
}

impl Bundle {
    /// Parses the bundled events into the typed vocabulary.
    ///
    /// Separate from [`import`] because the two questions are separate: `import` asks whether this
    /// is a bundle this build understands, and this asks what its events are. A caller that wants
    /// to inspect a bundle without restoring it — to show a preview, say — can stop at `import`.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError::InvalidEvent`] naming the first event that does not parse. The first
    /// rather than all of them: one bad event means the bundle cannot be restored, and listing a
    /// thousand positions would bury the one that has to be fixed.
    pub fn events(&self) -> Result<Vec<SessionEvent>, BundleError> {
        let mut parsed = Vec::with_capacity(self.events.len());
        for (index, value) in self.events.iter().enumerate() {
            let event = serde_json::from_value::<SessionEvent>(value.clone()).map_err(|error| {
                BundleError::InvalidEvent {
                    index,
                    detail: error.to_string(),
                }
            })?;
            parsed.push(event);
        }
        Ok(parsed)
    }
}

/// Packs a session into a bundle string.
///
/// `title` is optional because a session may not have been named yet, and an empty string is not
/// the same thing as a missing title to a UI that renders one or nothing. `now` is a parameter
/// rather than a call to the clock so the exporter is deterministic: a test asserts the exact
/// bytes, and a caller that batches exports can stamp them all with the same instant.
///
/// The events are serialized individually and collected as values, which is what lets [`import`]
/// report an index later. Exporting re-validates nothing: `events` are typed, so they are known
/// good, and a bundle this function wrote imports unless the bytes were damaged in transit.
///
/// # Errors
///
/// Returns [`BundleError::InvalidJson`] when an event cannot be represented as JSON. This is not
/// expected for the current vocabulary — every variant is plain data — but a future variant holding
/// something JSON cannot represent would surface here rather than as a silently dropped event.
pub fn export(
    events: &[SessionEvent],
    title: Option<&str>,
    now: u64,
) -> Result<String, BundleError> {
    let mut packed = Vec::with_capacity(events.len());
    for event in events {
        let value = serde_json::to_value(event).map_err(|error| BundleError::InvalidJson {
            detail: error.to_string(),
        })?;
        packed.push(value);
    }

    let bundle = Bundle {
        version: VERSION,
        exported_at: now,
        title: title.map(ToOwned::to_owned),
        events: packed,
    };

    serde_json::to_string(&bundle).map_err(|error| BundleError::InvalidJson {
        detail: error.to_string(),
    })
}

/// Reads a bundle string and checks that this build can restore it.
///
/// The version is checked *before* the events, and before the events are even parsed into
/// [`Bundle::events`] beyond their JSON shape. That ordering is the point of the version: a bundle
/// from a newer build is refused with a message about the format, not with a complaint about some
/// event whose variant this build happens to lack.
///
/// The returned bundle's events are still untyped. Call [`Bundle::events`] to validate and obtain
/// the [`SessionEvent`]s; a caller that only previews a bundle never needs to.
///
/// # Errors
///
/// - [`BundleError::InvalidJson`] when the input is not a JSON object of this shape, including the
///   case where `version` is absent: a bundle without a version cannot be checked, and treating it
///   as version 1 would accept a format nobody declared.
/// - [`BundleError::UnsupportedVersion`] when the version is not [`VERSION`].
pub fn import(bundle: &str) -> Result<Bundle, BundleError> {
    let parsed = serde_json::from_str::<Bundle>(bundle).map_err(|error| {
        BundleError::InvalidJson {
            detail: error.to_string(),
        }
    })?;

    if parsed.version != VERSION {
        return Err(BundleError::UnsupportedVersion {
            found: parsed.version,
            supported: VERSION,
        });
    }

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_zero_is_refused_even_though_it_parses() {
        // The guard exists for values that are *well-formed* and still wrong, which is the case a
        // shape check would pass.
        let error = import(r#"{"version":0,"exported_at":0,"events":[]}"#).unwrap_err();
        assert_eq!(
            error,
            BundleError::UnsupportedVersion {
                found: 0,
                supported: VERSION
            }
        );
    }

    #[test]
    fn a_bundle_without_a_version_is_not_treated_as_version_one() {
        let error = import(r#"{"exported_at":0,"events":[]}"#).unwrap_err();
        assert!(matches!(error, BundleError::InvalidJson { .. }), "{error}");
    }
}
