//! `account.*`: what a client needs to know about the signed-in user.
//!
//! # What upstream has
//!
//! `packages/credentials/deepseek-account/src/index.ts` marks twelve methods across three kinds:
//!
//! | Kind | Methods | Needs |
//! | --- | --- | --- |
//! | Local | `getState`, `startSignIn`, `cancelSignIn`, `signOut`, `hasRunningAccountTasks` | A credential store |
//! | Remote reads | `getProfile`, `getBalance`, `getNotifications`, `getNotificationContent`, `reportNotificationEvent` | A Platform endpoint |
//! | Streams | `watchState`, `watch` | Both, plus a carrier |
//!
//! # What this port serves, and what it refuses
//!
//! `getState` is answered from the credential store: whether a grant is stored, and the usage links.
//! The rest answer a structured refusal naming what is missing, which is the honest option twice over:
//!
//! - **A Platform read** (`getProfile`, `getBalance`, …) needs an endpoint this build has no
//!   configured URL or token exchange for. Answering `null` -- which is what upstream returns when
//!   signed out -- would tell a client "you are not signed in" when the truth is "this deployment
//!   cannot ask".
//! - **A sign-in flow** (`startSignIn`) needs the PKCE exchange upstream performs against the Platform
//!   origin. This build has the *shape* of an attempt but no origin to exchange with, so it answers
//!   `no_platform` rather than starting a flow that cannot complete.
//!
//! The distinction matters because the two are indistinguishable to a client that only sees `null`:
//! one means "ask the user to sign in", the other means "signing in here will not help".

use std::sync::Arc;

use serde_json::{Value, json};

use crate::credentials::account::CredentialSource;
use crate::api::remote::{RemoteCall, RemoteRegistry};

/// The namespace these methods register under, as upstream spells it.
pub const NAMESPACE: &str = "account";

/// The credential reference a stored grant lives under.
///
/// Upstream's `KEY`. One name because a deployment stores one account: a client with two would need
/// two roots, not two references.
pub const CREDENTIAL_KEY: &str = "deepseek-account";

/// The origin the usage links point at.
///
/// Upstream derives these from its configured origin. Here it is the Platform's public site, which
/// is where a person checks usage regardless of which deployment they signed in through.
pub const ORIGIN: &str = "https://platform.deepseek.com";

/// Every method upstream marks, so a client can see the surface even where it is not served.
pub const METHODS: &[&str] = &[
    "getState",
    "getProfile",
    "getBalance",
    "getNotifications",
    "getNotificationContent",
    "reportNotificationEvent",
    "startSignIn",
    "cancelSignIn",
    "signOut",
    "hasRunningAccountTasks",
    "watchState",
    "watch",
];

/// The methods that need a Platform endpoint rather than the local credential store.
pub const NEEDS_PLATFORM: &[&str] = &[
    "getProfile",
    "getBalance",
    "getNotifications",
    "getNotificationContent",
    "reportNotificationEvent",
    "startSignIn",
    "cancelSignIn",
    "signOut",
];

/// Renders the refusal a Platform-dependent method answers.
///
/// `null` is what upstream answers when signed out, and it is the wrong answer here: a client that
/// read it would show a sign-in prompt for a deployment where signing in cannot succeed.
fn no_platform(method: &str) -> Value {
    json!({
        "ok": false,
        "code": "no_platform",
        "message": format!(
            "`account.{method}` needs a Platform endpoint; this deployment has none configured"
        ),
    })
}

/// The usage links every state carries.
///
/// A function rather than two inline `format!` calls because the two must agree on the origin: a
/// usage link pointing somewhere the top-up link does not is a client that sends a person to two
/// different sites.
#[must_use]
pub fn links() -> Value {
    json!({
        "usageUrl": format!("{ORIGIN}/usage"),
        "topUpUrl": format!("{ORIGIN}/top_up"),
    })
}

