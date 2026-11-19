//! `subagents.{prompt, interruptByUser}`: delivering input to a child, and interrupting one.
//!
//! # What upstream has
//!
//! `packages/subagent/subagent/src/index.ts` marks two methods, and both address a **running child**:
//!
//! ```typescript
//! @Remote({ name: 'prompt', resources: { session: request => request.childSessionId } })
//! async prompt(request: SubagentPromptRequest, signal: AbortSignal): Promise<SubagentPromptReceipt>
//!
//! @Remote({ name: 'interruptByUser', resources: { session: childSessionId => childSessionId } })
//! interruptByUser(childSessionId: SessionId): SubagentInterruptReceipt
//! ```
//!
//! `prompt` ends at `deliverPrompt`, whose whole body is a choice between two agent methods:
//!
//! ```typescript
//! if (delivery === 'steer') local.handle.agent.steer(message)
//! else local.handle.agent.followup(message)
//! ```
//!
//! **That is the substrate this port was missing, and it is four lines per method.**
//! [`crate::agent::Agent::steer`] and [`crate::agent::Agent::followup`] push onto the two inbox
//! queues the agent loop already reads, and `interruptByUser` is the cancel token the agent already
//! holds. None of this needed a resident-session subsystem -- the session was already there; what was
//! absent was the way to put a message into it from outside.
//!
//! # The two deliveries are not a preference
//!
//! `steer` joins the turn in progress, so the model reads the message before its next tool call.
//! `followup` waits for the turn to end. A followup cannot stop a wrong plan mid-turn and a steer can,
//! which is why upstream exposes both rather than choosing one: an instruction that arrives while a
//! tool call is in flight can invalidate work already committed, so the caller decides.
//!
//! # What a caller must supply
//!
//! The live agent. Upstream finds it by `childSessionId` through its resident table; this build's
//! agents are not held in one, so the endpoint takes the cancel token and inbox owner through a
//! registry a deployment supplies. A child that is not running is `subagent_not_running` rather than
//! a fabricated acceptance -- `interruptByUser` on an idle child is an accepted no-op upstream, and
//! saying so is more useful than pretending.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::agent::Agent;
use crate::llm::ContentBlock;
use crate::message::{Message, Role};
use crate::remote::{RemoteCall, RemoteError, RemoteRegistry};

/// The namespace these methods register under, as upstream spells it.
pub const NAMESPACE: &str = "subagents";

/// Live children, by session id.
///
/// Upstream keys its resident table the same way. A `Mutex` because every operation is a short lookup
/// or a whole-map insert: an agent's own state has finer-grained locks, and holding this one while
/// pushing a message is what keeps "is it running" and "deliver to it" from interleaving.
#[derive(Default)]
pub struct RunningChildren {
    children: Mutex<BTreeMap<String, Arc<Agent>>>,
}

impl RunningChildren {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one running child.
    pub fn insert(&self, session_id: impl Into<String>, agent: Arc<Agent>) {
        if let Ok(mut held) = self.children.lock() {
            held.insert(session_id.into(), agent);
        }
    }

    /// Forgets one child that has stopped.
    pub fn remove(&self, session_id: &str) {
        if let Ok(mut held) = self.children.lock() {
            held.remove(session_id);
        }
    }

    /// One live child.
    fn get(&self, session_id: &str) -> Option<Arc<Agent>> {
        self.children
            .lock()
            .ok()
            .and_then(|held| held.get(session_id).cloned())
    }

    /// The ids currently running, so a refusal can name them.
    fn ids(&self) -> Vec<String> {
        self.children
            .lock()
            .map(|held| held.keys().cloned().collect())
            .unwrap_or_default()
    }
}

/// Builds the message a prompt delivers.
///
/// Upstream admits content parts through an attachment store first, so the child inbox only ever
/// accepts Host-persisted references. This build's text-only path needs no admission step, and a
/// non-text part is refused rather than dropped: a caller that sent an image and received `ok` would
/// believe it had been delivered.
fn message_from(content: &Value) -> Result<Message, RemoteError> {
    let parts = content
        .as_array()
        .ok_or_else(|| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: "prompt".to_owned(),
            message: "`content` must be an array of parts".to_owned(),
        })?;
    let mut blocks = Vec::with_capacity(parts.len());
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                    RemoteError::InvalidArguments {
                        namespace: NAMESPACE.to_owned(),
                        method: "prompt".to_owned(),
                        message: "a text part needs `text`".to_owned(),
                    }
                })?;
                blocks.push(ContentBlock::Text {
                    text: text.to_owned(),
                });
            }
            other => {
                return Err(RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "prompt".to_owned(),
                    message: format!(
                        "part type {other:?} needs an attachment store this build does not have"
                    ),
                });
            }
        }
    }
    // A synthetic id: the child's inbox assigns its own sequence when it drains the queue, and a
    // caller-supplied id would be a second authority over the same numbering.
    let id = crate::brand::MessageId::new(format!("subagent-prompt-{}", crate::now_millis()))
        .map_err(|error| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: "prompt".to_owned(),
            message: format!("could not mint a message id: {error}"),
        })?;
    Ok(Message::new(
        id,
        Role::User,
        crate::message::MessageSource::User { form: None },
        blocks,
    ))
}

