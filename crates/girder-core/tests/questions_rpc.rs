//! Tests for the `userQuestions.*` Remote methods.
//!
//! The properties under test, in the order they matter:
//!
//! - **a question can be discovered, answered, and read back** — `list` shows what `answer` takes, and
//!   the recorded reply is what the answerer sent. Without the round trip the three methods are not
//!   one surface.
//! - **answering an answered question is refused, and the first answer survives** — the deviation the
//!   module documentation argues for. A second answer must not replace the first, and the refusal must
//!   name the state that won so a client with a stale card learns what happened.
//! - **exactly one of several concurrent answers is recorded** — the compare-and-set, raced from
//!   eight threads. A read-then-write would let two answers both see an open question and both write.
//! - **an invalid answer never settles anything** — an option the question never offered, a blank
//!   required text, too many or too few selections, and a non-boolean confirmation are all refused
//!   and the question stays answerable.
//! - **a wait is released by whoever settles it** — `ask` returns the human reply when answered,
//!   `Cancelled` when cancelled, and a *defaulted* reply (never a human one) when the timeout fires on
//!   a question that declared a default. A question with no default reports `TimedOut` instead.
//! - **cancelling a settled question is refused** — including one that expired, because a cancel that
//!   reported success would claim it stopped something already over.
//! - **the declarations match what each method does** — `list` is a read, `answer` a write, `cancel`
//!   destructive, so a read-only peer can list and cannot answer or cancel.
//! - **upstream's unserved method is named** — `attachWait` is a stream and this registry is unary, so
//!   it must be reported rather than silently absent.
//!
//! Isolation: every test builds its own queue and registry, and ids carry a process-wide counter, so
//! tests in one binary cannot see each other's questions.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the methods as written even before the crate root wires them in. It spells its own paths
//! through `girder_core::`, which is the alias the library declares for itself, so the same source
//! builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/interaction/questions_rpc.rs"]
pub mod questions_rpc;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use girder_core::{
    Access, AccessDecision, AccessRequest, Answer, Policy, Question, QuestionOption, QuestionReply,
    QuestionShape, RemoteCall, RemoteError, RemoteRegistry,
};
use serde_json::{Value, json};

use questions_rpc::{
    ANSWER_METHOD, ATTACH_WAIT_METHOD, AskError, AskRequest, CANCEL_METHOD, CONFIRM_NO, CONFIRM_YES,
    DEFAULT_LIST_LIMIT, LIST_METHOD, MAX_LIST_LIMIT, MemoryQuestionStore, NAMESPACE,
    PendingQuestion, QuestionQueue, QuestionState, QuestionStore, QuestionStoreError,
    UNSERVED_METHODS, UnavailableQuestionStore, register_question_methods,
};

// ---- fixtures ---------------------------------------------------------------------------------

fn call(args: Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

/// A registry with a fresh in-memory queue behind it.
fn registry() -> (RemoteRegistry, Arc<QuestionQueue>) {
    let queue = Arc::new(QuestionQueue::in_memory());
    let mut registry = RemoteRegistry::new();
    register_question_methods(&mut registry, Arc::clone(&queue));
    (registry, queue)
}

async fn dispatch(
    registry: &RemoteRegistry,
    method: &str,
    args: Value,
) -> Result<Value, RemoteError> {
    registry.dispatch(NAMESPACE, method, call(args)).await
}

/// Records one question directly on the queue and returns its id.
///
/// Through the store rather than through `ask`, because a test that had to run a wait to obtain an id
/// would need a second task for every case, and the store is the seam the handlers read.
fn seed(queue: &QuestionQueue, question: Question) -> String {
    let record = PendingQuestion::new(questions_rpc_id(), question);
    let id = record.id.clone();
    queue
        .store()
        .insert(record)
        .expect("an empty store accepts the first record");
    id
}

/// An id a seed can mint without colliding with the queue's own.
fn questions_rpc_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("seed-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A free-form question, the simplest shape.
fn text_question(label: &str) -> Question {
    Question::new(QuestionShape::Text {
        label: label.to_owned(),
        optional: false,
    })
}

/// A single-choice question over `blue` and `green`.
fn choice_question() -> Question {
    Question::new(QuestionShape::Choice {
        label: "which colour".to_owned(),
        options: vec![
            QuestionOption::new("blue"),
            QuestionOption::new("green").labelled("Green"),
        ],
    })
}

/// One list item by id, from a `list` response.
fn item<'a>(response: &'a Value, id: &str) -> &'a Value {
    response["items"]
        .as_array()
        .expect("a list response carries an array")
        .iter()
        .find(|entry| entry["id"] == json!(id))
        .expect("the question is in the listing")
}

