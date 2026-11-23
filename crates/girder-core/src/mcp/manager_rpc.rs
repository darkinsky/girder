//! `mcpManager.{probe, restart}`: what a configured MCP server offers, and starting it again.
//!
//! # What upstream has
//!
//! `packages/mcp/mcp-manager/src/index.ts` marks three methods: a `watch` stream, `restart`, and
//! `probe`. `probe` answers "does this configured server respond, and what would it contribute";
//! `restart` tears down and starts a server again.
//!
//! # What this port already had, and did not register
//!
//! The subsystem is here and is not small: [`crate::mcp::client`] owns the JSON-RPC wire layer with
//! an id allocator that refuses to wrap, [`crate::mcp::stdio`] the pipe framing,
//! [`crate::mcp::process`] the child lifecycle, and [`crate::mcp::config::start_all`] does the
//! handshake and tool listing. What was missing was the endpoint -- which is the same shape this
//! pass has met five times now: written, tested, never registered.
//!
//! # Why `restart` stops the live child first
//!
//! Upstream's `restart` stops a live connection and starts it again. When a registry was passed in,
//! that is what happens: the child the model is calling is stopped, then started, so the next call
//! reaches the new process. With no registry, the method still connects once and reports the result.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::api::remote::{RemoteCall, RemoteRegistry};
use crate::mcp::config::{ServerConfig, start_all};

/// The namespace these methods register under, as upstream spells it.
pub const NAMESPACE: &str = "mcpManager";

/// Renders one start outcome the way `probe` answers.
///
/// A failure is `ok: false` with the server's own diagnostic rather than an error: a probe that
/// raised would make one unreachable server abort a client's whole refresh, when what it needs is a
/// row saying which one failed.
fn server_views(live: Option<&crate::mcp::Registry>) -> Value {
    let Some(live) = live else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        live.names()
            .into_iter()
            .map(|name| {
                let status = live.status(&name);
                let state = status
                    .as_ref()
                    .map(crate::mcp::Status::as_str)
                    .unwrap_or("unknown");
                let mut row = json!({
                    "name": name,
                    "configured": true,
                    "status": {"state": state},
                    "ready": status.as_ref().is_some_and(crate::mcp::Status::is_ready),
                });
                if let Some(crate::mcp::Status::Failed { reason }) = status {
                    row["status"]["lastError"] = json!(reason);
                }
                row
            })
            .collect(),
    )
}

fn outcome_to_wire(server: &crate::mcp::config::StartedServer) -> Value {
    let mut row = json!({
        "name": server.name,
        "ok": server.failed.is_none(),
        "tools": server.tools,
    });
    if let Some(failure) = &server.failed {
        row["code"] = json!("probe_failed");
        row["message"] = json!(failure);
    }
    row
}

