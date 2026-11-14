//! Tests for the `OpenAI` Chat Completions provider: wire shape, SSE framing, and translation.
//!
//! The protocol differences that matter are all pinned here rather than described: a `tool` result
//! is its own wire message, tool calls arrive as fragments keyed by `index`, and the stream ends
//! with `[DONE]` rather than an event type. A test that only checked "text arrives" would pass
//! against an adapter that had silently kept the Anthropic shape.
//!
//! Nothing here proves the module works against a live endpoint — no credential exists in this
//! environment. What it proves is that the body matches the documented Chat Completions shape, that
//! the framing handles split and multi-line payloads, and that a recorded-shaped stream becomes the
//! right chunks.
//!
//! The module is compiled into this test binary through `#[path]` because exporting it from
//! `lib.rs` is a separate change. When it is exported, the import below can become
//! `use girder_core::llm_openai::{...}`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use girder_core::{
    AssistantMessage, ContentBlock, FinishReason, LlmProvider, LlmRequest, Message, Role,
    StreamChunk, TokenUsage, ToolCallId, ToolSchema,
};
use girder_core::{
    OPENAI_DEFAULT_BASE_URL as DEFAULT_BASE_URL, OpenAiConfig, OpenAiProvider,
    OpenAiSseParser as SseParser, OpenAiTranslator as Translator,
};
use tokio_util::sync::CancellationToken;

fn provider() -> OpenAiProvider {
    OpenAiProvider::new(OpenAiConfig::new(
        "http://localhost:1",
        "test-key",
        "gpt-test",
    ))
    .expect("a provider with a plain config must build")
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_owned(),
    }
}

// ---- request body -------------------------------------------------------------------------------

#[test]
fn the_body_uses_the_chat_completions_shape() {
    let body = provider().body(&LlmRequest::new("gpt-4o", vec![]));

    assert_eq!(body["model"], "gpt-4o");
    assert_eq!(body["stream"], true);
    assert_eq!(
        body["stream_options"]["include_usage"], true,
        "without this a streamed response reports no usage at all"
    );
    assert_eq!(body["max_tokens"], 8192, "the configured default");
    assert!(body["messages"].is_array());
    assert!(
        body.get("system").is_none(),
        "this protocol has no top-level system field"
    );
}

#[test]
fn an_empty_request_model_falls_back_to_the_configured_one() {
    let body = provider().body(&LlmRequest::new("", vec![]));
    assert_eq!(body["model"], "gpt-test");
}

#[test]
fn a_request_limit_overrides_the_configured_default() {
    let body = provider().body(&LlmRequest::new("m", vec![]).with_max_tokens(64));
    assert_eq!(body["max_tokens"], 64);
}

#[test]
fn a_system_prompt_becomes_the_first_message() {
    // The Messages API has a top-level `system`; here it is a message, and its position matters
    // because a `system` turn after a user turn is rejected or reinterpreted.
    let request = LlmRequest::new("m", vec![Message::new(Role::User, vec![text_block("hi")])])
        .with_system_prompt("be brief");
    let body = provider().body(&request);

    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "be brief");
    assert_eq!(body["messages"][1]["role"], "user");
}

#[test]
fn an_empty_system_prompt_is_not_sent_as_a_message() {
    let body = provider().body(&LlmRequest::new("m", vec![]).with_system_prompt(""));
    assert_eq!(body["messages"].as_array().unwrap().len(), 0);
}

#[test]
fn tools_are_wrapped_in_a_function_object() {
    let request = LlmRequest::new("m", vec![]).with_tools(vec![ToolSchema {
        name: "bash".to_owned(),
        description: "run".to_owned(),
        parameters: serde_json::json!({"type": "object"}),
    }]);
    let body = provider().body(&request);

    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "bash");
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["type"], "object",
        "the Messages API calls this input_schema; calling it that here is a 400"
    );
    assert!(body["tools"][0].get("input_schema").is_none());
}

// ---- role and content mapping -------------------------------------------------------------------

#[test]
fn a_tool_result_becomes_its_own_tool_message() {
    // The Messages API carries a tool result as a `tool_result` block on a user turn. Here it is a
    // message with role `tool` and a top-level `tool_call_id`; reusing the block shape would be
    // rejected by every Chat Completions endpoint.
    let call_id = ToolCallId::new("call-1").expect("valid test id");
    let messages = vec![Message::tool_result(call_id, vec![text_block("result")])];
    let body = provider().body(&LlmRequest::new("m", messages));

    assert_eq!(body["messages"][0]["role"], "tool");
    assert_eq!(body["messages"][0]["tool_call_id"], "call-1");
    assert_eq!(body["messages"][0]["content"], "result");
    assert!(
        body["messages"][0].get("tool_calls").is_none(),
        "a tool result is not a call"
    );
}

