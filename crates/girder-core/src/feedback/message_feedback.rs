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

use crate::api::remote::{RemoteCall, RemoteError, RemoteRegistry};
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

/// The categories upstream accepts, in the order a product surface presents them.
pub const CATEGORIES: &[&str] = &[
    "task-result",
    "instruction-following",
    "product-interaction",
    "service-stability",
    "resource-cost",
    "security-privacy-permission",
    "other",
];

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

/// A successful call, with the value nested the way the client reads it.
fn success(value: Value) -> Value {
    json!({ "ok": true, "value": value })
}

/// A business refusal. The call reached the server; `error.code` is what the client branches on.
fn rejected(error: Value) -> Value {
    json!({ "ok": false, "error": error })
}

/// The catalog can name the session but cannot append to it.
fn store_unavailable() -> Value {
    json!({
        "ok": false,
        "code": "store_unavailable",
        "message": "this deployment's session catalog cannot append; supply a store to enable feedback",
    })
}

/// Current items in the order they were first recorded.
fn ordered_items(events: &[SessionEvent]) -> Vec<Item> {
    let mut order = Vec::new();
    let mut items = BTreeMap::new();
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
                if !items.contains_key(message_id) {
                    order.push(message_id.clone());
                }
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
                order.retain(|id| id != message_id);
            }
            _ => {}
        }
    }
    order
        .into_iter()
        .filter_map(|id| items.remove(&id))
        .collect()
}

fn has_assistant_message(events: &[SessionEvent], message_id: &str) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            SessionEvent::AssistantMessage { message } if message.id.as_str() == message_id
        )
    })
}

fn observed_version<'a>(args: &'a Value, method: &str) -> Result<Option<&'a str>, RemoteError> {
    match args.get("ifVersion") {
        None => Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: "`ifVersion` is required".to_owned(),
        }),
        Some(Value::Null) => Ok(None),
        Some(Value::String(version)) if !version.is_empty() => Ok(Some(version.as_str())),
        Some(_) => Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: "`ifVersion` must be a version string or null".to_owned(),
        }),
    }
}

fn note_refusal(note: &str) -> Option<Value> {
    if note.trim().is_empty() {
        return Some(rejected(json!({ "code": "note-blank" })));
    }
    if note.len() > MAX_NOTE_BYTES {
        return Some(rejected(json!({
            "code": "note-too-large",
            "maxBytes": MAX_NOTE_BYTES,
            "actualBytes": note.len(),
        })));
    }
    None
}

fn version_conflict(current: Option<&Item>) -> Value {
    rejected(json!({
        "code": "version-conflict",
        "current": current.map(item_to_wire),
    }))
}

fn next_version() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let millis = crate::now_millis();
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (millis & 0xffff_ffff) as u32,
        ((millis >> 32) & 0xffff) as u16,
        (n & 0xfff) as u16,
        ((n >> 12) & 0xfff) as u16,
        n & 0xffff_ffff_ffff
    )
}

async fn put_feedback(
    catalog: &std::sync::Arc<dyn crate::api::remote::SessionCatalog>,
    args: &Value,
) -> Result<Value, RemoteError> {
    let session_id = string_arg(args, "sessionId", NAMESPACE, "put")?;
    let message_id = string_arg(args, "messageId", NAMESPACE, "put")?;
    if message_id.trim().is_empty() {
        return Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: "put".to_owned(),
            message: Refusal::MessageIdBlank.message().to_owned(),
        });
    }
    let rating = match args.get("rating").and_then(Value::as_str) {
        Some("positive") => Rating::Positive,
        Some("negative") => Rating::Negative,
        other => {
            return Err(RemoteError::InvalidArguments {
                namespace: NAMESPACE.to_owned(),
                method: "put".to_owned(),
                message: format!("`rating` must be positive or negative, got {other:?}"),
            });
        }
    };
    let note = args.get("note").and_then(Value::as_str).map(str::to_owned);
    if let Some(note) = note.as_deref() {
        if let Some(refusal) = note_refusal(note) {
            return Ok(refusal);
        }
    }
    let category = args
        .get("category")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(category) = category.as_deref() {
        if !CATEGORIES.contains(&category) {
            return Err(RemoteError::InvalidArguments {
                namespace: NAMESPACE.to_owned(),
                method: "put".to_owned(),
                message: format!("`category` {category:?} is not one this build recognises"),
            });
        }
    }
    let observed = observed_version(args, "put")?;
    let Some(record) = catalog.read(&session_id).await else {
        return Ok(rejected(json!({
            "code": "session-not-found",
            "sessionId": session_id,
        })));
    };
    if !has_assistant_message(&record.events, &message_id) {
        return Ok(rejected(json!({
            "code": "target-not-found",
            "sessionId": session_id,
            "messageId": message_id,
        })));
    }
    let existing = current_items(&record.events).remove(&message_id);
    if observed != existing.as_ref().map(|item| item.version.as_str()) {
        return Ok(version_conflict(existing.as_ref()));
    }
    if let Some(existing) = existing.as_ref() {
        if existing.rating == rating && existing.note == note && existing.category == category {
            return Ok(success(item_to_wire(existing)));
        }
    }
    let now = i64::try_from(crate::now_millis()).unwrap_or(i64::MAX);
    let item = Item {
        message_id: message_id.clone(),
        rating,
        note: note.clone(),
        category: category.clone(),
        version: next_version(),
        created_at: existing.as_ref().map_or(now, |item| item.created_at),
        updated_at: existing
            .as_ref()
            .map_or(now, |item| item.updated_at.max(now)),
    };
    let Some(session) = catalog.open(&session_id) else {
        return Ok(store_unavailable());
    };
    session
        .append(SessionEvent::MessageFeedbackPut {
            message_id,
            rating,
            note,
            category,
            version: item.version.clone(),
            created_at: item.created_at,
            updated_at: item.updated_at,
        })
        .await
        .map_err(|error| RemoteError::Failed(error.to_string()))?;
    Ok(success(item_to_wire(&item)))
}

