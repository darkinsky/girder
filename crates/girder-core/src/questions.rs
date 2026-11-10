//! Asking a person a question and waiting for the answer.
//!
//! Different from approval in what is being decided. An approval is a yes or no about one action; a
//! question asks for *information* — a choice between options, a missing value, a clarification. The two
//! are separate because conflating them produces an approval dialog with a text field, which is a bad UI
//! for both.
//!
//! # Why a question declares its shape
//!
//! A question with free-form text would make the UI guess what to render and the caller guess what it
//! received. Declaring options lets a client show buttons for a choice and a field for a value, before the
//! person has typed anything.
//!
//! # Why a default matters
//!
//! A person who does not answer must not block the session forever. A question therefore carries a default
//! where one exists, so a timeout produces the caller's expected answer rather than an error — which is the
//! difference between a session that continues and one that stalls because nobody was watching.
//!
//! # Why the answer records who answered
//!
//! A default taken on timeout is not the same as a person choosing. A replay that showed both as "the user
//! chose X" would misattribute a decision, and a caller that treated a default as consent would be wrong
//! about what happened.

use std::time::Duration;

/// What kind of answer a question wants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    /// One free-form line.
    Text {
        /// What to ask for.
        label: String,
        /// Whether an empty answer is acceptable.
        optional: bool,
    },
    /// One of a fixed set.
    Choice {
        /// What to ask for.
        label: String,
        /// The options, in display order.
        options: Vec<Option_>,
    },
    /// Several of a fixed set.
    Multiple {
        /// What to ask for.
        label: String,
        /// The options.
        options: Vec<Option_>,
        /// The fewest that must be chosen.
        min: usize,
        /// The most that may be chosen.
        max: usize,
    },
    /// Yes or no.
    Confirm {
        /// What to ask.
        label: String,
    },
}

/// One option a person may choose.
///
/// A value and a label rather than one string: the label is what a person reads and the value is what the
/// caller receives, and a caller that matched on the label would break when the wording changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Option_ {
    /// The value the caller receives.
    pub value: String,
    /// What a person reads.
    pub label: String,
    /// Extra explanation, when a label is not enough.
    pub detail: Option<String>,
}

impl Option_ {
    /// Builds an option whose label is its value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            value,
            detail: None,
        }
    }

    /// Gives it a different label.
    #[must_use]
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Adds an explanation.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl Shape {
    /// The prompt a client shows.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Text { label, .. }
            | Self::Choice { label, .. }
            | Self::Multiple { label, .. }
            | Self::Confirm { label } => label,
        }
    }

    /// Validates an answer against this shape.
    ///
    /// # Errors
    ///
    /// Returns [`Answer`]'s mismatch reason. Checked here rather than inside the consumer so every question
    /// validates the same way: a consumer that forgot would accept an answer its own options never offered.
    pub fn accepts(&self, answer: &Answer) -> Result<(), QuestionError> {
        match (self, answer) {
            (Self::Text { optional, .. }, Answer::Text(text)) => {
                if !optional && text.trim().is_empty() {
                    return Err(QuestionError::Empty {
                        label: self.label().to_owned(),
                    });
                }
                Ok(())
            }
            (Self::Confirm { .. }, Answer::Confirmed(_)) => Ok(()),
            (Self::Choice { options, .. }, Answer::Chosen(value)) => {
                if options.iter().any(|option| &option.value == value) {
                    Ok(())
                } else {
                    Err(QuestionError::NotAnOption {
                        value: value.clone(),
                    })
                }
            }
            (
                Self::Multiple {
                    options, min, max, ..
                },
                Answer::ChosenMany(values),
            ) => {
                for value in values {
                    if !options.iter().any(|option| &option.value == value) {
                        return Err(QuestionError::NotAnOption {
                            value: value.clone(),
                        });
                    }
                }
                if values.len() < *min {
                    return Err(QuestionError::TooFew {
                        min: *min,
                        got: values.len(),
                    });
                }
                if values.len() > *max {
                    return Err(QuestionError::TooMany {
                        max: *max,
                        got: values.len(),
                    });
                }
                Ok(())
            }
            (shape, answer) => Err(QuestionError::WrongShape {
                expected: shape.kind(),
                got: answer.kind(),
            }),
        }
    }

    /// The shape's name, for an error message.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Choice { .. } => "a choice",
            Self::Multiple { .. } => "several choices",
            Self::Confirm { .. } => "a confirmation",
        }
    }
}

/// What a person answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// One line.
    Text(String),
    /// One option's value.
    Chosen(String),
    /// Several options' values.
    ChosenMany(Vec<String>),
    /// Yes or no.
    Confirmed(bool),
}

impl Answer {
    /// The answer's name, for an error message.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Chosen(_) => "a choice",
            Self::ChosenMany(_) => "several choices",
            Self::Confirmed(_) => "a confirmation",
        }
    }
}

