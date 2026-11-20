//! Model-backed session titles: the request policy, the prompt, and the output interpretation.
//!
//! Upstream path: `packages/session/session-title-llm/src/index.ts` (291 lines) plus its one shipped
//! provider, `packages/session/session-title-first-prompt-llm/src/index.ts` (167 lines).
//!
//! # Why this module exists
//!
//! [`crate::session::title`] ports only the deterministic half of upstream's title stack —
//! `fallbackSessionTitle` and `normalizeSessionTitle` in `normalize.ts` — and says so in its own
//! header: the model-backed provider is "deliberately not implemented here". That leaves the
//! interesting half missing. The fallback quotes the opening words of a conversation; a provider
//! reads them and names the *subject*. "Can you fix this" and "why does the release job fail on the
//! second retry" are the same length and wildly different titles, and only a model can turn the
//! second into "release job retry failure" while turning the first into "greeting".
//!
//! # What this module is, and what it is not
//!
//! Nothing here calls a model. Upstream itself splits the work that way: `session-title-llm` owns
//! *policy* — route preparation, input/token/time bounds, cancellation, the exact request record,
//! stream assembly — while each provider owns its *strategy*: the system prompt, the framing, the
//! length targets, and the interpretation of the reply. This file ports the parts of both that can
//! be decided without a network round trip:
//!
//! - **when to run** ([`schedule`]) — the cadence rules, including the two conditions that
//!   surprise readers: an explicit rename pins the title, and a first-prompt provider runs once;
//! - **what to send** ([`system_prompt`], [`frame_messages`], [`check_input`]) — the instruction,
//!   the JSON framing that keeps user text from breaking the delimiters, and the byte budget;
//! - **what the reply means** ([`title_from_response`]) — which terminal finishes and block kinds
//!   are acceptable, and how the title is lifted out of a reply that ignored "one line";
//! - **what happens when it goes wrong** ([`accept_or_fallback`]) — the deterministic fallback is
//!   not a nicety: without it a model hiccup leaves the session nameless, the exact state the whole
//!   stack exists to avoid.
//!
//! The stream loop lives next door, in [`crate::session::title_dispatch`]. This file stays the policy:
//! when to run, what to send, and how to read the reply. [`RequestRecord`] is still the log shape
//! upstream writes as `session/title-llm-request` before the call.
//!
//! # Why the title is normalized twice, and why that is not a mistake
//!
//! A provider returns raw model text to the service, and both sides clean it: the provider strips
//! formatting so it can reject an empty reply, the service re-cleans against the durable byte cap so
//! a provider bug cannot write a 4 KiB title. [`title_from_response`] does both in one pass — parse,
//! normalize, then truncate to the caller's cap — because the two passes are idempotent and the
//! intermediate value is never observed. What must not be collapsed is the *emptiness* check: it runs
//! after the final truncation, since a one-byte budget can turn a valid CJK reply into nothing.
//!
//! # Why bytes here but characters in `session_title`
//!
//! [`crate::session::title`] budgets characters, and documents why: it derives titles locally, and a
//! character cap is what a reader would guess. The model-facing caps are bytes because that is what
//! upstream's fields and request budget count — [`check_input`] guards a *request* against a provider
//! limit, where the unit is bytes on the wire. Titles coming *back* are also truncated by bytes, to
//! match the durable cap the service will apply; [`truncate_title_utf8`] is the same
//! never-split-a-code-point rule, expressed in the other unit.
//!
//! This file is also compiled by `tests/session_title_llm_gap.rs` through `#[path]`, so every import
//! below goes through the crate root's public re-exports: in that build `crate::` names the test
//! crate, and a `crate::…` path would fail to resolve.

// The crate root declares this module but re-exports nothing from it yet, so in a library build no
// item here has a caller and `dead_code` would report the whole surface as waste. The allow says
// that plainly; the test target exercises every item below.
#![allow(dead_code)]

use std::fmt;

use girder_core::{ChatRole, ContentBlock, FinishReason, MessageSource, SessionEvent};
use serde_json::{Map, Value, json};

/// Largest delay a JavaScript timer can hold, and therefore the ceiling upstream puts on a title
/// request's deadline.
///
/// A larger value is not "a long timeout", it is a negative delay: the number overflows a signed
/// 32-bit millisecond count when the platform arms the timer, so the request would either fire
/// immediately or never. The configuration rejects it rather than letting a setTimeout decide.
pub const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;

// There is deliberately no `DEFAULT_MAX_INPUT_BYTES` beside this constant. Upstream requires every
// limit to be supplied by configuration, with no library defaults, so that a misconfiguration is a
// startup error instead of a silently tiny budget; [`Config`] carries the caller's numbers.

/// The provider id recorded with a title this module's strategy produced.
pub const FIRST_PROMPT_PROVIDER_ID: &str = "session-title-first-prompt-llm";

