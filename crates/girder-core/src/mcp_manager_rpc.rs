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
//! The subsystem is here and is not small: [`crate::mcp_client`] owns the JSON-RPC wire layer with
//! an id allocator that refuses to wrap, [`crate::mcp_stdio`] the pipe framing,
//! [`crate::mcp_process`] the child lifecycle, and [`crate::mcp_config::start_all`] does the
//! handshake and tool listing. What was missing was the endpoint -- which is the same shape this
//! pass has met five times now: written, tested, never registered.
//!
//! # Why `restart` connects rather than kills
//!
//! Upstream's `restart` stops a live connection and starts it again. This build starts servers on
//! demand and holds no long-lived connection, so there is nothing to stop -- a `probe` connects
//! fresh, which achieves what a restart is for. The method is served because a client calls it, and
//! answering "restarted" by actually performing the connect is more useful than refusing.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::mcp_config::{ServerConfig, start_all};
use crate::remote::{RemoteCall, RemoteRegistry};

/// The namespace these methods register under, as upstream spells it.
pub const NAMESPACE: &str = "mcpManager";

/// Renders one start outcome the way `probe` answers.
///
/// A failure is `ok: false` with the server's own diagnostic rather than an error: a probe that
/// raised would make one unreachable server abort a client's whole refresh, when what it needs is a
/// row saying which one failed.
fn outcome_to_wire(server: &crate::mcp_config::StartedServer) -> Value {
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
    registry.register(NAMESPACE, "restart", move |call: RemoteCall| {
        let servers = Arc::clone(&restarting);
        Box::pin(async move {
            let name = call
                .args
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| crate::remote::RemoteError::InvalidArguments {
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
            // Connecting fresh is what a restart achieves: this build holds no long-lived
            // connection to tear down.
            let outcomes = tokio::task::spawn_blocking(move || start_all(&[server]))
                .await
                .unwrap_or_default();
            Ok(json!({
                "ok": outcomes.iter().all(|server| server.failed.is_none()),
                "servers": outcomes.iter().map(outcome_to_wire).collect::<Vec<_>>(),
            }))
        })
    });

    registry.declare(
        NAMESPACE,
        "probe",
        crate::access::Declaration::read("mcpServer"),
    );
    registry.declare(
        NAMESPACE,
        "restart",
        crate::access::Declaration::write("mcpServer"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_is_a_row_not_an_error() {
        // One unreachable server must not abort a client's whole refresh: it needs a row saying which
        // one failed.
        let row = outcome_to_wire(&crate::mcp_config::StartedServer {
            name: "srv".to_owned(),
            tools: 0,
            failed: Some("no such command".to_owned()),
        });
        assert_eq!(row["name"], "srv");
        assert_eq!(row["ok"], json!(false));
    }
}
