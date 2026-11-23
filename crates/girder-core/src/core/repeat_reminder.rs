//! Consecutive identical tool calls, and the reminder a model gets when one runs long.
//!
//! Upstream `packages/guard/repeat-tool-reminder` counts a call after it is attempted, including
//! one a policy denied, and injects a user-visible notice at the counts 3, 5, and 8. A different
//! tool or different arguments starts the count over. The notice does not block the call.

use serde_json::Value;

/// Counts that produce a reminder, in the order the wording escalates.
pub const THRESHOLDS: [u32; 3] = [3, 5, 8];

/// How much of the canonical arguments the detailed reminder quotes.
const PREVIEW_CHARS: usize = 500;

const GENTLE: &str = "You are repeating the exact same tool call with identical arguments. \
Carefully analyze the previous result before calling again: if the task is not complete, try a \
different approach or different arguments instead of repeating the call.";

/// One agent's run of identical calls.
#[derive(Clone, Debug, Default)]
pub struct Chain {
    key: Option<String>,
    count: u32,
}

impl Chain {
    /// An empty chain.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets the run. A new user message is a new task, so the previous repetition is not a loop.
    pub fn reset(&mut self) {
        self.key = None;
        self.count = 0;
    }

    /// Records one attempt and returns the reminder when this count is a threshold.
    #[must_use]
    pub fn observe(&mut self, name: &str, arguments: &str) -> Option<String> {
        let canonical = canonicalize(arguments);
        let key = format!("{name}\u{0}{canonical}");
        self.count = if self.key.as_deref() == Some(key.as_str()) {
            self.count.saturating_add(1)
        } else {
            1
        };
        self.key = Some(key);
        if !THRESHOLDS.contains(&self.count) {
            return None;
        }
        if self.count == THRESHOLDS[0] {
            return Some(GENTLE.to_owned());
        }
        Some(detailed(name, self.count, &preview(&canonical)))
    }
}

fn detailed(name: &str, count: u32, arguments: &str) -> String {
    format!(
        "Repeated tool call detected:\n- tool: {name}\n- consecutive_calls: {count}\n- arguments: {arguments}\n\
         The repeated calls are not making progress. Do not call this tool with these exact arguments again. \
         Inspect the latest result and choose a different action, different arguments, or finish the task if \
         enough evidence has been gathered."
    )
}

fn preview(canonical: &str) -> String {
    let count = canonical.chars().count();
    if count <= PREVIEW_CHARS {
        return canonical.to_owned();
    }
    let end = canonical
        .char_indices()
        .nth(PREVIEW_CHARS)
        .map(|(index, _)| index)
        .unwrap_or(canonical.len());
    format!(
        "{}… (+{} more chars)",
        &canonical[..end],
        count - PREVIEW_CHARS
    )
}

/// Key order does not make two objects different calls.
fn canonicalize(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(value) => {
            serde_json::to_string(&sort_json(value)).unwrap_or_else(|_| arguments.to_owned())
        }
        Err(_) => arguments.to_owned(),
    }
}

fn sort_json(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(sort_json).collect()),
        Value::Object(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            let mut sorted = serde_json::Map::new();
            for key in keys {
                if let Some(child) = map.get(&key) {
                    sorted.insert(key, sort_json(child.clone()));
                }
            }
            Value::Object(sorted)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::Chain;

    #[test]
    fn the_third_identical_call_is_a_gentle_reminder() {
        let mut chain = Chain::new();
        assert!(chain.observe("read", r#"{"path":"a"}"#).is_none());
        assert!(chain.observe("read", r#"{"path":"a"}"#).is_none());
        let reminder = chain
            .observe("read", r#"{"path":"a"}"#)
            .expect("threshold 3");
        assert!(reminder.contains("repeating the exact same tool call"));
    }

    #[test]
    fn a_different_argument_order_is_still_the_same_call() {
        let mut chain = Chain::new();
        let _ = chain.observe("read", r#"{"path":"a","line":1}"#);
        let _ = chain.observe("read", r#"{"line":1,"path":"a"}"#);
        let reminder = chain.observe("read", r#"{"path":"a","line":1}"#);
        assert!(reminder.is_some());
    }

    #[test]
    fn a_different_tool_starts_the_count_over() {
        let mut chain = Chain::new();
        let _ = chain.observe("read", r#"{"path":"a"}"#);
        let _ = chain.observe("read", r#"{"path":"a"}"#);
        assert!(chain.observe("grep", r#"{"path":"a"}"#).is_none());
        assert!(chain.observe("grep", r#"{"path":"a"}"#).is_none());
    }

    #[test]
    fn the_fifth_call_names_the_tool_and_the_count() {
        let mut chain = Chain::new();
        let mut fifth = None;
        for index in 1..=5 {
            let reminder = chain.observe("bash", "{\"cmd\":\"ls\"}");
            if index == 5 {
                fifth = reminder;
            }
        }
        let fifth = fifth.expect("threshold 5");
        assert!(fifth.contains("consecutive_calls: 5"));
        assert!(fifth.contains("tool: bash"));
    }
}