/// Why a title request, or its reply, was rejected.
///
/// One enum rather than a string, because the callers downstream genuinely branch: a tool-call
/// finish means the model misbehaved and the fallback must stand in, while a configuration error
/// means the plugin is broken and nothing should be written at all. Upstream throws plain `Error`s
/// with a message and lets the boundary stringify them; the messages below keep upstream's exact
/// wording so a log from either build reads the same, but the variant is what a Rust caller matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The plugin configuration is missing, unknown-keyed, or out of range.
    Configuration(String),
    /// The prepared input exceeds the byte budget of the request.
    InputTooLong {
        /// Encoded size of the input, in bytes.
        bytes: usize,
        /// The configured `maxInputBytes`.
        limit: usize,
    },
    /// A request must be attributed to at least one source message.
    NoSourceMessages,
    /// The chosen source message seqs are empty, out of order, repeated, or not in the request.
    InvalidMessageSeqs,
    /// The model asked for a tool instead of answering with text.
    ToolCalls,
    /// The model ran into its output-token cap, so the reply is truncated.
    MaxTokens,
    /// The model finished with an operational failure or was aborted.
    Operational(String),
    /// The reply contained no text block at all, or only whitespace.
    NoText,
    /// The text survived parsing but not normalization, so there is no visible title.
    EmptyTitle,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(message) => write!(formatter, "session-title-llm: {message}"),
            Self::InputTooLong { bytes, limit } => write!(
                formatter,
                "session-title-llm: input is {bytes} bytes, exceeding maxInputBytes {limit}"
            ),
            Self::NoSourceMessages => write!(
                formatter,
                "session-title-llm: at least one source message is required"
            ),
            Self::InvalidMessageSeqs => write!(
                formatter,
                "session-title-llm: messageSeqs must be unique, ordered seqs from the request"
            ),
            Self::ToolCalls => write!(
                formatter,
                "session-title-llm: title output must contain text only"
            ),
            Self::MaxTokens => write!(
                formatter,
                "session-title-llm: title output reached maxOutputTokens"
            ),
            Self::Operational(message) => write!(formatter, "{message}"),
            Self::NoText => write!(formatter, "session-title-llm: title model produced no text"),
            Self::EmptyTitle => write!(formatter, "session-title provider returned an empty title"),
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

/// The execution controls every model-backed title request needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum UTF-8 bytes in the provider-prepared user input.
    pub max_input_bytes: usize,
    /// Output-token cap for the title request, independent of conversation requests.
    ///
    /// Separate because a title is three words: inheriting the conversation's cap would let a
    /// runaway title request bill like a turn, and reusing the conversation's *value* would starve
    /// the reply on a route configured for long answers.
    pub max_output_tokens: u64,
    /// End-to-end deadline for the auxiliary request, in milliseconds.
    pub timeout_ms: u64,
}

/// This provider's title-length targets, in the two units a length instruction needs.
///
/// Two numbers because "about eight words" is wrong advice for Chinese: a CJK title of eight
/// *characters* is a phrase, and eight words would be a paragraph. Upstream states both and lets the
/// model pick the one matching the language of the messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Targets {
    /// Target word count for non-CJK titles.
    pub target_words: u64,
    /// Target character count for Chinese, Japanese, or Korean titles.
    pub target_cjk_characters: u64,
}

/// One validated provider policy: execution controls, title targets, and the optional fixed route.
///
/// The route is optional and paired: `provider` and `model` travel together or not at all, because
/// half a route is not a route. When both are absent the request reuses the route the session's own
/// last request logged, which is what makes titles cost no extra configuration in the common case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Byte, token, and time bounds.
    pub limits: Limits,
    /// Language-aware title-length targets.
    pub targets: Targets,
    /// Explicit LLM provider route, when the title call should not follow the session's.
    pub provider: Option<String>,
    /// Explicit model id, paired with [`Config::provider`].
    pub model: Option<String>,
}

/// Every key a title provider's configuration may carry.
///
/// A closed list because the alternative — accepting anything and ignoring what is not understood —
/// turns a misspelled `maxInputByte` into an unbounded request that fails at the provider. Upstream
/// rejects unknown keys for the same reason and with the same words.
const CONFIG_KEYS: [&str; 7] = [
    "maxInputBytes",
    "maxOutputTokens",
    "timeoutMs",
    "provider",
    "model",
    "targetWords",
    "targetCjkCharacters",
];

/// Read one required positive integer from the configuration object.
///
/// Fractions and negative numbers are rejected rather than floored: a `timeoutMs` of `0.5` is a
/// caller mistake, and silently rounding it to zero would arm an already-expired deadline.
fn required_positive_integer(object: &Map<String, Value>, key: &str) -> Result<u64, Error> {
    match object.get(key) {
        Some(value) => match value.as_u64() {
            Some(number) if number > 0 => Ok(number),
            _ => Err(Error::Configuration(format!(
                "{key} must be a positive integer"
            ))),
        },
        None => Err(Error::Configuration(format!("{key} is required"))),
    }
}

/// Read one optional non-empty string.
fn optional_non_empty_string(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, Error> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(text.clone())),
        Some(_) => Err(Error::Configuration(format!(
            "{key} must be a non-empty string"
        ))),
    }
}

impl Config {
    /// Validates one untrusted configuration object.
    ///
    /// The object is the plugin's own configuration, so it is JSON rather than a typed struct: a
    /// loader hands over whatever the user wrote, and the point of validation is to reject what the
    /// loader could not.
    pub fn from_json(value: &Value) -> Result<Self, Error> {
        let Value::Object(object) = value else {
            return Err(Error::Configuration("configuration is required".to_owned()));
        };
        for key in object.keys() {
            if !CONFIG_KEYS.contains(&key.as_str()) {
                return Err(Error::Configuration(format!(
                    "unknown config key \"{key}\""
                )));
            }
        }
        let max_input_bytes = required_positive_integer(object, "maxInputBytes")?;
        let max_output_tokens = required_positive_integer(object, "maxOutputTokens")?;
        let timeout_ms = required_positive_integer(object, "timeoutMs")?;
        if timeout_ms > MAX_TIMER_DELAY_MS {
            return Err(Error::Configuration(format!(
                "timeoutMs must not exceed {MAX_TIMER_DELAY_MS}"
            )));
        }
        let provider = optional_non_empty_string(object, "provider")?;
        let model = optional_non_empty_string(object, "model")?;
        if provider.is_some() != model.is_some() {
            return Err(Error::Configuration(
                "provider and model must be supplied together".to_owned(),
            ));
        }
        Ok(Self {
            limits: Limits {
                max_input_bytes: usize::try_from(max_input_bytes).map_err(|_| {
                    Error::Configuration("maxInputBytes is too large for this platform".to_owned())
                })?,
                max_output_tokens,
                timeout_ms,
            },
            targets: Targets {
                target_words: required_positive_integer(object, "targetWords")?,
                target_cjk_characters: required_positive_integer(object, "targetCjkCharacters")?,
            },
            provider,
            model,
        })
    }

