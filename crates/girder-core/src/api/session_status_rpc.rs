//! `sessionStatus.*`: the Web client's unread/completed-turn reading records.
//!
//! Records stay process-local. A turn that ends on the agent bus advances `completedTurn`; explicit
//! `mark` calls move the read cursor. History written before the service starts does not raise a
//! reminder, matching the upstream listener.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::api::remote::{RemoteCall, RemoteError, RemoteRegistry, SessionCatalog};
use crate::{AgentEvent, AgentTurn, EventBus, FinishReason, Order};

const NAMESPACE: &str = "sessionStatus";

#[derive(Clone, Default)]
struct Store {
    records: Arc<Mutex<BTreeMap<String, Value>>>,
    pending: Arc<Mutex<Vec<Value>>>,
    changed: Arc<Notify>,
}

impl Store {
    fn complete(&self, session_id: &str, turn: u64, failed: bool) {
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let current = records
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| empty(session_id));
        let state = json!({
            "sessionId": session_id,
            "completedTurn": turn,
            "failed": failed,
            "readTurn": current["readTurn"].as_u64().unwrap_or(0),
            "manualUnread": current["manualUnread"].as_bool().unwrap_or(false),
        });
        records.insert(session_id.to_owned(), state.clone());
        drop(records);
        self.publish(state);
    }

    fn publish(&self, state: Value) {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(state);
        self.changed.notify_waiters();
    }
}