#[test]
fn an_empty_tool_result_gets_a_placeholder() {
    // Some gateways reject an empty tool content outright; the model still has to be told the call
    // produced nothing.
    let call_id = ToolCallId::new("c").expect("valid test id");
    let messages = vec![Message::tool_result(call_id, vec![])];
    let body = provider().body(&LlmRequest::new("m", messages));

    assert_eq!(body["messages"][0]["content"], "(no output)");
}

#[test]
fn an_assistant_tool_call_becomes_a_tool_calls_entry_with_a_raw_argument_string() {
    let messages = vec![Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall {
            id: ToolCallId::new("call-1").expect("valid test id"),
            name: "bash".to_owned(),
            arguments: r#"{"command":"ls"}"#.to_owned(),
        }],
    )];
    let body = provider().body(&LlmRequest::new("m", messages));

    let call = &body["messages"][0]["tool_calls"][0];
    assert_eq!(call["id"], "call-1");
    assert_eq!(call["type"], "function");
    assert_eq!(call["function"]["name"], "bash");
    assert_eq!(
        call["function"]["arguments"], r#"{"command":"ls"}"#,
        "this protocol wants the JSON text, not a parsed object"
    );
    assert_eq!(
        body["messages"][0]["content"], "",
        "a tool-call-only turn sends an empty string, never null"
    );
}

#[test]
fn a_malformed_argument_accumulator_is_passed_through_verbatim() {
    // Parsing it here would hide the defect from the model, which is the only party that can
    // produce a valid call instead.
    let messages = vec![Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall {
            id: ToolCallId::new("c").expect("valid test id"),
            name: "t".to_owned(),
            arguments: "{not json".to_owned(),
        }],
    )];
    let body = provider().body(&LlmRequest::new("m", messages));

    assert_eq!(
        body["messages"][0]["tool_calls"][0]["function"]["arguments"],
        "{not json"
    );
}

#[test]
fn reasoning_blocks_are_not_replayed_as_content() {
    // No vendor-neutral field carries reasoning here, and rendering it as text would make the model
    // read its own thinking as conversation.
    let messages = vec![Message::new(
        Role::Assistant,
        vec![
            ContentBlock::Reasoning {
                text: "thinking".to_owned(),
            },
            text_block("answer"),
        ],
    )];
    let body = provider().body(&LlmRequest::new("m", messages));

    assert_eq!(body["messages"][0]["content"], "answer");
}

#[test]
fn image_and_file_blocks_survive_as_handle_text() {
    let messages = vec![Message::new(
        Role::User,
        vec![
            ContentBlock::Image {
                attachment: "img-1".to_owned(),
            },
            ContentBlock::File {
                attachment: "f-1".to_owned(),
            },
        ],
    )];
    let body = provider().body(&LlmRequest::new("m", messages));

    let content = body["messages"][0]["content"].as_str().unwrap();
    assert!(content.contains("[image img-1]"), "got: {content}");
    assert!(content.contains("[file f-1]"), "got: {content}");
}

#[test]
fn the_endpoint_appends_chat_completions() {
    assert_eq!(
        OpenAiConfig::new("https://api.openai.com/v1/", "k", "m").endpoint(),
        "https://api.openai.com/v1/chat/completions"
    );
    assert_eq!(
        OpenAiConfig::new(DEFAULT_BASE_URL, "k", "m").endpoint(),
        "https://api.openai.com/v1/chat/completions"
    );
}

#[test]
fn a_base_url_that_is_already_the_full_route_is_not_doubled() {
    let config = OpenAiConfig::new("http://host/v1/chat/completions", "k", "m");
    assert_eq!(config.endpoint(), "http://host/v1/chat/completions");
}

#[test]
fn the_config_is_readable_from_the_provider() {
    let provider = provider();
    assert_eq!(provider.config().default_model, "gpt-test");
    assert_eq!(provider.config().api_key, "test-key");
}

// ---- SSE framing --------------------------------------------------------------------------------

