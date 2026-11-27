//! The `userQuestions` projection: timed `ask_user_question` calls a person can still answer.
//!
//! The published value is `{ active, settled }`. A call is tracked only after a `request/header`
//! whose `ask_user_question` schema declares `timeout`. A pending result keeps the call open as
//! `continued`; an answer batch settles it; a failure drops it.

use serde_json::{Map, Value, json};

use crate::llm::ContentBlock;
use crate::session::SessionEvent;
use crate::session::projection::{Projection, Seq};

const TOOL: &str = "ask_user_question";

/// Folds timed user questions out of a session log.
#[derive(Debug, Default)]
pub struct UserQuestionsProjection;

impl Projection for UserQuestionsProjection {
    fn key(&self) -> &'static str {
        "userQuestions"
    }

    fn init(&self) -> Value {
        json!({ "timed": false, "active": [], "settled": [] })
    }

    fn apply(&self, mut state: Value, event: &SessionEvent, _seq: Seq) -> Value {
        match event {
            SessionEvent::RequestHeader { header } => {
                state["timed"] = json!(header_is_timed(header));
            }
            SessionEvent::ToolCall {
                id,
                name,
                arguments,
            } if state.get("timed").and_then(Value::as_bool) == Some(true) && name == TOOL => {
                if let Some(questions) = questions_of(arguments) {
                    upsert(&mut state, id.as_str(), questions, "open");
                }
            }
            SessionEvent::ToolResult {
                id,
                content,
                is_error,
            } => apply_result(&mut state, id.as_str(), content, *is_error),
            _ => {}
        }
        state
    }

    fn in_summary(&self) -> bool {
        false
    }

    fn wire(&self, state: &Value) -> Value {
        json!({
            "active": state.get("active").cloned().unwrap_or_else(|| json!([])),
            "settled": state.get("settled").cloned().unwrap_or_else(|| json!([])),
        })
    }
}

fn header_is_timed(header: &Value) -> bool {
    header
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(tool_is_timed))
}

fn tool_is_timed(tool: &Value) -> bool {
    tool.get("name").and_then(Value::as_str) == Some(TOOL)
        && tool
            .get("parameters")
            .and_then(|parameters| parameters.get("properties"))
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.contains_key("timeout"))
}

fn questions_of(arguments: &str) -> Option<Vec<Value>> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    let questions = value.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }
    questions.iter().map(question_of).collect()
}

fn question_of(question: &Value) -> Option<Value> {
    let mut item = json!({
        "id": question.get("id")?.as_str()?,
        "question": question.get("question")?.as_str()?,
    });
    if let Some(header) = question.get("header").and_then(Value::as_str) {
        item["header"] = json!(header);
    }
    if let Some(options) = question.get("options").and_then(Value::as_array) {
        item["options"] = Value::Array(options.iter().map(option_of).collect::<Option<_>>()?);
    }
    if let Some(multi) = question.get("multi_select").and_then(Value::as_bool) {
        item["multiSelect"] = json!(multi);
    }
    Some(item)
}

fn option_of(option: &Value) -> Option<Value> {
    let mut mapped = json!({ "label": option.get("label")?.as_str()? });
    if let Some(description) = option.get("description").and_then(Value::as_str) {
        mapped["description"] = json!(description);
    }
    Some(mapped)
}

fn upsert(state: &mut Value, call_id: &str, questions: Vec<Value>, phase: &str) {
    let Some(active) = state.get_mut("active").and_then(Value::as_array_mut) else {
        return;
    };
    active.retain(|item| item.get("callId").and_then(Value::as_str) != Some(call_id));
    active.push(json!({
        "callId": call_id,
        "questions": questions,
        "state": phase,
    }));
}

fn apply_result(state: &mut Value, call_id: &str, content: &[ContentBlock], is_error: bool) {
    if !active_has(state, call_id) {
        return;
    }
    if !is_error && content_text(content).is_some_and(is_pending) {
        if let Some(questions) = questions_for(state, call_id) {
            upsert(state, call_id, questions, "continued");
        }
        return;
    }
    let answers = (!is_error)
        .then(|| content_text(content).and_then(answers_of))
        .flatten();
    match answers {
        Some(answers) => settle(state, call_id, answers),
        None => drop_active(state, call_id),
    }
}

fn active_has(state: &Value, call_id: &str) -> bool {
    state
        .get("active")
        .and_then(Value::as_array)
        .is_some_and(|active| {
            active
                .iter()
                .any(|item| item.get("callId").and_then(Value::as_str) == Some(call_id))
        })
}

fn questions_for(state: &Value, call_id: &str) -> Option<Vec<Value>> {
    state
        .get("active")
        .and_then(Value::as_array)?
        .iter()
        .find(|item| item.get("callId").and_then(Value::as_str) == Some(call_id))?
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
}

