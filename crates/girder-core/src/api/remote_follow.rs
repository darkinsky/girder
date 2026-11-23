//! `remoteAccess.follow`: the calling peer's access snapshot.
//!
//! Upstream yields one baseline, then `upsert` and `remove` frames when grants change. The owner
//! baseline is only `{ type: "baseline", owner: true }`. The default registration serves the
//! single-owner deployment; [`register_remote_access_follow_with`] supplies live non-owner grants
//! for a host that has a peer policy.

use crate::api::remote::RemoteRegistry;
use crate::guard::access::Declaration;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Notify;

/// One peer's current remote-access snapshot.
///
/// The host callback decides whether the peer is the owner and supplies the non-owner grants.
/// Grants use the wire shape from `remoteAccess.follow`: each value has a `resource` and sorted
/// `actions` field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteAccessSnapshot {
    /// Owner peers receive only the owner baseline.
    pub owner: bool,
    /// Grants currently available to a non-owner peer.
    pub grants: Vec<Value>,
}

/// Registers `remoteAccess.follow` with a live snapshot source.
///
/// The callback is evaluated once for the opening baseline and after every notification. A host
/// can therefore replace or remove grants without rebuilding the Remote registry.
pub fn register_remote_access_follow_with(
    registry: &mut RemoteRegistry,
    snapshot: Arc<dyn Fn(&str) -> RemoteAccessSnapshot + Send + Sync>,
    changed: Arc<Notify>,
) {
    registry.declare("remoteAccess", "follow", Declaration::read("global"));
    registry.register_stream("remoteAccess", "follow", move |open| {
        let snapshot = Arc::clone(&snapshot);
        let changed = Arc::clone(&changed);
        Box::pin(async move {
            let peer = open.call.peer.clone();
            let initial = snapshot(&peer);
            if initial.owner {
                if !open.push(serde_json::json!({ "type": "baseline", "owner": true })) {
                    return;
                }
            } else {
                let mut previous = initial.grants;
                if !open.push(serde_json::json!({
                    "type": "baseline",
                    "owner": false,
                    "grants": previous.clone(),
                })) {
                    return;
                }
                loop {
                    let notified = changed.notified();
                    tokio::pin!(notified);
                    tokio::select! {
                        () = open.cancel.cancelled() => return,
                        incoming = open.uplink.recv() => {
                            if !matches!(incoming, Ok(Some(_))) {
                                return;
                            }
                        }
                        () = &mut notified => {
                            let current = snapshot(&peer);
                            if current.owner {
                                return;
                            }
                            for grant in &current.grants {
                                let Some(resource) = grant.get("resource") else {
                                    continue;
                                };
                                let prior = previous.iter().find(|item| {
                                    item.get("resource") == Some(resource)
                                });
                                if prior != Some(grant) && !open.push(serde_json::json!({
                                    "type": "upsert",
                                    "grant": grant,
                                })) {
                                    return;
                                }
                            }
                            for grant in &previous {
                                let Some(resource) = grant.get("resource") else {
                                    continue;
                                };
                                if !current.grants.iter().any(|item| {
                                    item.get("resource") == Some(resource)
                                }) && !open.push(serde_json::json!({
                                    "type": "remove",
                                    "resource": resource,
                                })) {
                                    return;
                                }
                            }
                            previous = current.grants;
                        }
                    }
                }
            }
            loop {
                tokio::select! {
                    () = open.cancel.cancelled() => return,
                    incoming = open.uplink.recv() => {
                        if !matches!(incoming, Ok(Some(_))) {
                            return;
                        }
                    }
                    () = changed.notified() => {}
                }
            }
        })
    });
}

/// Registers `remoteAccess.follow`.
pub fn register_remote_access_follow(registry: &mut RemoteRegistry) {
    let snapshot = Arc::new(|_peer: &str| RemoteAccessSnapshot {
        owner: true,
        grants: Vec::new(),
    });
    register_remote_access_follow_with(registry, snapshot, Arc::new(Notify::new()));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        RemoteAccessSnapshot, register_remote_access_follow, register_remote_access_follow_with,
    };
    use crate::api::extra::UplinkInbox;
    use crate::api::remote::{RemoteCall, RemoteRegistry, StreamOpen};

    #[tokio::test]
    async fn follow_opens_with_the_owner_baseline() {
        let mut registry = RemoteRegistry::new();
        register_remote_access_follow(&mut registry);
        let call = RemoteCall {
            args: serde_json::json!({}),
            peer: "browser".to_owned(),
        };
        let handler = registry
            .stream_method("remoteAccess", "follow", &call)
            .expect("follow is a stream");
        let inbox = Arc::new(UplinkInbox::new(1024, "remoteAccess/follow"));
        let cancel = tokio_util::sync::CancellationToken::new();
        let (open, mut frames) = StreamOpen::pair(call, inbox, cancel.clone());
        let running = tokio::spawn(async move {
            handler(open).await;
        });
        let frame = frames.recv().await.expect("baseline").expect("ok");
        assert_eq!(frame["type"], "baseline");
        assert_eq!(frame["owner"], true);
        cancel.cancel();
        let _ = running.await;
    }

    #[tokio::test]
    async fn follow_streams_non_owner_grant_updates_and_removals() {
        let state = Arc::new(std::sync::Mutex::new(RemoteAccessSnapshot {
            owner: false,
            grants: vec![serde_json::json!({
                "resource": { "kind": "session", "id": "s1" },
                "actions": ["session/read"],
            })],
        }));
        let changed = Arc::new(tokio::sync::Notify::new());
        let source = {
            let state = Arc::clone(&state);
            Arc::new(move |_peer: &str| {
                state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
        };
        let mut registry = RemoteRegistry::new();
        register_remote_access_follow_with(&mut registry, source, Arc::clone(&changed));
        let call = RemoteCall {
            args: serde_json::json!({}),
            peer: "guest".to_owned(),
        };
        let handler = registry
            .stream_method("remoteAccess", "follow", &call)
            .expect("follow is a stream");
        let inbox = Arc::new(UplinkInbox::new(1024, "remoteAccess/follow"));
        let cancel = tokio_util::sync::CancellationToken::new();
        let (open, mut frames) = StreamOpen::pair(call, inbox, cancel.clone());
        let running = tokio::spawn(async move {
            handler(open).await;
        });
        let baseline = frames.recv().await.expect("baseline").expect("ok");
        assert_eq!(baseline["owner"], false);
        assert_eq!(baseline["grants"][0]["resource"]["id"], "s1");

        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .grants = vec![serde_json::json!({
            "resource": { "kind": "session", "id": "s2" },
            "actions": ["session/read", "session/prompt"],
        })];
        changed.notify_waiters();
        let upsert = tokio::time::timeout(std::time::Duration::from_secs(1), frames.recv())
            .await
            .expect("upsert")
            .expect("stream")
            .expect("ok");
        assert_eq!(upsert["type"], "upsert");
        assert_eq!(upsert["grant"]["resource"]["id"], "s2");
        let remove = tokio::time::timeout(std::time::Duration::from_secs(1), frames.recv())
            .await
            .expect("remove")
            .expect("stream")
            .expect("ok");
        assert_eq!(remove["type"], "remove");
        assert_eq!(remove["resource"]["id"], "s1");

        cancel.cancel();
        let _ = running.await;
    }
}
