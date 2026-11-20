//! Sends one title request and writes the accepted title back onto the session log.
//!
//! [`crate::session::title_llm`] decides when a title is due and how a reply becomes a title.
//! This module is the call: the same provider the turn is already using, one short request, and a
//! `provider` title appended after the fallback. A failed request leaves the fallback in place.

use std::sync::Arc;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::llm::{ContentBlock, FinishReason, LlmProvider, LlmRequest, Message, Role, StreamChunk};
use crate::session::title_llm::{
    check_input, frame_messages, system_prompt, title_from_response, Response, Targets, UserMessage,
};
use crate::session::SessionEvent;
use crate::Session;

/// Length targets from the upstream first-prompt provider: about five words, or ten CJK characters.
const TARGETS: Targets = Targets {
    target_words: 5,
    target_cjk_characters: 10,
};

/// Byte cap on the framed user input, matching the upstream `maxInputBytes`.
const MAX_INPUT_BYTES: usize = 4096;

/// Output cap for the title call. A title is one line; a longer reply is rejected as truncated.
const MAX_OUTPUT_TOKENS: u32 = 256;

/// Durable title length, matching the fallback's eighty characters of budget in bytes.
const MAX_TITLE_BYTES: usize = 80;

/// The first human message, when a model title is still due.
///
/// A user rename and a title a provider already wrote are left alone. A second human message is
/// past the first-prompt cadence, so it is not retitled. A fallback title does not block the call:
/// it is the placeholder the model title replaces.
#[must_use]
pub fn opening_subject(events: &[SessionEvent]) -> Option<UserMessage> {
    if events.iter().any(|event| {
        matches!(
            event,
            SessionEvent::SessionTitle { source, .. } if source == "user" || source == "provider"
        )
    }) {
        return None;
    }
    let mut first = None;
    let mut count = 0usize;
    for (index, event) in events.iter().enumerate() {
        let SessionEvent::UserMessage { message } = event else {
            continue;
        };
        count += 1;
        if count == 1 {
            let text = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            let seq = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            first = Some(UserMessage { seq, text });
        }
    }
    if count == 1 {
        first.filter(|message| !message.text.trim().is_empty())
    } else {
        None
    }
}

/// Asks the provider for a title without holding the turn.
///
/// The fallback is already on the log. This task replaces it when the model answers, and leaves it
/// when the request fails, the input is too long, or a rename landed while the model was thinking.
pub fn spawn_provider_title(
    provider: Arc<dyn LlmProvider>,
    model: String,
    session: Arc<dyn Session>,
) {
    tokio::spawn(async move {
        if let Err(error) = write_provider_title(provider.as_ref(), &model, session.as_ref()).await
        {
            eprintln!("[title] {error}");
        }
    });
}

/// Performs one title request and appends the result.
///
/// # Errors
///
/// Returns the provider or session failure. The caller logs it and keeps the fallback title.
pub async fn write_provider_title(
    provider: &dyn LlmProvider,
    model: &str,
    session: &dyn Session,
) -> Result<(), String> {
    let events = session.replay().await.map_err(|error| error.to_string())?;
    let Some(message) = opening_subject(&events) else {
        return Ok(());
    };
    let framed = frame_messages(std::slice::from_ref(&message));
    check_input(&framed, MAX_INPUT_BYTES).map_err(|error| format!("{error:?}"))?;
    let request = LlmRequest::new(
        model,
        vec![Message::new(
            Role::User,
            vec![ContentBlock::Text { text: framed }],
        )],
    )
    .with_system_prompt(system_prompt(TARGETS))
    .with_max_tokens(MAX_OUTPUT_TOKENS);
    let mut stream = provider
        .stream(request, CancellationToken::new())
        .await
        .map_err(|error| error.to_string())?;
    let mut blocks = Vec::new();
    let mut text = String::new();
    let mut finish = FinishReason::Stop;
    while let Some(chunk) = stream.next().await {
        match chunk {
            StreamChunk::TextDelta { text: delta, .. } => text.push_str(&delta),
            StreamChunk::Finish { reason } => finish = reason,
            StreamChunk::BlockEnd { block, .. } => blocks.push(block),
            _ => {}
        }
    }
    if blocks.is_empty() && !text.is_empty() {
        blocks.push(ContentBlock::Text { text });
    }
    let response = Response::from_content(&blocks, &finish);
    let title = title_from_response(&response, MAX_TITLE_BYTES)
        .map_err(|error| format!("{error:?}"))?;
    let events = session.replay().await.map_err(|error| error.to_string())?;
    if events.iter().any(|event| {
        matches!(
            event,
            SessionEvent::SessionTitle { source, .. } if source == "user" || source == "provider"
        )
    }) {
        return Ok(());
    }
    session
        .append(SessionEvent::SessionTitle {
            title,
            message_seqs: vec![message.seq],
            source: "provider".to_owned(),
        })
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{opening_subject, SessionEvent};

    #[test]
    fn the_first_message_is_the_subject_while_only_a_fallback_stands() {
        let events = vec![
            SessionEvent::user_text("m1", "why does the release job fail"),
            SessionEvent::SessionTitle {
                title: "why does".to_owned(),
                message_seqs: vec![1],
                source: "fallback".to_owned(),
            },
        ];
        let subject = opening_subject(&events).expect("a fallback still leaves the title due");
        assert_eq!(subject.text, "why does the release job fail");
        assert_eq!(subject.seq, 1);
    }

    #[test]
    fn a_second_user_message_is_not_retitled() {
        let events = vec![
            SessionEvent::user_text("m1", "why does the release job fail"),
            SessionEvent::SessionTitle {
                title: "why does".to_owned(),
                message_seqs: vec![1],
                source: "fallback".to_owned(),
            },
            SessionEvent::user_text("m2", "and the second retry"),
        ];
        assert!(opening_subject(&events).is_none());
    }

    #[test]
    fn a_provider_title_is_left_alone() {
        let events = vec![
            SessionEvent::user_text("m1", "why does the release job fail"),
            SessionEvent::SessionTitle {
                title: "release job retry".to_owned(),
                message_seqs: vec![1],
                source: "provider".to_owned(),
            },
        ];
        assert!(opening_subject(&events).is_none());
    }
}
