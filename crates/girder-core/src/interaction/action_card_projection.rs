//! The `actionCards` projection: cards offered in an answer, and the decisions taken on them.
//!
//! Upstream folds this from `action-card` fences in assistant text and from `action-cards/decision`
//! events. A card's id is the assistant message that offered it. The first valid fence in a turn
//! becomes the card; a later answer in that same turn does not add another. The value a client
//! reads is `proposals` and `decisions`. `turn` and `admittedTurn` are the fold's own memory of
//! which turn is open and which turn already offered a card.

use serde_json::{Map, Value, json};

use crate::session::SessionEvent;
use crate::session::message::Message;
use crate::session::projection::{Projection, Seq};

/// Folds action cards out of a session log.
#[derive(Debug, Default)]
pub struct ActionCardsProjection;

impl Projection for ActionCardsProjection {
    fn key(&self) -> &'static str {
        "actionCards"
    }

    fn init(&self) -> Value {
        json!({
            "proposals": {},
            "decisions": {},
            "turn": 0,
            "admittedTurn": Value::Null,
        })
    }

    fn apply(&self, mut state: Value, event: &SessionEvent, _seq: Seq) -> Value {
        match event {
            SessionEvent::TurnStart { turn } => {
                state["turn"] = json!(turn);
                state
            }
            SessionEvent::AssistantMessage { message } => admit(&mut state, message),
            SessionEvent::ActionCardsDecision {
                card_id,
                action,
                phase,
                data,
            } => {
                let mut decision = json!({
                    "cardId": card_id,
                    "action": action,
                    "phase": phase,
                });
                if let Some(data) = data {
                    decision["data"] = data.clone();
                }
                if let Some(decisions) = state.get_mut("decisions").and_then(Value::as_object_mut) {
                    decisions.insert(card_id.clone(), decision);
                }
                state
            }
            _ => state,
        }
    }

    fn in_summary(&self) -> bool {
        false
    }

    fn wire(&self, state: &Value) -> Value {
        json!({
            "proposals": state.get("proposals").cloned().unwrap_or_else(|| json!({})),
            "decisions": state.get("decisions").cloned().unwrap_or_else(|| json!({})),
        })
    }
}

/// Folds `events` the same way [`crate::session::projection::ProjectionRegistry::project`] does.
#[must_use]
pub fn fold(events: &[SessionEvent]) -> Value {
    let projection = ActionCardsProjection;
    let mut state = projection.init();
    for (index, event) in events.iter().enumerate() {
        let seq = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        state = projection.apply(state, event, seq);
    }
    state
}

/// Contents of the `action-card` fences in `text`, in order.
///
/// A fence opens with at least three backticks or tildes after at most three spaces, optionally
/// after the marker of the list item it starts. It closes on a line of the same character at least
/// as long. An unclosed fence runs to the end of the text. A block indented four or more spaces is
/// not a fence.
#[must_use]
pub fn action_card_fences(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut open: Option<OpenFence> = None;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(fence) = open.as_ref() {
            if fence.closes(line) {
                if let Some(content) = open.take().and_then(|fence| fence.content) {
                    found.push(content.join("\n"));
                }
            } else if let Some(fence) = open.as_mut() {
                if let Some(content) = fence.content.as_mut() {
                    content.push(outdent(line, fence.width));
                }
            }
        } else {
            open = opened(line);
        }
    }
    if let Some(content) = open.and_then(|fence| fence.content) {
        found.push(content.join("\n"));
    }
    found
}

struct OpenFence {
    closing_char: char,
    closing_len: usize,
    closing_indent: usize,
    width: usize,
    content: Option<Vec<String>>,
}

impl OpenFence {
    fn closes(&self, line: &str) -> bool {
        let bytes = line.as_bytes();
        let mut index = 0;
        while index < bytes.len() && bytes[index] == b' ' && index < self.closing_indent {
            index += 1;
        }
        if index < bytes.len() && bytes[index] == b' ' {
            return false;
        }
        let rest = &line[index..];
        let mut count = 0;
        for character in rest.chars() {
            if character == self.closing_char {
                count += 1;
            } else {
                break;
            }
        }
        count >= self.closing_len
            && rest
                .chars()
                .skip(count)
                .all(|character| character == ' ' || character == '\t')
    }
}

fn opened(line: &str) -> Option<OpenFence> {
    let mut chars = line.chars();
    let mut indent = 0;
    while indent < 3 && chars.as_str().starts_with(' ') {
        chars.next();
        indent += 1;
    }
    if chars.as_str().starts_with(' ') {
        return None;
    }
    let rest = chars.as_str();
    let item_len = list_prefix(rest).unwrap_or(0);
    let (marker, marker_len, info) = fence_marker(&rest[item_len..])?;
    if marker == '`' && info.contains('`') {
        return None;
    }
    let width = indent + item_len;
    Some(OpenFence {
        closing_char: marker,
        closing_len: marker_len,
        closing_indent: if item_len == 0 { 0 } else { width + 3 },
        width,
        content: (language_of(info) == "action-card").then(Vec::new),
    })
}

fn list_prefix(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let marker_end = if matches!(bytes[0], b'-' | b'+' | b'*') {
        1
    } else {
        let mut index = 0;
        while index < bytes.len() && index < 9 && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == 0 || index >= bytes.len() || !matches!(bytes[index], b'.' | b')') {
            return None;
        }
        index + 1
    };
    let mut spaces = 0;
    let mut index = marker_end;
    while index < bytes.len() && bytes[index] == b' ' && spaces < 4 {
        spaces += 1;
        index += 1;
    }
    (spaces > 0).then_some(index)
}

