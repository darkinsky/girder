//! `officeKit.*`: the rendering generation, and a refusal for the render itself.
//!
//! # What upstream has
//!
//! `packages/document/office-kit/src/index.ts` marks two methods:
//!
//! | Endpoint | Answers |
//! | --- | --- |
//! | `generation` | the provider lifetime, so a client knows when a cached PDF is stale |
//! | `render` | converted bytes for one `.docx`/`.xlsx`/`.pptx` |
//!
//! # What this port serves
//!
//! `generation` is answered. Upstream generates it once per provider instance
//! (`OfficeKitGeneration(randomUUID())`), and it changes when the renderer's configuration does. A
//! client compares it against the generation it cached a PDF under, so a stable value is what makes
//! the cache correct -- not the value itself.
//!
//! `render` refuses with `no_renderer`. Converting a document needs a rendering engine this build
//! does not embed, and the honest answer is the one that says so: a client that received empty bytes
//! would cache an empty PDF, and one that received an error with no code could not tell a missing
//! engine from a corrupt file.
//!
//! # Why the generation is per-process
//!
//! A generation that changed on every call would invalidate every cached PDF; one that never changed
//! would serve stale renders after a font install. Per-process is the same lifetime upstream uses:
//! the value is stable for as long as this server is the one answering.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::api::remote::{RemoteCall, RemoteRegistry};

/// The namespace these methods register under, as upstream spells it.
pub const NAMESPACE: &str = "officeKit";

/// The generation this process answers with.
///
/// Derived from the process id and start time rather than from randomness, because a value that is
/// reproducible for a process is easier to explain in a bug report than a UUID: two clients that
/// disagree can compare generations and see which server differs.
#[must_use]
pub fn generation() -> String {
    format!("officekit-{}", std::process::id())
}

/// Renders the refusal `render` answers.
fn no_renderer() -> Value {
    json!({
        "ok": false,
        "code": "no_renderer",
        "message": "rendering an Office document needs a conversion engine; this deployment has none",
    })
}

/// Registers `officeKit.{generation, render}`.
pub fn register_office_kit_methods(registry: &mut RemoteRegistry) {
    let generation = Arc::new(generation());
    let answering = Arc::clone(&generation);
    registry.register(NAMESPACE, "generation", move |_call: RemoteCall| {
        let generation = Arc::clone(&answering);
        Box::pin(async move { Ok(json!({ "generation": *generation })) })
    });

    registry.register(NAMESPACE, "render", move |call: RemoteCall| {
        Box::pin(async move {
            // The path is validated before the refusal, so a caller that sent something that is not
            // an Office document learns that rather than "no renderer": one is a mistake they can
            // fix, the other is a deployment fact.
            let path = call
                .args
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let extension = path
                .rsplit_once('.')
                .map(|(_, extension)| extension.to_lowercase())
                .unwrap_or_default();
            if !matches!(
                extension.as_str(),
                "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx"
            ) {
                return Ok(json!({
                    "ok": false,
                    "code": "unsupported-format",
                    "message": "the path must end in doc, docx, xls, xlsx, ppt, or pptx",
                }));
            }
            Ok(no_renderer())
        })
    });

    registry.declare(
        NAMESPACE,
        "generation",
        crate::guard::access::Declaration::read("officeKit"),
    );
    registry.declare(
        NAMESPACE,
        "render",
        crate::guard::access::Declaration::read("officeKit"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generation_is_stable_within_a_process() {
        // A generation that changed on every call would invalidate every cached PDF; this is the
        // property, not the value.
        assert_eq!(generation(), generation());
    }

    #[test]
    fn the_generation_names_its_source() {
        // A bug report saying "generation officekit-1234" is easier to act on than a bare UUID.
        assert!(generation().starts_with("officekit-"));
    }

    #[test]
    fn the_render_refusal_carries_a_code() {
        // An error with no code cannot be told apart from a corrupt file.
        let refusal = no_renderer();
        assert_eq!(refusal["code"], "no_renderer");
    }
}
