//! Tests for the `DeepSeek` provider: wire shape, SSE parsing, and chunk translation.
//!
//! Everything here is verified against a **local mock server** or against the parser directly. No
//! credential is available in this environment, so nothing here proves the provider works against
//! `api.deepseek.com`. What it does prove is that the request body matches the Anthropic Messages
//! shape, that the SSE parser handles chunk boundaries and multi-byte characters, and that a
//! recorded-shaped stream translates into the right chunks.

use std::sync::{Arc, Mutex};

use futures::StreamExt as _;
use girder_core::{
    ContentBlock, DeepSeekConfig, DeepSeekProvider, FinishReason, LlmProvider, LlmRequest, Message,
    Role, SseEvent, SseParser, StreamChunk, TokenUsage, ToolCallId, Translator,
};
use tokio_util::sync::CancellationToken;

// ---- request body -------------------------------------------------------------------------------

fn provider() -> DeepSeekProvider {
    DeepSeekProvider::new(DeepSeekConfig::new("http://localhost:1", "test-key")).unwrap()
}

#[test]
fn the_body_uses_the_messages_api_shape() {
    let request = LlmRequest::new("deepseek-chat", vec![]);
    let body = provider().body(&request);

    assert_eq!(body["model"], "deepseek-chat");
    assert_eq!(body["stream"], true);
    assert!(
        body["max_tokens"].as_u64().unwrap() > 0,
        "max_tokens is required by the Messages API"
    );
    assert!(body["messages"].is_array());
}

#[test]
fn a_tool_role_becomes_a_user_turn_holding_a_tool_result() {
    // The Messages API has no `tool` role; tool results ride on a user turn. The block must be a
    // `tool_result`, not plain text: every `tool_use` has to be answered by one in the immediately
    // following message, and the API rejects the request otherwise. This was caught by running
    // against the real endpoint -- a mock server did not check the protocol.
    let call_id = girder_core::ToolCallId::new("call-1").expect("valid");
    let messages = vec![Message::tool_result(
        call_id,
        vec![ContentBlock::Text {
            text: "result".to_owned(),
        }],
    )];
    let body = provider().body(&LlmRequest::new("m", messages));

    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(
        body["messages"][0]["content"][0]["type"], "tool_result",
        "a tool answer must be a tool_result block, not text"
    );
    assert_eq!(
        body["messages"][0]["content"][0]["tool_use_id"], "call-1",
        "the block must name the call it answers"
    );
}

#[test]
fn a_tool_message_without_a_call_id_is_still_serialized() {
    // Defensive: a caller that builds one by hand gets an empty id rather than a panic. The loop
    // always attaches one, so this only guards the seam.
    let messages = vec![Message::new(
        Role::Tool,
        vec![ContentBlock::Text {
            text: "result".to_owned(),
        }],
    )];
    let body = provider().body(&LlmRequest::new("m", messages));

    assert_eq!(body["messages"][0]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][0]["content"][0]["tool_use_id"], "");
}

#[test]
fn reasoning_blocks_are_not_sent_back() {
    // The API does not accept reasoning as input; sending it would be rejected.
    let messages = vec![Message::new(
        Role::Assistant,
        vec![
            ContentBlock::Reasoning {
                text: "thinking".to_owned(),
            },
            ContentBlock::Text {
                text: "answer".to_owned(),
            },
        ],
    )];
    let body = provider().body(&LlmRequest::new("m", messages));

    let content = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2, "the shape keeps both slots");
    assert_eq!(content[0]["text"], "", "reasoning is blanked, not leaked");
    assert_eq!(content[1]["text"], "answer");
}

#[test]
fn a_tool_call_becomes_a_tool_use_block_with_parsed_input() {
    let messages = vec![Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall {
            id: ToolCallId::new("call-1").expect("valid test id"),
            name: "bash".to_owned(),
            arguments: r#"{"command":"ls"}"#.to_owned(),
        }],
    )];
    let body = provider().body(&LlmRequest::new("m", messages));

    let block = &body["messages"][0]["content"][0];
    assert_eq!(block["type"], "tool_use");
    assert_eq!(block["id"], "call-1");
    assert_eq!(block["name"], "bash");
    assert_eq!(
        block["input"]["command"], "ls",
        "the API wants a parsed object, not a JSON string"
    );
}