fn drop_active(state: &mut Value, call_id: &str) {
    if let Some(active) = state.get_mut("active").and_then(Value::as_array_mut) {
        active.retain(|item| item.get("callId").and_then(Value::as_str) != Some(call_id));
    }
}

fn settle(state: &mut Value, call_id: &str, answers: Vec<Value>) {
    drop_active(state, call_id);
    if let Some(settled) = state.get_mut("settled").and_then(Value::as_array_mut) {
        settled.push(json!({ "callId": call_id, "answers": answers }));
    }
}

fn content_text(content: &[ContentBlock]) -> Option<&str> {
    content.iter().find_map(|block| match block {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

fn is_pending(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| value.get("pending").and_then(Value::as_bool))
        == Some(true)
}

fn answers_of(text: &str) -> Option<Vec<Value>> {
    let value: Value = serde_json::from_str(text).ok()?;
    value
        .get("answers")?
        .as_array()?
        .iter()
        .map(answer_of)
        .collect()
}

fn answer_of(answer: &Value) -> Option<Value> {
    let selected = answer.get("selected")?.as_array()?;
    if !selected.iter().all(Value::is_string) {
        return None;
    }
    let mut mapped = Map::new();
    mapped.insert("id".to_owned(), json!(answer.get("id")?.as_str()?));
    mapped.insert("selected".to_owned(), Value::Array(selected.clone()));
    if let Some(custom) = answer.get("custom").and_then(Value::as_str) {
        mapped.insert("custom".to_owned(), json!(custom));
    }
    Some(Value::Object(mapped))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::UserQuestionsProjection;
    use crate::llm::ContentBlock;
    use crate::session::SessionEvent;
    use crate::session::projection::ProjectionRegistry;
    use crate::util::brand::ToolCallId;

    fn header(timed: bool) -> SessionEvent {
        let mut parameters = json!({ "type": "object", "properties": { "questions": {} } });
        if timed {
            parameters["properties"]["timeout"] = json!({ "type": "number" });
        }
        SessionEvent::RequestHeader {
            header: json!({
                "tools": [{
                    "name": "ask_user_question",
                    "parameters": parameters,
                }],
            }),
        }
    }

    fn call(arguments: &str) -> SessionEvent {
        SessionEvent::ToolCall {
            id: ToolCallId::new("call-1").expect("id"),
            name: "ask_user_question".to_owned(),
            arguments: arguments.to_owned(),
        }
    }

    fn result(text: &str, is_error: bool) -> SessionEvent {
        SessionEvent::ToolResult {
            id: ToolCallId::new("call-1").expect("id"),
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            is_error,
        }
    }

    fn project(events: &[SessionEvent]) -> serde_json::Value {
        let mut registry = ProjectionRegistry::new();
        registry
            .register(Arc::new(UserQuestionsProjection))
            .expect("registered");
        registry.project(events).values["userQuestions"].clone()
    }

    #[test]
    fn an_untimed_call_stays_out_of_the_view() {
        let view = project(&[call(r#"{"questions":[{"id":"q","question":"Ship it?"}]}"#)]);
        assert_eq!(view, json!({ "active": [], "settled": [] }));
    }

    #[test]
    fn a_timed_call_opens_continues_and_settles() {
        let arguments = r#"{"questions":[{"id":"q","question":"Ship it?","header":"Plan","options":[{"label":"Yes","description":"Go"}],"multi_select":false}]}"#;
        let opened = project(&[header(true), call(arguments)]);
        assert_eq!(opened["active"][0]["state"], "open");
        assert_eq!(opened["active"][0]["questions"][0]["multiSelect"], false);
        assert_eq!(opened["active"][0]["questions"][0]["header"], "Plan");
        assert!(opened.get("timed").is_none());

        let continued = project(&[
            header(true),
            call(arguments),
            result(r#"{"pending":true}"#, false),
        ]);
        assert_eq!(continued["active"][0]["state"], "continued");

        let settled = project(&[
            header(true),
            call(arguments),
            result(
                r#"{"answers":[{"id":"q","selected":["Yes"],"custom":"now"}]}"#,
                false,
            ),
        ]);
        assert!(settled["active"].as_array().unwrap().is_empty());
        assert_eq!(settled["settled"][0]["answers"][0]["selected"][0], "Yes");
        assert_eq!(settled["settled"][0]["answers"][0]["custom"], "now");
    }

    #[test]
    fn an_error_result_drops_the_open_question() {
        let view = project(&[
            header(true),
            call(r#"{"questions":[{"id":"q","question":"Ship it?"}]}"#),
            result("cancelled", true),
        ]);
        assert_eq!(view, json!({ "active": [], "settled": [] }));
    }
}
