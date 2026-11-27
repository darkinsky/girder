//! Live assistant frames for one `session.follow` generation.
//!
//! The Web client opens a session with `assistantStream: true` and then expects dense
//! `start` / `chunk` / `end` revisions. Durable history stays in the log. This hub only
//! remembers the attempt that is still being generated, plus the frames a follower has
//! not yet forwarded.

use std::collections::{HashMap, VecDeque};

use serde_json::{Value, json};

/// How many frames one session keeps for a slow follower.
const FRAME_LIMIT: usize = 1024;

/// One process-wide assistant stream, shared by the runtime and every follow connection.
#[derive(Default)]
pub struct AssistantHub {
    sessions: std::sync::Mutex<HashMap<String, LiveSession>>,
    notify: std::sync::Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct LiveSession {
    revision: u64,
    attempt: Option<Attempt>,
    frames: VecDeque<Value>,
}

struct Attempt {
    id: String,
    turn: u64,
    step: u64,
    started_after: i64,
    next_index: u64,
    stream: Vec<CompactRecord>,
}

/// One packed run a reconnecting client can expand back into deltas.
enum CompactRecord {
    Text(TextRun),
    Tool(ToolRun),
    Raw { time: u64, chunk: Value },
}

/// One packed run of text or reasoning deltas.
struct TextRun {
    kind: &'static str,
    time0: u64,
    index: u64,
    last: u64,
    dt: Vec<u64>,
    texts: Vec<String>,
}

/// One packed run of tool-call argument deltas.
struct ToolRun {
    time0: u64,
    index: u64,
    last: u64,
    dt: Vec<u64>,
    id: String,
    name: Option<String>,
    args: Vec<String>,
}

impl AssistantHub {
    /// Creates an empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the wake-up source for followers waiting for a new frame.
    #[must_use]
    pub fn notifier(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.notify)
    }

    /// The baseline a follower reads when it opens.
    ///
    /// `revision` is the last frame already folded into `activeAttempt`. The next frame
    /// this hub emits uses the following revision.
    #[must_use]
    pub fn baseline(&self, session_id: &str) -> Value {
        let sessions = self.lock();
        let Some(session) = sessions.get(session_id) else {
            return json!({ "revision": 0 });
        };
        let mut baseline = json!({ "revision": session.revision });
        if let Some(attempt) = &session.attempt {
            baseline["activeAttempt"] = json!({
                "attemptId": attempt.id,
                "startedAfterSeq": attempt.started_after,
                "turn": attempt.turn,
                "step": attempt.step,
                "nextIndex": attempt.next_index,
                "stream": attempt.stream.iter().map(CompactRecord::snapshot).collect::<Vec<_>>(),
            });
        }
        baseline
    }