async fn delete_feedback(
    catalog: &std::sync::Arc<dyn crate::api::remote::SessionCatalog>,
    args: &Value,
) -> Result<Value, RemoteError> {
    let session_id = string_arg(args, "sessionId", NAMESPACE, "delete")?;
    let message_id = string_arg(args, "messageId", NAMESPACE, "delete")?;
    if message_id.trim().is_empty() {
        return Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: "delete".to_owned(),
            message: Refusal::MessageIdBlank.message().to_owned(),
        });
    }
    let Some(record) = catalog.read(&session_id).await else {
        return Ok(rejected(json!({
            "code": "session-not-found",
            "sessionId": session_id,
        })));
    };
    let existing = current_items(&record.events).remove(&message_id);
    if let Some(existing) = existing {
        let observed = observed_version(args, "delete")?;
        if observed != Some(existing.version.as_str()) {
            return Ok(version_conflict(Some(&existing)));
        }
        let Some(session) = catalog.open(&session_id) else {
            return Ok(store_unavailable());
        };
        session
            .append(SessionEvent::MessageFeedbackDelete {
                message_id: message_id.clone(),
            })
            .await
            .map_err(|error| RemoteError::Failed(error.to_string()))?;
    }
    Ok(success(json!({ "absent": true })))
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
    catalog: &std::sync::Arc<dyn crate::api::remote::SessionCatalog>,
) {
    let listing = std::sync::Arc::clone(catalog);
    registry.register(NAMESPACE, "list", move |call: RemoteCall| {
        let catalog = std::sync::Arc::clone(&listing);
        Box::pin(async move {
            let session_id = string_arg(&call.args, "sessionId", NAMESPACE, "list")?;
            let Some(record) = catalog.read(&session_id).await else {
                return Ok(rejected(json!({
                    "code": "session-not-found",
                    "sessionId": session_id,
                })));
            };
            let items: Vec<Value> = ordered_items(&record.events)
                .iter()
                .map(item_to_wire)
                .collect();
            Ok(success(json!({ "items": items })))
        })
    });

    let putting = std::sync::Arc::clone(catalog);
    registry.register(NAMESPACE, "put", move |call: RemoteCall| {
        let catalog = std::sync::Arc::clone(&putting);
        Box::pin(async move { put_feedback(&catalog, &call.args).await })
    });

    let deleting = std::sync::Arc::clone(catalog);
    registry.register(NAMESPACE, "delete", move |call: RemoteCall| {
        let catalog = std::sync::Arc::clone(&deleting);
        Box::pin(async move { delete_feedback(&catalog, &call.args).await })
    });
}

