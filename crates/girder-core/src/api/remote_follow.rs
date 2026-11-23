//! `remoteAccess.follow`: the calling peer's access snapshot.
//!
//! Upstream yields one baseline, then `upsert` and `remove` frames when grants change. The owner
//! baseline is only `{ type: "baseline", owner: true }`. This registration produces that baseline
//! and stays open until the client cancels or half-closes the uplink. Grant diffs for a non-owner
//! peer are still missing.

use crate::api::remote::RemoteRegistry;
use crate::guard::access::Declaration;

/// Registers `remoteAccess.follow`.
pub fn register_remote_access_follow(registry: &mut RemoteRegistry) {
    registry.declare("remoteAccess", "follow", Declaration::read("global"));
    registry.register_stream("remoteAccess", "follow", |open| {
        Box::pin(async move {
            if !open.push(serde_json::json!({ "type": "baseline", "owner": true })) {
                return;
            }
            loop {
                tokio::select! {
                    () = open.cancel.cancelled() => return,
                    incoming = open.uplink.recv() => {
                        if !matches!(incoming, Ok(Some(_))) {
                            return;
                        }
                    }
                }
            }
        })
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::register_remote_access_follow;
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
}
