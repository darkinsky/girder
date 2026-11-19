//! Message feedback: the fold over `feedback/*` events, and the three endpoints that write them.
//!
//! # What upstream has
//!
//! `packages/feedback/message-feedback/src/index.ts` (313 lines) marks three methods:
//!
//! | Endpoint | Answers |
//! | --- | --- |
//! | `list` | every item currently recorded for one session |
//! | `put` | records or replaces one message's feedback |
//! | `delete` | withdraws one message's feedback |
//!
//! # Why feedback is a log event
//!
//! Upstream appends `feedback/message-put` and `feedback/message-delete` to the session log rather
//! than writing a side table. That is the load-bearing decision: feedback belongs to a message, and a
//! store beside the log would be lost by anything that preserved the conversation -- a fork would
//! carry the messages and not the ratings, and a compaction would keep the summary and drop them.
//!
//! The fold is a `BTreeMap` replay, so the *last* put for a message wins and a delete removes it.
//! Upstream's `currentItems` does the same walk; deriving the current set from the log rather than
//! keeping a mutable copy is what makes a replay and a live read agree.
//!
//! # What this port refuses
//!
//! Upstream validates the note is non-blank and within a byte cap, and reports both as structured
//! refusals rather than errors. They are [`Refusal`] here: a caller asks "did it save", and "your
//! note was blank" is an answer rather than a failure of the call.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::remote::{RemoteCall, RemoteError, RemoteRegistry};
use crate::session::SessionEvent;

/// The namespace these methods register under, as upstream spells it.
pub const NAMESPACE: &str = "messageFeedback";

/// The largest note a caller may attach.
///
/// Upstream's cap. A note is read by a person reviewing the conversation, and a note larger than
/// this is an essay attached to a thumbs-down.
pub const MAX_NOTE_BYTES: usize = 2_000;

/// Whether feedback was positive or negative.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rating {
    /// A thumbs-up.
    Positive,
    /// A thumbs-down.
    Negative,
}

/// The categories upstream accepts.
pub const CATEGORIES: &[&str] = &["incorrect", "incomplete", "unhelpful", "unsafe", "other"];

/// One message's current feedback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// The message it is about.
    pub message_id: String,
    /// Whether it was positive or negative.
    pub rating: Rating,
    /// A note, when one was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// A category, when one was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// The item's identity, so a replacement is recognisable as one.
    pub version: String,
    /// When it was first recorded.
    pub created_at: i64,
    /// When it last changed.
    pub updated_at: i64,
}

/// Why a write was refused.
///
/// A refusal is an answer, not an error: the call reached the server and the server said no. Upstream
/// returns these as `ok: false` results with a code, and a client branches on the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The note was only whitespace.
    NoteBlank,
    /// The note was past [`MAX_NOTE_BYTES`].
    NoteTooLarge,
    /// The category is not one of [`CATEGORIES`].
    UnknownCategory,
    /// The message id was empty.
    MessageIdBlank,
}

impl Refusal {
    /// The wire code, matching upstream's discriminator.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NoteBlank => "note_blank",
            Self::NoteTooLarge => "note_too_large",
            Self::UnknownCategory => "unknown_category",
            Self::MessageIdBlank => "message_id_blank",
        }
    }

    /// A sentence a person can act on.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::NoteBlank => "a note must not be only whitespace; leave it out instead",
            Self::NoteTooLarge => "a note must be 2000 bytes or fewer",
            Self::UnknownCategory => "the category is not one this build recognises",
            Self::MessageIdBlank => "a feedback item needs a message id",
        }
    }
}

/// Validates the note and category a caller supplied.
///
/// # Errors
///
/// The first refusal, in the order a person would fix them: a missing id, then a blank note, then a
/// note too large, then an unknown category.
pub fn validate(
    message_id: &str,
    note: Option<&str>,
    category: Option<&str>,
) -> Result<(), Refusal> {
    if message_id.trim().is_empty() {
        return Err(Refusal::MessageIdBlank);
    }
    if let Some(note) = note {
        // Checked in this order so `"   "` reports blank rather than too large when both would
        // apply: the blank reason is the one the person can act on.
        if note.trim().is_empty() {
            return Err(Refusal::NoteBlank);
        }
        if note.len() > MAX_NOTE_BYTES {
            return Err(Refusal::NoteTooLarge);
        }
    }
    if let Some(category) = category {
        if !CATEGORIES.contains(&category) {
            return Err(Refusal::UnknownCategory);
        }
    }
    Ok(())
}

