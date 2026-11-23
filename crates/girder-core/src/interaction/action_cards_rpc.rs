//! `actionCards.act`: apply a decision to a Session action card.
//!
//! The stable protocol exposes this method even when a deployment has no action-card producer.
//! Returning a typed refusal preserves the upstream result shape and lets a client render the
//! unavailable action instead of treating the endpoint as a transport failure.

use serde_json::{Value, json};

use crate::api::remote::{RemoteCall, RemoteRegistry};

/// Registers `actionCards.act`.
pub fn register_action_card_methods(registry: &mut RemoteRegistry) {
    registry.register("actionCards", "act", |call: RemoteCall| {
        Box::pin(async move {
            let Some(card_id) = call.args.get("cardId").and_then(Value::as_str) else {
                return Ok(json!({
                    "status": "refused",
                    "reason": "invalid"
                }));
            };
            if card_id.is_empty() || call.args.get("verb").and_then(Value::as_str).is_none() {
                return Ok(json!({
                    "status": "refused",
                    "reason": "invalid"
                }));
            }
            Ok(json!({
                "status": "refused",
                "reason": "unavailable"
            }))
        })
    });
    registry.declare(
        "actionCards",
        "act",
        crate::guard::access::Declaration::write("session"),
    );
}
