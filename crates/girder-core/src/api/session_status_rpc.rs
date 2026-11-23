//! `sessionStatus.*`: the Web client's unread/completed-turn reading records.
//!
//! The upstream service lives beside the client package, but it is a Host Remote namespace used by
//! the WebUI. Girder keeps the records process-local until durable session-status storage is added.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify};

use crate::api::remote::{RemoteCall, RemoteRegistry};

const NAMESPACE: &str = "sessionStatus";

#[derive(Clone, Default)]
struct Store {
    records: Arc<Mutex<BTreeMap<String, Value>>>,
    changed: Arc<Notify>,
}

/// Registers `sessionStatus.mark` and the live `sessionStatus.follow` stream.
pub fn register_session_status_methods(registry: &mut RemoteRegistry) {
    let store = Store::default();
    let mark_store = store.clone();
    registry.register(NAMESPACE, "mark", move |call: RemoteCall| {
        let store = mark_store.clone();
        Box::pin(async move {
            let session_id = call
                .args
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| crate::api::remote::RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "mark".to_owned(),
                    message: "`sessionId` is required".to_owned(),
                })?;
            let action = call
                .args
                .get("action")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let current = store
                .records
                .lock()
                .await
                .get(session_id)
                .cloned()
                .unwrap_or_else(|| {
                    json!({
                        "sessionId": session_id,
                        "completedTurn": 0,
                        "failed": false,
                        "readTurn": 0,
                        "manualUnread": false,
                    })
                });
            let through_turn = action
                .get("throughTurn")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| current["readTurn"].as_u64().unwrap_or(0));
            let manual_unread = action.get("type").and_then(Value::as_str) == Some("unread");
            let state = json!({
                "sessionId": session_id,
                "completedTurn": current["completedTurn"].as_u64().unwrap_or(0),
                "failed": current["failed"].as_bool().unwrap_or(false),
                "readTurn": through_turn,
                "manualUnread": manual_unread,
            });
            store
                .records
                .lock()
                .await
                .insert(session_id.to_owned(), state);
            store.changed.notify_waiters();
            Ok(Value::Null)
        })
    });
    registry.declare(
        NAMESPACE,
        "mark",
        crate::guard::access::Declaration::write("session"),
    );

    registry.register_stream(NAMESPACE, "follow", move |open| {
        let store = store.clone();
        Box::pin(async move {
            let baseline = store
                .records
                .lock()
                .await
                .values()
                .cloned()
                .collect::<Vec<_>>();
            if !open.push(json!({ "type": "baseline", "items": baseline })) {
                return;
            }
            loop {
                tokio::select! {
                    () = open.cancel.cancelled() => return,
                    _ = store.changed.notified() => {
                        let changed = store.records.lock().await.values().cloned().collect::<Vec<_>>();
                        for state in changed {
                            if !open.push(json!({ "type": "changed", "state": state })) {
                                return;
                            }
                        }
                    }
                    incoming = open.uplink.recv() => {
                        if !matches!(incoming, Ok(Some(_))) {
                            return;
                        }
                    }
                }
            }
        })
    });
    registry.declare(
        NAMESPACE,
        "follow",
        crate::guard::access::Declaration::read("session"),
    );
}
