//! The vertical slice: one end-to-end conversation against a fake provider.
//!
//! The plan's phase 4 acceptance is `girder "列出当前目录的文件"` streaming an answer, calling a tool,
//! and continuing. That needs a real provider; this file proves the *machinery* is sound with a
//! scripted one, which is the part that can be verified deterministically. The live-provider run is
//! a separate, opt-in check (see `tests/live_provider.rs`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use girder_core::{
    AgentId, AssembleError, AssistantMessage, ChunkStream, ContentBlock, EventBus, FinishReason,
    LlmError, LlmProvider, LlmRequest, LoopContext, LoopError, LoopStep, MemorySession,
    PostToolDecision, PreToolDecision, Session, SessionEvent, StreamChunk, ToolDefinition,
    ToolExecute, ToolExecution, ToolPostExecute, ToolPreExecute, ToolRegistry, ToolRegistryView,
    ToolResult,
};
use tokio_util::sync::CancellationToken;

// ---- a scripted provider ------------------------------------------------------------------------

/// A provider that replays a fixed script and records what it was asked.
struct ScriptedProvider {
    script: Mutex<Vec<Vec<StreamChunk>>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl ScriptedProvider {
    fn new(script: Vec<Vec<StreamChunk>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<LlmRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl LlmProvider for ScriptedProvider {
    fn stream(
        &self,
        request: LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<ChunkStream, LlmError>> {
        self.requests.lock().unwrap().push(request);
        let next = self.script.lock().unwrap().remove(0);
        Box::pin(async move {
            let stream = futures::stream::iter(next);
            Ok(Box::pin(stream) as ChunkStream)
        })
    }
}

// ---- a real tool --------------------------------------------------------------------------------

/// Lists "directory entries" — a stand-in for the bash/read tools phase 5 will build.
fn list_tool(calls: Arc<AtomicUsize>) -> ToolDefinition {
    ToolDefinition::new(
        "list",
        "Lists the files in a directory",
        serde_json::json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": []
        }),
        Arc::new(move |args: serde_json::Value, _ctx| {
            calls.fetch_add(1, Ordering::SeqCst);
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".")
                .to_owned();
            Box::pin(async move { Ok(ToolResult::text(format!("{path}/a.txt\n{path}/b.txt"))) })
        }),
    )
}

fn text_chunks(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "text".to_owned(),
        },
        StreamChunk::TextDelta {
            index: 0,
            text: text.to_owned(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text {
                text: text.to_owned(),
            },
        },
        StreamChunk::Usage(girder_core::TokenUsage {
            input: 10,
            output: 5,
            cached: 0,
        }),
        StreamChunk::Finish {
            reason: FinishReason::Stop,
        },
    ]
}

fn tool_call_chunks(id: &str, name: &str, arguments: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "tool-call".to_owned(),
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: id.to_owned(),
            name: Some(name.to_owned()),
            arguments_delta: arguments.to_owned(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments: arguments.to_owned(),
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
        },
    ]
}

fn context(
    provider: Arc<ScriptedProvider>,
    tools: ToolRegistry,
    session: Arc<dyn Session>,
) -> Arc<LoopContext> {
    let mut registry = tools;
    let _ = &mut registry;
    Arc::new(LoopContext::new(
        provider,
        Arc::new(registry) as Arc<dyn ToolRegistryView>,
        session,
        Arc::new(EventBus::new()),
        "deepseek-test",
        AgentId("agent-1".to_owned()),
    ))
}

// ---- the slice ----------------------------------------------------------------------------------