    /// The system instruction for a title derived from human messages.
    ///
    /// Five sentences, each carrying weight, and both of the middle ones exist because of a failure
    /// seen in practice:
    ///
    /// - "Return only the title on one line" is load-bearing but insufficient, so the next clause
    ///   enumerates the wrappers models reach for anyway. Upstream still parses defensively, because
    ///   an instruction is a request and not a constraint.
    /// - "Use the language of the messages" without the paired *length* instruction produces the
    ///   wrong length for one of the two scripts, which is why [`Targets`] carries both numbers.
    /// - "No code is allowed" matters for a coding assistant specifically: without it a session that
    ///   opens with a stack trace gets a title that is a fragment of the stack trace.
    /// - The last sentence is the anti-explanation clause. Given no material, a model's instinct is
    ///   to explain why it cannot comply, and "The messages do not contain enough information to
    ///   create a title" is a worse title than "Greeting".
    pub fn system_prompt(&self) -> String {
        system_prompt(self.targets)
    }
}

/// Builds the system instruction for one set of title targets.
pub fn system_prompt(targets: Targets) -> String {
    [
        "Create a concise title for an AI coding-assistant session from the supplied human messages.",
        "Return only the title on one line, **in plain text of natural language**, with no quotes, prefix, explanation, Markdown, XML, or terminal control codes. No code is allowed.",
        "Use the language of the messages.",
        &format!(
            "Aim for about {} words in non-CJK languages or {} CJK characters.",
            targets.target_words, targets.target_cjk_characters
        ),
        "If the messages give little to name, still return a short best-effort title, such as Greeting, instead of explaining.",
    ]
    .join("\n")
}

// ---------------------------------------------------------------------------------------------
// Input: which messages, and how they are framed
// ---------------------------------------------------------------------------------------------

/// One eligible human message, with the seq a title is attributed to.
///
/// The text is the *raw* concatenation, not a normalized form: the provider decides how to frame
/// it, and normalizing here would hide from the model the structure — a pasted stack trace, an
/// indented snippet — that often carries the subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserMessage {
    /// One-based position of the message's event in the session log.
    pub seq: u64,
    /// Exact concatenated text-block content.
    pub text: String,
}

impl UserMessage {
    /// The message as the framed-input JSON represents it.
    ///
    /// Key order is fixed so the framed input — and therefore the byte count [`check_input`]
    /// measures — is stable for the same conversation. A model-facing request that changes shape
    /// between identical sessions defeats request caching for no benefit.
    fn to_json(&self) -> Value {
        json!({ "seq": self.seq, "text": self.text })
    }
}

/// Extracts the eligible human messages from a session log, in seq order.
///
/// Three filters, each of which upstream applies:
///
/// - the event is a `user/message`, so an assistant reply or a tool result never becomes a title;
/// - its source is the *person*, not a synthetic injection, because a context snapshot injected at
///   the top of a session is not what the session is about;
/// - its text is non-empty after cleaning, because a blank message must not become the title and
///   must not shadow the first real prompt behind it.
///
/// `through_seq` is an inclusive upper bound. It exists because a title request is attributed to a
/// watermark: a message that arrived after the request was scheduled must not leak into the reply's
/// attribution, or the recorded `messageSeqs` would claim evidence the model never saw.
pub fn collect_user_messages(
    events: &[SessionEvent],
    through_seq: Option<u64>,
) -> Vec<UserMessage> {
    let mut messages = Vec::new();
    for (index, event) in events.iter().enumerate() {
        // Seqs are one-based, positional, and gapless in this crate: an event's seq is its index
        // plus one. Session events carry no seq of their own, which is what keeps a re-serialized
        // log byte-identical to the one that was written.
        let seq = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        if let Some(bound) = through_seq {
            if seq > bound {
                break;
            }
        }
        let Some(text) = eligible_text(event) else {
            continue;
        };
        messages.push(UserMessage { seq, text });
    }
    messages
}

/// The concatenated text of one event, when that event is a person's message worth titling from.
fn eligible_text(event: &SessionEvent) -> Option<String> {
    let SessionEvent::UserMessage { message } = event else {
        return None;
    };
    if message.role != ChatRole::User {
        return None;
    }
    if !matches!(message.source, MessageSource::User { .. }) {
        return None;
    }
    let mut text = String::new();
    for block in &message.content {
        if let ContentBlock::Text { text: block_text } = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(block_text);
        }
    }
    // Cleaning is the emptiness test on purpose: a message of control characters and spaces reads
    // as blank to a person and would render as a blank title, so it is not eligible input.
    if clean_title_text(&text).is_empty() {
        return None;
    }
    Some(text)
}

