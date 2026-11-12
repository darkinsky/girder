//! `session.openWorkspacePath`: the cursor-named spelling of `host.openPath`.
//!
//! Upstream has both. `openWorkspacePath` lives on the session controller because a client opens a path *in the context
//! of a session*, and `host.openPath` lives on the host because the operation is the host's. They are one operation with
//! two names, and this module makes the session-named one call the host-named one rather than implementing opening
//! twice -- two implementations of "hand a path to the desktop" would diverge first on the platform quirks, which are
//! exactly the part nobody re-tests.
//!
//! # Why the response shape differs from `host.openPath`
//!
//! Upstream's session method answers `{ opened, action, path }`; the host method answers the same. The session-named
//! one also answers `workspacePathApplications`, which lists what could open the path, and that needs a platform probe
//! this build does not have. It is declared a gap rather than answered with a guess.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::host_rpc::Opener;
use crate::remote::{RemoteCall, RemoteError, RemoteRegistry};

/// The namespace and method this module registers.
pub const NAMESPACE: &str = "session";
/// The method name.
pub const METHOD: &str = "openWorkspacePath";

/// Registers `session.openWorkspacePath`.
///
/// Delegates to the same opener `host.openPath` uses, so the two answers cannot disagree about whether opening is
/// possible or about which program did it.
pub fn register_open_workspace_path(
    registry: &mut RemoteRegistry,
    workspace: &Path,
    opener: &Arc<dyn Opener>,
) {
    let workspace = workspace.to_path_buf();
    let opener = Arc::clone(opener);
    registry.register(NAMESPACE, METHOD, move |call: RemoteCall| {
        let workspace = workspace.clone();
        let opener = Arc::clone(&opener);
        Box::pin(async move { open(&call.args, &workspace, &opener) })
    });
    // Declared as a write: opening a path is not a read of the harness's data, and a read-only policy that refused it
    // is refusing the desktop handoff rather than a listing.
    registry.declare(
        NAMESPACE,
        METHOD,
        crate::access::Declaration::write("workspace"),
    );
}

/// Runs one open request.
fn open(args: &Value, workspace: &Path, opener: &Arc<dyn Opener>) -> Result<Value, RemoteError> {
    let path =
        args.get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| RemoteError::InvalidArguments {
                namespace: NAMESPACE.to_owned(),
                method: METHOD.to_owned(),
                message: "`path` is required".to_owned(),
            })?;

    // `action` mirrors upstream: omitted or `open` hands the file to its default program, `reveal` shows it in the file
    // manager. Any other value is refused rather than defaulted, because a client that asked for something specific and
    // silently got `open` would report the wrong thing to its user.
    let reveal = match args.get("action").and_then(Value::as_str) {
        None | Some("open") => false,
        Some("reveal") => true,
        Some(other) => {
            return Err(RemoteError::InvalidArguments {
                namespace: NAMESPACE.to_owned(),
                method: METHOD.to_owned(),
                message: format!("`action` must be `open` or `reveal`, got `{other}`"),
            });
        }
    };

    // The same two-layer containment check the host methods use, through the same helper: a path this method accepted
    // and `host.openPath` refused would be a boundary that depends on which name a client happened to call.
    let resolved = crate::workspace_path::resolve(Some(workspace), path);
    if !crate::workspace_path::contains(workspace, &crate::workspace_path::normalize(&resolved)) {
        return Err(RemoteError::Refused {
            namespace: NAMESPACE.to_owned(),
            method: METHOD.to_owned(),
            reason: format!("`{path}` is outside the workspace"),
        });
    }
    if !resolved.exists() {
        return Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: METHOD.to_owned(),
            message: format!("`{path}` does not exist"),
        });
    }
    // Canonicalised so a symlink inside the workspace cannot smuggle a target outside it past the lexical check above.
    let canonical =
        std::fs::canonicalize(&resolved).map_err(|error| RemoteError::Failed(error.to_string()))?;
    if !crate::workspace_path::contains(workspace, &canonical) {
        return Err(RemoteError::Refused {
            namespace: NAMESPACE.to_owned(),
            method: METHOD.to_owned(),
            reason: format!("`{path}` resolves outside the workspace"),
        });
    }

    let outcome = if reveal {
        opener.reveal(&canonical)
    } else {
        opener.open(&canonical)
    };
    outcome.map_err(RemoteError::Failed)?;

    Ok(serde_json::json!({
        "opened": true,
        "action": if reveal { "reveal" } else { "open" },
        // The canonical path rather than the caller's spelling: what was opened is what the host resolved, and echoing
        // the caller's spelling would let two different inputs claim the same outcome.
        "path": canonical.display().to_string(),
    }))
}