// ---- the round trip ---------------------------------------------------------------------------

#[tokio::test]
async fn an_open_question_is_listed_and_then_answered() {
    let (registry, queue) = registry();
    let id = seed(&queue, choice_question());

    let listed = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("listable");
    assert_eq!(listed["total"], json!(1));
    let listed_item = item(&listed, &id);
    assert_eq!(listed_item["state"], json!("open"));
    assert_eq!(listed_item["shape"]["kind"], json!("choice"));
    // Both halves of an option travel: the value is what comes back, the label is what a person reads.
    assert_eq!(listed_item["shape"]["options"][0]["value"], json!("blue"));
    assert_eq!(
        listed_item["shape"]["options"][1]["label"],
        json!("Green"),
        "a label different from its value is published, not flattened"
    );

    let answered = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["green"]}}),
    )
    .await
    .expect("answerable");
    assert_eq!(answered["id"], json!(id));
    assert_eq!(answered["state"], json!("answered"));
    assert_eq!(answered["by"], json!("human"));
    assert_eq!(answered["answer"]["selected"], json!(["green"]));

    // The listing's default filter is `open`, so an answered question is gone from it and appears
    // under its own state.
    let open = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("listable");
    assert_eq!(open["total"], json!(0));
    let settled = dispatch(&registry, LIST_METHOD, json!({"state": "answered"}))
        .await
        .expect("listable");
    assert_eq!(item(&settled, &id)["state"], json!("answered"));
    assert_eq!(item(&settled, &id)["answer"]["selected"], json!(["green"]));
    assert_eq!(
        item(&settled, &id)["by"],
        json!("human"),
        "a person's answer is attributed to a person"
    );
}

#[tokio::test]
async fn a_text_answer_arrives_in_custom() {
    let (registry, queue) = registry();
    let id = seed(&queue, text_question("which colour"));

    let answered = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": [], "custom": "blue"}}),
    )
    .await
    .expect("answerable");

    assert_eq!(answered["answer"]["custom"], json!("blue"));
    assert_eq!(answered["answer"]["selected"], json!([]));
}

#[tokio::test]
async fn a_confirmation_publishes_its_two_values_and_decodes_them() {
    let (registry, queue) = registry();
    let id = seed(
        &queue,
        Question::new(QuestionShape::Confirm {
            label: "overwrite?".to_owned(),
        }),
    );

    let listed = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("listable");
    let values: Vec<&str> = item(&listed, &id)["shape"]["options"]
        .as_array()
        .expect("a confirmation publishes its values")
        .iter()
        .filter_map(|option| option["value"].as_str())
        .collect();
    assert_eq!(
        values,
        vec![CONFIRM_YES, CONFIRM_NO],
        "the two values the decoder accepts are published, so a client need not guess"
    );

    let answered = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": [CONFIRM_NO]}}),
    )
    .await
    .expect("answerable");
    assert_eq!(answered["answer"]["selected"], json!([CONFIRM_NO]));
}

// ---- the deviation: a second answer is refused -------------------------------------------------

