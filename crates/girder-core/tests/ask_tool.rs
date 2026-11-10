//! Tests for the `ask_user` tool.
//!
//! The property that matters is that a model can tell a person's answer from a default taken on timeout: a model that
//! built on consent it never received is a worse outcome than one that asked again.

use std::sync::Arc;

use girder_core::{
    Answer, QuestionError, QuestionReply, ScriptedResponder, ToolResult, ToolRunContext, ask_tool,
};

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            girder_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn ask(args: serde_json::Value, responder: Arc<dyn girder_core::Responder>) -> ToolResult {
    let tool = ask_tool(responder);
    (tool.execute)(args, ToolRunContext::default())
        .await
        .expect("a question is a result")
}

#[tokio::test]
async fn a_free_form_question_returns_the_answer() {
    let responder = Arc::new(ScriptedResponder::answering(Answer::Text(
        "blue".to_owned(),
    )));

    let result = ask(serde_json::json!({"question": "which colour"}), responder).await;

    assert!(!result.is_error);
    assert_eq!(text_of(&result), "blue");
}

#[tokio::test]
async fn a_single_choice_is_refused_when_the_model_asked_for_one() {
    // Offering several options without `multiple` means one answer; a multi-answer would be a shape mismatch.
    let responder = Arc::new(ScriptedResponder::answering(Answer::ChosenMany(vec![
        "a".to_owned(),
    ])));

    let result = ask(
        serde_json::json!({"question": "which", "options": ["a", "b"]}),
        responder,
    )
    .await;

    assert!(
        result.is_error,
        "a multi-answer to a single-choice question is refused"
    );
}

#[tokio::test]
async fn options_become_a_choice() {
    // A model that offered choices meant them as a choice, not as free text.
    let responder = Arc::new(ScriptedResponder::answering(Answer::Chosen("b".to_owned())));

    let result = ask(
        serde_json::json!({"question": "which", "options": ["a", "b"]}),
        responder,
    )
    .await;

    assert!(!result.is_error);
    assert_eq!(text_of(&result), "b");
}

#[tokio::test]
async fn one_option_becomes_a_confirmation() {
    // A single option means the model wanted a yes or no without saying so.
    let responder = Arc::new(ScriptedResponder::answering(Answer::Confirmed(true)));

    let result = ask(
        serde_json::json!({"question": "overwrite the file?", "options": ["yes"]}),
        responder,
    )
    .await;

    assert_eq!(text_of(&result), "yes");
}

#[tokio::test]
async fn a_declined_confirmation_reads_as_no() {
    let responder = Arc::new(ScriptedResponder::answering(Answer::Confirmed(false)));

    let result = ask(
        serde_json::json!({"question": "overwrite?", "options": ["yes"]}),
        responder,
    )
    .await;

    assert_eq!(text_of(&result), "no");
}

#[tokio::test]
async fn a_default_is_labelled_so_a_model_does_not_treat_it_as_consent() {
    // The whole reason this label exists: a model that built on defaulted consent would be wrong about what happened.
    struct Defaulting;
    impl girder_core::Responder for Defaulting {
        fn ask(
            &self,
            _question: girder_core::Question,
        ) -> girder_core::BoxFut<Result<QuestionReply, QuestionError>> {
            Box::pin(async { Ok(QuestionReply::defaulted(Answer::Confirmed(true))) })
        }
    }

    let result = ask(
        serde_json::json!({"question": "proceed?", "options": ["yes"]}),
        Arc::new(Defaulting),
    )
    .await;

    let text = text_of(&result);
    assert!(text.contains("default"), "{text}");
    assert!(text.contains("nobody answered"), "{text}");
}

#[tokio::test]
async fn an_unavailable_responder_is_reported_as_a_result() {
    // A deployment with no interactive surface: the model needs to know it cannot ask, not to have the turn fail.
    let result = ask(
        serde_json::json!({"question": "which"}),
        Arc::new(girder_core::UnavailableResponder),
    )
    .await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("cannot ask"));
}

#[tokio::test]
async fn a_missing_question_is_refused() {
    let responder = Arc::new(ScriptedResponder::answering(Answer::Text("x".to_owned())));

    let result = ask(serde_json::json!({}), responder).await;

    assert!(result.is_error);
    assert!(text_of(&result).contains("required"));
}

#[tokio::test]
async fn several_choices_read_as_a_list() {
    let responder = Arc::new(ScriptedResponder::answering(Answer::ChosenMany(vec![
        "a".to_owned(),
        "c".to_owned(),
    ])));

    let result = ask(
        serde_json::json!({"question": "which", "options": ["a", "b", "c"], "multiple": true}),
        responder,
    )
    .await;

    assert_eq!(text_of(&result), "a, c");
}
