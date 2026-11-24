//! `actionCards.act`: apply a decision to a Session action card.
//!
//! A card the client names and this process has never seen is `unknown`. A card that exists can be
//! dismissed even when its kind is gone. Any other verb is `unavailable` until that kind registers
//! it. Invalid arguments stay `invalid`. Those four answers are the ones the browser branches on.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};

use crate::api::remote::{RemoteCall, RemoteRegistry};

/// One proposed card and the latest decision recorded for it.
#[derive(Clone, Debug)]
struct Card {
    session_id: String,
    action: String,
    title: String,
    brief: String,
    details: Value,
    decision: Option<Value>,
}

/// The cards a deployment can act on.
#[derive(Debug, Default)]
pub struct ActionCards {
    cards: Mutex<BTreeMap<String, Card>>,
    verbs: Mutex<BTreeMap<String, Vec<String>>>,
}

impl ActionCards {
    /// An empty board.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a card the client may dismiss or act on.
    pub fn propose(
        &self,
        session_id: impl Into<String>,
        card_id: impl Into<String>,
        action: impl Into<String>,
        title: impl Into<String>,
        brief: impl Into<String>,
        details: Value,
    ) {
        let card_id = card_id.into();
        self.cards
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                card_id,
                Card {
                    session_id: session_id.into(),
                    action: action.into(),
                    title: title.into(),
                    brief: brief.into(),
                    details,
                    decision: None,
                },
            );
    }

    /// The cards recorded for one session, in card-id order.
    #[must_use]
    pub fn view(&self, session_id: &str) -> Value {
        let cards = self.cards.lock().unwrap_or_else(PoisonError::into_inner);
        let proposals: Vec<Value> = cards
            .iter()
            .filter(|(_, card)| card.session_id == session_id)
            .map(|(id, card)| {
                json!({
                    "cardId": id,
                    "action": card.action,
                    "title": card.title,
                    "brief": card.brief,
                    "details": card.details,
                    "decision": card.decision,
                })
            })
            .collect();
        json!({ "proposals": proposals })
    }

    /// Allows `verbs` on cards whose action is `action`.
    pub fn register_kind(
        &self,
        action: impl Into<String>,
        verbs: impl IntoIterator<Item = impl Into<String>>,
    ) {
        let mut names: Vec<String> = verbs.into_iter().map(Into::into).collect();
        names.sort();
        names.dedup();
        self.verbs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(action.into(), names);
    }

    fn act(&self, args: &Value) -> Value {
        let Some(card_id) = args.get("cardId").and_then(Value::as_str) else {
            return refused("invalid");
        };
        let Some(verb) = args.get("verb").and_then(Value::as_str) else {
            return refused("invalid");
        };
        if card_id.is_empty() || verb.is_empty() {
            return refused("invalid");
        }
        let mut cards = self.cards.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(card) = cards.get_mut(card_id) else {
            return refused("unknown");
        };
        if let Some(session_id) = args.get("sessionId").and_then(Value::as_str)
            && session_id != card.session_id
        {
            return refused("unknown");
        }
        if verb == "dismiss" {
            if card
                .decision
                .as_ref()
                .and_then(|decision| decision.get("phase"))
                .and_then(Value::as_str)
                == Some("dismissed")
            {
                return json!({ "status": "settled", "decision": card.decision.clone().unwrap_or(Value::Null) });
            }
            let decision = json!({
                "cardId": card_id,
                "action": card.action,
                "phase": "dismissed",
            });
            card.decision = Some(decision.clone());
            return json!({ "status": "settled", "decision": decision });
        }
        let allowed = self
            .verbs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&card.action)
            .is_some_and(|verbs| verbs.iter().any(|candidate| candidate == verb));
        if !allowed {
            return refused("unavailable");
        }
        let decision = json!({
            "cardId": card_id,
            "action": card.action,
            "phase": verb,
            "data": args.get("payload").cloned().unwrap_or_else(|| json!({})),
        });
        card.decision = Some(decision.clone());
        json!({ "status": "settled", "decision": decision })
    }
}

fn refused(reason: &str) -> Value {
    json!({ "status": "refused", "reason": reason })
}

/// Registers `actionCards.act` over a fresh board.
pub fn register_action_card_methods(registry: &mut RemoteRegistry) {
    register_action_card_methods_with(registry, Arc::new(ActionCards::new()));
}