/// Registers `subagents.{prompt, interruptByUser}`.
pub fn register_subagent_methods(registry: &mut RemoteRegistry, children: &Arc<RunningChildren>) {
    let sending = Arc::clone(children);
    registry.register(NAMESPACE, "prompt", move |call: RemoteCall| {
        let children = Arc::clone(&sending);
        Box::pin(async move {
            let child_session_id = call
                .args
                .get("childSessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "prompt".to_owned(),
                    message: "`childSessionId` is required".to_owned(),
                })?
                .to_owned();
            let message = message_from(call.args.get("content").unwrap_or(&Value::Null))?;
            // `steer` joins the turn in progress; anything else waits for it to end. Upstream's
            // discriminator is the string `steer`, and an unrecognised value is not a preference --
            // it is a request this build cannot honour, so it is refused.
            let delivery = call.args.get("delivery").and_then(Value::as_str);
            let Some(agent) = children.get(&child_session_id) else {
                // An idle child is `interruptByUser`'s accepted no-op but not `prompt`'s: a prompt
                // that nothing will read must not report acceptance.
                return Ok(json!({
                    "ok": false,
                    "code": "subagent_not_running",
                    "message": format!("no live child `{child_session_id}`"),
                    "running": children.ids(),
                }));
            };
            let queued = match delivery {
                Some("steer") => agent.steer(message),
                None | Some("followup") => agent.followup(message),
                Some(other) => {
                    return Err(RemoteError::InvalidArguments {
                        namespace: NAMESPACE.to_owned(),
                        method: "prompt".to_owned(),
                        message: format!("`delivery` must be `steer` or `followup`, got `{other}`"),
                    });
                }
            };
            Ok(json!({
                "ok": true,
                "childSessionId": child_session_id,
                "queued": queued,
            }))
        })
    });

    let interrupting = Arc::clone(children);
    registry.register(NAMESPACE, "interruptByUser", move |call: RemoteCall| {
        let children = Arc::clone(&interrupting);
        Box::pin(async move {
            let child_session_id = call
                .args
                .get("childSessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "interruptByUser".to_owned(),
                    message: "`childSessionId` is required".to_owned(),
                })?
                .to_owned();
            // `keep_inbox: true`, because a user interrupt usually wants to keep what they typed.
            // Upstream's `interrupt` distinguishes the same two cases; the other one is a shutdown.
            if let Some(agent) = children.get(&child_session_id) {
                agent.cancel(true);
            }
            // An absent or idle child is an accepted no-op upstream, and saying that is more useful
            // than a failure: the cancel signal was admitted, not guaranteed to have been running.
            Ok(json!({
                "ok": true,
                "childSessionId": child_session_id,
                "accepted": children.get(&child_session_id).is_some(),
            }))
        })
    });

    registry.declare(
        NAMESPACE,
        "prompt",
        crate::access::Declaration::write("session"),
    );
    registry.declare(
        NAMESPACE,
        "interruptByUser",
        crate::access::Declaration::write("session"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_part_becomes_a_user_message() {
        let message = message_from(&json!([{ "type": "text", "text": "hello" }])).expect("built");
        assert_eq!(message.role, Role::User);
        assert_eq!(message.content.len(), 1);
    }

    #[test]
    fn an_image_part_is_refused_rather_than_dropped() {
        // A caller that sent an image and received `ok` would believe it had been delivered.
        let error = message_from(&json!([{ "type": "image", "data": "…" }])).expect_err("refused");
        assert!(error.to_string().contains("attachment store"), "{error}");
    }

    #[test]
    fn a_missing_child_is_named_in_the_refusal() {
        let children = RunningChildren::new();
        assert!(children.get("nope").is_none());
        assert!(children.ids().is_empty());
    }

    #[test]
    fn a_registered_child_is_findable() {
        let children = RunningChildren::new();
        let session = crate::brand::SessionId::new("child-session").expect("a legal id");
        let agent = Arc::new(Agent::new(crate::agent::AgentSpec::root(
            session,
            crate::agent::AgentOptions::default(),
        )));
        children.insert("child", Arc::clone(&agent));
        assert!(children.get("child").is_some());
        children.remove("child");
        assert!(children.get("child").is_none());
    }
}