#[test]
fn malformed_tool_arguments_do_not_fail_the_request() {
    // A malformed accumulator becomes an empty object; the tool executor reports the bad arguments
    // to the model, which is more useful than dropping the whole turn.
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
        body["messages"][0]["content"][0]["input"],
        serde_json::json!({})
    );
}

#[test]
fn tools_are_sent_with_an_input_schema() {
    let request = LlmRequest::new("m", vec![]).with_tools(vec![girder_core::ToolSchema {
        name: "bash".to_owned(),
        description: "run".to_owned(),
        parameters: serde_json::json!({"type": "object"}),
    }]);
    let body = provider().body(&request);

    assert_eq!(body["tools"][0]["name"], "bash");
    assert_eq!(
        body["tools"][0]["input_schema"]["type"], "object",
        "the Messages API calls it input_schema"
    );
}

#[test]
fn a_system_prompt_is_sent_as_a_top_level_field() {
    let request = LlmRequest::new("m", vec![]).with_system_prompt("be brief");
    let body = provider().body(&request);

    assert_eq!(body["system"], "be brief");
}

#[test]
fn the_endpoint_appends_messages_and_tolerates_a_trailing_slash() {
    let config = DeepSeekConfig::new("https://api.deepseek.com/anthropic/", "k");
    assert_eq!(
        config.endpoint(),
        "https://api.deepseek.com/anthropic/messages"
    );
}

// ---- SSE parsing --------------------------------------------------------------------------------

#[test]
fn the_parser_emits_one_event_per_blank_line() {
    let mut parser = SseParser::default();
    let events =
        parser.feed(b"event: message_start\ndata: {\"a\":1}\n\nevent: message_stop\ndata: {}\n\n");

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event, "message_start");
    assert_eq!(events[0].data, "{\"a\":1}");
    assert_eq!(events[1].event, "message_stop");
}

#[test]
fn the_parser_handles_an_event_split_across_chunks() {
    // The network splits wherever it likes; a parser that assumed one chunk per event would drop
    // frames under load.
    let mut parser = SseParser::default();
    assert!(parser.feed(b"event: content_blo").is_empty());
    assert!(parser.feed(b"ck_delta\ndata: {\"x\"").is_empty());
    let events = parser.feed(b":1}\n\n");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "content_block_delta");
    assert_eq!(events[0].data, "{\"x\":1}");
}

#[test]
fn the_parser_does_not_corrupt_a_multibyte_character_split_across_chunks() {
    // A naive `String::from_utf8` per chunk would replace half a character with U+FFFD. The parser
    // buffers bytes and decodes only complete lines.
    let text = "你好世界";
    let payload = format!("event: x\ndata: {{\"t\":\"{text}\"}}\n\n");
    let bytes = payload.as_bytes();
    let split = payload.find('好').expect("character present");

    let mut parser = SseParser::default();
    // Split inside the multi-byte sequence.
    assert!(parser.feed(&bytes[..=split]).is_empty());
    let events = parser.feed(&bytes[split + 1..]);

    assert_eq!(events.len(), 1);
    assert!(events[0].data.contains(text), "got: {}", events[0].data);
}