    /// Frames whose revision is newer than `after`, in emission order.
    #[must_use]
    pub fn frames_after(&self, session_id: &str, after: u64) -> Vec<Value> {
        let sessions = self.lock();
        sessions
            .get(session_id)
            .map(|session| {
                session
                    .frames
                    .iter()
                    .filter(|frame| frame_revision(frame) > after)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Opens an attempt. An attempt already open is abandoned first.
    pub fn begin(&self, session_id: &str, turn: u64, step: u64, started_after: i64) {
        self.abandon(session_id);
        let mut sessions = self.lock();
        let session = sessions.entry(session_id.to_owned()).or_default();
        let id = format!(
            "attempt-{session_id}-{turn}-{step}-{}",
            crate::schedule::now_millis()
        );
        session.revision = session.revision.saturating_add(1);
        session.frames.push_back(json!({
            "type": "assistant-stream",
            "frame": {
                "type": "start",
                "attemptId": id,
                "revision": session.revision,
                "startedAfterSeq": started_after,
                "turn": turn,
                "step": step,
            }
        }));
        trim(&mut session.frames);
        session.attempt = Some(Attempt {
            id,
            turn,
            step,
            started_after,
            next_index: 0,
            stream: Vec::new(),
        });
        self.notify.notify_waiters();
    }

    /// Appends one visible model chunk to the open attempt.
    ///
    /// A chunk that arrives before `begin` opens an attempt at turn 0. The chunk itself is
    /// the JSON object the browser renders, such as `{ "type": "text-delta", "index": 0, "text" }`.
    pub fn push_chunk(&self, session_id: &str, chunk: Value) {
        if self
            .lock()
            .get(session_id)
            .is_none_or(|session| session.attempt.is_none())
        {
            self.begin(session_id, 0, 0, -1);
        }
        let mut sessions = self.lock();
        let Some(session) = sessions.get_mut(session_id) else {
            return;
        };
        let Some(attempt) = session.attempt.as_mut() else {
            return;
        };
        let index = attempt.next_index;
        let time = crate::schedule::now_millis();
        attempt.fold(time, &chunk);
        attempt.next_index = attempt.next_index.saturating_add(1);
        session.revision = session.revision.saturating_add(1);
        let attempt_id = attempt.id.clone();
        session.frames.push_back(json!({
            "type": "assistant-stream",
            "frame": {
                "type": "chunk",
                "attemptId": attempt_id,
                "revision": session.revision,
                "index": index,
                "time": time,
                "chunk": chunk,
            }
        }));
        trim(&mut session.frames);
        self.notify.notify_waiters();
    }

    /// Closes the open attempt because its assistant message is now in the log at `seq`.
    pub fn commit(&self, session_id: &str, seq: u64) {
        self.finish(
            session_id,
            json!({
                "kind": "committed",
                "eventType": "assistant/message",
                "seq": seq,
            }),
        );
    }

    /// Closes the open attempt without a durable message.
    pub fn abandon(&self, session_id: &str) {
        self.finish(session_id, json!({ "kind": "abandoned" }));
    }

    fn finish(&self, session_id: &str, outcome: Value) {
        let mut sessions = self.lock();
        let Some(session) = sessions.get_mut(session_id) else {
            return;
        };
        let Some(attempt) = session.attempt.take() else {
            return;
        };
        session.revision = session.revision.saturating_add(1);
        session.frames.push_back(json!({
            "type": "assistant-stream",
            "frame": {
                "type": "end",
                "attemptId": attempt.id,
                "revision": session.revision,
                "index": attempt.next_index,
                "outcome": outcome,
            }
        }));
        trim(&mut session.frames);
        self.notify.notify_waiters();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, LiveSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Attempt {
    fn fold(&mut self, time: u64, chunk: &Value) {
        match chunk.get("type").and_then(Value::as_str) {
            Some("text-delta" | "reasoning-delta") => self.fold_text(time, chunk),
            Some("tool-call-delta") => self.fold_tool(time, chunk),
            _ => self.stream.push(CompactRecord::Raw {
                time,
                chunk: chunk.clone(),
            }),
        }
    }

    fn fold_text(&mut self, time: u64, chunk: &Value) {
        let kind = if chunk.get("type").and_then(Value::as_str) == Some("reasoning-delta") {
            "reasoning-chunks"
        } else {
            "text-chunks"
        };
        let index = chunk.get("index").and_then(Value::as_u64).unwrap_or(0);
        let text = chunk
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if let Some(CompactRecord::Text(run)) = self.stream.last_mut() {
            if run.kind == kind && run.index == index {
                if let Some(gap) = time.checked_sub(run.last) {
                    run.dt.push(gap);
                    run.texts.push(text);
                    run.last = time;
                    return;
                }
            }
        }
        self.stream.push(CompactRecord::Text(TextRun {
            kind,
            time0: time,
            index,
            last: time,
            dt: Vec::new(),
            texts: vec![text],
        }));
    }

    fn fold_tool(&mut self, time: u64, chunk: &Value) {
        let id = chunk
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let name = chunk.get("name").and_then(Value::as_str).map(str::to_owned);
        let arguments = chunk
            .get("argumentsDelta")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        // An empty id or an empty name cannot be packed; the client keeps it raw.
        if id.is_empty() || name.as_deref() == Some("") {
            self.stream.push(CompactRecord::Raw {
                time,
                chunk: chunk.clone(),
            });
            return;
        }
        let index = chunk.get("index").and_then(Value::as_u64).unwrap_or(0);
        if let Some(CompactRecord::Tool(run)) = self.stream.last_mut() {
            let same_name = run.name.is_some() == name.is_some() && run.name == name;
            if run.index == index && run.id == id && same_name {
                if let Some(gap) = time.checked_sub(run.last) {
                    run.dt.push(gap);
                    run.args.push(arguments);
                    run.last = time;
                    return;
                }
            }
        }
        self.stream.push(CompactRecord::Tool(ToolRun {
            time0: time,
            index,
            last: time,
            dt: Vec::new(),
            id,
            name,
            args: vec![arguments],
        }));
    }
}

impl CompactRecord {
    fn snapshot(&self) -> Value {
        match self {
            Self::Text(run) => json!({
                "type": run.kind,
                "time0": run.time0,
                "index": run.index,
                "dt": run.dt,
                "texts": run.texts,
            }),
            Self::Tool(run) => {
                let mut value = json!({
                    "type": "tool-call-chunks",
                    "time0": run.time0,
                    "index": run.index,
                    "dt": run.dt,
                    "id": run.id,
                    "args": run.args,
                });
                if let Some(name) = &run.name {
                    value["name"] = Value::String(name.clone());
                }
                value
            }
            Self::Raw { time, chunk } => json!({
                "type": "chunk",
                "time": time,
                "chunk": chunk,
            }),
        }
    }
}

fn frame_revision(frame: &Value) -> u64 {
    frame
        .get("frame")
        .and_then(|frame| frame.get("revision"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn trim(frames: &mut VecDeque<Value>) {
    while frames.len() > FRAME_LIMIT {
        frames.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::AssistantHub;
    use serde_json::json;

    #[test]
    fn a_text_chunk_then_a_commit_keeps_dense_revisions() {
        let hub = AssistantHub::new();
        hub.begin("s1", 1, 0, -1);
        hub.push_chunk(
            "s1",
            json!({"type": "text-delta", "index": 0, "text": "hi"}),
        );
        let mid = hub.baseline("s1");
        assert_eq!(mid["revision"], 2);
        assert_eq!(mid["activeAttempt"]["nextIndex"], 1);
        assert_eq!(mid["activeAttempt"]["stream"][0]["type"], "text-chunks");
        assert_eq!(mid["activeAttempt"]["stream"][0]["texts"], json!(["hi"]));
        assert_eq!(mid["activeAttempt"]["stream"][0]["dt"], json!([]));
        hub.commit("s1", 4);
        let frames = hub.frames_after("s1", 0);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0]["frame"]["type"], "start");
        assert_eq!(frames[0]["frame"]["revision"], 1);
        assert_eq!(frames[1]["frame"]["type"], "chunk");
        assert_eq!(frames[1]["frame"]["index"], 0);
        assert_eq!(frames[2]["frame"]["type"], "end");
        assert_eq!(frames[2]["frame"]["index"], 1);
        assert_eq!(frames[2]["frame"]["revision"], 3);
        assert_eq!(
            frames[2]["frame"]["outcome"],
            json!({"kind": "committed", "eventType": "assistant/message", "seq": 4})
        );
        assert!(hub.baseline("s1").get("activeAttempt").is_none());
    }

    #[test]
    fn tool_deltas_pack_into_one_reconnect_run() {
        let hub = AssistantHub::new();
        hub.begin("s1", 1, 0, -1);
        hub.push_chunk(
            "s1",
            json!({
                "type": "tool-call-delta",
                "index": 1,
                "id": "call-1",
                "name": "read",
                "argumentsDelta": "{\"path\":",
            }),
        );
        hub.push_chunk(
            "s1",
            json!({
                "type": "tool-call-delta",
                "index": 1,
                "id": "call-1",
                "name": "read",
                "argumentsDelta": "\"a.rs\"}",
            }),
        );
        let stream = &hub.baseline("s1")["activeAttempt"]["stream"];
        assert_eq!(stream[0]["type"], "tool-call-chunks");
        assert_eq!(stream[0]["id"], "call-1");
        assert_eq!(stream[0]["name"], "read");
        assert_eq!(stream[0]["index"], 1);
        assert_eq!(stream[0]["args"], json!(["{\"path\":", "\"a.rs\"}"]));
        assert_eq!(stream[0]["dt"].as_array().map(Vec::len), Some(1));
        assert_eq!(hub.baseline("s1")["activeAttempt"]["nextIndex"], 2);
    }
}
