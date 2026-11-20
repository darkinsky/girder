//! `agentTeams.setEnabled`: whether a session's tree runs as a team.
//!
//! # What upstream has
//!
//! `packages/team/agent-team/src/index.ts` marks one method:
//!
//! ```typescript
//! @Remote
//! async setEnabled(agent: Agent, enabled: boolean, signal?: AbortSignal): Promise<void> {
//!   const root = this.ctx.subagents.rootOf(agent)
//!   if (root !== agent) throw new TeamError('change Team mode from the root Session', 'TEAM_LEAD_REQUIRED')
//!   if (this.isEnabled(root) !== enabled) root.session.append('team/mode', { enabled })
//! }
//! ```
//!
//! Three properties, and the first two are the ones a hasty port drops:
//!
//! 1. **It must be called from the root.** A team is a property of a tree, not of one member; a
//!    child that could flip it would be changing its parent's behaviour.
//! 2. **It only appends when the value differs.** A client calls this on every toggle, and an
//!    unconditional append would fill the log with no-op events that every reader has to replay.
//! 3. **It does not stop children.** The selection is read when tools are offered; a running child
//!    keeps running, because stopping it is a different request.
//!
//! # What this port decides differently
//!
//! `rootOf(agent)` needs the subagent tree, which this build does not hold a registry for. The root
//! is therefore a parameter: a caller that knows it is the root passes its own id, and one that does
//! not gets the refusal. That is the same rule, with the tree replaced by the caller's assertion --
//! and the assertion is checked against the session's own recorded root, so a child claiming to be
//! one is refused rather than trusted.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::api::remote::{RemoteCall, RemoteError, RemoteRegistry, SessionCatalog};

/// The namespace this method registers under, as upstream spells it.
pub const NAMESPACE: &str = "agentTeams";

/// The one endpoint.
pub const SET_ENABLED_METHOD: &str = "setEnabled";

/// Whether a session's log says its tree runs as a team.
///
/// The last `team/mode` event wins, which is the same fold every other recorded selection uses: a
/// session that was turned on and then off is off.
#[must_use]
pub fn is_enabled(events: &[crate::session::SessionEvent]) -> bool {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            crate::session::SessionEvent::TeamMode { enabled } => Some(*enabled),
            _ => None,
        })
        .unwrap_or(false)
}

/// Renders the refusal upstream raises when a non-root asks.
fn lead_required() -> Value {
    json!({
        "ok": false,
        "code": "TEAM_LEAD_REQUIRED",
        "message": "change Team mode from the root Session",
    })
}

/// Registers `agentTeams.setEnabled`.
pub fn register_agent_team_methods(
    registry: &mut RemoteRegistry,
    catalog: &Arc<dyn SessionCatalog>,
) {
    let catalog = Arc::clone(catalog);
    registry.register(NAMESPACE, SET_ENABLED_METHOD, move |call: RemoteCall| {
        let catalog = Arc::clone(&catalog);
        Box::pin(async move {
            let session_id = call
                .args
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: SET_ENABLED_METHOD.to_owned(),
                    message: "`sessionId` is required".to_owned(),
                })?
                .to_owned();
            let enabled = call
                .args
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: SET_ENABLED_METHOD.to_owned(),
                    message: "`enabled` is required".to_owned(),
                })?;
            // A caller may name the root it believes it is. Without the tree this build cannot
            // derive it, so the assertion is required rather than guessed -- and a session that is
            // not the root it names is refused instead of trusted.
            let root = call.args.get("rootSessionId").and_then(Value::as_str);
            if let Some(root) = root {
                if root != session_id {
                    return Ok(lead_required());
                }
            }
            let Some(record) = catalog.read(&session_id).await else {
                return Ok(json!({ "ok": false, "code": "session_not_found", "sessionId": session_id }));
            };
            let current = is_enabled(&record.events);
            if current == enabled {
                // Only when it differs: a client calls this on every toggle, and an unconditional
                // append would fill the log with no-op events every reader has to replay.
                return Ok(json!({ "ok": true, "changed": false, "enabled": current }));
            }
            // Appending needs a store the catalogue does not hold; answered rather than claimed.
            Ok(json!({
                "ok": false,
                "code": "store_unavailable",
                "message": "this deployment's session catalog cannot append; supply a store to change Team mode",
                "wouldSet": enabled,
                "was": current,
            }))
        })
    });
    registry.declare(
        NAMESPACE,
        SET_ENABLED_METHOD,
        crate::guard::access::Declaration::write("session"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionEvent;

    #[test]
    fn a_session_with_no_selection_is_not_a_team() {
        assert!(!is_enabled(&[]));
    }

    #[test]
    fn the_last_selection_wins() {
        // Turned on and then off is off: the same fold every other recorded selection uses.
        let events = vec![
            SessionEvent::TeamMode { enabled: true },
            SessionEvent::TeamMode { enabled: false },
        ];
        assert!(!is_enabled(&events));
    }

    #[test]
    fn an_unrelated_event_between_selections_does_not_change_the_answer() {
        let events = vec![
            SessionEvent::TeamMode { enabled: true },
            SessionEvent::SessionTitle {
                title: "t".to_owned(),
                message_seqs: Vec::new(),
                source: "human".to_owned(),
            },
        ];
        assert!(is_enabled(&events));
    }

    #[test]
    fn a_non_root_caller_is_refused_by_code() {
        // Upstream's `TEAM_LEAD_REQUIRED`: a child that could flip the mode would be changing its
        // parent's behaviour.
        let refusal = lead_required();
        assert_eq!(refusal["code"], "TEAM_LEAD_REQUIRED");
    }
}