/// The feedback currently recorded for one session, by replaying its events.
///
/// The last put for a message wins, and a delete removes it -- the same walk upstream's
/// `currentItems` performs. Deriving the set rather than keeping a mutable copy is what makes a
/// replay and a live read agree.
#[must_use]
pub fn current_items(events: &[SessionEvent]) -> BTreeMap<String, Item> {
    let mut items: BTreeMap<String, Item> = BTreeMap::new();
    for event in events {
        match event {
            SessionEvent::MessageFeedbackPut {
                message_id,
                rating,
                note,
                category,
                version,
                created_at,
                updated_at,
            } => {
                items.insert(
                    message_id.clone(),
                    Item {
                        message_id: message_id.clone(),
                        rating: *rating,
                        note: note.clone(),
                        category: category.clone(),
                        version: version.clone(),
                        created_at: *created_at,
                        updated_at: *updated_at,
                    },
                );
            }
            SessionEvent::MessageFeedbackDelete { message_id } => {
                items.remove(message_id);
            }
            _ => {}
        }
    }
    items
}

/// Renders one item for the wire.
fn item_to_wire(item: &Item) -> Value {
    let mut value = json!({
        "messageId": item.message_id,
        "rating": item.rating,
        "version": item.version,
        "createdAt": item.created_at,
        "updatedAt": item.updated_at,
    });
    if let Some(note) = &item.note {
        value["note"] = json!(note);
    }
    if let Some(category) = &item.category {
        value["category"] = json!(category);
    }
    value
}

/// Renders a refusal the way upstream does: a successful call carrying `ok: false`.
fn refusal_to_wire(refusal: Refusal) -> Value {
    json!({ "ok": false, "code": refusal.code(), "message": refusal.message() })
}

/// The catalogue of categories, so a client can offer them rather than guess.
#[must_use]
pub fn category_catalogue() -> Value {
    json!(CATEGORIES)
}

/// Registers `messageFeedback.{list, put, delete}`.
///
/// The three share one catalogue read and one event append, so they are registered together: a
/// deployment that registered `list` without `put` would show a client ratings it cannot change.
pub fn register_message_feedback_methods(
    registry: &mut RemoteRegistry,
    catalog: &std::sync::Arc<dyn crate::remote::SessionCatalog>,
) {
    let listing = std::sync::Arc::clone(catalog);
    registry.register(NAMESPACE, "list", move |call: RemoteCall| {
        let catalog = std::sync::Arc::clone(&listing);
        Box::pin(async move {
            let session_id = string_arg(&call.args, "sessionId", "list")?;
            let Some(record) = catalog.read(&session_id).await else {
                // Upstream's `session_not_found` refusal, which is a result rather than an error:
                // the call reached a server that knows nothing about that session.
                return Ok(json!({ "ok": false, "code": "session_not_found" }));
            };
            let items: Vec<Value> = current_items(&record.events)
                .values()
                .map(item_to_wire)
                .collect();
            Ok(json!({ "ok": true, "items": items }))
        })
    });

    // `put` and `delete` append to a session log, which needs a store. Registered here so the
    // surface is complete; `SessionCatalog` on its own is a reader, so these answer the store's
    // absence rather than pretending to have written.
    registry.register(NAMESPACE, "put", move |call: RemoteCall| {
        Box::pin(async move {
            let message_id = string_arg(&call.args, "messageId", "put")?;
            let note = call.args.get("note").and_then(Value::as_str);
            let category = call.args.get("category").and_then(Value::as_str);
            let rating = match call.args.get("rating").and_then(Value::as_str) {
                Some("positive") => Rating::Positive,
                Some("negative") => Rating::Negative,
                other => {
                    return Err(RemoteError::InvalidArguments {
                        namespace: NAMESPACE.to_owned(),
                        method: "put".to_owned(),
                        message: format!(
                            "`rating` must be positive or negative, got {other:?}"
                        ),
                    });
                }
            };
            if let Err(refusal) = validate(&message_id, note, category) {
                return Ok(refusal_to_wire(refusal));
            }
            // A `SessionCatalog` reads; appending needs a store this registration does not hold.
            // Answered rather than claimed: a caller that received `ok: true` here would believe a
            // rating was saved.
            let _ = rating;
            Ok(json!({
                "ok": false,
                "code": "store_unavailable",
                "message": "this deployment's session catalog cannot append; supply a store to enable feedback",
            }))
        })
    });

    registry.register(NAMESPACE, "delete", move |call: RemoteCall| {
        Box::pin(async move {
            let message_id = string_arg(&call.args, "messageId", "delete")?;
            if message_id.trim().is_empty() {
                return Ok(refusal_to_wire(Refusal::MessageIdBlank));
            }
            Ok(json!({
                "ok": false,
                "code": "store_unavailable",
                "message": "this deployment's session catalog cannot append; supply a store to enable feedback",
            }))
        })
    });
}