/// Feeds one parser, returning every payload in arrival order.
fn payloads(chunks: &[&[u8]]) -> Vec<String> {
    let mut parser = SseParser::default();
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(parser.feed(chunk));
    }
    out
}

#[test]
fn every_blank_line_terminates_one_payload() {
    let out = payloads(&[b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n"]);
    assert_eq!(out, vec!["{\"a\":1}", "{\"b\":2}"]);
}

#[test]
fn the_done_sentinel_is_delivered_like_any_other_payload() {
    // The framing layer stays protocol-agnostic: `[DONE]` is a payload the translator recognizes,
    // not a special case the byte parser hides.
    let out = payloads(&[b"data: [DONE]\n\n"]);
    assert_eq!(out, vec!["[DONE]"]);
}

#[test]
fn a_payload_split_across_network_chunks_is_reassembled() {
    let out = payloads(&[b"data: {\"choi", b"ces\":[]}\n", b"\n"]);
    assert_eq!(out, vec!["{\"choices\":[]}"]);
}

#[test]
fn multiple_data_lines_join_with_a_newline() {
    // The SSE specification joins repeated `data:` fields; a proxy that re-indents nested JSON
    // relies on it.
    let out = payloads(&[b"data: {\"a\":\ndata: 1}\n\n"]);
    assert_eq!(out, vec!["{\"a\":\n1}"]);
}

#[test]
fn carriage_returns_and_comments_are_tolerated() {
    let out = payloads(&[b": keep-alive\r\ndata: {\"a\":1}\r\n\r\n"]);
    assert_eq!(out, vec!["{\"a\":1}"]);
}

#[test]
fn a_named_event_field_does_not_replace_the_payload() {
    // Dispatch is by the payload's own shape, not by `event:`. A proxy is free to add names, and a
    // parser that keyed on them would drop every frame the moment one appeared.
    let out = payloads(&[b"event: message\ndata: {\"a\":1}\n\n"]);
    assert_eq!(out, vec!["{\"a\":1}"]);
}

#[test]
fn one_leading_space_after_the_prefix_is_framing_not_content() {
    let out = payloads(&[b"data:  {\"a\":1}\n\n"]);
    assert_eq!(
        out,
        vec![" {\"a\":1}"],
        "only the first space is framing; the rest belongs to the payload"
    );
}

#[test]
fn a_multibyte_character_split_across_chunks_is_not_corrupted() {
    // Decoding each network chunk independently would replace half a character with U+FFFD.
    let payload = "data: {\"t\":\"你好世界\"}\n\n";
    let split = payload.find('好').expect("character present");
    let out = payloads(&[
        &payload.as_bytes()[..=split],
        &payload.as_bytes()[split + 1..],
    ]);

    assert_eq!(out.len(), 1);
    assert!(out[0].contains("你好世界"), "got: {}", out[0]);
}

#[test]
fn an_unterminated_tail_is_flushed_at_end_of_stream() {
    // A server may close without the final blank line. Dropping the tail would lose `[DONE]` and
    // make a complete response look truncated.
    let mut parser = SseParser::default();
    assert!(parser.feed(b"data: [DONE]\n").is_empty());
    assert!(parser.has_pending());
    assert_eq!(parser.flush().as_deref(), Some("[DONE]"));
    assert!(!parser.has_pending(), "flushing consumes the buffer");
}

#[test]
fn flushing_an_empty_parser_reports_nothing() {
    let mut parser = SseParser::default();
    assert_eq!(parser.flush(), None);
}

// ---- translation --------------------------------------------------------------------------------

/// Translates a list of payloads, flattening the chunks in order.
fn translate(payloads: &[&str]) -> Vec<StreamChunk> {
    let mut translator = Translator::default();
    payloads
        .iter()
        .flat_map(|payload| translator.translate(payload))
        .collect()
}

#[test]
fn a_text_delta_opens_one_block_and_the_finish_closes_it() {
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"content":"hel"}}]}"#,
        r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]);

    assert_eq!(
        chunks[0],
        StreamChunk::BlockStart {
            index: 0,
            block_type: "text".to_owned()
        }
    );
    assert_eq!(
        chunks[1],
        StreamChunk::TextDelta {
            index: 0,
            text: "hel".to_owned()
        }
    );
    assert_eq!(
        chunks[2],
        StreamChunk::TextDelta {
            index: 0,
            text: "lo".to_owned()
        }
    );
    assert_eq!(
        chunks[3],
        StreamChunk::BlockEnd {
            index: 0,
            block: text_block("hello")
        }
    );
    assert_eq!(
        chunks[4],
        StreamChunk::Finish {
            reason: FinishReason::Stop
        }
    );
}