/// Frames the selected messages as a JSON array under one instruction line.
///
/// JSON rather than, say, a `<messages>` wrapper, and the reason is deliberate: the payload is
/// untrusted user text, and every hand-rolled delimiter can be closed by that text. A prompt
/// containing `</messages>` — which happens, since people paste XML and HTML into a coding session —
/// would escape a tag-based frame and land in the instruction plane. JSON's escaping rules are
/// unambiguous, so `"` and newline in the payload stay *inside* one string value no matter what
/// the payload says.
///
/// The leading line is part of the frame rather than part of the system prompt so it sits next to
/// the data it describes; a model is likelier to apply an instruction adjacent to its object.
pub fn frame_messages(messages: &[UserMessage]) -> String {
    let payload: Vec<Value> = messages.iter().map(UserMessage::to_json).collect();
    let rendered = Value::Array(payload).to_string();
    format!("Generate the session title from this JSON array of human messages:\n{rendered}")
}

/// Rejects an input over the request's byte budget.
///
/// Measured in UTF-8 bytes because that is what the provider bills and limits; a character count
/// would under-report a CJK or emoji-heavy prompt by a factor of three or four and let a request
/// through that the provider then rejects after a round trip. Rejecting locally keeps the failure
/// explainable: the caller learns the exact size against the exact limit instead of receiving an
/// opaque HTTP status.
pub fn check_input(input: &str, max_input_bytes: usize) -> Result<(), Error> {
    let bytes = input.len();
    if bytes > max_input_bytes {
        return Err(Error::InputTooLong {
            bytes,
            limit: max_input_bytes,
        });
    }
    Ok(())
}

/// Rejects a request that names no source message, or that normalizes to no message at all.
///
/// Attribution is not optional. A title event whose `messageSeqs` is empty says "this title came
/// from nowhere", and a reader — or a later re-derivation — cannot tell whether it is stale. The
/// check runs before dispatch so a provider can never be blamed for text it did not send.
pub fn require_sources(messages: &[UserMessage]) -> Result<(), Error> {
    if messages.is_empty() {
        return Err(Error::NoSourceMessages);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// When to run
// ---------------------------------------------------------------------------------------------

/// The cadence a provider asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Automatic {
    /// Title once, from the first human prompt, and never again.
    ///
    /// Cheap and stable: the title does not move under the reader as the conversation grows.
    FirstPrompt,
    /// Re-title after every human prompt.
    ///
    /// Worth it for a conversation whose opening was a fragment, at the cost of a request per
    /// message and a title that can change mid-conversation.
    AllPrompts,
}

/// Facts about the session that decide whether a first-prompt provider may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Envelope {
    /// Whether the session log was seeded with material that predates the person's own typing.
    pub is_seeded: bool,
    /// Whether the session is a subagent's, rather than a session a client opened.
    pub is_subagent: bool,
}

/// What produced the standing title, when one stands.
///
/// The variants are not a taxonomy for its own sake: the *ownership* of a title decides whether it
/// may be replaced, and the three owners have three different answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleOwner {
    /// A person renamed the session. The title is pinned: nothing automatic may overwrite it.
    User,
    /// The deterministic fallback produced it. A model title may replace it, and should.
    Fallback,
    /// A provider produced it. A later provider revision may replace it under its own cadence.
    Provider,
}

/// The title state a scheduling decision reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TitleState {
    /// Total eligible human messages folded so far, including the one just appended.
    pub count: usize,
    /// The owner of the standing title, or `None` before any title is accepted.
    pub owner: Option<TitleOwner>,
}

/// What to do for one newly appended eligible human message.
///
/// Three outcomes rather than a bool, because "do nothing" and "write the fallback" are different
/// jobs and upstream performs them on different paths: the fallback is synchronous and deduplicated
/// per session, while provider work is async, cancellable, and revision-checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// Nothing to do: either the title is pinned by a rename, or it already exists and this
    /// message is not a trigger.
    Ignore,
    /// Ensure the deterministic fallback stands, and do not call a model.
    ///
    /// This is the state of every session on a build with no provider registered: the title is
    /// honest and shallow, which beats unnamed.
    FallbackOnly,
    /// Schedule provider work; the fallback still runs underneath it, so a failed request leaves a
    /// title rather than nothing.
    Provider,
}

/// Decides what a newly appended eligible human message triggers.
///
/// The rules, each with the reason it is not gratuitous:
///
/// - **A user rename pins the title** and returns [`Schedule::Ignore`] before anything else. This is
///   the one rule a reader may find surprising, so it is checked first: a person who names a
///   conversation has answered the question the whole subsystem asks, and overwriting that answer
///   with a model's guess is the subsystem arguing with its user.
/// - **`AllPrompts` always re-titles**, and only an existing user title stops it.
/// - **`FirstPrompt` runs exactly once**: at the first eligible message, when no *provider* title
///   stands, and only in a session that a client opened. A standing fallback does not block it — the
///   fallback is the placeholder a model title is meant to replace — while a provider title does,
///   because running twice would contradict the cadence's own promise. Seeded sessions already had
///   their first message chosen by the seeding, and a subagent's title is never displayed in a list,
///   so both would pay for a request nobody reads.
/// - **The fallback runs whenever no title stands**, provider or not, because the fallback is what
///   a session shows between its first message and its first model answer.
pub fn schedule(
    mode: Automatic,
    envelope: Envelope,
    state: TitleState,
    provider_registered: bool,
) -> Schedule {
    if state.owner == Some(TitleOwner::User) {
        return Schedule::Ignore;
    }
    // "No provider title stands": a fallback is a placeholder, so a due model title replaces it. A
    // provider title is final under `FirstPrompt`, which is the cadence's whole promise.
    let first_prompt_due = !envelope.is_seeded
        && !envelope.is_subagent
        && state.count == 1
        && state.owner != Some(TitleOwner::Provider);
    let due = match mode {
        Automatic::AllPrompts => true,
        Automatic::FirstPrompt => first_prompt_due,
    };
    if provider_registered && due {
        return Schedule::Provider;
    }
    if state.owner.is_none() {
        return Schedule::FallbackOnly;
    }
    Schedule::Ignore
}

