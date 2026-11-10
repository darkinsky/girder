//! Tests for asking a person a question.
//!
//! The properties that matter are about what an answer means: a default taken on timeout is not a person's
//! choice, and an answer naming a value the question never offered must be refused.

use std::time::Duration;

use girder_core::{
    Answer, AnsweredBy, DEFAULT_QUESTION_TIMEOUT, Question, QuestionError, QuestionOption,
    QuestionReply, Responder, ScriptedResponder, UnavailableResponder,
};

fn choice() -> Question {
    Question::new(girder_core::QuestionShape::Choice {
        label: "which branch".to_owned(),
        options: vec![
            OptionValue::new("main").labelled("main branch"),
            OptionValue::new("dev").labelled("development"),
        ],
    })
}

use girder_core::QuestionOption as OptionValue;

// ---- shapes -----------------------------------------------------------------------------------

#[test]
fn a_text_answer_must_not_be_blank_when_required() {
    let shape = girder_core::QuestionShape::Text {
        label: "name".to_owned(),
        optional: false,
    };

    assert!(matches!(
        shape.accepts(&Answer::Text("  ".to_owned())),
        Err(QuestionError::Empty { .. })
    ));
    assert!(shape.accepts(&Answer::Text("ok".to_owned())).is_ok());
}

#[test]
fn an_optional_text_answer_may_be_blank() {
    let shape = girder_core::QuestionShape::Text {
        label: "note".to_owned(),
        optional: true,
    };

    assert!(shape.accepts(&Answer::Text(String::new())).is_ok());
}

#[test]
fn a_choice_answer_must_name_an_offered_option() {
    // A caller that matched on the label would break when the wording changed, so the value is matched.
    let question = choice();

    assert!(
        question
            .shape
            .accepts(&Answer::Chosen("main".to_owned()))
            .is_ok()
    );
    assert!(matches!(
        question.shape.accepts(&Answer::Chosen("trunk".to_owned())),
        Err(QuestionError::NotAnOption { .. })
    ));
}

#[test]
fn a_choice_answer_matching_a_label_is_refused() {
    // "main branch" is the label; "main" is the value.
    let question = choice();

    assert!(matches!(
        question
            .shape
            .accepts(&Answer::Chosen("main branch".to_owned())),
        Err(QuestionError::NotAnOption { .. })
    ));
}

#[test]
fn a_multiple_answer_respects_its_bounds() {
    let shape = girder_core::QuestionShape::Multiple {
        label: "which".to_owned(),
        options: vec![
            OptionValue::new("a"),
            OptionValue::new("b"),
            OptionValue::new("c"),
        ],
        min: 1,
        max: 2,
    };

    assert!(
        shape
            .accepts(&Answer::ChosenMany(vec!["a".to_owned()]))
            .is_ok()
    );
    assert!(matches!(
        shape.accepts(&Answer::ChosenMany(Vec::new())),
        Err(QuestionError::TooFew { .. })
    ));
    assert!(matches!(
        shape.accepts(&Answer::ChosenMany(vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned()
        ])),
        Err(QuestionError::TooMany { .. })
    ));
}

#[test]
fn a_multiple_answer_refuses_an_unknown_value_even_within_bounds() {
    let shape = girder_core::QuestionShape::Multiple {
        label: "which".to_owned(),
        options: vec![OptionValue::new("a")],
        min: 1,
        max: 1,
    };

    assert!(matches!(
        shape.accepts(&Answer::ChosenMany(vec!["z".to_owned()])),
        Err(QuestionError::NotAnOption { .. })
    ));
}

#[test]
fn a_shape_refuses_an_answer_of_the_wrong_kind() {
    let question = choice();

    let error = question
        .shape
        .accepts(&Answer::Text("main".to_owned()))
        .expect_err("wrong kind");
    let QuestionError::WrongShape { expected, got } = error else {
        panic!("expected WrongShape");
    };
    assert_eq!(expected, "a choice");
    assert_eq!(got, "text");
}

#[test]
fn a_confirmation_accepts_both_answers() {
    let shape = girder_core::QuestionShape::Confirm {
        label: "proceed".to_owned(),
    };

    assert!(shape.accepts(&Answer::Confirmed(true)).is_ok());
    assert!(shape.accepts(&Answer::Confirmed(false)).is_ok());
}