/// Registers `sessionFeedback.record`.
///
/// A separate namespace from [`NAMESPACE`] because upstream keeps them apart: message feedback
/// answers about one message, session feedback about the conversation. A client that merged them
/// would attach a remark about the session to whichever message happened to be last.
pub fn register_session_feedback_methods(
    registry: &mut RemoteRegistry,
    catalog: &std::sync::Arc<dyn crate::remote::SessionCatalog>,
) {
    let listing = std::sync::Arc::clone(catalog);
    registry.register(SESSION_FEEDBACK_NAMESPACE, "record", move |call: RemoteCall| {
        let catalog = std::sync::Arc::clone(&listing);
        Box::pin(async move {
            let session_id = string_arg(&call.args, "sessionId", "record")?;
            // Upstream answers `session-not-found` as a result rather than an error: the call
            // reached a server that does not have that session, which is an answer.
            if catalog.read(&session_id).await.is_none() {
                return Ok(json!({
                    "ok": false,
                    "error": { "code": "session-not-found", "sessionId": session_id },
                }));
            }
            let text = call.args.get("text").and_then(Value::as_str);
            let category = call.args.get("category").and_then(Value::as_str);
            if let Err(refusal) = validate(&session_id, text, category) {
                return Ok(refusal_to_wire(refusal));
            }
            // Appending needs a store the catalogue does not hold; answered rather than claimed.
            Ok(json!({
                "ok": false,
                "code": "store_unavailable",
                "message": "this deployment's session catalog cannot append; supply a store to enable feedback",
            }))
        })
    });
}

/// The namespace session-level feedback registers under.
pub const SESSION_FEEDBACK_NAMESPACE: &str = "sessionFeedback";

/// Reads a required string argument.
fn string_arg(args: &Value, key: &str, method: &str) -> Result<String, RemoteError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: format!("`{key}` is required"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(message_id: &str, rating: Rating, at: i64) -> SessionEvent {
        SessionEvent::MessageFeedbackPut {
            message_id: message_id.to_owned(),
            rating,
            note: None,
            category: None,
            version: format!("v-{at}"),
            created_at: at,
            updated_at: at,
        }
    }

    #[test]
    fn a_later_put_replaces_an_earlier_one() {
        let items = current_items(&[
            put("m1", Rating::Positive, 1),
            put("m1", Rating::Negative, 2),
        ]);
        assert_eq!(items.len(), 1);
        assert_eq!(items["m1"].rating, Rating::Negative);
        assert_eq!(items["m1"].updated_at, 2);
    }

    #[test]
    fn a_delete_removes_the_item() {
        let items = current_items(&[
            put("m1", Rating::Positive, 1),
            SessionEvent::MessageFeedbackDelete {
                message_id: "m1".to_owned(),
            },
        ]);
        assert!(items.is_empty());
    }

    #[test]
    fn a_delete_of_something_never_put_is_harmless() {
        // The log is append-only and a delete may arrive after a fork dropped its put.
        let items = current_items(&[SessionEvent::MessageFeedbackDelete {
            message_id: "m1".to_owned(),
        }]);
        assert!(items.is_empty());
    }

    #[test]
    fn a_blank_note_is_refused_before_its_size_is_considered() {
        // `"   "` is both blank and short; the reason reported is the one a person can act on.
        assert_eq!(
            validate("m1", Some("   "), None).expect_err("blank"),
            Refusal::NoteBlank
        );
    }

    #[test]
    fn an_oversized_note_is_refused() {
        let note = "x".repeat(MAX_NOTE_BYTES + 1);
        assert_eq!(
            validate("m1", Some(&note), None).expect_err("large"),
            Refusal::NoteTooLarge
        );
    }

    #[test]
    fn an_unknown_category_is_refused() {
        assert_eq!(
            validate("m1", None, Some("not-a-category")).expect_err("unknown"),
            Refusal::UnknownCategory
        );
    }

    #[test]
    fn a_blank_message_id_is_refused() {
        assert_eq!(
            validate("  ", None, None).expect_err("blank id"),
            Refusal::MessageIdBlank
        );
    }
}