/// Whether a pending provider request has the route it needs.
///
/// A title call is not dispatched on the strength of the session's *configuration*; it waits for the
/// session's own next request to log a header, and then copies that exact provider/model pair. Two
/// reasons, and upstream states both: the auxiliary call must travel the route the conversation is
/// actually using (a configured route can be stale, or overridden per session), and the title request
/// is recorded *after* that header, so the log explains which route produced the title without the
/// reader correlating two events by timestamp.
///
/// `pending_through_seq >= header_seq` means the header predates the message that scheduled the work,
/// so the route it names may already have been superseded; the work waits for a later header.
pub fn route_ready(pending_through_seq: u64, header_seq: u64) -> bool {
    header_seq > pending_through_seq
}

// ---------------------------------------------------------------------------------------------
// Output interpretation
// ---------------------------------------------------------------------------------------------

/// The block kinds a title reply is read as.
///
/// Collapsed from the wire's five into three because a title only distinguishes them by one
/// question: may this reply be read as text, or does its presence mean the model did something else?
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    /// Visible text. The only kind that contributes to the title.
    Text,
    /// A tool invocation. Its mere presence rejects the reply.
    ToolCall,
    /// Reasoning, image, or file content: ignored, and neither accepted nor rejected.
    Other,
}

/// The terminal finish a title reply is judged by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finish {
    /// A normal stop. The only finish whose text is trusted.
    Stop,
    /// The model asked for tools instead of answering.
    ToolCalls,
    /// The reply hit the output-token cap, so it is cut mid-sentence.
    Length,
    /// The caller or the deadline cancelled the request.
    Aborted,
    /// The provider failed.
    Failed(String),
}

/// One assembled auxiliary reply, reduced to what title interpretation needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// Block kinds in stream order; text carried alongside.
    pub blocks: Vec<(Block, Option<String>)>,
    /// How the stream ended.
    pub finish: Finish,
}

impl Response {
    /// Reduces a crate-level content list and finish reason to a title reply.
    ///
    /// `FinishReason::Length` maps to [`Finish::Length`]: upstream calls the same condition
    /// `max-tokens`, and the distinction matters because a length-cut title is not a short title, it
    /// is an unknown title — the visible prefix of a sentence that continues.
    pub fn from_content(content: &[ContentBlock], finish: &FinishReason) -> Self {
        let blocks = content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => (Block::Text, Some(text.clone())),
                ContentBlock::ToolCall { .. } => (Block::ToolCall, None),
                ContentBlock::Reasoning { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::File { .. } => (Block::Other, None),
            })
            .collect();
        let finish = match finish {
            FinishReason::Stop => Finish::Stop,
            FinishReason::ToolCalls => Finish::ToolCalls,
            FinishReason::Length => Finish::Length,
            FinishReason::Aborted => Finish::Aborted,
            FinishReason::Error { message } => Finish::Failed(message.clone()),
        };
        Self { blocks, finish }
    }

    /// The reply's visible text, with blocks joined by a space.
    ///
    /// A space rather than nothing, because a model that splits one title across two text blocks is
    /// answering in two fragments, not one word: joining bare would weld them together.
    fn text(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for (kind, text) in &self.blocks {
            if *kind == Block::Text {
                if let Some(text) = text.as_deref() {
                    parts.push(text);
                }
            }
        }
        parts.join(" ")
    }
}

/// Lifts one title out of a reply, or explains why the reply cannot yield one.
///
/// The order of the checks is the order of trust: an operational failure and a cancelled request are
/// reported before the content is even looked at (a cancelled stream's partial text is not an
/// answer), then the terminal finish, then the block kinds, then the text.
pub fn title_from_response(response: &Response, max_bytes: usize) -> Result<String, Error> {
    match &response.finish {
        Finish::Stop => {}
        Finish::ToolCalls => return Err(Error::ToolCalls),
        Finish::Length => return Err(Error::MaxTokens),
        Finish::Aborted => {
            return Err(Error::Operational(
                "session-title-llm: title request was aborted".to_owned(),
            ));
        }
        Finish::Failed(message) => return Err(Error::Operational(message.clone())),
    }
    if response
        .blocks
        .iter()
        .any(|(kind, _)| *kind == Block::ToolCall)
    {
        return Err(Error::ToolCalls);
    }
    let title = normalize_title(&title_from_output(&response.text()), max_bytes);
    if title.is_empty() {
        return Err(Error::EmptyTitle);
    }
    Ok(title)
}

/// Takes the title from model output: the first non-empty line, without whole-line emphasis.
///
/// Models that disregard "one line" reliably put the title first and the commentary after it, so the
/// first non-blank line is the title and the rest is dropped rather than concatenated — joining them
/// would produce a long line of the model talking about the title.
///
/// The asterisk rule exists because `**Title**` is what Markdown muscle memory produces, and a
/// rendered title of `**Fix the retry loop**` looks like a formatting bug in the client rather than
/// a title. The rule is deliberately narrow: only emphasis that wraps the *whole* line is removed,
/// and only when the inner text does not itself contain the marker. A line that is entirely
/// asterisks, or that uses emphasis inside a sentence, keeps its characters, because there the
/// markers are the content and stripping them would edit the model's answer.
pub fn title_from_output(text: &str) -> String {
    let line = text
        .split(['\r', '\n'])
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    emphasize::unwrap(line)
}