#[test]
fn an_empty_content_delta_produces_no_chunk() {
    // Providers open a stream with `content: ""`; counting that as content would open a block that
    // assembles into an empty message.
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
        r#"{"choices":[{"delta":{}}]}"#,
    ]);
    assert!(chunks.is_empty(), "got: {chunks:?}");
}

#[test]
fn an_empty_first_reasoning_delta_does_not_open_a_reasoning_block() {
    // Thinking models send `reasoning_content: ""` on the opening frame. Opening a block there
    // yields an empty reasoning block in the assembled message.
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"reasoning_content":"","content":""}}]}"#,
        r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
        "[DONE]",
    ]);

    assert_eq!(
        chunks[0],
        StreamChunk::BlockStart {
            index: 0,
            block_type: "text".to_owned()
        },
        "the text block is index 0 because no reasoning block opened: {chunks:?}"
    );
}

#[test]
fn reasoning_and_text_are_separate_blocks_in_arrival_order() {
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"reasoning_content":"why"}}]}"#,
        r#"{"choices":[{"delta":{"content":"answer"}}]}"#,
        "[DONE]",
    ]);

    let starts: Vec<usize> = chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::BlockStart { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec![0, 1]);

    let assembled = AssistantMessage::from_chunks(chunks).expect("the stream finishes");
    assert_eq!(
        assembled.content,
        vec![
            ContentBlock::Reasoning {
                text: "why".to_owned()
            },
            text_block("answer"),
        ]
    );
}

#[test]
fn tool_call_fragments_sharing_an_index_become_one_call() {
    // The core of this protocol: the call is spread over deltas that only agree on `index`.
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"bash","arguments":""}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"comm"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ]);

    let assembled = AssistantMessage::from_chunks(chunks).expect("the stream finishes");
    let calls = assembled.tool_calls();
    assert_eq!(
        calls.len(),
        1,
        "three fragments must not become three calls"
    );
    let (id, name, arguments) = &calls[0];
    assert_eq!(id.as_str(), "call_a");
    assert_eq!(*name, "bash");
    assert_eq!(*arguments, r#"{"command":"ls"}"#);
    assert_eq!(assembled.finish, Some(FinishReason::ToolCalls));
}

#[test]
fn a_repeated_empty_id_or_name_does_not_rewrite_the_call() {
    // Gateways repeat `id: ""` and `name: ""` on continuation deltas to mean "unchanged". Taking
    // them literally either clears the identity or, because the assembler appends names, produces
    // `bashbash`.
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"bash","arguments":"{}"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"","function":{"name":"","arguments":""}}]}}]}"#,
        "[DONE]",
    ]);

    let assembled = AssistantMessage::from_chunks(chunks).expect("the stream finishes");
    let calls = assembled.tool_calls();
    assert_eq!(
        calls.len(),
        1,
        "an identity-only repeat must not open a second call"
    );
    assert_eq!(calls[0].0.as_str(), "call_a");
    assert_eq!(calls[0].1, "bash");
}

#[test]
fn parallel_tool_calls_stay_separate_by_wire_index() {
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"one","arguments":"{\"x\":1}"}},{"index":1,"id":"b","function":{"name":"two","arguments":"{\"y\":2}"}}]}}]}"#,
        "[DONE]",
    ]);

    let assembled = AssistantMessage::from_chunks(chunks).expect("the stream finishes");
    let calls = assembled.tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1, "one");
    assert_eq!(calls[1].1, "two");
}

#[test]
fn a_call_with_no_id_still_gets_a_stable_one() {
    // Some gateways omit the id. Dropping the call would lose work the model asked for; a random
    // id would make a replayed stream assemble differently, so it is derived from the wire index.
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":2,"function":{"name":"bash","arguments":"{}"}}]}}]}"#,
        "[DONE]",
    ]);

    let assembled = AssistantMessage::from_chunks(chunks).expect("the stream finishes");
    let calls = assembled.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.as_str(), "call-2");
}

#[test]
fn a_call_opens_on_the_delta_that_carries_content() {
    // A first delta of `{"index":0,"id":"c","function":{"name":""}}` says nothing dispat-able.
    // Opening there would put an unnamed, argument-less tool call in the message.
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":""}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"read","arguments":"{\"p\":1}"}}]}}]}"#,
        "[DONE]",
    ]);

    let assembled = AssistantMessage::from_chunks(chunks).expect("the stream finishes");
    let calls = assembled.tool_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.as_str(), "c");
    assert_eq!(calls[0].1, "read");
    assert_eq!(calls[0].2, r#"{"p":1}"#);
}