/// Registers `mcpManager.{probe, restart}`.
pub fn register_mcp_manager_methods(
    registry: &mut RemoteRegistry,
    servers: &Arc<Vec<ServerConfig>>,
    live: Option<Arc<crate::mcp::Registry>>,
) {
    let probing = Arc::clone(servers);
    registry.register(NAMESPACE, "probe", move |call: RemoteCall| {
        let servers = Arc::clone(&probing);
        Box::pin(async move {
            // A named server probes one; no name probes every configured server, which is what a
            // client refreshing its MCP page wants.
            let wanted = call.args.get("name").and_then(Value::as_str);
            let selected: Vec<ServerConfig> = match wanted {
                Some(name) => servers
                    .iter()
                    .filter(|server| server.name == name)
                    .cloned()
                    .collect(),
                None => servers.as_ref().clone(),
            };
            if let Some(name) = wanted {
                if selected.is_empty() {
                    return Ok(json!({
                        "ok": false,
                        "code": "unknown_server",
                        "message": format!("no MCP server named `{name}` is configured"),
                        "configured": servers.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
                    }));
                }
            }
            // `start_all` is blocking by design: it drives a child process over stdio, and the one
            // caller it was written for is a boot path. Off the reactor here because an RPC must not
            // stall every other connection while a server starts.
            let outcomes = tokio::task::spawn_blocking(move || start_all(&selected))
                .await
                .unwrap_or_default();
            Ok(json!({
                "ok": outcomes.iter().all(|server| server.failed.is_none()),
                "servers": outcomes.iter().map(outcome_to_wire).collect::<Vec<_>>(),
            }))
        })
    });

    let restarting = Arc::clone(servers);
    let restart_live = live.clone();
    registry.register(NAMESPACE, "restart", move |call: RemoteCall| {
        let servers = Arc::clone(&restarting);
        let live = restart_live.clone();
        Box::pin(async move {
            let name = call
                .args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| crate::api::remote::RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "restart".to_owned(),
                    message: "`name` is required".to_owned(),
                })?
                .to_owned();
            let Some(server) = servers.iter().find(|server| server.name == name).cloned() else {
                return Ok(json!({
                    "ok": false,
                    "code": "unknown_server",
                    "message": format!("no MCP server named `{name}` is configured"),
                }));
            };
            // A live registry is the connection the model is calling. Stop it before starting
            // again, so the old child is gone and the next call reaches the new process.
            if let Some(live) = &live {
                let _ = live.stop(&name).await;
                match live.start(&name).await {
                    Ok(catalogue) => {
                        return Ok(json!({
                            "ok": true,
                            "servers": [{
                                "name": name,
                                "ok": true,
                                "tools": catalogue.tools.len(),
                            }],
                        }));
                    }
                    Err(error) => {
                        return Ok(json!({
                            "ok": false,
                            "servers": [{
                                "name": name,
                                "ok": false,
                                "code": "probe_failed",
                                "message": error.to_string(),
                                "tools": 0,
                            }],
                        }));
                    }
                }
            }
            let outcomes = tokio::task::spawn_blocking(move || start_all(&[server]))
                .await
                .unwrap_or_default();
            Ok(json!({
                "ok": outcomes.iter().all(|server| server.failed.is_none()),
                "servers": outcomes.iter().map(outcome_to_wire).collect::<Vec<_>>(),
            }))
        })
    });

    let watching = live.clone();
    registry.register_stream(NAMESPACE, "watch", move |open| {
        let live = watching.clone();
        Box::pin(async move {
            let mut last = Value::Null;
            loop {
                let snapshot = server_views(live.as_deref());
                if snapshot != last {
                    if !open.push(snapshot.clone()) {
                        return;
                    }
                    last = snapshot;
                }
                let notified = live.as_ref().map(|live| live.notified());
                if server_views(live.as_deref()) != last {
                    continue;
                }
                tokio::select! {
                    () = open.cancel.cancelled() => return,
                    incoming = open.uplink.recv() => {
                        if !matches!(incoming, Ok(Some(_))) {
                            return;
                        }
                    }
                    () = async {
                        if let Some(notified) = notified {
                            notified.await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => {}
                }
            }
        })
    });
    registry.declare(
        NAMESPACE,
        "watch",
        crate::guard::access::Declaration::read("mcpServer"),
    );

    registry.declare(
        NAMESPACE,
        "probe",
        crate::guard::access::Declaration::read("mcpServer"),
    );
    registry.declare(
        NAMESPACE,
        "restart",
        crate::guard::access::Declaration::write("mcpServer"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Idle;

    impl crate::mcp::Transport for Idle {
        fn start(&self) -> crate::BoxFut<Result<crate::mcp::Catalogue, crate::mcp::McpError>> {
            Box::pin(async { Ok(crate::mcp::Catalogue::default()) })
        }

        fn call(
            &self,
            _tool: &str,
            _arguments: Value,
        ) -> crate::BoxFut<Result<crate::mcp::CallResult, crate::mcp::McpError>> {
            Box::pin(async {
                Err(crate::mcp::McpError::UnknownTool {
                    name: "idle".to_owned(),
                })
            })
        }

        fn stop(&self) -> crate::BoxFut<Result<(), crate::mcp::McpError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn watch_pushes_the_server_list_then_the_next_change() {
        let live = Arc::new(crate::mcp::Registry::new());
        let mut registry = RemoteRegistry::new();
        register_mcp_manager_methods(
            &mut registry,
            &Arc::new(Vec::new()),
            Some(Arc::clone(&live)),
        );
        let call = RemoteCall {
            args: json!({}),
            peer: "test".to_owned(),
        };
        let handler = registry
            .stream_method(NAMESPACE, "watch", &call)
            .expect("watch is a stream");
        let inbox = Arc::new(crate::api::extra::UplinkInbox::new(
            1024,
            "mcpManager/watch",
        ));
        let cancel = tokio_util::sync::CancellationToken::new();
        let (open, mut frames) = crate::api::remote::StreamOpen::pair(call, inbox, cancel.clone());
        let running = tokio::spawn(async move {
            handler(open).await;
        });

        let first = frames.recv().await.expect("opens").expect("ok");
        assert_eq!(first, json!([]));

        live.add("alpha", Arc::new(Idle)).expect("added");
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), frames.recv())
            .await
            .expect("the change arrives")
            .expect("frame")
            .expect("ok");
        assert_eq!(second[0]["name"], "alpha");
        assert_eq!(second[0]["status"]["state"], "stopped");

        cancel.cancel();
        let _ = running.await;
    }

    #[test]
    fn a_failure_is_a_row_not_an_error() {
        // One unreachable server must not abort a client's whole refresh: it needs a row saying which
        // one failed.
        let row = outcome_to_wire(&crate::mcp::config::StartedServer {
            name: "srv".to_owned(),
            tools: 0,
            failed: Some("no such command".to_owned()),
        });
        assert_eq!(row["name"], "srv");
        assert_eq!(row["ok"], json!(false));
    }
}
