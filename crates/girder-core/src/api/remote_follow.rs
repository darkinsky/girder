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

/// The snapshot for one admitted peer.
///
/// An empty peer is the legacy single-user connection, and the shared browser peer is the local
/// owner. A published owner stays an owner. Every other identity receives the grants stored
/// for it, and an identity with none receives an empty list.
#[must_use]
pub fn snapshot_for_directory(
    directory: &crate::api::gateway::PeerDirectory,
    peer: &str,
) -> RemoteAccessSnapshot {
    if peer.is_empty() || peer == directory.browser().id().as_str() {
        return RemoteAccessSnapshot {
            owner: true,
            grants: Vec::new(),
        };
    }
    let id = crate::api::gateway::PeerId::new(peer);
    let owner = directory
        .peer(&id)
        .is_some_and(|peer| peer.principal().is_owner());
    RemoteAccessSnapshot {
        owner,
        grants: if owner {
            Vec::new()
        } else {
            directory
                .grants(&id)
                .iter()
                .map(crate::api::gateway::AccessGrant::to_wire)
                .collect()
        },
    }
}

/// Enforces the grants stored on a [`PeerDirectory`](crate::api::gateway::PeerDirectory).
///
/// Owners and the shared browser peer may call every method. A non-owner may call
/// `remoteAccess.follow` so it can learn its grants, and every other call only when each declared
/// resource is covered by a grant whose actions include that endpoint.
#[derive(Clone, Debug)]
pub struct DirectoryPolicy {
    directory: crate::api::gateway::PeerDirectory,
}

impl DirectoryPolicy {
    /// Builds a policy over the same directory that admits peers.
    #[must_use]
    pub fn new(directory: crate::api::gateway::PeerDirectory) -> Self {
        Self { directory }
    }
}

impl crate::guard::access::Policy for DirectoryPolicy {
    fn decide(&self, request: &crate::guard::access::Request) -> crate::guard::access::Decision {
        if request.method == "remoteAccess.follow" {
            return crate::guard::access::Decision::Allow;
        }
        let peer = request.peer.as_str();
        if peer.is_empty() || peer == self.directory.browser().id().as_str() {
            return crate::guard::access::Decision::Allow;
        }
        let id = crate::api::gateway::PeerId::new(peer);
        if self
            .directory
            .peer(&id)
            .is_some_and(|peer| peer.principal().is_owner())
        {
            return crate::guard::access::Decision::Allow;
        }
        let action = request.method.replace('.', "/");
        let grants = self.directory.grants(&id);
        if request
            .resources
            .iter()
            .all(|resource| grant_covers(&grants, &action, resource))
        {
            return crate::guard::access::Decision::Allow;
        }
        crate::guard::access::Decision::Refuse {
            reason: "no-allow".to_owned(),
        }
    }
}