/// Cards read from a session log, and the verbs a deployment has registered.
pub struct ActionCardLog {
    catalog: Arc<dyn crate::api::remote::SessionCatalog>,
    verbs: Mutex<BTreeMap<String, Vec<String>>>,
}

impl ActionCardLog {
    /// A log-backed board with no registered verbs. Dismiss still works.
    #[must_use]
    pub fn new(catalog: Arc<dyn crate::api::remote::SessionCatalog>) -> Self {
        Self {
            catalog,
            verbs: Mutex::new(BTreeMap::new()),
        }
    }

    /// Allows `verbs` on cards whose action is `action`.
    pub fn register_kind(
        &self,
        action: impl Into<String>,
        verbs: impl IntoIterator<Item = impl Into<String>>,
    ) {
        let mut names: Vec<String> = verbs.into_iter().map(Into::into).collect();
        names.sort();
        names.dedup();
        self.verbs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(action.into(), names);
    }

    async fn act(&self, args: &Value) -> Result<Value, crate::api::remote::RemoteError> {
        let Some(card_id) = args
            .get("cardId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            return Ok(refused("invalid"));
        };
        let Some(verb) = args
            .get("verb")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            return Ok(refused("invalid"));
        };
        let Some(session_id) = session_id_of(args) else {
            return Ok(refused("invalid"));
        };
        let Some(record) = self.catalog.read(&session_id).await else {
            return Err(crate::api::remote::RemoteError::Failed(format!(
                "no session `{session_id}` is known to this deployment"
            )));
        };
        let state = crate::interaction::action_card_projection::fold(&record.events);
        let Some(action) = state["proposals"]
            .get(card_id)
            .and_then(|card| card.get("action"))
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return Ok(refused("unknown"));
        };
        if verb == "dismiss"
            && state["decisions"]
                .get(card_id)
                .and_then(|decision| decision.get("phase"))
                .and_then(Value::as_str)
                == Some("dismissed")
        {
            return Ok(json!({
                "status": "settled",
                "decision": state["decisions"][card_id].clone(),
            }));
        }
        if verb != "dismiss" {
            let allowed = self
                .verbs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&action)
                .is_some_and(|verbs| verbs.iter().any(|candidate| candidate == verb));
            if !allowed {
                return Ok(refused("unavailable"));
            }
        }
        let data = args
            .get("payload")
            .filter(|value| value.is_object())
            .cloned();
        let phase = if verb == "dismiss" {
            "dismissed".to_owned()
        } else {
            verb.to_owned()
        };
        let Some(session) = self.catalog.open(&session_id) else {
            return Err(crate::api::remote::RemoteError::Failed(format!(
                "session `{session_id}` cannot be written"
            )));
        };
        session
            .append(crate::session::SessionEvent::ActionCardsDecision {
                card_id: card_id.to_owned(),
                action: action.clone(),
                phase: phase.clone(),
                data: data.clone(),
            })
            .await
            .map_err(|error| crate::api::remote::RemoteError::Failed(error.to_string()))?;
        let mut decision = json!({
            "cardId": card_id,
            "action": action,
            "phase": phase,
        });
        if let Some(data) = data {
            decision["data"] = data;
        }
        Ok(json!({ "status": "settled", "decision": decision }))
    }
}