/// Unwrapping of whole-line asterisk emphasis, kept in one place so the rule can be read alone.
mod emphasize {
    /// The longest marker run that counts as emphasis.
    const MAX_MARKER: usize = 3;

    /// Returns the line without a wrapping `*`, `**`, or `***` pair, when one wraps all of it.
    pub(super) fn unwrap(line: &str) -> String {
        // Longest marker first, matching a greedy quantifier: `***a***` is triple emphasis, not a
        // one-character marker around `**a**`, and the visual result differs.
        for marker_len in (1..=MAX_MARKER).rev() {
            let Some(marker) = marker_of(line, marker_len) else {
                continue;
            };
            let inner = &line[marker_len..line.len() - marker_len];
            if inner.is_empty() {
                continue;
            }
            // The inner text must itself start and end with non-whitespace, or `* a *` — emphasis
            // with padding — would lose its markers while `a` keeps its surrounding spaces as a
            // title.
            let starts_tight = !inner.starts_with(char::is_whitespace);
            let ends_tight = !inner.ends_with(char::is_whitespace);
            if !starts_tight || !ends_tight {
                continue;
            }
            // If the inner text still contains the marker, the pair did not wrap it: the model was
            // writing asterisks, not emphasis, and removing two of them would be an edit.
            if inner.contains(marker) {
                continue;
            }
            return inner.to_owned();
        }
        line.to_owned()
    }

    /// The marker string when `line` begins and ends with `marker_len` asterisks.
    fn marker_of(line: &str, marker_len: usize) -> Option<&'static str> {
        let marker = match marker_len {
            1 => "*",
            2 => "**",
            _ => "***",
        };
        // `len` check first: `"***"` starts with `"***"` and ends with it, but has no inner text,
        // and a later subtraction on a shorter line would underflow.
        if line.len() < marker.len() * 2 {
            return None;
        }
        if line.starts_with(marker) && line.ends_with(marker) {
            return Some(marker);
        }
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------------------------

/// Removes escape sequences, control characters, and invisible directionals, then collapses
/// whitespace to one trimmed line.
///
/// Ported from upstream's `cleanTitleText`, and each pass is a distinct hazard rather than one
/// general scrub:
///
/// - **OSC** (`ESC ]` … `BEL` or `ESC \`) sets a terminal's window title. A model that copies it into
///   a title would retitle the user's terminal, which is a real effect from a text field.
/// - **CSI** (`ESC [` … final) rewrites colour and cursor position: a title carrying
///   `\u{1b}[2K` erases the line it is printed on.
/// - **two-byte ESC** sequences are the remainder, and are dropped for the same reason.
/// - **control characters** (`\u{0}` and friends) are invisible or line-mangling: `pass\u{0}word`
///   and `password` are different strings that read alike.
/// - **directionals and zero-widths** reorder or hide text: `\u{202e}` makes the rest of a line read
///   backwards, so a title can be induced to display something other than what it says.
///
/// Whitespace collapsing is last and is not cosmetic: a title rendered with a newline or a run of
/// spaces breaks the alignment of every row around it in a session list.
pub fn clean_title_text(input: &str) -> String {
    let text = strip_osc(input);
    let text = strip_csi(&text);
    let text = strip_esc(&text);
    let text: String = text
        .chars()
        .filter(|character| !is_control(*character))
        .collect();
    let text: String = text
        .chars()
        .filter(|character| !is_directional(*character))
        .collect();
    let mut collapsed = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            pending_space = !collapsed.is_empty();
            continue;
        }
        if pending_space {
            collapsed.push(' ');
            pending_space = false;
        }
        collapsed.push(character);
    }
    collapsed
}

/// Removes operating-system-command sequences, including an unterminated tail.
///
/// An unterminated sequence runs to the end of the string and takes the rest of the title with it.
/// That is the right answer: the alternative is keeping both the terminator-less instruction and the
/// text it was about to overwrite.
fn strip_osc(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        // Two introducers: `ESC ]` and the single-character C1 form. A lone `ESC` that does not
        // introduce an OSC belongs to the other passes and is emitted here untouched.
        let after_introducer = if character == '\u{1b}' && chars.get(index + 1) == Some(&']') {
            index + 2
        } else if character == '\u{9d}' {
            index + 1
        } else {
            output.push(character);
            index += 1;
            continue;
        };
        // Consume to BEL or `ESC \`. Reaching the end of the string is the unterminated case, and
        // dropping the remainder is deliberate.
        let mut cursor = after_introducer;
        loop {
            match chars.get(cursor) {
                None => break,
                Some('\u{7}') => {
                    cursor += 1;
                    break;
                }
                Some('\u{1b}') if chars.get(cursor + 1) == Some(&'\\') => {
                    cursor += 2;
                    break;
                }
                Some(_) => cursor += 1,
            }
        }
        index = cursor;
    }
    output
}