// ---- options ----------------------------------------------------------------------------------

#[test]
fn an_option_defaults_its_label_to_its_value() {
    let option = QuestionOption::new("main");

    assert_eq!(option.value, "main");
    assert_eq!(option.label, "main");
}

#[test]
fn an_option_can_carry_a_different_label_and_detail() {
    let option = QuestionOption::new("main")
        .labelled("main branch")
        .with_detail("the default branch");

    assert_eq!(option.label, "main branch");
    assert_eq!(option.detail.as_deref(), Some("the default branch"));
    assert_eq!(option.value, "main", "the value is unchanged");
}

// ---- defaults ---------------------------------------------------------------------------------

#[test]
fn a_question_defaults_to_a_five_minute_wait() {
    assert_eq!(
        Question::new(girder_core::QuestionShape::Confirm {
            label: "x".to_owned()
        })
        .timeout,
        DEFAULT_QUESTION_TIMEOUT
    );
    assert_eq!(DEFAULT_QUESTION_TIMEOUT, Duration::from_secs(300));
}

#[test]
fn a_default_is_validated_against_the_shape() {
    // A default the question would reject must be refused where it was written rather than where it was used.
    let error = choice()
        .with_default(Answer::Chosen("trunk".to_owned()))
        .expect_err("not an option");

    assert!(matches!(error, QuestionError::NotAnOption { .. }));
}

#[test]
fn a_valid_default_is_accepted() {
    let question = choice()
        .with_default(Answer::Chosen("main".to_owned()))
        .expect("valid");

    assert_eq!(question.default, Some(Answer::Chosen("main".to_owned())));
}

#[test]
fn a_question_with_no_default_cannot_be_defaulted() {
    let question = choice();

    assert!(question.default.is_none());
}

// ---- replies ----------------------------------------------------------------------------------

#[test]
fn a_defaulted_reply_is_not_attributed_to_a_person() {
    // A caller that treated a default as consent would be wrong about what happened.
    let reply = QuestionReply::defaulted(Answer::Chosen("main".to_owned()));

    assert!(!reply.is_human());
    assert_eq!(reply.by, AnsweredBy::Default);
}

#[test]
fn a_human_reply_is_attributed_to_a_person() {
    let reply = QuestionReply::human(Answer::Confirmed(true));

    assert!(reply.is_human());
    assert_eq!(reply.by, AnsweredBy::Human);
}

#[test]
fn the_answered_by_names_are_stable() {
    assert_eq!(AnsweredBy::Human.as_str(), "human");
    assert_eq!(AnsweredBy::Default.as_str(), "default");
}

// ---- responders -------------------------------------------------------------------------------

#[tokio::test]
async fn an_unavailable_responder_reports_that_rather_than_defaulting() {
    // A caller that forgot to mount a responder must get a named error, not a silent default.
    let error = UnavailableResponder
        .ask(choice())
        .await
        .expect_err("no responder");

    assert!(matches!(error, QuestionError::NoResponder));
}

#[tokio::test]
async fn a_scripted_responder_validates_its_answer() {
    // A scripted responder that skipped the check would let a test pass with an answer the question never
    // allowed.
    let error = ScriptedResponder::answering(Answer::Chosen("trunk".to_owned()))
        .ask(choice())
        .await
        .expect_err("not an option");

    assert!(matches!(error, QuestionError::NotAnOption { .. }));
}

#[tokio::test]
async fn a_scripted_responder_answers_a_matching_question() {
    let reply = ScriptedResponder::answering(Answer::Chosen("dev".to_owned()))
        .ask(choice())
        .await
        .expect("answered");

    assert!(reply.is_human());
    assert_eq!(reply.answer, Answer::Chosen("dev".to_owned()));
}

#[test]
fn the_shapes_name_themselves_for_an_error() {
    assert_eq!(
        girder_core::QuestionShape::Text {
            label: "x".to_owned(),
            optional: false
        }
        .kind(),
        "text"
    );
    assert_eq!(
        girder_core::QuestionShape::Confirm {
            label: "x".to_owned()
        }
        .kind(),
        "a confirmation"
    );
}

#[test]
fn an_option_records_its_detail_for_a_person() {
    let option = OptionValue::new("a").with_detail("explains a");
    assert_eq!(option.detail.as_deref(), Some("explains a"));
}