/// Who produced an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnsweredBy {
    /// A person, interactively.
    Human,
    /// The default, after the wait expired.
    ///
    /// Recorded rather than shown as `Human`: a caller that treated a default as consent would be wrong about
    /// what happened.
    Default,
}

impl AnsweredBy {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Default => "default",
        }
    }
}

/// Why a question could not be asked or its answer rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QuestionError {
    /// Nobody is available to ask.
    #[error("no responder is available in this deployment")]
    NoResponder,

    /// The answer did not match the question's shape.
    #[error("expected {expected}, got {got}")]
    WrongShape {
        /// What was asked for.
        expected: &'static str,
        /// What arrived.
        got: &'static str,
    },

    /// A required text answer was blank.
    #[error("`{label}` needs a value")]
    Empty {
        /// The question.
        label: String,
    },

    /// An answer named a value the question never offered.
    #[error("`{value}` is not one of the options")]
    NotAnOption {
        /// The value.
        value: String,
    },

    /// Too few options were chosen.
    #[error("choose at least {min}, got {got}")]
    TooFew {
        /// The minimum.
        min: usize,
        /// How many arrived.
        got: usize,
    },

    /// Too many options were chosen.
    #[error("choose at most {max}, got {got}")]
    TooMany {
        /// The maximum.
        max: usize,
        /// How many arrived.
        got: usize,
    },
}

/// One question to ask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    /// What is being asked.
    pub shape: Shape,
    /// Why it is being asked, so a person can judge whether to answer carefully.
    pub context: Option<String>,
    /// How long to wait.
    pub timeout: Duration,
    /// The answer to take when nobody responds.
    ///
    /// `None` means the timeout is an error rather than a default: a question whose answer changes what
    /// happens must not be defaulted silently.
    pub default: Option<Answer>,
}

/// The default wait for an answer.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

impl Question {
    /// Builds a question.
    #[must_use]
    pub fn new(shape: Shape) -> Self {
        Self {
            shape,
            context: None,
            timeout: DEFAULT_TIMEOUT,
            default: None,
        }
    }

    /// Explains why it is being asked.
    #[must_use]
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Provides the answer to take on timeout.
    ///
    /// The default is validated against the shape at construction, so a default the question would reject is
    /// refused where it was written rather than where it was used.
    ///
    /// # Errors
    ///
    /// Returns [`QuestionError`] when the default does not match the shape.
    pub fn with_default(mut self, answer: Answer) -> Result<Self, QuestionError> {
        self.shape.accepts(&answer)?;
        self.default = Some(answer);
        Ok(self)
    }

    /// Bounds the wait.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// One recorded answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// The answer.
    pub answer: Answer,
    /// Who produced it.
    pub by: AnsweredBy,
}

impl Reply {
    /// A person's answer.
    #[must_use]
    pub fn human(answer: Answer) -> Self {
        Self {
            answer,
            by: AnsweredBy::Human,
        }
    }

    /// The default, after the wait expired.
    #[must_use]
    pub fn defaulted(answer: Answer) -> Self {
        Self {
            answer,
            by: AnsweredBy::Default,
        }
    }

    /// Whether a person answered rather than a default being taken.
    #[must_use]
    pub fn is_human(&self) -> bool {
        self.by == AnsweredBy::Human
    }
}

/// The seam a deployment implements to ask a person.
pub trait Responder: Send + Sync {
    /// Asks, and waits no longer than the question's timeout.
    fn ask(&self, question: Question) -> crate::scope::BoxFut<Result<Reply, QuestionError>>;
}

/// A responder that refuses to ask.
///
/// The default for a deployment with no interactive surface: a caller that forgot to mount a responder gets
/// a named error rather than a silent default.
#[derive(Debug, Default)]
pub struct Unavailable;

impl Responder for Unavailable {
    fn ask(&self, _question: Question) -> crate::scope::BoxFut<Result<Reply, QuestionError>> {
        Box::pin(async { Err(QuestionError::NoResponder) })
    }
}

/// A responder that answers from a fixed value, for tests and for a non-interactive run.
pub struct Scripted {
    answer: Answer,
}

impl Scripted {
    /// Answers every question with `answer`.
    #[must_use]
    pub fn answering(answer: Answer) -> Self {
        Self { answer }
    }
}

impl Responder for Scripted {
    fn ask(&self, question: Question) -> crate::scope::BoxFut<Result<Reply, QuestionError>> {
        let answer = self.answer.clone();
        Box::pin(async move {
            // Validated like a real answer: a scripted responder that skipped the check would let a test pass
            // with an answer the question never allowed.
            question.shape.accepts(&answer)?;
            Ok(Reply::human(answer))
        })
    }
}
