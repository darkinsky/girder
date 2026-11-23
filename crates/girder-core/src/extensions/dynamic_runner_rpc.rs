//! The stable `dynamicCordisRunner.*` protocol surface.
//!
//! The JavaScript host runner owns a VM-backed dynamic package registry. Girder does not mount that
//! runner yet, but the methods remain part of the stable backend contract. These handlers return
//! the same refusal-oriented result families used by the other optional integrations, while the
//! runner implementation is added behind this seam later.

use serde_json::{Value, json};

use crate::api::remote::{RemoteCall, RemoteError, RemoteRegistry};

const NAMESPACE: &str = "dynamicCordisRunner";

const METHODS: &[&str] = &[
    "getClientCode",
    "inventory",
    "invoke",
    "reportClientGuardFailure",
    "reportRenderFailure",
    "resolveInspectQuery",
    "resolveRequestRun",
    "runHostHalf",
    "settleUserRun",
    "stopFromPanel",
    "syncInspectManifest",
    "undefineFromPanel",
];

fn unavailable(method: &str) -> Value {
    json!({
        "ok": false,
        "reason": "runner-unavailable",
        "code": "runner-unavailable",
        "message": format!("`{NAMESPACE}.{method}` needs the dynamic Cordis host runner"),
    })
}

/// Registers every stable method owned by the dynamic Cordis host runner.
pub fn register_dynamic_runner_methods(registry: &mut RemoteRegistry) {
    for method in METHODS {
        let method = (*method).to_owned();
        let wire = method.clone();
        let declaration_method = method.clone();
        registry.register(NAMESPACE, &wire, move |_call: RemoteCall| {
            let method = method.clone();
            Box::pin(async move {
                match method.as_str() {
                    "inventory" => Ok(json!([])),
                    "syncInspectManifest" | "reportClientGuardFailure" | "reportRenderFailure" => {
                        Ok(Value::Null)
                    }
                    "resolveInspectQuery" | "resolveRequestRun" => Ok(json!({ "accepted": false })),
                    "invoke" => Ok(unavailable(&method)),
                    "runHostHalf" | "settleUserRun" | "stopFromPanel" | "undefineFromPanel" => {
                        Ok(unavailable(&method))
                    }
                    // Upstream reports these as thrown lookup errors rather than a result union.
                    "getClientCode" => Err(RemoteError::Failed(unavailable(&method).to_string())),
                    _ => Err(RemoteError::Failed(format!(
                        "unhandled `{NAMESPACE}.{method}`"
                    ))),
                }
            })
        });
        registry.declare(
            NAMESPACE,
            &wire,
            if matches!(
                declaration_method.as_str(),
                "inventory" | "getClientCode" | "resolveInspectQuery"
            ) {
                crate::guard::access::Declaration::read("dynamicCordisRunner")
            } else {
                crate::guard::access::Declaration::write("dynamicCordisRunner")
            },
        );
    }
}