#[tokio::test]
async fn one_turn_streams_an_answer_and_logs_it() {
    let provider = ScriptedProvider::new(vec![text_chunks("hi there")]);
    let session = Arc::new(MemorySession::new());
    let ctx = context(Arc::clone(&provider), ToolRegistry::new(), session.clone());

    let mut stream = Box::pin(girder_core::run(ctx, "hello", CancellationToken::new()));
    let mut steps = Vec::new();
    while let Some(step) = stream.next().await {
        steps.push(step.unwrap());
    }

    // The final step is the turn end with a finish reason.
    assert!(matches!(
        steps.last(),
        Some(LoopStep::TurnEnded {
            reason: FinishReason::Stop,
            ..
        })
    ));

    // The text reached the caller as deltas.
    let text: String = steps
        .iter()
        .filter_map(|step| match step {
            LoopStep::Chunk(StreamChunk::TextDelta { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hi there");

    // The session recorded the conversation.
    let events = session.events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::UserMessage { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::AssistantMessage { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::TurnEnd { turn: 1, .. }))
    );
}

#[tokio::test]
async fn the_loop_calls_a_tool_then_continues_to_a_final_answer() {
    // The acceptance path: model asks for a tool, harness runs it, model answers.
    let provider = ScriptedProvider::new(vec![
        tool_call_chunks("call-1", "list", r#"{"path":"/tmp"}"#),
        text_chunks("there are two files"),
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = ToolRegistry::new();
    tools.register(list_tool(Arc::clone(&calls)));
    let session = Arc::new(MemorySession::new());
    let ctx = context(Arc::clone(&provider), tools, session.clone());

    let mut stream = Box::pin(girder_core::run(
        ctx,
        "list files",
        CancellationToken::new(),
    ));
    let mut steps = Vec::new();
    while let Some(step) = stream.next().await {
        steps.push(step.unwrap());
    }

    assert_eq!(calls.load(Ordering::SeqCst), 1, "the tool ran exactly once");
    let tool_steps: Vec<&LoopStep> = steps
        .iter()
        .filter(|step| {
            matches!(
                step,
                LoopStep::ToolStarted { .. } | LoopStep::ToolFinished { .. }
            )
        })
        .collect();
    assert_eq!(tool_steps.len(), 2, "started and finished");

    // Two turns were logged: the tool call, then the answer.
    let events = session.events();
    let turns: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::TurnStart { turn } => Some(*turn),
            _ => None,
        })
        .collect();
    assert_eq!(turns, vec![1, 2]);

    // The second request carried the tool result back to the model.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.role == girder_core::Role::Tool),
        "the tool result must be in the second request: {second:?}"
    );
    assert!(
        second
            .messages
            .iter()
            .any(|m| m.role == girder_core::Role::Assistant),
        "the model must see its own tool call"
    );
}

#[tokio::test]
async fn the_tool_schemas_are_sent_to_the_model() {
    let provider = ScriptedProvider::new(vec![text_chunks("ok")]);
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = ToolRegistry::new();
    tools.register(list_tool(calls));
    let ctx = context(Arc::clone(&provider), tools, Arc::new(MemorySession::new()));

    let mut stream = Box::pin(girder_core::run(ctx, "hi", CancellationToken::new()));
    while stream.next().await.is_some() {}

    let request = &provider.requests()[0];
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].name, "list");
    assert_eq!(request.model, "deepseek-test");
}

#[tokio::test]
async fn cancellation_stops_the_loop() {
    let provider = ScriptedProvider::new(vec![text_chunks("never seen")]);
    let ctx = context(
        Arc::clone(&provider),
        ToolRegistry::new(),
        Arc::new(MemorySession::new()),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();

    let mut stream = Box::pin(girder_core::run(ctx, "hi", cancel));
    let mut saw_cancel = false;
    while let Some(step) = stream.next().await {
        if matches!(step, Err(LoopError::Cancelled)) {
            saw_cancel = true;
        }
    }

    assert!(saw_cancel, "a pre-cancelled loop must report cancellation");
    assert!(
        provider.requests().is_empty(),
        "a cancelled loop must not call the provider"
    );
}

// ---- the three-waterfall pipeline ----------------------------------------------------------------

fn execution(name: &str) -> ToolExecution {
    ToolExecution {
        call_id: "call-1".to_owned(),
        name: name.to_owned(),
        arguments: "{}".to_owned(),
        agent_id: AgentId("agent-1".to_owned()),
    }
}

fn pipeline_bus() -> Arc<EventBus> {
    Arc::new(EventBus::new())
}

fn tool_registry_with(name: &str, calls: Arc<AtomicUsize>) -> ToolRegistry {
    let tools = ToolRegistry::new();
    tools.register(ToolDefinition::new(
        name,
        "test tool",
        serde_json::json!({ "type": "object" }),
        Arc::new(move |_args, _ctx| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(ToolResult::text("ran")) })
        }),
    ));
    tools
}