/// Registers the twelve `account` methods.
pub fn register_account_methods(
    registry: &mut RemoteRegistry,
    credentials: &Arc<dyn CredentialSource>,
    platform_configured: bool,
) {
    let store = Arc::clone(credentials);
    registry.register(NAMESPACE, "getState", move |_call: RemoteCall| {
        let store = Arc::clone(&store);
        Box::pin(async move {
            // A stored grant is a presence check, not a read of the secret: the store's `describe`
            // answers whether a value exists without returning it, which is what a state snapshot
            // needs and what keeps the token out of a reply.
            let info = store.describe(CREDENTIAL_KEY);
            Ok(json!({
                "status": if info.configured { "credential-stored" } else { "signed-out" },
                // This build runs no sign-in attempt, so there is none to report. `null` is
                // upstream's shape for that, and unlike the Platform reads it is accurate here.
                "attempt": Value::Null,
                "links": links(),
            }))
        })
    });

    // The Platform-dependent ten. Registered rather than omitted, so a client discovers the
    // refusal from the method rather than from a 404 it would read as a typo.
    for method in NEEDS_PLATFORM {
        let method = (*method).to_owned();
        let configured = platform_configured;
        let wire = method.clone();
        registry.register(NAMESPACE, &wire, move |_call: RemoteCall| {
            let method = method.clone();
            Box::pin(async move {
                if configured {
                    // Configured but not implemented: a deployment that set an endpoint expects these
                    // to work, and saying so is better than a refusal that blames configuration.
                    return Ok(json!({
                        "ok": false,
                        "code": "not_implemented",
                        "message": format!("`account.{method}` is not implemented in this build"),
                    }));
                }
                Ok(no_platform(&method))
            })
        });
    }

    // The two streams need a carrier and a Platform endpoint; the carrier exists, the endpoint does
    // not, so they answer the same refusal as the reads.
    for method in ["watchState", "watch"] {
        let method = method.to_owned();
        let wire = method.clone();
        registry.register(NAMESPACE, &wire, move |_call: RemoteCall| {
            let method = method.clone();
            Box::pin(async move { Ok(no_platform(&method)) })
        });
    }

    // `hasRunningAccountTasks` answers false rather than refusing: no sign-in attempt this build
    // runs can be in flight, so "no tasks" is a fact rather than a gap.
    registry.register(
        NAMESPACE,
        "hasRunningAccountTasks",
        move |_call: RemoteCall| Box::pin(async move { Ok(json!({ "running": false })) }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::account::MemoryCredentialSource;

    #[test]
    fn the_links_agree_on_their_origin() {
        // A usage link pointing somewhere the top-up link does not is a client that sends a person
        // to two different sites.
        let links = links();
        let usage = links["usageUrl"].as_str().expect("usage");
        let top_up = links["topUpUrl"].as_str().expect("top up");
        assert!(usage.starts_with(ORIGIN), "{usage}");
        assert!(top_up.starts_with(ORIGIN), "{top_up}");
    }

    #[test]
    fn a_stored_credential_reads_as_stored() {
        let store = MemoryCredentialSource::new();
        store.set(CREDENTIAL_KEY, "token").expect("set");
        let info = store.describe(CREDENTIAL_KEY);
        assert!(info.configured);
    }

    #[test]
    fn an_empty_store_reads_as_signed_out() {
        let store = MemoryCredentialSource::new();
        assert!(!store.describe(CREDENTIAL_KEY).configured);
    }

    #[test]
    fn a_platform_read_refuses_rather_than_answering_null() {
        // `null` is upstream's "signed out" and the wrong answer here: a client that read it would
        // show a sign-in prompt for a deployment where signing in cannot succeed.
        let refusal = no_platform("getProfile");
        assert_eq!(refusal["code"], "no_platform");
        assert!(refusal.get("ok").is_some());
    }

    #[test]
    fn every_upstream_method_is_named() {
        // A client can see the surface even where it is not served.
        for method in [
            "getState",
            "getProfile",
            "getBalance",
            "getNotifications",
            "getNotificationContent",
            "reportNotificationEvent",
            "startSignIn",
            "cancelSignIn",
            "signOut",
            "hasRunningAccountTasks",
            "watchState",
            "watch",
        ] {
            assert!(METHODS.contains(&method), "`{method}` must be listed");
        }
    }
}