/// Registers `sessionStatus.mark` and the live `sessionStatus.follow` stream.
pub fn register_session_status_methods(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
    bus: &Arc<EventBus>,
) {
    let store = Store::default();
    let listened = store.clone();
    bus.on::<AgentTurn, _>("session-status", Order::Append, move |event| {
        let AgentEvent::TurnEnded {
            session_id,
            turn,
            reason,
        } = event
        else {
            return;
        };
        listened.complete(
            &session_id,
            turn,
            matches!(reason, FinishReason::Error { .. }),
        );
    });

    let mark_store = store.clone();
    let mark_catalog = Arc::clone(catalog);
    registry.register(NAMESPACE, "mark", move |call: RemoteCall| {
        let store = mark_store.clone();
        let catalog = Arc::clone(&mark_catalog);
        Box::pin(async move { mark(call, store, catalog).await })
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
                .unwrap_or_else(|error| error.into_inner())
                .values()
                .cloned()
                .collect::<Vec<_>>();
            if !open.push(json!({ "type": "baseline", "items": baseline })) {
                return;
            }
            loop {
                let notified = store.changed.notified();
                tokio::pin!(notified);
                let batch = std::mem::take(
                    &mut *store
                        .pending
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()),
                );
                if batch.is_empty() {
                    tokio::select! {
                        () = open.cancel.cancelled() => return,
                        () = &mut notified => continue,
                        incoming = open.uplink.recv() => {
                            if !matches!(incoming, Ok(Some(_))) {
                                return;
                            }
                        }
                    }
                } else {
                    for state in batch {
                        if !open.push(json!({ "type": "changed", "state": state })) {
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

async fn mark(
    call: RemoteCall,
    store: Store,
    catalog: Arc<dyn SessionCatalog>,
) -> Result<Value, RemoteError> {
    let session_id = call
        .args
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid("`sessionId` is required"))?
        .to_owned();
    if catalog.read(&session_id).await.is_none() {
        return Err(RemoteError::Failed(format!(
            "session-status/not-found: session `{session_id}` no longer exists"
        )));
    }
    let action = call
        .args
        .get("action")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let action_type = action
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("`action.type` is required"))?;
    let mut records = store
        .records
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let current = records
        .get(&session_id)
        .cloned()
        .unwrap_or_else(|| empty(&session_id));
    let completed = current["completedTurn"].as_u64().unwrap_or(0);
    let read_turn = current["readTurn"].as_u64().unwrap_or(0);
    let manual_unread = current["manualUnread"].as_bool().unwrap_or(false);
    let (manual_unread, read_turn) = match action_type {
        "unread" => (true, read_turn),
        "read" | "acknowledge" => {
            let through = action
                .get("throughTurn")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    invalid("`action.throughTurn` is required for read and acknowledge")
                })?;
            let advanced = read_turn.max(through.min(completed));
            (action_type == "acknowledge" && manual_unread, advanced)
        }
        _ => {
            return Err(invalid(
                "`action.type` must be unread, read, or acknowledge",
            ));
        }
    };
    let state = json!({
        "sessionId": session_id,
        "completedTurn": completed,
        "failed": current["failed"].as_bool().unwrap_or(false),
        "readTurn": read_turn,
        "manualUnread": manual_unread,
    });
    records.insert(session_id, state.clone());
    drop(records);
    store.publish(state);
    Ok(Value::Null)
}

fn empty(session_id: &str) -> Value {
    json!({
        "sessionId": session_id,
        "completedTurn": 0,
        "failed": false,
        "readTurn": 0,
        "manualUnread": false,
    })
}

fn invalid(message: &str) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: "mark".to_owned(),
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::register_session_status_methods;
    use crate::api::extra::UplinkInbox;
    use crate::api::remote::{DirectoryCatalog, RemoteCall, RemoteRegistry, StreamOpen};
    use crate::{AgentEvent, AgentTurn, EventBus, FinishReason, SessionStore};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "girder-session-status-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("created");
        dir
    }

    #[tokio::test]
    async fn a_completed_turn_stays_unread_until_it_is_marked() {
        let dir = scratch("turns");
        SessionStore::new(dir.join("s1.jsonl.zst"))
            .touch()
            .expect("session");
        let catalog: Arc<dyn crate::SessionCatalog> = Arc::new(DirectoryCatalog::new(&dir));
        let bus = Arc::new(EventBus::new());
        let mut registry = RemoteRegistry::new();
        register_session_status_methods(&mut registry, &catalog, &bus);

        let call = RemoteCall {
            args: serde_json::json!({}),
            peer: "test".to_owned(),
        };
        let handler = registry
            .stream_method("sessionStatus", "follow", &call)
            .expect("follow");
        let cancel = tokio_util::sync::CancellationToken::new();
        let (open, mut frames) = StreamOpen::pair(
            call,
            Arc::new(UplinkInbox::new(1024, "sessionStatus/follow")),
            cancel.clone(),
        );
        let running = tokio::spawn(async move { handler(open).await });
        let baseline = frames.recv().await.expect("baseline").expect("ok");
        assert_eq!(baseline["items"].as_array().map(Vec::len), Some(0));

        bus.emit::<AgentTurn>(AgentEvent::TurnEnded {
            session_id: "s1".to_owned(),
            turn: 3,
            reason: FinishReason::Error {
                message: "boom".to_owned(),
            },
        });
        let changed = frames.recv().await.expect("changed").expect("ok");
        assert_eq!(changed["type"], "changed");
        assert_eq!(changed["state"]["completedTurn"], 3);
        assert_eq!(changed["state"]["failed"], true);
        assert_eq!(changed["state"]["readTurn"], 0);

        registry
            .dispatch(
                "sessionStatus",
                "mark",
                RemoteCall {
                    args: serde_json::json!({
                        "sessionId": "s1",
                        "action": { "type": "read", "throughTurn": 3 }
                    }),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect("marked");
        let read = frames.recv().await.expect("read").expect("ok");
        assert_eq!(read["state"]["readTurn"], 3);
        assert_eq!(read["state"]["manualUnread"], false);

        registry
            .dispatch(
                "sessionStatus",
                "mark",
                RemoteCall {
                    args: serde_json::json!({
                        "sessionId": "s1",
                        "action": { "type": "unread" }
                    }),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect("unread");
        let unread = frames.recv().await.expect("unread").expect("ok");
        assert_eq!(unread["state"]["readTurn"], 3);
        assert_eq!(unread["state"]["manualUnread"], true);

        let missing = registry
            .dispatch(
                "sessionStatus",
                "mark",
                RemoteCall {
                    args: serde_json::json!({
                        "sessionId": "absent",
                        "action": { "type": "unread" }
                    }),
                    peer: "test".to_owned(),
                },
            )
            .await
            .expect_err("missing session");
        assert!(missing.to_string().contains("session-status/not-found"));

        cancel.cancel();
        let _ = running.await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