fn pipeline_context(tools: ToolRegistry, bus: Arc<EventBus>) -> Arc<LoopContext> {
    let provider = ScriptedProvider::new(vec![]);
    Arc::new(LoopContext::new(
        provider,
        Arc::new(tools) as Arc<dyn ToolRegistryView>,
        Arc::new(MemorySession::new()),
        bus,
        "m",
        AgentId("a".to_owned()),
    ))
}

#[tokio::test]
async fn pre_execute_deny_stops_the_tool_from_running() {
    let calls = Arc::new(AtomicUsize::new(0));
    let bus = pipeline_bus();
    bus.on_waterfall::<ToolPreExecute, _>("policy", girder_core::Order::Append, |_args, _next| {
        Box::pin(async {
            PreToolDecision::Deny {
                reason: "not allowed here".to_owned(),
            }
        })
    });
    let ctx = pipeline_context(tool_registry_with("list", Arc::clone(&calls)), bus);

    let result = girder_core::dispatch(&ctx, execution("list"), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 0, "the body must not run");
    assert!(result.is_error);
    assert!(result.content[0].clone().type_name() == "text");
}

#[tokio::test]
async fn pre_execute_allow_lets_the_tool_run() {
    let calls = Arc::new(AtomicUsize::new(0));
    let bus = pipeline_bus();
    bus.on_waterfall::<ToolPreExecute, _>("policy", girder_core::Order::Append, |_args, next| {
        Box::pin(async move {
            let _ = next;
            PreToolDecision::Allow
        })
    });
    let ctx = pipeline_context(tool_registry_with("list", Arc::clone(&calls)), bus);

    let result = girder_core::dispatch(&ctx, execution("list"), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!result.is_error);
}

#[tokio::test]
async fn post_execute_can_block_a_result_into_feedback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let bus = pipeline_bus();
    bus.on_waterfall::<ToolPostExecute, _>("policy", girder_core::Order::Append, |_args, _next| {
        Box::pin(async {
            PostToolDecision::Block {
                feedback: vec![ContentBlock::Text {
                    text: "try a smaller directory".to_owned(),
                }],
            }
        })
    });
    let ctx = pipeline_context(tool_registry_with("list", Arc::clone(&calls)), bus);

    let result = girder_core::dispatch(&ctx, execution("list"), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1, "the body still ran");
    assert!(result.is_error, "the block turns it into an error");
    assert_eq!(
        result.content,
        vec![ContentBlock::Text {
            text: "try a smaller directory".to_owned()
        }]
    );
}

#[tokio::test]
async fn the_execute_waterfall_wraps_the_body() {
    // A wrapper that replaces the result without calling next: the body never runs.
    let calls = Arc::new(AtomicUsize::new(0));
    let bus = pipeline_bus();
    bus.on_waterfall::<ToolExecute, _>("cache", girder_core::Order::Append, |_args, _next| {
        Box::pin(async { ToolResult::text("cached") })
    });
    let ctx = pipeline_context(tool_registry_with("list", Arc::clone(&calls)), bus);

    let result = girder_core::dispatch(&ctx, execution("list"), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a vetoing wrapper skips the body"
    );
    assert_eq!(
        result.content,
        vec![ContentBlock::Text {
            text: "cached".to_owned()
        }]
    );
}