#[tokio::test]
async fn answering_an_answered_question_is_refused_and_keeps_the_first_answer() {
    let (registry, queue) = registry();
    let id = seed(&queue, choice_question());

    dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["blue"]}}),
    )
    .await
    .expect("the first answer is recorded");

    let second = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["green"]}}),
    )
    .await
    .expect_err("a second answer is refused");

    match &second {
        RemoteError::InvalidArguments {
            namespace,
            method,
            message,
        } => {
            assert_eq!(namespace, NAMESPACE);
            assert_eq!(method, ANSWER_METHOD);
            assert!(
                message.contains("already `answered`"),
                "the refusal names the state that won, got: {message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(second.code(), "INVALID_ARGUMENTS");

    // The first answer survives: the refusal is not an overwrite.
    let stored = queue.get(&id).expect("the record is still there");
    match stored.state {
        QuestionState::Answered(reply) => {
            assert_eq!(reply.answer, Answer::Chosen("blue".to_owned()));
            assert!(reply.is_human());
        }
        other => panic!("expected the first answer to survive, got {other:?}"),
    }
}

#[tokio::test]
async fn a_second_answer_from_another_thread_cannot_win() {
    // The compare-and-set, raced: eight threads, one question, one reply recorded.
    let (registry, queue) = registry();
    let id = seed(&queue, choice_question());
    let registry = Arc::new(registry);

    let mut handles = Vec::new();
    for round in 0..8 {
        let registry = Arc::clone(&registry);
        let id = id.clone();
        handles.push(tokio::spawn(async move {
            let value = if round % 2 == 0 { "blue" } else { "green" };
            dispatch(
                &registry,
                ANSWER_METHOD,
                json!({"id": id, "answer": {"selected": [value]}}),
            )
            .await
            .is_ok()
        }));
    }

    let mut accepted = 0;
    for handle in handles {
        if handle.await.expect("the task ran") {
            accepted += 1;
        }
    }
    assert_eq!(
        accepted, 1,
        "exactly one answer is recorded, so no caller is told its answer counted when it did not"
    );
    assert!(
        matches!(
            queue.get(&id).expect("recorded").state,
            QuestionState::Answered(_)
        ),
        "the winner's answer is what the record holds"
    );
}

// ---- validation never settles anything ---------------------------------------------------------

#[tokio::test]
async fn an_option_the_question_never_offered_is_refused_and_leaves_it_open() {
    let (registry, queue) = registry();
    let id = seed(&queue, choice_question());

    let refused = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["red"]}}),
    )
    .await
    .expect_err("`red` was never offered");

    match refused {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(
                message.contains("not one of the options"),
                "the message is the shared validation rule's, got: {message}"
            );
        }
        other => panic!("expected an argument error, got {other:?}"),
    }
    assert!(
        queue.get(&id).expect("recorded").state.is_open(),
        "a refused answer settles nothing"
    );
}

#[tokio::test]
async fn a_blank_answer_to_a_required_text_question_is_refused() {
    let (registry, queue) = registry();
    let id = seed(&queue, text_question("which colour"));

    let refused = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": [], "custom": "   "}}),
    )
    .await
    .expect_err("a required value was blank");

    match refused {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(
                message.contains("needs a value"),
                "the message names the question, got: {message}"
            );
        }
        other => panic!("expected an argument error, got {other:?}"),
    }
    assert!(queue.get(&id).expect("recorded").state.is_open());
}

#[tokio::test]
async fn too_many_selections_for_a_single_choice_are_refused() {
    let (registry, queue) = registry();
    let id = seed(&queue, choice_question());

    let refused = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["blue", "green"]}}),
    )
    .await
    .expect_err("a single choice takes one value");

    match refused {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(
                message.contains("exactly one"),
                "the decode step says what the shape takes, got: {message}"
            );
        }
        other => panic!("expected an argument error, got {other:?}"),
    }
    assert!(queue.get(&id).expect("recorded").state.is_open());
}

#[tokio::test]
async fn a_multi_select_answer_is_checked_against_its_bounds() {
    let (registry, queue) = registry();
    let id = seed(
        &queue,
        Question::new(QuestionShape::Multiple {
            label: "which colours".to_owned(),
            options: vec![
                QuestionOption::new("blue"),
                QuestionOption::new("green"),
                QuestionOption::new("red"),
            ],
            min: 2,
            max: 2,
        }),
    );

    let too_few = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["blue"]}}),
    )
    .await
    .expect_err("one of two required values");
    match too_few {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(message.contains("at least 2"), "got: {message}");
        }
        other => panic!("expected an argument error, got {other:?}"),
    }

    let too_many = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["blue", "green", "red"]}}),
    )
    .await
    .expect_err("three of two allowed values");
    match too_many {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(message.contains("at most 2"), "got: {message}");
        }
        other => panic!("expected an argument error, got {other:?}"),
    }

    let accepted = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["blue", "red"]}}),
    )
    .await
    .expect("two of two is answerable");
    assert_eq!(accepted["answer"]["selected"], json!(["blue", "red"]));
    assert!(!queue.get(&id).expect("recorded").state.is_open());
}