/// Removes control-sequence-introducer escapes such as SGR colour codes.
///
/// A CSI sequence is `ESC [` followed by parameters, intermediates, and one final byte. Without a
/// final byte the sequence is not a CSI sequence at all, so an unterminated one is left for the
/// two-byte pass — matching upstream's three sequential regex passes rather than a bespoke scanner
/// that would disagree on that tail.
fn strip_csi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        let is_introducer = character == '\u{1b}' || character == '\u{9b}';
        if !is_introducer {
            output.push(character);
            index += 1;
            continue;
        }
        let mut cursor = index + 1;
        if character == '\u{1b}' {
            if chars.get(cursor) != Some(&'[') {
                output.push(character);
                index += 1;
                continue;
            }
            cursor += 1;
        }
        while matches!(chars.get(cursor), Some('0'..='?')) {
            cursor += 1;
        }
        while matches!(chars.get(cursor), Some(' '..='/')) {
            cursor += 1;
        }
        if matches!(chars.get(cursor), Some('@'..='~')) {
            index = cursor + 1;
            continue;
        }
        // No final byte: not a CSI sequence. Emit the introducer and rescan.
        output.push(character);
        index += 1;
    }
    output
}

/// Removes the remaining two-byte escape sequences, `ESC` plus one byte in `@` through `_`.
fn strip_esc(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            if let Some(&next) = chars.peek() {
                if ('@'..='_').contains(&next) {
                    chars.next();
                    continue;
                }
            }
            output.push(character);
            continue;
        }
        output.push(character);
    }
    output
}

/// Whether a character is a C0 or C1 control that is not whitespace.
///
/// Tab, newline, and carriage return are excluded here because the whitespace pass turns them into
/// the single separating space they are; removing them instead would weld two words together.
fn is_control(character: char) -> bool {
    let code = u32::from(character);
    matches!(code, 0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F | 0x7F..=0x9F)
}

/// Whether a character is an invisible directional or width-zero control.
fn is_directional(character: char) -> bool {
    matches!(
        u32::from(character),
        0x200B
            | 0x200E
            | 0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
    )
}

/// Truncates to a UTF-8 byte budget without splitting a code point.
///
/// Byte-exact because the durable title field is byte-counted: a cut through a three-byte CJK
/// character produces a replacement character, which is mojibake in the one string a user reads for
/// every session. Iterating `char` and comparing `len_utf8` keeps every unit whole and lands at or
/// under the budget.
pub fn truncate_title_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_owned();
    }
    let mut output = String::new();
    for character in input.chars() {
        if output.len() + character.len_utf8() > max_bytes {
            break;
        }
        output.push(character);
    }
    output
}

/// Normalizes one accepted title and enforces its byte budget.
///
/// A zero budget yields the empty string rather than an error. There is exactly one answer to "how
/// much text fits in zero bytes", so a `Result` would only move the same decision to every caller;
/// the empty result is what [`title_from_response`] turns into a rejection, which is the behavior
/// that matters.
pub fn normalize_title(input: &str, max_bytes: usize) -> String {
    let cleaned = clean_title_text(input);
    let truncated = truncate_title_utf8(&cleaned, max_bytes);
    truncated.trim_end().to_owned()
}

// ---------------------------------------------------------------------------------------------
// Fallback
// ---------------------------------------------------------------------------------------------

/// The deterministic fallback's limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FallbackLimits {
    /// Maximum whitespace-delimited words.
    pub max_words: usize,
    /// Maximum UTF-8 bytes.
    pub max_bytes: usize,
}

/// Derives the deterministic title from the first eligible message.
///
/// This is the provider-failure answer, and it is the same function the no-provider build uses, so a
/// failed request degrades to exactly the title the session would have had without a provider at
/// all. That equivalence is the point: a failure must not be a *worse* outcome than not trying.
///
/// Returns the empty string when nothing visible survives the limits; the caller decides whether
/// that means "no title" ([`accept_or_fallback`] does).
pub fn fallback_title(input: &str, limits: FallbackLimits) -> String {
    if limits.max_words == 0 {
        return String::new();
    }
    let cleaned = clean_title_text(input);
    let words: Vec<&str> = cleaned
        .split(' ')
        .filter(|word| !word.is_empty())
        .take(limits.max_words)
        .collect();
    truncate_title_utf8(&words.join(" "), limits.max_bytes)
        .trim_end()
        .to_owned()
}

/// What produced an accepted title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleSource {
    /// A model named the session.
    Provider,
    /// The deterministic first-prompt fallback stood in.
    Fallback,
}

/// A title the service may write, with its provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    /// Normalized, non-empty title text.
    pub title: String,
    /// Who produced it.
    pub source: TitleSource,
}

/// Turns one provider attempt into a title the service may accept, falling back on failure.
///
/// `generated` is the whole outcome of the request, not just its text: cancellation, a provider
/// error, a tool-call finish, and an empty reply all arrive here as `Err`, and all four have the same
/// correct answer — keep the fallback. The distinction between them is for logging, which is why the
/// original error is returned unchanged when the fallback cannot produce anything either.
///
/// The fallback is derived from the *first* message rather than the newest, matching what a
/// no-provider build would have written. A fallback that tracked the newest message would be a
/// second, worse provider.
pub fn accept_or_fallback(
    generated: Result<String, Error>,
    first: Option<&UserMessage>,
    limits: FallbackLimits,
) -> Result<Accepted, Error> {
    let failure = match generated {
        Ok(title) if !title.is_empty() => {
            return Ok(Accepted {
                title,
                source: TitleSource::Provider,
            });
        }
        // An empty successful reply is a failure, not a title: a provider that returns "" has
        // nothing to say, and accepting it would erase the fallback that already stands.
        Ok(_) => Error::EmptyTitle,
        Err(error) => error,
    };
    let fallback = first
        .map(|message| fallback_title(&message.text, limits))
        .unwrap_or_default();
    if fallback.is_empty() {
        return Err(failure);
    }
    Ok(Accepted {
        title: fallback,
        source: TitleSource::Fallback,
    })
}