#[tokio::test]
async fn an_unknown_tool_returns_a_model_facing_error() {
    let ctx = pipeline_context(ToolRegistry::new(), pipeline_bus());

    let result = girder_core::dispatch(&ctx, execution("nope"), CancellationToken::new())
        .await
        .unwrap();

    assert!(result.is_error);
    assert!(matches!(
        &result.content[0],
        ContentBlock::Text { text } if text.contains("no tool named")
    ));
}

#[tokio::test]
async fn invalid_json_arguments_are_a_model_facing_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let ctx = pipeline_context(
        tool_registry_with("list", Arc::clone(&calls)),
        pipeline_bus(),
    );
    let mut bad = execution("list");
    bad.arguments = "{not json".to_owned();

    let result = girder_core::dispatch(&ctx, bad, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the body must not see bad arguments"
    );
    assert!(result.is_error);
}

// ---- assembly -----------------------------------------------------------------------------------

#[test]
fn assembling_a_stream_folds_deltas_into_blocks() {
    let chunks = vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "text".to_owned(),
        },
        StreamChunk::TextDelta {
            index: 0,
            text: "he".to_owned(),
        },
        StreamChunk::TextDelta {
            index: 0,
            text: "llo".to_owned(),
        },
        StreamChunk::Finish {
            reason: FinishReason::Stop,
        },
    ];

    let message = AssistantMessage::from_chunks(chunks).unwrap();

    assert_eq!(message.text(), "hello");
    assert_eq!(message.finish, Some(FinishReason::Stop));
}

#[test]
fn assembly_accumulates_streamed_tool_arguments() {
    // Providers stream tool arguments as fragments; they must concatenate into one JSON string.
    let chunks = vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "tool-call".to_owned(),
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: "c1".to_owned(),
            name: Some("list".to_owned()),
            arguments_delta: "{\"pa".to_owned(),
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: "c1".to_owned(),
            name: None,
            arguments_delta: "th\":\"/tmp\"}".to_owned(),
        },
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
        },
    ];

    let message = AssistantMessage::from_chunks(chunks).unwrap();
    let calls = message.tool_calls();

    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "list");
    assert_eq!(calls[0].2, r#"{"path":"/tmp"}"#);
}

#[test]
fn a_stream_without_a_finish_chunk_is_an_error() {
    // A silently truncated answer must not look like a complete one.
    let chunks = vec![StreamChunk::TextDelta {
        index: 0,
        text: "partial".to_owned(),
    }];

    let error =
        AssistantMessage::from_chunks(chunks).expect_err("must not accept a truncated stream");
    assert!(matches!(error, AssembleError::Unfinished));
}

#[test]
fn interleaved_blocks_keep_their_positions() {
    let chunks = vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "reasoning".to_owned(),
        },
        StreamChunk::ReasoningDelta {
            index: 0,
            text: "thinking".to_owned(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Reasoning {
                text: "thinking".to_owned(),
            },
        },
        StreamChunk::BlockStart {
            index: 1,
            block_type: "text".to_owned(),
        },
        StreamChunk::TextDelta {
            index: 1,
            text: "answer".to_owned(),
        },
        StreamChunk::BlockEnd {
            index: 1,
            block: ContentBlock::Text {
                text: "answer".to_owned(),
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::Stop,
        },
    ];

    let message = AssistantMessage::from_chunks(chunks).unwrap();

    assert_eq!(message.content.len(), 2);
    assert_eq!(message.content[0].type_name(), "reasoning");
    assert_eq!(message.content[1].type_name(), "text");
    assert_eq!(message.text(), "answer");
}

// ---- session ------------------------------------------------------------------------------------