fn grant_covers(
    grants: &[crate::api::gateway::AccessGrant],
    action: &str,
    resource: &crate::guard::access::Resource,
) -> bool {
    grants.iter().any(|grant| {
        grant.kind == resource.kind
            && grant.actions.iter().any(|candidate| candidate == action)
            && match (&grant.id, resource.id.is_empty()) {
                (_, true) => grant.id.is_none(),
                (None, false) => true,
                (Some(id), false) => id == &resource.id,
            }
    })
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
        DirectoryPolicy, RemoteAccessSnapshot, register_remote_access_follow,
        register_remote_access_follow_with, snapshot_for_directory,
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

    #[test]
    fn a_directory_snapshot_keeps_owners_and_hides_guests() {
        use crate::api::gateway::{Interactions, PeerDirectory, Principal};

        let directory = PeerDirectory::new();
        assert!(snapshot_for_directory(&directory, "").owner);
        assert!(snapshot_for_directory(&directory, directory.browser().id().as_str()).owner);

        let guest = directory.publish(Principal::named("test-guest", "spec"), Interactions::None);
        let guest_snapshot = snapshot_for_directory(&directory, guest.id().as_str());
        assert!(!guest_snapshot.owner);
        assert!(guest_snapshot.grants.is_empty());

        let owner = directory.publish(Principal::owner(), Interactions::Answer);
        assert!(snapshot_for_directory(&directory, owner.id().as_str()).owner);

        directory.set_grants(
            guest.id(),
            vec![crate::api::gateway::AccessGrant::new(
                "session",
                Some("s1".to_owned()),
                ["session/page"],
            )],
        );
        let granted = snapshot_for_directory(&directory, guest.id().as_str());
        assert_eq!(granted.grants[0]["resource"]["id"], "s1");
        assert_eq!(granted.grants[0]["actions"][0], "session/page");
    }

    #[tokio::test]
    async fn directory_policy_allows_only_the_granted_resource() {
        use crate::api::gateway::{AccessGrant, Interactions, PeerDirectory, Principal};
        use crate::guard::access::Declaration;

        let directory = PeerDirectory::new();
        let guest = directory.publish(Principal::named("test-guest", "spec"), Interactions::None);
        directory.set_grants(
            guest.id(),
            vec![AccessGrant::new(
                "session",
                Some("s1".to_owned()),
                ["session/page"],
            )],
        );
        let mut registry = RemoteRegistry::new();
        registry.declare("session", "page", Declaration::read("session"));
        registry.register("session", "page", |call| {
            Box::pin(async move { Ok(call.args) })
        });
        registry.set_policy(Arc::new(DirectoryPolicy::new(directory.clone())));

        let allowed = registry
            .dispatch(
                "session",
                "page",
                RemoteCall {
                    args: serde_json::json!({"sessionId": "s1"}),
                    peer: guest.id().as_str().to_owned(),
                },
            )
            .await;
        assert!(allowed.is_ok(), "{allowed:?}");
        let refused = registry
            .dispatch(
                "session",
                "page",
                RemoteCall {
                    args: serde_json::json!({"sessionId": "s2"}),
                    peer: guest.id().as_str().to_owned(),
                },
            )
            .await
            .expect_err("another session is outside the grant");
        assert_eq!(refused.code(), "REFUSED");

        let owner = directory.publish(Principal::owner(), Interactions::Answer);
        let owner_call = registry
            .dispatch(
                "session",
                "page",
                RemoteCall {
                    args: serde_json::json!({"sessionId": "s2"}),
                    peer: owner.id().as_str().to_owned(),
                },
            )
            .await;
        assert!(owner_call.is_ok(), "{owner_call:?}");
    }

    #[tokio::test]
    async fn set_grants_pushes_an_upsert_on_follow() {
        use crate::api::gateway::{AccessGrant, Interactions, PeerDirectory, Principal};

        let directory = PeerDirectory::new();
        let guest = directory.publish(Principal::named("test-guest", "spec"), Interactions::None);
        let source = {
            let directory = directory.clone();
            Arc::new(move |peer: &str| snapshot_for_directory(&directory, peer))
        };
        let mut registry = RemoteRegistry::new();
        register_remote_access_follow_with(&mut registry, source, directory.access_changed());
        let call = RemoteCall {
            args: serde_json::json!({}),
            peer: guest.id().as_str().to_owned(),
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
        assert!(baseline["grants"].as_array().unwrap().is_empty());

        directory.set_grants(
            guest.id(),
            vec![AccessGrant::new(
                "session",
                Some("s1".to_owned()),
                ["session/page"],
            )],
        );
        let upsert = tokio::time::timeout(std::time::Duration::from_secs(1), frames.recv())
            .await
            .expect("upsert")
            .expect("stream")
            .expect("ok");
        assert_eq!(upsert["type"], "upsert");
        assert_eq!(upsert["grant"]["resource"]["id"], "s1");
        assert_eq!(upsert["grant"]["actions"][0], "session/page");

        cancel.cancel();
        let _ = running.await;
    }
}