#[test]
fn a_finish_reason_maps_to_a_finish_reason() {
    for (wire, expected) in [
        ("stop", FinishReason::Stop),
        ("length", FinishReason::Length),
        ("tool_calls", FinishReason::ToolCalls),
    ] {
        let chunks = translate(&[
            r#"{"choices":[{"delta":{"content":"x"}}]}"#,
            &format!(r#"{{"choices":[{{"delta":{{}},"finish_reason":"{wire}"}}]}}"#),
            "[DONE]",
        ]);
        assert_eq!(
            chunks.last(),
            Some(&StreamChunk::Finish { reason: expected }),
            "finish reason {wire}"
        );
    }
}

#[test]
fn an_unknown_finish_reason_is_an_error_rather_than_a_silent_stop() {
    // `content_filter` and a future value both mean the answer is not what the model meant to say.
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"content":"x"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#,
        "[DONE]",
    ]);
    assert!(matches!(
        chunks.last(),
        Some(StreamChunk::Finish {
            reason: FinishReason::Error { .. }
        })
    ));
}

#[test]
fn usage_is_deferred_until_done_and_subtracts_the_cached_prefix() {
    // `prompt_tokens` includes the cache here, while the harness records disjoint counts; leaving
    // the cache in `input` would double-count every cached turn.
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"content":"x"}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":60}}}"#,
        "[DONE]",
    ]);

    let usage = chunks
        .iter()
        .find_map(|chunk| match chunk {
            StreamChunk::Usage(usage) => Some(*usage),
            _ => None,
        })
        .expect("usage must be reported");
    assert_eq!(
        usage,
        TokenUsage {
            input: 40,
            output: 7,
            cached: 60
        }
    );
    assert!(
        matches!(chunks.last(), Some(StreamChunk::Finish { .. })),
        "usage always precedes the terminal finish"
    );
}

#[test]
fn a_trailing_usage_only_chunk_is_honoured() {
    let chunks = translate(&[
        r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#,
        "[DONE]",
    ]);

    assert!(chunks.iter().any(|chunk| matches!(
        chunk,
        StreamChunk::Usage(TokenUsage {
            input: 10,
            output: 2,
            cached: 0
        })
    )));
}

#[test]
fn an_in_stream_error_payload_becomes_a_terminal_error_finish() {
    // Some gateways open the stream with 200 and then report the failure in-band.
    let chunks = translate(&[r#"{"error":{"message":"overloaded","type":"server_error"}}"#]);
    assert_eq!(
        chunks,
        vec![StreamChunk::Finish {
            reason: FinishReason::Error {
                message: "overloaded".to_owned()
            }
        }]
    );
}

#[test]
fn nothing_is_emitted_after_the_terminal_chunk() {
    let mut translator = Translator::default();
    translator.translate("[DONE]");
    assert!(translator.finished);
    assert!(
        translator
            .translate(r#"{"choices":[{"delta":{"content":"late"}}]}"#)
            .is_empty(),
        "a chunk after finish would break every consumer that stops at the terminal one"
    );
}

#[test]
fn a_completed_response_with_no_content_is_an_error() {
    // A blank assistant message looks like a model decision. Reporting the failure lets the retry
    // policy act on it instead.
    let chunks = translate(&[
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ]);
    assert!(matches!(
        chunks.last(),
        Some(StreamChunk::Finish {
            reason: FinishReason::Error { .. }
        })
    ));
}

#[test]
fn a_malformed_payload_stops_the_stream() {
    let chunks = translate(&["{not json"]);
    assert!(matches!(
        chunks[0],
        StreamChunk::Finish {
            reason: FinishReason::Error { .. }
        }
    ));
}

// ---- end to end against a mock server ------------------------------------------------------------

/// A minimal HTTP server that serves one SSE response and records the request.
async fn mock_server(
    sse_body: String,
) -> (
    String,
    Arc<Mutex<Vec<serde_json::Value>>>,
    Arc<Mutex<Vec<String>>>,
) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let paths = Arc::new(Mutex::new(Vec::new()));
    let recorded_bodies = Arc::clone(&bodies);
    let recorded_paths = Arc::clone(&paths);

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buffer = vec![0u8; 65536];
        let Ok(read) = socket.read(&mut buffer).await else {
            return;
        };
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();

        if let Some(first_line) = request.lines().next() {
            if let Some(path) = first_line.split_whitespace().nth(1) {
                recorded_paths.lock().unwrap().push(path.to_owned());
            }
        }
        if let Some(index) = request.find("\r\n\r\n") {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&request[index + 4..]) {
                recorded_bodies.lock().unwrap().push(value);
            }
        }

        let headers = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            sse_body.len()
        );
        let _ = socket.write_all(headers.as_bytes()).await;
        let _ = socket.write_all(sse_body.as_bytes()).await;
        let _ = socket.flush().await;
    });

    (format!("http://{addr}/v1"), bodies, paths)
}