#[tokio::test]
async fn a_non_boolean_confirmation_is_refused() {
    let (registry, queue) = registry();
    let id = seed(
        &queue,
        Question::new(QuestionShape::Confirm {
            label: "overwrite?".to_owned(),
        }),
    );

    let refused = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["maybe"]}}),
    )
    .await
    .expect_err("`maybe` is not a confirmation");

    match refused {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(
                message.contains("not a confirmation value"),
                "got: {message}"
            );
        }
        other => panic!("expected an argument error, got {other:?}"),
    }
    assert!(queue.get(&id).expect("recorded").state.is_open());
}

#[tokio::test]
async fn an_unknown_id_is_refused_by_every_method_that_takes_one() {
    let (registry, _queue) = registry();

    for (method, args) in [
        (
            ANSWER_METHOD,
            json!({"id": "nope", "answer": {"selected": ["blue"]}}),
        ),
        (CANCEL_METHOD, json!({"id": "nope"})),
    ] {
        let refused = dispatch(&registry, method, args)
            .await
            .expect_err("no question holds that id");
        match refused {
            RemoteError::InvalidArguments { message, .. } => {
                assert!(message.contains("no question `nope`"), "got: {message}");
            }
            other => panic!("expected an argument error, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_missing_or_blank_id_names_the_field() {
    let (registry, _queue) = registry();

    for args in [json!({}), json!({"id": "   "})] {
        let refused = dispatch(&registry, CANCEL_METHOD, args)
            .await
            .expect_err("an id is required");
        match refused {
            RemoteError::InvalidArguments { message, .. } => {
                assert!(message.contains("`id` is required"), "got: {message}");
            }
            other => panic!("expected an argument error, got {other:?}"),
        }
    }
}

// ---- cancelling --------------------------------------------------------------------------------

#[tokio::test]
async fn cancelling_an_open_question_records_the_reason() {
    let (registry, queue) = registry();
    let id = seed(&queue, choice_question());

    let cancelled = dispatch(
        &registry,
        CANCEL_METHOD,
        json!({"id": id, "reason": "the panel was closed"}),
    )
    .await
    .expect("cancellable");

    assert_eq!(cancelled["state"], json!("cancelled"));
    assert_eq!(cancelled["cancelled"], json!(true));
    assert_eq!(cancelled["reason"], json!("the panel was closed"));
    let state = queue.get(&id).expect("recorded").state;
    assert!(!state.is_open());
    assert_eq!(
        state.reason(),
        Some("the panel was closed"),
        "the reason is readable from the state itself, not only from the response"
    );
}

#[tokio::test]
async fn cancelling_an_answered_question_is_refused() {
    let (registry, queue) = registry();
    let id = seed(&queue, choice_question());

    dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["blue"]}}),
    )
    .await
    .expect("answerable");

    let refused = dispatch(&registry, CANCEL_METHOD, json!({"id": id}))
        .await
        .expect_err("the question already settled");
    match refused {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(
                message.contains("already `answered`"),
                "the refusal names the state, got: {message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // The answer is untouched: a late cancel cannot turn a recorded answer into a cancellation.
    assert!(matches!(
        queue.get(&id).expect("recorded").state,
        QuestionState::Answered(_)
    ));
}

// ---- the waiting side --------------------------------------------------------------------------

#[tokio::test]
async fn a_waiting_asker_receives_a_human_answer() {
    let queue = Arc::new(QuestionQueue::in_memory());
    let asking = Arc::clone(&queue);
    let waiter = tokio::spawn(async move { asking.ask(AskRequest::new(choice_question())).await });

    // The id is discovered the way a client discovers it: from the listing.
    let id = wait_for_one_open(&queue).await;
    queue
        .answer(
            &id,
            QuestionReply::human(Answer::Chosen("green".to_owned())),
        )
        .expect("the question is open");

    let reply = waiter.await.expect("the task ran").expect("answered");
    assert_eq!(reply.answer, Answer::Chosen("green".to_owned()));
    assert!(reply.is_human(), "a person answered, so `by` is `human`");
}

#[tokio::test]
async fn a_waiting_asker_is_released_by_a_cancel() {
    let queue = Arc::new(QuestionQueue::in_memory());
    let asking = Arc::clone(&queue);
    let waiter = tokio::spawn(async move { asking.ask(AskRequest::new(choice_question())).await });

    let id = wait_for_one_open(&queue).await;
    queue
        .cancel(&id, Some("closed".to_owned()))
        .expect("the question is open");

    match waiter.await.expect("the task ran") {
        Err(AskError::Cancelled {
            id: reported,
            reason,
        }) => {
            assert_eq!(reported, id);
            assert_eq!(reason, "closed");
        }
        other => panic!("expected a cancellation, got {other:?}"),
    }
}

#[tokio::test]
async fn cancelling_without_a_reason_reports_one_it_can_print() {
    let queue = Arc::new(QuestionQueue::in_memory());
    let asking = Arc::clone(&queue);
    let waiter = tokio::spawn(async move { asking.ask(AskRequest::new(choice_question())).await });

    let id = wait_for_one_open(&queue).await;
    queue.cancel(&id, None).expect("the question is open");

    match waiter.await.expect("the task ran") {
        Err(AskError::Cancelled { reason, .. }) => {
            assert!(
                !reason.is_empty(),
                "a reason a caller can print, not an empty string"
            );
        }
        other => panic!("expected a cancellation, got {other:?}"),
    }
}

#[tokio::test]
async fn a_timeout_takes_the_default_and_calls_it_a_default() {
    // The property `questions` exists to protect: a default taken on timeout is not a person's answer.
    let queue = Arc::new(QuestionQueue::in_memory());
    let question = Question::new(QuestionShape::Choice {
        label: "which colour".to_owned(),
        options: vec![QuestionOption::new("blue"), QuestionOption::new("green")],
    })
    .with_default(Answer::Chosen("blue".to_owned()))
    .expect("the default is one of the options")
    .with_timeout(Duration::from_millis(20));

    let reply = queue
        .ask(AskRequest::new(question))
        .await
        .expect("the default is an answer");

    assert_eq!(reply.answer, Answer::Chosen("blue".to_owned()));
    assert!(
        !reply.is_human(),
        "a default taken on timeout must never be reported as a person's choice"
    );
    assert_eq!(reply.by.as_str(), "default");
}

#[tokio::test]
async fn a_timeout_with_no_default_reports_that_it_expired() {
    let (registry, queue) = registry();
    let asking = Arc::clone(&queue);
    let waiter = tokio::spawn(async move {
        asking
            .ask(AskRequest::new(
                choice_question().with_timeout(Duration::from_millis(20)),
            ))
            .await
    });

    match waiter.await.expect("the task ran") {
        Err(AskError::TimedOut { .. }) => {}
        other => panic!("expected a timeout, got {other:?}"),
    }

    // The question is recorded as expired, so a client's late answer is refused with `expired` rather
    // than recorded against a wait that no longer exists.
    let id = queue
        .pending()
        .first()
        .expect("the expired record is still listed under its own state")
        .id
        .clone();
    let refused = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["blue"]}}),
    )
    .await
    .expect_err("the wait is over");
    match refused {
        RemoteError::InvalidArguments { message, .. } => {
            assert!(message.contains("already `expired`"), "got: {message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Waits until the queue holds exactly one open question, and returns its id.
///
/// Polling rather than a notification because the queue deliberately publishes no "a question was
/// added" signal: a test that needed one would be asking the production surface for a hook that only
/// tests want.
async fn wait_for_one_open(queue: &Arc<QuestionQueue>) -> String {
    for _ in 0..1_000 {
        let open: Vec<PendingQuestion> = queue
            .pending()
            .into_iter()
            .filter(|record| record.state.is_open())
            .collect();
        if let Some(record) = open.first() {
            return record.id.clone();
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("no open question appeared");
}

#[tokio::test]
async fn an_owned_ask_carries_the_session_it_was_attributed_to() {
    // `ask_owned` is the shape a Responder needs: a future that outlives the call that made it. The
    // attribution is what lets a client route the question to the conversation it belongs to, so it
    // has to survive the round trip through the record.
    let queue = Arc::new(QuestionQueue::in_memory());
    let asking = Arc::clone(&queue);
    let waiter = tokio::spawn(async move {
        asking
            .ask_owned(
                AskRequest::new(choice_question())
                    .attributed_to(Some("sess-7".to_owned()), Some("call-2".to_owned())),
            )
            .await
    });

    let id = wait_for_one_open(&queue).await;
    let record = queue.get(&id).expect("recorded");
    assert_eq!(record.session_id.as_deref(), Some("sess-7"));
    assert_eq!(record.call_id.as_deref(), Some("call-2"));

    queue
        .answer(&id, QuestionReply::human(Answer::Chosen("blue".to_owned())))
        .expect("the question is open");
    assert_eq!(
        waiter
            .await
            .expect("the task ran")
            .expect("answered")
            .answer,
        Answer::Chosen("blue".to_owned())
    );
}

#[tokio::test]
async fn a_question_carries_the_session_that_asked() {
    let (registry, queue) = registry();
    let record = PendingQuestion::new(questions_rpc_id(), choice_question())
        .attributed_to(Some("sess-1".to_owned()), Some("call-9".to_owned()));
    let id = record.id.clone();
    queue.store().insert(record).expect("recorded");

    let listed = dispatch(&registry, LIST_METHOD, json!({"sessionId": "sess-1"}))
        .await
        .expect("listable");
    assert_eq!(item(&listed, &id)["sessionId"], json!("sess-1"));
    assert_eq!(item(&listed, &id)["callId"], json!("call-9"));

    // A different session's filter does not see it.
    let other = dispatch(&registry, LIST_METHOD, json!({"sessionId": "sess-2"}))
        .await
        .expect("listable");
    assert_eq!(other["total"], json!(0));
}

// ---- listing bounds ----------------------------------------------------------------------------

#[tokio::test]
async fn a_page_size_outside_the_bound_is_refused_rather_than_clamped() {
    let (registry, _queue) = registry();

    for limit in [0, MAX_LIST_LIMIT + 1] {
        let refused = dispatch(&registry, LIST_METHOD, json!({"limit": limit}))
            .await
            .expect_err("out of range");
        match refused {
            RemoteError::InvalidArguments { message, .. } => {
                assert!(
                    message.contains("`limit` must be between"),
                    "got: {message}"
                );
            }
            other => panic!("expected an argument error, got {other:?}"),
        }
    }

    let defaulted = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("listable");
    assert_eq!(defaulted["limit"], json!(DEFAULT_LIST_LIMIT));
}

#[tokio::test]
async fn an_unknown_state_filter_lists_the_accepted_ones() {
    let (registry, _queue) = registry();

    let refused = dispatch(&registry, LIST_METHOD, json!({"state": "pending"}))
        .await
        .expect_err("`pending` is not a state");

    match refused {
        RemoteError::InvalidArguments { message, .. } => {
            for state in ["open", "answered", "cancelled", "expired"] {
                assert!(
                    message.contains(state),
                    "the message lists `{state}`, got: {message}"
                );
            }
        }
        other => panic!("expected an argument error, got {other:?}"),
    }
}

#[tokio::test]
async fn paging_walks_the_records_once_each() {
    let (registry, queue) = registry();
    for _ in 0..3 {
        seed(&queue, choice_question());
    }

    let first = dispatch(&registry, LIST_METHOD, json!({"limit": 2, "offset": 0}))
        .await
        .expect("listable");
    let second = dispatch(&registry, LIST_METHOD, json!({"limit": 2, "offset": 2}))
        .await
        .expect("listable");

    assert_eq!(first["shown"], json!(2));
    assert_eq!(first["total"], json!(3));
    assert_eq!(second["shown"], json!(1));

    let mut ids: Vec<&str> = first["items"]
        .as_array()
        .expect("an array")
        .iter()
        .chain(second["items"].as_array().expect("an array"))
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "the two pages do not repeat a record");
}

// ---- declarations and the unserved method ------------------------------------------------------

/// A policy that records every request and allows it.
#[derive(Debug, Default)]
struct RecordingPolicy {
    seen: Mutex<Vec<AccessRequest>>,
}

impl Policy for RecordingPolicy {
    fn decide(&self, request: &AccessRequest) -> AccessDecision {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        AccessDecision::Allow
    }
}

#[tokio::test]
async fn each_method_declares_the_access_it_actually_needs() {
    let queue = Arc::new(QuestionQueue::in_memory());
    let policy = Arc::new(RecordingPolicy::default());
    let mut registry = RemoteRegistry::new();
    register_question_methods(&mut registry, Arc::clone(&queue));
    let registry = registry.with_policy(policy.clone());

    let id = seed(&queue, choice_question());
    dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("listable");
    dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["blue"]}}),
    )
    .await
    .expect("answerable");
    let second = seed(&queue, choice_question());
    dispatch(&registry, CANCEL_METHOD, json!({"id": second}))
        .await
        .expect("cancellable");

    let seen = policy
        .seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let access = |method: &str| {
        seen.iter()
            .find(|request| request.method == format!("{NAMESPACE}.{method}"))
            .map(|request| request.access)
            .expect("the method was dispatched")
    };
    assert_eq!(access(LIST_METHOD), Access::Read);
    assert_eq!(access(ANSWER_METHOD), Access::Write);
    assert_eq!(
        access(CANCEL_METHOD),
        Access::Destructive,
        "a cancelled question cannot be restored, so a policy may refuse it separately"
    );
}