fn session_id_of(args: &Value) -> Option<String> {
    args.get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            args.get("address")
                .and_then(|address| address.get("sessionId"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
}

/// Registers `actionCards.act` over a session log.
pub fn register_action_card_log(registry: &mut RemoteRegistry, service: &Arc<ActionCardLog>) {
    let service = Arc::clone(service);
    registry.register("actionCards", "act", move |call: RemoteCall| {
        let service = Arc::clone(&service);
        Box::pin(async move { service.act(&call.args).await })
    });
    registry.declare(
        "actionCards",
        "act",
        crate::guard::access::Declaration::write("session"),
    );
}

/// Registers `actionCards.act` over `cards`.
pub fn register_action_card_methods_with(registry: &mut RemoteRegistry, cards: Arc<ActionCards>) {
    registry.register("actionCards", "act", move |call: RemoteCall| {
        let cards = Arc::clone(&cards);
        Box::pin(async move { Ok(cards.act(&call.args)) })
    });
    registry.declare(
        "actionCards",
        "act",
        crate::guard::access::Declaration::write("session"),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::{ActionCards, register_action_card_methods_with};
    use crate::api::remote::{RemoteCall, RemoteRegistry, SessionCatalog};

    async fn act(registry: &RemoteRegistry, args: serde_json::Value) -> serde_json::Value {
        registry
            .dispatch(
                "actionCards",
                "act",
                RemoteCall {
                    args,
                    peer: "browser".to_owned(),
                },
            )
            .await
            .expect("act answers")
    }

    #[tokio::test]
    async fn act_distinguishes_invalid_unknown_dismiss_and_unavailable() {
        let cards = Arc::new(ActionCards::new());
        cards.propose(
            "s1",
            "card-1",
            "review",
            "Review",
            "Look at this",
            json!({}),
        );
        cards.register_kind("review", ["accept"]);
        let mut registry = RemoteRegistry::new();
        register_action_card_methods_with(&mut registry, Arc::clone(&cards));

        assert_eq!(act(&registry, json!({})).await["reason"], "invalid");
        assert_eq!(
            act(&registry, json!({"cardId": "missing", "verb": "dismiss"})).await["reason"],
            "unknown"
        );
        let dismissed = act(&registry, json!({"cardId": "card-1", "verb": "dismiss"})).await;
        assert_eq!(dismissed["status"], "settled");
        assert_eq!(dismissed["decision"]["phase"], "dismissed");
        cards.propose("s1", "card-2", "review", "Review", "Again", json!({}));
        assert_eq!(
            act(&registry, json!({"cardId": "card-2", "verb": "archive"})).await["reason"],
            "unavailable"
        );
        let accepted = act(
            &registry,
            json!({"cardId": "card-2", "verb": "accept", "payload": {"ok": true}}),
        )
        .await;
        assert_eq!(accepted["decision"]["phase"], "accept");
        assert_eq!(accepted["decision"]["data"]["ok"], true);
        assert_eq!(
            cards.view("s1")["proposals"]
                .as_array()
                .expect("cards")
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn a_logged_dismiss_is_folded_back_into_the_session() {
        let dir = std::env::temp_dir().join(format!(
            "girder-action-cards-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let catalog = Arc::new(crate::api::remote::DirectoryCatalog::new(&dir));
        catalog.create("s1").await.expect("session");
        let store = crate::session::SessionStore::new(dir.join("s1.jsonl.zst"));
        store
            .append(&crate::session::SessionEvent::TurnStart { turn: 1 })
            .expect("turn");
        store
            .append(&crate::session::SessionEvent::AssistantMessage {
                message: crate::session::message::Message::new(
                    crate::util::brand::MessageId::new("m1").expect("id"),
                    crate::session::message::Role::Assistant,
                    crate::session::message::MessageSource::Model {
                        provider: "p".to_owned(),
                        model: "m".to_owned(),
                        replay_state: None,
                    },
                    vec![crate::llm::ContentBlock::Text {
                        text: "```action-card\n{\"action\":\"review\",\"title\":\"Review\",\"brief\":\"Look\",\"details\":{}}\n```".to_owned(),
                    }],
                ),
            })
            .expect("answer");
        let shared: Arc<dyn SessionCatalog> = catalog.clone();
        let service = Arc::new(super::ActionCardLog::new(shared));
        let mut registry = RemoteRegistry::new();
        super::register_action_card_log(&mut registry, &service);

        let dismissed = act(
            &registry,
            json!({"sessionId": "s1", "cardId": "m1", "verb": "dismiss"}),
        )
        .await;
        assert_eq!(dismissed["status"], "settled");
        assert_eq!(dismissed["decision"]["phase"], "dismissed");
        let again = act(
            &registry,
            json!({"sessionId": "s1", "cardId": "m1", "verb": "dismiss"}),
        )
        .await;
        assert_eq!(again, dismissed);
        assert_eq!(
            act(&registry, json!({"cardId": "m1", "verb": "dismiss"})).await["reason"],
            "invalid"
        );
        assert_eq!(
            act(
                &registry,
                json!({"sessionId": "s1", "cardId": "m1", "verb": "accept"})
            )
            .await["reason"],
            "unavailable"
        );

        let record = catalog.read("s1").await.expect("session");
        let folded = crate::interaction::action_card_projection::fold(&record.events);
        assert_eq!(folded["proposals"]["m1"]["action"], "review");
        assert_eq!(folded["decisions"]["m1"]["phase"], "dismissed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