#[test]
fn the_session_round_trips_through_zstd_jsonl() {
    let dir = std::env::temp_dir().join(format!("girder-session-{}", std::process::id()));
    let path = dir.join("session.jsonl.zst");
    let store = girder_core::SessionStore::new(&path);

    let events = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::UserMessage {
            content: vec![ContentBlock::Text {
                text: "hello".to_owned(),
            }],
        },
        SessionEvent::AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "hi".to_owned(),
            }],
        },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: FinishReason::Stop,
        },
    ];
    for event in &events {
        store.append(event).unwrap();
    }

    let replayed = girder_core::read_session_file(&path).unwrap();
    assert_eq!(replayed, events);

    // The file really is compressed: its bytes are not the JSONL text.
    let raw = std::fs::read(&path).unwrap();
    assert!(
        raw.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]),
        "zstd magic number"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_session_file_reads_as_empty() {
    let store = girder_core::SessionStore::new("/nonexistent/girder/session.jsonl.zst");
    assert!(store.read_all().unwrap().is_empty());
}

#[test]
fn session_events_have_stable_type_tags() {
    // The tags are the wire format; changing one silently breaks every existing log.
    assert_eq!(
        SessionEvent::TurnStart { turn: 1 }.type_name(),
        "turn/start"
    );
    assert_eq!(
        SessionEvent::ToolCall {
            id: "c".to_owned(),
            name: "n".to_owned(),
            arguments: "{}".to_owned()
        }
        .type_name(),
        "tool/call"
    );

    let json = serde_json::to_value(SessionEvent::TurnStart { turn: 1 }).unwrap();
    assert_eq!(json["type"], "turn/start");
    assert_eq!(json["turn"], 1);
}

#[test]
fn an_unknown_event_type_is_rejected_not_skipped() {
    // A log written by a newer version must fail loudly rather than replay a partial history.
    let error = serde_json::from_str::<SessionEvent>(r#"{"type":"turn/rewind","turn":1}"#)
        .expect_err("unknown variants must not deserialize");
    assert!(error.to_string().contains("turn/rewind") || error.to_string().contains("unknown"));
}

// ---- session concurrency -------------------------------------------------------------------------

#[test]
fn concurrent_appends_do_not_lose_events() {
    // `append` is read-modify-write over the whole compressed frame: decode, push, rewrite. Without
    // a per-path lock, interleaved appends drop each other's events and every writer shares one
    // staging file. Measured before the lock: 80 concurrent appends left 12 events and 41 errors.
    let dir = std::env::temp_dir().join(format!("girder-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("race.jsonl.zst");
    let store = Arc::new(girder_core::SessionStore::new(&path));

    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let handles: Vec<_> = (0..8)
        .map(|worker| {
            let store = Arc::clone(&store);
            let errors = Arc::clone(&errors);
            std::thread::spawn(move || {
                for index in 0..10 {
                    let turn = u64::try_from(worker * 10 + index).expect("test ids are small");
                    if let Err(error) = store.append(&SessionEvent::TurnStart { turn }) {
                        errors.lock().unwrap().push(error.to_string());
                    }
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let events = girder_core::read_session_file(&path).unwrap();
    assert_eq!(events.len(), 80, "every append must survive");
    assert!(
        errors.lock().unwrap().is_empty(),
        "no append may fail: {:?}",
        errors.lock().unwrap()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_appends_keep_the_log_readable() {
    // The staging-file collision corrupted the frame itself on the original probe
    // ("Unknown frame descriptor"), which would make a session unrecoverable rather than merely
    // short.
    let dir = std::env::temp_dir().join(format!("girder-race2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("race.jsonl.zst");
    let store = Arc::new(girder_core::SessionStore::new(&path));

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                for _ in 0..5 {
                    store
                        .append(&SessionEvent::TurnStart { turn: 1 })
                        .expect("append must not fail");
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    // A corrupt frame surfaces here as a decode error rather than a short read.
    let events = girder_core::read_session_file(&path).expect("the log must decode");
    assert_eq!(events.len(), 20);

    let _ = std::fs::remove_dir_all(&dir);
}