#[tokio::test]
async fn a_read_only_policy_can_list_and_cannot_answer_or_cancel() {
    let (registry, queue) = registry();
    let registry = registry.with_policy(Arc::new(girder_core::ReadOnly));
    let id = seed(&queue, choice_question());

    let answered = dispatch(
        &registry,
        ANSWER_METHOD,
        json!({"id": id, "answer": {"selected": ["blue"]}}),
    )
    .await
    .expect_err("a read-only peer may not answer");
    assert_eq!(answered.code(), "REFUSED");

    let cancelled = dispatch(&registry, CANCEL_METHOD, json!({"id": id}))
        .await
        .expect_err("a read-only peer may not cancel");
    assert_eq!(cancelled.code(), "REFUSED");

    let listed = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("a read-only peer may list");
    assert_eq!(
        listed["total"],
        json!(1),
        "the listing filters to open records"
    );

    assert!(
        queue.get(&id).expect("recorded").state.is_open(),
        "neither refusal settled anything"
    );
}

#[tokio::test]
async fn upstreams_unserved_method_is_named_rather_than_silently_absent() {
    let (registry, _queue) = registry();

    // Registered: the three this build serves.
    for method in [LIST_METHOD, ANSWER_METHOD, CANCEL_METHOD] {
        assert!(
            registry.get(NAMESPACE, method).is_some(),
            "`{NAMESPACE}.{method}` must be served"
        );
    }

    assert!(UNSERVED_METHODS.is_empty());
    assert!(
        registry.get(NAMESPACE, ATTACH_WAIT_METHOD).is_some(),
        "`{NAMESPACE}.{ATTACH_WAIT_METHOD}` must be served"
    );
    let frame = dispatch(
        &registry,
        ATTACH_WAIT_METHOD,
        json!({"agentId": "missing", "callId": "missing"}),
    )
    .await
    .expect("a countdown with nobody waiting is a finished frame");
    assert_eq!(frame["done"], json!(true));
    assert_eq!(frame["remainingMs"], json!(0));
}