#[test]
fn carriage_returns_are_tolerated() {
    let mut parser = SseParser::default();
    let events = parser.feed(b"event: a\r\ndata: {}\r\n\r\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "a");
}

#[test]
fn comments_and_id_fields_are_ignored() {
    let mut parser = SseParser::default();
    let events = parser.feed(b": keep-alive\nid: 42\nevent: a\ndata: {}\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "a");
}

// ---- translation --------------------------------------------------------------------------------

fn translate_all(events: &[(&str, &str)]) -> Vec<StreamChunk> {
    let mut translator = Translator::default();
    let mut out = Vec::new();
    for (event, data) in events {
        for chunk in translator.translate(&SseEvent {
            event: (*event).to_owned(),
            data: (*data).to_owned(),
        }) {
            out.push(chunk.expect("translation must not fail"));
        }
    }
    out
}

#[test]
fn a_text_block_streams_as_start_deltas_stop() {
    let chunks = translate_all(&[
        (
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
        ),
        (
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
        ),
        ("content_block_stop", r#"{"index":0}"#),
        (
            "message_delta",
            r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
        ),
    ]);

    assert!(matches!(
        chunks[0],
        StreamChunk::BlockStart { block_type: _, .. }
    ));
    assert_eq!(
        chunks[1],
        StreamChunk::TextDelta {
            index: 0,
            text: "hello".to_owned()
        }
    );
    assert!(matches!(chunks[2], StreamChunk::BlockEnd { .. }));
    assert!(matches!(
        chunks[3],
        StreamChunk::Usage(TokenUsage { output: 5, .. })
    ));
    assert_eq!(
        chunks[4],
        StreamChunk::Finish {
            reason: FinishReason::Stop
        }
    );
}

#[test]
fn a_tool_use_block_carries_its_id_and_name_on_the_start_event() {
    let chunks = translate_all(&[(
        "content_block_start",
        r#"{"index":1,"content_block":{"type":"tool_use","id":"call-1","name":"bash"}}"#,
    )]);

    let StreamChunk::ToolCallDelta {
        index,
        id,
        name,
        arguments_delta,
    } = &chunks[1]
    else {
        panic!("expected a tool call delta, got {:?}", chunks[1]);
    };
    assert_eq!(*index, 1);
    assert_eq!(id.as_str(), "call-1");
    assert_eq!(name.as_deref(), Some("bash"));
    assert!(arguments_delta.is_empty(), "arguments stream separately");
}

#[test]
fn tool_arguments_accumulate_across_input_json_deltas() {
    let chunks = translate_all(&[
        (
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"c","name":"bash"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"comm"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"and\":\"ls\"}"}}"#,
        ),
    ]);

    let fragment: String = chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::ToolCallDelta {
                arguments_delta, ..
            } => Some(arguments_delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(fragment, r#"{"command":"ls"}"#);
}

#[test]
fn a_stop_reason_maps_to_a_finish_reason() {
    for (wire, expected) in [
        ("end_turn", FinishReason::Stop),
        ("max_tokens", FinishReason::Length),
        ("tool_use", FinishReason::ToolCalls),
    ] {
        let chunks = translate_all(&[(
            "message_delta",
            &format!(r#"{{"delta":{{"stop_reason":"{wire}"}}}}"#),
        )]);
        assert_eq!(
            chunks[0],
            StreamChunk::Finish { reason: expected },
            "stop reason {wire}"
        );
    }
}

#[test]
fn an_unknown_stop_reason_is_an_error_rather_than_a_silent_stop() {
    // Guessing `Stop` for an unknown reason would hide a protocol change.
    let chunks = translate_all(&[(
        "message_delta",
        r#"{"delta":{"stop_reason":"something_new"}}"#,
    )]);
    assert!(matches!(
        chunks[0],
        StreamChunk::Finish {
            reason: FinishReason::Error { .. }
        }
    ));
}

#[test]
fn a_ping_event_is_ignored() {
    let chunks = translate_all(&[("ping", "{}")]);
    assert!(chunks.is_empty(), "a keep-alive must not produce a chunk");
}

#[test]
fn an_error_event_becomes_a_terminal_error_finish() {
    let chunks = translate_all(&[(
        "error",
        r#"{"type":"error","error":{"message":"overloaded"}}"#,
    )]);
    assert!(matches!(
        chunks[0],
        StreamChunk::Finish {
            reason: FinishReason::Error { .. }
        }
    ));
}

#[test]
fn message_stop_marks_the_stream_finished() {
    let mut translator = Translator::default();
    assert!(!translator.finished);
    translator.translate(&SseEvent {
        event: "message_stop".to_owned(),
        data: "{}".to_owned(),
    });
    assert!(translator.finished);
}

// ---- end to end against a mock server -----------------------------------------------------------

/// A minimal HTTP server that serves one SSE response and records the request body.
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

        // Record the path and JSON body.
        if let Some(first_line) = request.lines().next() {
            if let Some(path) = first_line.split_whitespace().nth(1) {
                recorded_paths.lock().unwrap().push(path.to_owned());
            }
        }
        if let Some(index) = request.find("\r\n\r\n") {
            let body = &request[index + 4..];
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
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

    (format!("http://{addr}"), bodies, paths)
}

fn recorded_sse() -> String {
    [
        r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":11}}}

"#,
        r#"event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

"#,
        r#"event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi there"}}

"#,
        r#"event: content_block_stop
data: {"type":"content_block_stop","index":0}

"#,
        r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}

"#,
        r#"event: message_stop
data: {"type":"message_stop"}

"#,
    ]
    .concat()
}

#[tokio::test]
async fn the_provider_posts_to_messages_and_streams_chunks() {
    let (base, bodies, paths) = mock_server(recorded_sse()).await;
    let provider = DeepSeekProvider::new(DeepSeekConfig::new(&base, "test-key")).unwrap();

    let request = LlmRequest::new(
        "deepseek-chat",
        vec![Message::new(
            Role::User,
            vec![ContentBlock::Text {
                text: "hello".to_owned(),
            }],
        )],
    );

    let stream = provider
        .stream(request, CancellationToken::new())
        .await
        .unwrap();
    let chunks: Vec<StreamChunk> = stream.collect().await;

    assert_eq!(
        paths.lock().unwrap()[0],
        "/messages",
        "the route is /messages"
    );
    assert_eq!(
        bodies.lock().unwrap()[0]["messages"][0]["content"][0]["text"],
        "hello"
    );

    let text: String = chunks
        .iter()
        .filter_map(|chunk| match chunk {
            StreamChunk::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hi there");
    assert!(chunks.iter().any(|chunk| matches!(
        chunk,
        StreamChunk::Finish {
            reason: FinishReason::Stop
        }
    )));
}

#[tokio::test]
async fn a_truncated_stream_is_reported_as_an_error_finish() {
    // A stream that ends without `message_stop` is incomplete; silently accepting it would make a
    // network cut look like a complete answer.
    let truncated = concat!(
        "event: content_block_start\n",
        "data: {\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
    );
    let (base, _bodies, _paths) = mock_server(truncated.to_owned()).await;
    let provider = DeepSeekProvider::new(DeepSeekConfig::new(&base, "k")).unwrap();

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
async fn an_http_error_becomes_a_provider_error() {
    // The status and body must reach the caller: a generic "request failed" hides rate limits and
    // auth failures, which need different responses.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0u8; 4096];
        let _ = socket.read(&mut buffer).await;
        let body = r#"{"error":{"message":"invalid api key"}}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });

    let provider =
        DeepSeekProvider::new(DeepSeekConfig::new(format!("http://{addr}"), "bad")).unwrap();
    let Err(error) = provider
        .stream(LlmRequest::new("m", vec![]), CancellationToken::new())
        .await
    else {
        panic!("a 401 must surface");
    };

    let message = error.to_string();
    assert!(message.contains("401"), "{message}");
    assert!(message.contains("invalid api key"), "{message}");
}

#[tokio::test]
async fn a_pre_cancelled_request_does_not_reach_the_network() {
    let (base, bodies, _paths) = mock_server(recorded_sse()).await;
    let provider = DeepSeekProvider::new(DeepSeekConfig::new(&base, "k")).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = provider.stream(LlmRequest::new("m", vec![]), cancel).await;

    assert!(matches!(result, Err(girder_core::LlmError::Cancelled)));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        bodies.lock().unwrap().is_empty(),
        "a cancelled request must not be sent"
    );
}

#[test]
fn the_environment_config_is_absent_here() {
    // Documents the local state rather than asserting a value: without a key the live provider is
    // unavailable, which is why the tests above use a mock server.
    let from_env = DeepSeekConfig::from_env();
    if std::env::var("DEEPSEEK_API_KEY").is_ok() {
        assert!(from_env.is_some(), "a set key must yield a config");
    } else {
        assert!(from_env.is_none(), "no key means no live provider");
    }
}
