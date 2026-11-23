//! `pluginRecommendations.describe`: read the catalog text for one recommendation card.
//!
//! The plugin recommendation extension is not mounted in this Rust build yet. The endpoint still
//! answers the stable result union, so clients can distinguish an unavailable card from an absent
//! RPC method.

use serde_json::{Value, json};

use crate::api::remote::{RemoteCall, RemoteRegistry};

/// Registers `pluginRecommendations.describe`.
pub fn register_plugin_recommendation_methods(registry: &mut RemoteRegistry) {
    registry.register("pluginRecommendations", "describe", |call: RemoteCall| {
        Box::pin(async move {
            if call
                .args
                .get("cardId")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Ok(json!({
                    "status": "refused",
                    "reason": "unknown"
                }));
            }
            Ok(json!({
                "status": "refused",
                "reason": "unavailable"
            }))
        })
    });
    registry.declare(
        "pluginRecommendations",
        "describe",
        crate::guard::access::Declaration::read("session"),
    );
}