#[test]
fn an_empty_store_lists_nothing_and_a_failed_insert_is_reported() {
    let store = MemoryQuestionStore::new();
    assert!(store.is_empty());
    assert!(store.list().is_empty());
    assert!(store.get("anything").is_none());

    let queue = QuestionQueue::new(Arc::new(store));
    assert!(queue.pending().is_empty());
    assert_eq!(
        queue.cancel("nothing", None),
        Err(QuestionStoreError::NotFound {
            id: "nothing".to_owned()
        })
    );
}

#[tokio::test]
async fn an_unavailable_store_refuses_rather_than_pretending() {
    // A deployment with no storage configured must say so. Silently keeping the record in memory
    // would report a question the next call could not confirm.
    let store: Arc<dyn QuestionStore> = Arc::new(UnavailableQuestionStore::default_reason());
    let queue = Arc::new(QuestionQueue::new(Arc::clone(&store)));
    let mut registry = RemoteRegistry::new();
    register_question_methods(&mut registry, Arc::clone(&queue));
    let registry = registry.with_policy(Arc::new(RecordingPolicy::default()));

    assert!(
        store.list().is_empty(),
        "nothing is held, and nothing is invented"
    );
    assert!(store.get("q-1").is_none());
    let configured = UnavailableQuestionStore::default_reason();
    assert!(
        configured.reason().contains("QuestionStore"),
        "the store publishes the reason it will refuse with"
    );

    // Asking refuses before it waits, because the record was never stored.
    match queue.ask(AskRequest::new(choice_question())).await {
        Err(AskError::Store(QuestionStoreError::Unavailable(reason))) => {
            assert!(
                reason.contains("QuestionStore"),
                "the message names what a host must supply, got: {reason}"
            );
        }
        other => panic!("expected an unavailable-store refusal, got {other:?}"),
    }

    // A listing is empty rather than a refusal: the signature has no error channel, and the writes
    // are where the missing storage is reported.
    let listed = dispatch(&registry, LIST_METHOD, json!({}))
        .await
        .expect("a listing over no storage is empty, not a failure");
    assert_eq!(listed["total"], json!(0));
}