fn fence_marker(source: &str) -> Option<(char, usize, &str)> {
    let mut chars = source.chars();
    let marker = chars.next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let mut len = 1;
    while chars.as_str().starts_with(marker) {
        chars.next();
        len += 1;
    }
    (len >= 3).then_some((marker, len, chars.as_str()))
}

fn language_of(info: &str) -> String {
    let trimmed = info.trim();
    let end = trimmed
        .find(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '-'
        })
        .unwrap_or(trimmed.len());
    trimmed[..end].to_ascii_lowercase()
}

fn outdent(line: &str, width: usize) -> String {
    let mut skipped = 0;
    let mut rest = line;
    while skipped < width && rest.starts_with(' ') {
        rest = &rest[1..];
        skipped += 1;
    }
    rest.to_owned()
}

fn admit(state: &mut Value, message: &Message) -> Value {
    let turn = state.get("turn").and_then(Value::as_u64).unwrap_or(0);
    if state.get("admittedTurn").and_then(Value::as_u64) == Some(turn) {
        return state.clone();
    }
    let Some(fields) = first_card(message) else {
        return state.clone();
    };
    let card_id = message.id.as_str();
    if let Some(proposals) = state.get_mut("proposals").and_then(Value::as_object_mut) {
        proposals.insert(
            card_id.to_owned(),
            json!({
                "cardId": card_id,
                "turn": turn,
                "action": fields.action,
                "title": fields.title,
                "brief": fields.brief,
                "details": fields.details,
            }),
        );
    }
    state["admittedTurn"] = json!(turn);
    state.clone()
}

struct CardFields {
    action: String,
    title: String,
    brief: String,
    details: Map<String, Value>,
}

fn first_card(message: &Message) -> Option<CardFields> {
    for block in &message.content {
        let crate::llm::ContentBlock::Text { text } = block else {
            continue;
        };
        for fence in action_card_fences(text) {
            if let Some(fields) = card_fields(&fence) {
                return Some(fields);
            }
        }
    }
    None
}

fn card_fields(text: &str) -> Option<CardFields> {
    let value: Value = serde_json::from_str(text).ok()?;
    let action = value.get("action")?.as_str()?.to_owned();
    let title = value.get("title")?.as_str()?.trim().to_owned();
    let brief = value.get("brief")?.as_str()?.trim().to_owned();
    let details = value.get("details")?.as_object()?.clone();
    if action.is_empty() || title.is_empty() || brief.is_empty() {
        return None;
    }
    Some(CardFields {
        action,
        title,
        brief,
        details,
    })
}

#[cfg(test)]
mod tests {
    use super::{ActionCardsProjection, action_card_fences, fold};
    use crate::llm::ContentBlock;
    use crate::session::SessionEvent;
    use crate::session::message::{Message, MessageSource, Role};
    use crate::session::projection::Projection;
    use crate::util::brand::MessageId;
    use serde_json::json;

    fn assistant(id: &str, text: &str) -> SessionEvent {
        SessionEvent::AssistantMessage {
            message: Message::new(
                MessageId::new(id).expect("id"),
                Role::Assistant,
                MessageSource::Model {
                    provider: "p".to_owned(),
                    model: "m".to_owned(),
                    replay_state: None,
                },
                vec![ContentBlock::Text {
                    text: text.to_owned(),
                }],
            ),
        }
    }

    #[test]
    fn a_fence_is_read_and_a_deeply_indented_block_is_not() {
        let text =
            "before\n```action-card\n{\"action\":\"a\"}\n```\n    ```action-card\nnope\n```\n";
        assert_eq!(action_card_fences(text), vec!["{\"action\":\"a\"}"]);
    }

    #[test]
    fn a_list_item_fence_keeps_its_content() {
        let text = "- ```action-card\n  {\"action\":\"a\"}\n  ```\n";
        assert_eq!(action_card_fences(text), vec!["{\"action\":\"a\"}"]);
    }

    #[test]
    fn the_first_valid_card_of_a_turn_is_the_proposal() {
        let fence = "```action-card\n{\"action\":\"review\",\"title\":\" Review \",\"brief\":\"Look\",\"details\":{\"n\":1}}\n```";
        let state = fold(&[
            SessionEvent::TurnStart { turn: 2 },
            assistant("m1", "no card here"),
            assistant("m2", fence),
            assistant("m3", fence),
        ]);
        assert_eq!(state["proposals"]["m2"]["action"], "review");
        assert_eq!(state["proposals"]["m2"]["title"], "Review");
        assert_eq!(state["proposals"]["m2"]["turn"], 2);
        assert!(state["proposals"].get("m3").is_none());
    }

    #[test]
    fn a_decision_is_recorded_against_its_card() {
        let state = fold(&[SessionEvent::ActionCardsDecision {
            card_id: "m2".to_owned(),
            action: "review".to_owned(),
            phase: "dismissed".to_owned(),
            data: None,
        }]);
        assert_eq!(state["decisions"]["m2"]["phase"], "dismissed");
        assert!(state["decisions"]["m2"].get("data").is_none());
    }

    #[test]
    fn the_wire_view_excludes_fold_bookkeeping() {
        let projection = ActionCardsProjection;
        let state = json!({
            "proposals": {"card-1": {"cardId": "card-1"}},
            "decisions": {"card-1": {"cardId": "card-1", "phase": "dismissed"}},
            "turn": 3,
            "admittedTurn": 3,
        });

        assert_eq!(
            projection.wire(&state),
            json!({
                "proposals": {"card-1": {"cardId": "card-1"}},
                "decisions": {"card-1": {"cardId": "card-1", "phase": "dismissed"}},
            })
        );
    }
}
