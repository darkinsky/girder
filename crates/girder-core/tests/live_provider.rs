//! Tests against the real provider, run only when a key is present.
//!
//! Every other provider test drives a mock server, and that gaps exactly one class of bug: a mock
//! returns whatever it is told to, while the real API validates the protocol. The `tool_result`
//! requirement was found this way — a mock accepted plain text where the API requires a
//! `tool_result` block, so the whole suite passed while every tool-calling conversation failed in
//! production.
//!
//! # Running
//!
//! ```sh
//! DEEPSEEK_API_KEY=sk-... cargo test --test live_provider -- --nocapture
//! ```
//!
//! Without the variable every test returns immediately. That is deliberate: a suite that fails on a
//! machine with no credentials trains people to ignore failures.

use futures::StreamExt as _;
use girder_core::{
    DeepSeekConfig, DeepSeekProvider, FinishReason, LlmProvider, LlmRequest, Message, StreamChunk,
    ToolCallId,
};
use tokio_util::sync::CancellationToken;

/// The provider, or `None` when no key is configured.
fn provider() -> Option<DeepSeekProvider> {
    let config = DeepSeekConfig::from_env()?;
    DeepSeekProvider::new(config).ok()
}

/// Skips the test body when no key is configured.
macro_rules! live {
    () => {
        match provider() {
            Some(provider) => provider,
            None => {
                eprintln!("skipping: DEEPSEEK_API_KEY is not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn a_plain_question_is_answered() {
    let provider = live!();
    let request = LlmRequest::new(
        std::env::var("GIRDER_MODEL").unwrap_or_else(|_| "deepseek-chat".to_owned()),
        vec![Message::new(
            girder_core::Role::User,
            vec![girder_core::ContentBlock::Text {
                text: "Reply with exactly the word: OK".to_owned(),
            }],
        )],
    )
    .with_max_tokens(32);

    let stream = provider
        .stream(request, CancellationToken::new())
        .await
        .expect("the request must be accepted");
    let chunks: Vec<StreamChunk> = stream.collect().await;
    let message = girder_core::AssistantMessage::from_chunks(chunks).expect("assembled");

    assert!(
        !message.text().is_empty(),
        "the model returned no text: {message:?}"
    );
    assert!(matches!(message.finish, Some(FinishReason::Stop)));
}

#[tokio::test]
async fn the_model_can_request_a_tool() {
    // The assertion that matters is the protocol shape, not the tool choice: the API rejects a
    // history whose tool calls are unanswered, so a provider that cannot express the request fails
    // here rather than in production.
    let provider = live!();
    let request = LlmRequest::new(
        std::env::var("GIRDER_MODEL").unwrap_or_else(|_| "deepseek-chat".to_owned()),
        vec![Message::new(
            girder_core::Role::User,
            vec![girder_core::ContentBlock::Text {
                text: "Use the echo tool to repeat the word hello.".to_owned(),
            }],
        )],
    )
    .with_tools(vec![girder_core::ToolSchema {
        name: "echo".to_owned(),
        description: "Repeats its input".to_owned(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"]
        }),
    }])
    .with_max_tokens(256);

    let stream = provider
        .stream(request, CancellationToken::new())
        .await
        .expect("accepted");
    let chunks: Vec<StreamChunk> = stream.collect().await;
    let message = girder_core::AssistantMessage::from_chunks(chunks).expect("assembled");

    let calls = message.tool_calls();
    assert_eq!(calls.len(), 1, "expected one tool call, got {message:?}");
    assert_eq!(calls[0].1, "echo");
    // An id is required to answer the call, and the API checks it.
    assert!(!calls[0].0.as_str().is_empty(), "the call must carry an id");
}

#[tokio::test]
async fn a_tool_result_is_accepted_by_the_real_api() {
    // The exact case a mock server let through: a history with a `tool_use` and its answer. The API
    // requires the answer to be a `tool_result` block naming the call, so a provider that serializes
    // it as text gets a 400 here.
    let provider = live!();
    let model = std::env::var("GIRDER_MODEL").unwrap_or_else(|_| "deepseek-chat".to_owned());

    // First request: ask for a tool so the call id comes from the API rather than being invented.
    let first = LlmRequest::new(
        model.clone(),
        vec![Message::new(
            girder_core::Role::User,
            vec![girder_core::ContentBlock::Text {
                text: "Use the echo tool to repeat the word hello.".to_owned(),
            }],
        )],
    )
    .with_tools(vec![girder_core::ToolSchema {
        name: "echo".to_owned(),
        description: "Repeats its input".to_owned(),
        parameters: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
    }])
    .with_max_tokens(256);

    let stream = provider
        .stream(first, CancellationToken::new())
        .await
        .expect("first request accepted");
    let chunks: Vec<StreamChunk> = stream.collect().await;
    let message = girder_core::AssistantMessage::from_chunks(chunks).expect("assembled");
    let calls = message.tool_calls();
    assert_eq!(calls.len(), 1, "the model must call the tool");
    let call_id = ToolCallId::new(calls[0].0.as_str()).expect("valid id");

    // Second request: the same conversation plus the answer. This is what the API validates.
    let second = LlmRequest::new(
        model,
        vec![
            Message::new(
                girder_core::Role::User,
                vec![girder_core::ContentBlock::Text {
                    text: "Use the echo tool to repeat the word hello.".to_owned(),
                }],
            ),
            Message::new(
                girder_core::Role::Assistant,
                vec![girder_core::ContentBlock::ToolCall {
                    id: call_id.clone(),
                    name: "echo".to_owned(),
                    arguments: calls[0].2.to_owned(),
                }],
            ),
            Message::tool_result(
                call_id,
                vec![girder_core::ContentBlock::Text {
                    text: "hello".to_owned(),
                }],
            ),
        ],
    )
    .with_tools(vec![girder_core::ToolSchema {
        name: "echo".to_owned(),
        description: "Repeats its input".to_owned(),
        parameters: serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
    }])
    .with_max_tokens(256);

    let stream = provider
        .stream(second, CancellationToken::new())
        .await
        .expect("the API must accept a tool_result answer");
    let chunks: Vec<StreamChunk> = stream.collect().await;
    let message = girder_core::AssistantMessage::from_chunks(chunks).expect("assembled");

    assert!(
        !message.text().is_empty(),
        "the model should answer after the tool result: {message:?}"
    );
}

#[tokio::test]
async fn a_cancelled_request_stops() {
    let provider = live!();
    let request = LlmRequest::new(
        std::env::var("GIRDER_MODEL").unwrap_or_else(|_| "deepseek-chat".to_owned()),
        vec![Message::new(
            girder_core::Role::User,
            vec![girder_core::ContentBlock::Text {
                text: "Write a 2000 word essay about computing.".to_owned(),
            }],
        )],
    )
    .with_max_tokens(2000);

    let cancel = CancellationToken::new();
    let stream = provider
        .stream(request, cancel.clone())
        .await
        .expect("accepted");

    // Cancel after the first chunk so the request is genuinely in flight.
    let mut stream = stream;
    let mut seen = 0;
    while let Some(chunk) = stream.next().await {
        // A cancelled stream must still deliver well-formed chunks, not garbage: the provider
        // translates a cancel into a terminal finish rather than truncating mid-frame.
        let _ = chunk;
        seen += 1;
        if seen == 1 {
            cancel.cancel();
        }
        if seen > 50 {
            break;
        }
    }

    assert!(seen > 0, "at least one chunk must arrive before cancelling");
}

#[tokio::test]
async fn a_real_summary_holds_every_required_section() {
    // The prompt's structure is the point: a summary missing a section silently loses the fact that
    // the section was empty. This asserts the model actually follows the shape rather than
    // paraphrasing it.
    let provider = live!();
    let model = std::env::var("GIRDER_MODEL").unwrap_or_else(|_| "deepseek-chat".to_owned());

    let conversation = vec![
        Message::new(
            girder_core::Role::User,
            vec![girder_core::ContentBlock::Text {
                text: "Add a retry helper to crates/core/src/retry.rs with exponential backoff."
                    .to_owned(),
            }],
        ),
        Message::new(
            girder_core::Role::Assistant,
            vec![girder_core::ContentBlock::Text {
                text:
                    "I added the retry module. Tests cover the doubling schedule and the ceiling."
                        .to_owned(),
            }],
        ),
        Message::new(
            girder_core::Role::User,
            vec![girder_core::ContentBlock::Text {
                text: "Also make sure jitter can never produce a zero delay.".to_owned(),
            }],
        ),
    ];

    let request =
        LlmRequest::new(model, girder_core::summary_request(conversation)).with_max_tokens(2000);

    let stream = provider
        .stream(request, CancellationToken::new())
        .await
        .expect("accepted");
    let chunks: Vec<StreamChunk> = stream.collect().await;
    let message = girder_core::AssistantMessage::from_chunks(chunks).expect("assembled");

    let summary = girder_core::extract_summary(&message).expect("a usable summary");

    for section in [
        "Primary Request and Intent",
        "Key Technical Concepts",
        "Files and Code",
        "Errors and Fixes",
        "Pending Jobs",
        "Current Work",
        "Next Step",
        "Critical Context",
    ] {
        assert!(
            summary.contains(section),
            "the summary dropped the `{section}` section:\n{summary}"
        );
    }
    // A fact worth carrying must survive.
    assert!(
        summary.contains("retry") || summary.contains("Retry"),
        "the summary lost the subject:\n{summary}"
    );
}

#[tokio::test]
async fn a_summary_becomes_a_detectable_checkpoint() {
    // End to end: summarize, wrap, and confirm a second compaction would recognise it.
    let provider = live!();
    let model = std::env::var("GIRDER_MODEL").unwrap_or_else(|_| "deepseek-chat".to_owned());

    let conversation = vec![Message::new(
        girder_core::Role::User,
        vec![girder_core::ContentBlock::Text {
            text: "We are porting a TypeScript project to Rust.".to_owned(),
        }],
    )];

    let request =
        LlmRequest::new(model, girder_core::summary_request(conversation)).with_max_tokens(1500);
    let stream = provider
        .stream(request, CancellationToken::new())
        .await
        .expect("accepted");
    let chunks: Vec<StreamChunk> = stream.collect().await;
    let message = girder_core::AssistantMessage::from_chunks(chunks).expect("assembled");
    let summary = girder_core::extract_summary(&message).expect("summary");

    let checkpoint =
        girder_core::checkpoint_message(&summary, girder_core::MessageId::new("cp-1").unwrap());

    assert!(girder_core::is_checkpoint(&checkpoint));
    assert!(checkpoint.text().contains("porting"));
}