// ---------------------------------------------------------------------------------------------
// The request record
// ---------------------------------------------------------------------------------------------

/// The exact auxiliary request, recorded before dispatch.
///
/// Upstream appends this as a log-only `session/title-llm-request` event, and the reason is
/// accountability: a title is model output, and "why is this session named that" is answerable only
/// if the exact prompt, route, and token cap are in the log. It is written *before* the stream, so a
/// request that hangs still leaves a record of what was asked.
///
/// Nothing in this build dispatches, so the record is built and returned by the caller that will. The
/// JSON shape matches upstream's event data, with the message list reduced to the one user turn the
/// execution module always sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRecord {
    /// Provider id responsible for the request.
    pub title_provider: String,
    /// The exact request that provider prepared.
    pub prepared: Prepared,
}

impl RequestRecord {
    /// Wraps one prepared request, refusing an unattributed one.
    ///
    /// Attribution is checked here rather than in [`Prepared`] because this is the last point before
    /// a record exists: a request with no source message would produce a title event claiming it
    /// came from nowhere, and a later reader could not tell whether it is stale.
    pub fn new(title_provider: impl Into<String>, prepared: Prepared) -> Result<Self, Error> {
        if prepared.message_seqs.is_empty() {
            return Err(Error::NoSourceMessages);
        }
        Ok(Self {
            title_provider: title_provider.into(),
            prepared,
        })
    }

    /// The record as the log event's data.
    pub fn to_json(&self) -> Value {
        let message = json!({
            "role": "user",
            "content": [{ "type": "text", "text": self.prepared.input }],
            "source": { "kind": "dsh-session-title-llm" },
        });
        let mut record = json!({
            "titleProvider": self.title_provider,
            "messageSeqs": self.prepared.message_seqs,
            "route": {
                "provider": self.prepared.route.provider,
                "model": self.prepared.route.model,
            },
            "system": self.prepared.system,
            "messages": [message],
            "maxTokens": self.prepared.max_tokens,
        });
        if let (Some(effort), Value::Object(object)) =
            (&self.prepared.reasoning_effort, &mut record)
        {
            object.insert("reasoningEffort".to_owned(), Value::String(effort.clone()));
        }
        record
    }
}

/// An LLM route: the provider and the model on it.
///
/// One type rather than two `String` fields travelling separately, because half a route is not a
/// route: a constructor that takes the pair makes the invalid state unrepresentable, which is what
/// upstream's `provider`/`model` pairing check protects at runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// Registered LLM provider.
    pub provider: String,
    /// Provider model id.
    pub model: String,
}

/// The provider-prepared parts of one title request.
///
/// Upstream splits these from the execution controls deliberately: the provider owns the system
/// prompt, the input, and the source attribution, while the execution module owns the route, the
/// token cap, and the deadline. Keeping that split means a future dispatcher cannot accidentally
/// build a request whose prompt and bounds came from different providers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    /// Exact system prompt.
    pub system: String,
    /// Exact user input, framing included.
    pub input: String,
    /// Exact human message seqs represented by `input`, in log order.
    pub message_seqs: Vec<u64>,
    /// The route this request travels.
    pub route: Route,
    /// Output-token cap.
    pub max_tokens: u64,
    /// Resolved reasoning effort, when the route selected one.
    pub reasoning_effort: Option<String>,
}

impl Prepared {
    /// Prepares one request from the messages it is about.
    ///
    /// The source seqs are copied from the messages rather than supplied separately, so the
    /// attribution cannot disagree with the input it accompanies — the one inconsistency that would
    /// make the log actively misleading instead of merely incomplete.
    pub fn new(
        messages: &[UserMessage],
        route: Route,
        system: impl Into<String>,
        input: impl Into<String>,
        max_tokens: u64,
    ) -> Result<Self, Error> {
        require_sources(messages)?;
        Ok(Self {
            system: system.into(),
            input: input.into(),
            message_seqs: messages.iter().map(|message| message.seq).collect(),
            route,
            max_tokens,
            reasoning_effort: None,
        })
    }

    /// Records the effort the route resolved for this call.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = effort;
        self
    }
}

// ---------------------------------------------------------------------------------------------
// Route and attribution helpers
// ---------------------------------------------------------------------------------------------

/// Selects the least supported reasoning effort for a title call.
///
/// The *least* supported, not the most: a title is three words, and paying for the highest reasoning
/// tier to produce them would make the cheapest request in the system the most expensive. A route
/// that advertises no efforts yields `None`, which means "use the route's default" — the correct
/// answer, since there is no effort to ask for.
pub fn select_reasoning_effort(efforts: &[String]) -> Option<&str> {
    efforts.first().map(String::as_str)
}

/// Rejects a provider's chosen source seqs unless they are unique, ordered, and part of the request.
///
/// Three separate ways for attribution to lie, and all three matter to a reader of the log: a seq
/// that is not in the request claims evidence the model never saw, a repeated seq double-counts one
/// message, and an out-of-order list cannot be rendered as "messages 3 through 7".
pub fn validate_message_seqs(messages: &[UserMessage], seqs: &[u64]) -> Result<(), Error> {
    if seqs.is_empty() {
        return Err(Error::InvalidMessageSeqs);
    }
    let mut previous: Option<usize> = None;
    for seq in seqs {
        let Some(index) = messages.iter().position(|message| message.seq == *seq) else {
            return Err(Error::InvalidMessageSeqs);
        };
        if previous.is_some_and(|previous| index <= previous) {
            return Err(Error::InvalidMessageSeqs);
        }
        previous = Some(index);
    }
    Ok(())
}