/// Registers `sessionFeedback.record`.
///
/// A separate namespace from [`NAMESPACE`] because upstream keeps them apart: message feedback
/// answers about one message, session feedback about the conversation. A client that merged them
/// would attach a remark about the session to whichever message happened to be last.
pub fn register_session_feedback_methods(
    registry: &mut RemoteRegistry,
    catalog: &std::sync::Arc<dyn crate::api::remote::SessionCatalog>,
) {
    let listing = std::sync::Arc::clone(catalog);
    registry.register(
        SESSION_FEEDBACK_NAMESPACE,
        "record",
        move |call: RemoteCall| {
            let catalog = std::sync::Arc::clone(&listing);
            Box::pin(async move {
                let session_id = string_arg(
                    &call.args,
                    "sessionId",
                    SESSION_FEEDBACK_NAMESPACE,
                    "record",
                )?;
                if catalog.read(&session_id).await.is_none() {
                    return Ok(rejected(json!({
                        "code": "session-not-found",
                        "sessionId": session_id,
                    })));
                }
                let text = call
                    .args
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned);
                let category = call
                    .args
                    .get("category")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(category) = category.as_deref() {
                    if !CATEGORIES.contains(&category) {
                        return Err(RemoteError::InvalidArguments {
                            namespace: SESSION_FEEDBACK_NAMESPACE.to_owned(),
                            method: "record".to_owned(),
                            message: format!(
                                "`category` {category:?} is not one this build recognises"
                            ),
                        });
                    }
                }
                let Some(session) = catalog.open(&session_id) else {
                    return Ok(store_unavailable());
                };
                session
                    .append(SessionEvent::SessionFeedbackRecord {
                        text: text.clone(),
                        category: category.clone(),
                    })
                    .await
                    .map_err(|error| RemoteError::Failed(error.to_string()))?;
                Ok(success(json!({ "recorded": true })))
            })
        },
    );
}

/// The namespace session-level feedback registers under.
pub const SESSION_FEEDBACK_NAMESPACE: &str = "sessionFeedback";

/// Reads a required string argument.
fn string_arg(
    args: &Value,
    key: &str,
    namespace: &str,
    method: &str,
) -> Result<String, RemoteError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| RemoteError::InvalidArguments {
            namespace: namespace.to_owned(),
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

    #[test]
    fn a_rating_is_appended_and_still_there_after_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "girder-feedback-{}-{}",
            std::process::id(),
            crate::now_millis()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let catalog = std::sync::Arc::new(crate::api::remote::DirectoryCatalog::new(&dir));
            let shared: std::sync::Arc<dyn crate::api::remote::SessionCatalog> = catalog.clone();
            crate::api::remote::SessionCatalog::create(catalog.as_ref(), "s1")
                .await
                .expect("created");
            let session =
                crate::api::remote::SessionCatalog::open(catalog.as_ref(), "s1").expect("open");
            session
                .append(SessionEvent::assistant_text("m1", "answer", "model"))
                .await
                .expect("assistant");
            let mut registry = crate::api::remote::RemoteRegistry::new();
            register_message_feedback_methods(&mut registry, &shared);
            register_session_feedback_methods(&mut registry, &shared);
            let created = registry
                .dispatch(
                    "messageFeedback",
                    "put",
                    crate::api::remote::RemoteCall {
                        args: json!({
                            "sessionId": "s1",
                            "messageId": "m1",
                            "rating": "positive",
                            "ifVersion": null,
                        }),
                        peer: String::new(),
                    },
                )
                .await
                .expect("put");
            assert_eq!(created["ok"], true);
            let version = created["value"]["version"].as_str().expect("version");
            let listed = registry
                .dispatch(
                    "messageFeedback",
                    "list",
                    crate::api::remote::RemoteCall {
                        args: json!({ "sessionId": "s1" }),
                        peer: String::new(),
                    },
                )
                .await
                .expect("list");
            assert_eq!(listed["value"]["items"][0]["messageId"], "m1");
            let removed = registry
                .dispatch(
                    "messageFeedback",
                    "delete",
                    crate::api::remote::RemoteCall {
                        args: json!({
                            "sessionId": "s1",
                            "messageId": "m1",
                            "ifVersion": version,
                        }),
                        peer: String::new(),
                    },
                )
                .await
                .expect("delete");
            assert_eq!(removed["value"]["absent"], true);
            registry
                .dispatch(
                    "sessionFeedback",
                    "record",
                    crate::api::remote::RemoteCall {
                        args: json!({ "sessionId": "s1", "text": "  helpful  " }),
                        peer: String::new(),
                    },
                )
                .await
                .expect("record");
        });
        let reopened = crate::api::remote::DirectoryCatalog::new(&dir);
        let record = runtime
            .block_on(crate::api::remote::SessionCatalog::read(&reopened, "s1"))
            .expect("reopened");
        assert!(ordered_items(&record.events).is_empty());
        assert!(record.events.iter().any(|event| matches!(
            event,
            SessionEvent::SessionFeedbackRecord { text: Some(text), .. } if text == "helpful"
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