/// A recorded-shaped Chat Completions stream, text only.
fn recorded_sse() -> String {
    [
        r#"data: {"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"content":"hi "}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{"content":"there"}}]}"#,
        "\n\n",
        r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        "\n\n",
        r#"data: {"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":2}}"#,
        "\n\n",
        "data: [DONE]\n\n",
    ]
    .concat()
}

#[tokio::test]
async fn the_provider_posts_to_chat_completions_and_streams_chunks() {
    let (base, bodies, paths) = mock_server(recorded_sse()).await;
    let provider = OpenAiProvider::new(OpenAiConfig::new(&base, "test-key", "gpt-test")).unwrap();

    let request = LlmRequest::new(
        "gpt-4o",
        vec![Message::new(Role::User, vec![text_block("hello")])],
    );
    let stream = provider
        .stream(request, CancellationToken::new())
        .await
        .unwrap();
    let chunks: Vec<StreamChunk> = stream.collect().await;

    assert_eq!(paths.lock().unwrap()[0], "/v1/chat/completions");
    assert_eq!(bodies.lock().unwrap()[0]["messages"][0]["content"], "hello");

    let assembled = AssistantMessage::from_chunks(chunks).expect("the stream finishes");
    assert_eq!(assembled.text(), "hi there");
    assert_eq!(assembled.finish, Some(FinishReason::Stop));
    assert_eq!(
        assembled.usage,
        Some(TokenUsage {
            input: 9,
            output: 2,
            cached: 0
        })
    );
}

#[tokio::test]
async fn a_stream_cut_before_done_is_reported_as_an_error_finish() {
    // A network cut would otherwise look like a complete answer.
    let truncated = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let (base, _bodies, _paths) = mock_server(truncated.to_owned()).await;
    let provider = OpenAiProvider::new(OpenAiConfig::new(&base, "k", "m")).unwrap();

    let stream = provider
        .stream(LlmRequest::new("m", vec![]), CancellationToken::new())
        .await
        .unwrap();
    let chunks: Vec<StreamChunk> = stream.collect().await;

    assert!(
        chunks.iter().any(|chunk| matches!(
            chunk,
            StreamChunk::Finish {
                reason: FinishReason::Error { .. }
            }
        )),
        "a truncated stream must end with an error: {chunks:?}"
    );
}

#[tokio::test]
async fn an_http_error_surfaces_the_provider_message() {
    // The provider's own words are the only thing that distinguishes a bad key from a bad request;
    // a generic "request failed" leaves the operator guessing.
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buffer = vec![0u8; 4096];
        let _ = socket.read(&mut buffer).await;
        let body = r#"{"error":{"message":"invalid api key","type":"invalid_request_error"}}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });

    let provider =
        OpenAiProvider::new(OpenAiConfig::new(format!("http://{addr}"), "bad", "m")).unwrap();
    let Err(error) = provider
        .stream(LlmRequest::new("m", vec![]), CancellationToken::new())
        .await
    else {
        panic!("a 401 must surface as an error");
    };
    let message = error.to_string();
    assert!(message.contains("401"), "got: {message}");
    assert!(
        message.contains("invalid api key"),
        "the provider's explanation must reach the caller, got: {message}"
    );
}

#[tokio::test]
async fn a_pre_cancelled_request_never_reaches_the_network() {
    let provider = OpenAiProvider::new(OpenAiConfig::new("http://127.0.0.1:1", "k", "m")).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let Err(error) = provider.stream(LlmRequest::new("m", vec![]), cancel).await else {
        panic!("a cancelled request must not start");
    };
    assert!(matches!(error, girder_core::LlmError::Cancelled));
}