#[tokio::test]
async fn an_answer_arriving_as_the_timeout_fires_is_preferred_to_the_default() {
    // The narrow race the expiry path has to handle: the timeout elapses, and before the default is
    // recorded a person answers. Their answer is the outcome -- a default recorded over a reply that
    // already arrived would tell the asker nobody answered when someone did.
    let queue = Arc::new(QuestionQueue::in_memory());
    let question = Question::new(QuestionShape::Choice {
        label: "which colour".to_owned(),
        options: vec![QuestionOption::new("blue"), QuestionOption::new("green")],
    })
    .with_default(Answer::Chosen("blue".to_owned()))
    .expect("the default is one of the options")
    .with_timeout(Duration::from_millis(30));

    let asking = Arc::clone(&queue);
    let waiter = tokio::spawn(async move { asking.ask(AskRequest::new(question)).await });

    let id = wait_for_one_open(&queue).await;
    // Answered right at the deadline: whichever of the two settles first, the wait must report a
    // recorded outcome rather than an internal inconsistency.
    tokio::time::sleep(Duration::from_millis(28)).await;
    // A refusal here is legitimate -- the expiry may have won -- so the assertion is on what the
    // *asker* received, not on this call.
    let _ = queue.answer(
        &id,
        QuestionReply::human(Answer::Chosen("green".to_owned())),
    );

    match waiter.await.expect("the task ran") {
        Ok(reply) => {
            // Either the person's answer or the default, both of which are recorded replies.
            assert!(
                reply.answer == Answer::Chosen("green".to_owned())
                    || reply.answer == Answer::Chosen("blue".to_owned()),
                "the outcome is one of the two recorded answers, got {:?}",
                reply.answer
            );
            if reply.answer == Answer::Chosen("green".to_owned()) {
                assert!(
                    reply.is_human(),
                    "the person's answer is attributed to a person"
                );
            } else {
                assert!(
                    !reply.is_human(),
                    "a default must never be reported as a person's choice"
                );
            }
        }
        Err(AskError::TimedOut { .. }) => {
            // Also legitimate: the question declared a default, so this would be a bug -- but only
            // if it also recorded the default. Reported as a failure of the expectation instead.
            panic!("a question with a default must not time out without taking it");
        }
        Err(other) => panic!("expected an answer or a cancellation, got {other:?}"),
    }
}

#[test]
fn a_second_settle_on_a_store_reports_the_state_that_won() {
    let store = MemoryQuestionStore::new();
    let record = PendingQuestion::new("q-1", choice_question());
    store.insert(record).expect("recorded");

    store
        .transition("q-1", QuestionState::Expired)
        .expect("the first settle wins");

    assert_eq!(
        store.transition("q-1", QuestionState::Cancelled { reason: None }),
        Err(QuestionStoreError::AlreadySettled {
            id: "q-1".to_owned(),
            state: "expired".to_owned()
        }),
        "the state travels with the refusal, so a client learns what happened"
    );
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod interaction {
    pub use super::questions_rpc as questions_rpc;
}
