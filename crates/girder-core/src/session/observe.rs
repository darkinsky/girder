//! The three `observe*` endpoints: one observation per call, which the subscription carrier polls.
//!
//! # What upstream has
//!
//! `packages/api/session-controller/src/index.ts` marks three stream methods:
//!
//! | Endpoint | Answers |
//! | --- | --- |
//! | `observeEvent` | durable event frames from a cursor |
//! | `observeProjections` | projection frames |
//! | `observeState` | complete passive-state replacements |
//!
//! All three are `@Remote({ mode: 'stream' })`, because upstream's gateway holds one request open and
//! pushes frames through an inbox. This build's transport already models that
//! (`girder_rpc::stream`), and its `SUBSCRIBABLE_METHODS` already names all three -- what was missing
//! was the *method* behind the name.
//!
//! # One observation per call
//!
//! Each handler answers with the next frame from a cursor and says whether the series ended. The
//! subscription carrier re-invokes it on its interval, which is the shape
//! [`girder_rpc::stream`] documents: the host asks for a value when it has one, so pacing stays the
//! host's and a slow reader cannot be flooded.
//!
//! The `seq` field is what makes polling correct rather than approximate: a client that received
//! frame 3 and then frame 5 knows one was collapsed, and can re-request from 4. Without it, a polled
//! stream renders a gap as continuity.
//!
//! # Why `done` is a member rather than an error
//!
//! A polled method has no other way to say "stop calling me": its return type is a value, not an
//! outcome. [`is_terminal`] is the convention, checked in one place.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::api::remote::SessionCatalog;
use crate::session::projection::ProjectionRegistry;

/// How many frames one observation returns by default.
///
/// Small because this is one poll of a live watch: a client that asked for a thousand would render a
/// backlog before it caught up with now, which is worse than a smaller page and a next call.
pub const DEFAULT_OBSERVE_LIMIT: usize = 64;

/// One observation of a session's durable events.
///
/// `fromSeq` is the cursor. It defaults to 0 -- the beginning -- because a client that omits it is
/// asking for the log rather than for "nothing".
pub async fn observe_event(
    args: &Value,
    catalog: &Arc<dyn SessionCatalog>,
) -> Result<Value, crate::api::remote::RemoteError> {
    let session_id = required_str(args, "sessionId", "observeEvent")?;
    let from = args.get("fromSeq").and_then(Value::as_u64).unwrap_or(0);
    let limit = usize::try_from(
        args.get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_OBSERVE_LIMIT as u64),
    )
    .unwrap_or(DEFAULT_OBSERVE_LIMIT)
    .max(1);

    let Some(record) = catalog.read(&session_id).await else {
        // A session this deployment does not know is a caller error, not an empty stream: answering
        // `done` would tell a client the log ended when it never existed.
        return Err(crate::api::remote::RemoteError::Failed(format!(
            "no session `{session_id}` is known to this deployment"
        )));
    };

    // Through `SessionStream` rather than filtering the raw events: that type owns the seq
    // assignment and the frame shape, so this endpoint and `session.stream` cannot disagree about
    // which sequence a frame carries.
    let mut stream = crate::session::stream::SessionStream::new(record.events, from);
    let mut frames = Vec::new();
    while frames.len() < limit {
        let Some(frame) = stream.next_frame() else {
            break;
        };
        frames.push(json!({
            "seq": frame.seq,
            "type": frame.kind,
            "data": frame.payload,
        }));
    }
    let last = frames
        .last()
        .and_then(|frame| frame.get("seq"))
        .and_then(Value::as_u64);

    Ok(json!({
        "frames": frames,
        // Where to resume. `None` when nothing was returned, which a client reads as "poll again from
        // where you were" rather than "start over".
        "nextSeq": last.map(|seq| seq + 1),
        "done": false,
    }))
}

/// One observation of a session's projections.
///
/// Folded from that session's log. The registry's stored map is not a session: it is whatever the
/// last fold wrote, and a poll of one session would otherwise answer another's state.
pub async fn observe_projections(
    args: &Value,
    catalog: &Arc<dyn SessionCatalog>,
    projections: &Arc<ProjectionRegistry>,
) -> Result<Value, crate::api::remote::RemoteError> {
    let session_id = required_str(args, "sessionId", "observeProjections")?;
    let Some(record) = catalog.read(&session_id).await else {
        return Err(crate::api::remote::RemoteError::Failed(format!(
            "no session `{session_id}` is known to this deployment"
        )));
    };
    let projected = projections.project_summary(&record.events);
    Ok(json!({
        "frames": [{
            "sessionId": session_id,
            "asOfSeq": projected.as_of,
            "projections": projected.values,
        }],
        "keys": projections.keys(),
        "done": false,
    }))
}

/// One observation of a session's passive state.
pub fn observe_state(state: &Value) -> Value {
    json!({
        "frames": [state],
        "done": false,
    })
}

/// Reads a required string argument.
fn required_str(
    args: &Value,
    key: &str,
    method: &str,
) -> Result<String, crate::api::remote::RemoteError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| crate::api::remote::RemoteError::InvalidArguments {
            namespace: "session".to_owned(),
            method: method.to_owned(),
            message: format!("`{key}` is required"),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_session_id_names_the_field() {
        let error = required_str(&json!({}), "sessionId", "observeEvent").expect_err("required");
        assert!(error.to_string().contains("sessionId"), "{error}");
    }

    #[test]
    fn an_observation_always_says_whether_it_ended() {
        // The convention the subscription carrier reads: a value without `done` is polled forever.
        let value = observe_state(&json!({"sessionId": "s"}));
        assert_eq!(value["done"], json!(false));
        assert!(value["frames"].is_array());
    }
}
