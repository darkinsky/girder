//! Tests for the request fingerprint and the response cache.
//!
//! The tests are weighted toward one failure: a fingerprint that misses a field returns another
//! request's answer, which looks like a working cache. Every field of [`LlmRequest`] therefore gets
//! its own test that changes only that field, so a field dropped from the encoding shows up as one
//! failing test rather than as a subtly wrong response in production.
//!
//! The rest pins the two behaviours a cache is judged on: eviction of the *least recently used*
//! entry rather than the oldest one, and a hit rate that is honest about an unused cache.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against the cache as written even before the crate root wires it in. The module spells its own
//! dependencies through `girder_core::`, the alias the library declares for itself, so the same
//! source builds here and inside the crate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

#[path = "../src/response_cache.rs"]
mod response_cache;

use girder_core::{ContentBlock, FinishReason, LlmRequest, Message, Role, StreamChunk, ToolSchema};
use response_cache::{Cache, CachedResponse, Fingerprint, Stats, is_cacheable};

// ---- fixtures ----------------------------------------------------------------------------------

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_owned(),
    }
}

fn user_message(text: &str) -> Message {
    Message::new(Role::User, vec![text_block(text)])
}

fn schema(name: &str, description: &str, parameters: serde_json::Value) -> ToolSchema {
    ToolSchema {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters,
    }
}

fn parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"],
    })
}

/// The baseline request every variant test starts from.
///
/// It sets every field, including the optional ones, so a test that changes one field is actually
/// exercising that field rather than flipping it between absent and present.
fn baseline() -> LlmRequest {
    LlmRequest::new("deepseek-chat", vec![user_message("hello")])
        .with_system_prompt("you are a test harness")
        .with_max_tokens(512)
        .with_tools(vec![schema("read", "reads a file", parameters())])
}

fn fingerprint(request: &LlmRequest) -> Fingerprint {
    Fingerprint::of(request)
}

fn response(text: &str) -> CachedResponse {
    // The text is carried through the chunks so two cached responses are distinguishable; the stamp
    // is fixed so the fixture is deterministic.
    CachedResponse {
        chunks: vec![
            StreamChunk::TextDelta {
                index: 0,
                text: text.to_owned(),
            },
            StreamChunk::Finish {
                reason: FinishReason::Stop,
            },
        ],
        stored_ms: 1_000,
    }
}

// ---- Fingerprint: the same request -------------------------------------------------------------

#[test]
fn the_same_request_fingerprints_the_same() {
    assert_eq!(fingerprint(&baseline()), fingerprint(&baseline()));
}

#[test]
fn a_rebuilt_request_fingerprints_the_same() {
    // Equality has to survive the request being constructed twice rather than cloned, because that
    // is what a repeated prompt actually does.
    let again = LlmRequest::new("deepseek-chat", vec![user_message("hello")])
        .with_system_prompt("you are a test harness")
        .with_max_tokens(512)
        .with_tools(vec![schema("read", "reads a file", parameters())]);
    assert_eq!(
        fingerprint(&baseline()).as_str(),
        fingerprint(&again).as_str()
    );
}

#[test]
fn tool_parameter_key_order_does_not_change_the_fingerprint() {
    // Two JSON objects with the same members in a different construction order are the same schema,
    // and treating them as different would cost a hit for a reason the provider cannot see.
    let first = llm_with_parameters(serde_json::json!({ "a": 1, "b": 2 }));
    let second = llm_with_parameters(serde_json::json!({ "b": 2, "a": 1 }));
    assert_eq!(fingerprint(&first), fingerprint(&second));
}

fn llm_with_parameters(parameters: serde_json::Value) -> LlmRequest {
    LlmRequest::new("deepseek-chat", vec![user_message("hello")])
        .with_system_prompt("you are a test harness")
        .with_max_tokens(512)
        .with_tools(vec![schema("read", "reads a file", parameters)])
}

// ---- Fingerprint: every field that changes the answer changes the key -------------------------
//
// Each test below changes exactly one field of `baseline()` and asserts a different fingerprint. A
// field missing from the encoding fails its own test and no other, which is the point.

#[test]
fn a_different_model_is_not_a_hit() {
    let mut other = baseline();
    other.model = "deepseek-reasoner".to_owned();
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn a_different_system_prompt_is_not_a_hit() {
    let mut other = baseline();
    other.system_prompt = Some("you are a different harness".to_owned());
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn a_missing_system_prompt_is_not_a_hit() {
    let mut other = baseline();
    other.system_prompt = None;
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn an_empty_system_prompt_differs_from_an_absent_one() {
    // A provider distinguishes the two on the wire (no `system` key versus an empty string), so the
    // cache must as well.
    let absent = llm_without_system();
    let mut empty = absent.clone();
    empty.system_prompt = Some(String::new());
    assert_ne!(fingerprint(&absent), fingerprint(&empty));
}

fn llm_without_system() -> LlmRequest {
    let mut request = baseline();
    request.system_prompt = None;
    request
}

#[test]
fn a_different_max_tokens_is_not_a_hit() {
    // A smaller limit truncates the answer, so serving the longer cached one would break the
    // caller's budget.
    let mut other = baseline();
    other.max_tokens = Some(256);
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn a_missing_max_tokens_is_not_a_hit() {
    let mut other = baseline();
    other.max_tokens = None;
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn a_different_message_text_is_not_a_hit() {
    let mut other = baseline();
    other.messages = vec![user_message("hello!")];
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn a_different_message_role_is_not_a_hit() {
    let mut other = baseline();
    other.messages = vec![Message::new(Role::Assistant, vec![text_block("hello")])];
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn a_different_message_count_is_not_a_hit() {
    // A prefix of a conversation is a different question, which is the case a cache keyed on the
    // last message alone gets wrong.
    let mut other = baseline();
    other.messages.push(user_message("hello"));
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn a_different_message_order_is_not_a_hit() {
    let mut first = baseline();
    first.messages = vec![user_message("one"), user_message("two")];
    let mut second = baseline();
    second.messages = vec![user_message("two"), user_message("one")];
    assert_ne!(fingerprint(&first), fingerprint(&second));
}

#[test]
fn a_different_tool_call_id_is_not_a_hit() {
    // The provider keys a `tool_result` block by this id, so two results that answer different
    // calls are different requests even when their text matches.
    let mut first = baseline();
    first.messages = vec![Message::tool_result(
        girder_core::ToolCallId::new("call-1").expect("valid test id"),
        vec![text_block("ok")],
    )];
    let mut second = baseline();
    second.messages = vec![Message::tool_result(
        girder_core::ToolCallId::new("call-2").expect("valid test id"),
        vec![text_block("ok")],
    )];
    assert_ne!(fingerprint(&first), fingerprint(&second));
}

#[test]
fn a_missing_tool_call_id_is_not_a_hit() {
    let mut with_id = baseline();
    with_id.messages = vec![Message::tool_result(
        girder_core::ToolCallId::new("call-1").expect("valid test id"),
        vec![text_block("ok")],
    )];
    let mut without_id = with_id.clone();
    without_id.messages = vec![Message::new(Role::Tool, vec![text_block("ok")])];
    assert_ne!(fingerprint(&with_id), fingerprint(&without_id));
}

#[test]
fn a_different_block_kind_is_not_a_hit() {
    // Reasoning and text are separate channels; a provider that receives one where the other was
    // sent produces a different answer.
    let mut first = baseline();
    first.messages = vec![Message::new(
        Role::User,
        vec![ContentBlock::Text {
            text: "same".to_owned(),
        }],
    )];
    let mut second = baseline();
    second.messages = vec![Message::new(
        Role::User,
        vec![ContentBlock::Reasoning {
            text: "same".to_owned(),
        }],
    )];
    assert_ne!(fingerprint(&first), fingerprint(&second));
}

#[test]
fn a_different_attachment_is_not_a_hit() {
    let mut first = baseline();
    first.messages = vec![Message::new(
        Role::User,
        vec![ContentBlock::Image {
            attachment: "image-a".to_owned(),
        }],
    )];
    let mut second = baseline();
    second.messages = vec![Message::new(
        Role::User,
        vec![ContentBlock::Image {
            attachment: "image-b".to_owned(),
        }],
    )];
    assert_ne!(fingerprint(&first), fingerprint(&second));

    let mut file = baseline();
    file.messages = vec![Message::new(
        Role::User,
        vec![ContentBlock::File {
            attachment: "image-a".to_owned(),
        }],
    )];
    // A file and an image with the same handle are still different content.
    assert_ne!(fingerprint(&first), fingerprint(&file));
}

#[test]
fn a_different_tool_call_is_not_a_hit() {
    let mut first = baseline();
    first.messages = vec![Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall {
            id: girder_core::ToolCallId::new("call-1").expect("valid test id"),
            name: "read".to_owned(),
            arguments: "{\"path\":\"a\"}".to_owned(),
        }],
    )];
    let mut second = first.clone();
    second.messages = vec![Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall {
            id: girder_core::ToolCallId::new("call-1").expect("valid test id"),
            name: "read".to_owned(),
            arguments: "{\"path\":\"b\"}".to_owned(),
        }],
    )];
    assert_ne!(fingerprint(&first), fingerprint(&second));
}

#[test]
fn a_different_tool_name_is_not_a_hit() {
    let mut other = baseline();
    other.tools = vec![schema("write", "reads a file", parameters())];
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn a_different_tool_description_is_not_a_hit() {
    // The description is prompt text: rewording it changes what the model is told it may do.
    let mut other = baseline();
    other.tools = vec![schema("read", "reads a file, slowly", parameters())];
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn a_different_tool_schema_is_not_a_hit() {
    let mut other = baseline();
    other.tools = vec![schema(
        "read",
        "reads a file",
        serde_json::json!({ "type": "object", "properties": {} }),
    )];
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn a_different_tool_count_is_not_a_hit() {
    let mut other = baseline();
    other.tools.push(schema(
        "write",
        "writes a file",
        serde_json::json!({ "type": "object" }),
    ));
    assert_ne!(fingerprint(&baseline()), fingerprint(&other));
}

#[test]
fn values_containing_the_delimiter_do_not_collide() {
    // The encoding is length-prefixed precisely so that moving text across a field boundary cannot
    // produce the same string for two different requests.
    let mut first = baseline();
    first.system_prompt = Some("ab;c".to_owned());
    let mut second = baseline();
    second.system_prompt = Some("ab".to_owned());
    second.model = "deepseek-chat;c".to_owned();
    assert_ne!(fingerprint(&first), fingerprint(&second));
}

// ---- Cache: hits -------------------------------------------------------------------------------

#[test]
fn a_repeated_request_is_served_from_the_cache() {
    let mut cache = Cache::new(4);
    let key = fingerprint(&baseline());
    cache.put(key.clone(), response("first"));

    let hit = cache.get(&key).expect("the response just stored");
    assert_eq!(hit.chunks.len(), 2);
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.stats(), Stats { hits: 1, misses: 0 });
}

#[test]
fn a_different_request_misses() {
    let mut cache = Cache::new(4);
    cache.put(fingerprint(&baseline()), response("first"));

    let mut other = baseline();
    other.model = "deepseek-reasoner".to_owned();
    assert!(cache.get(&fingerprint(&other)).is_none());
    assert_eq!(cache.stats(), Stats { hits: 0, misses: 1 });
}

#[test]
fn storing_the_same_key_twice_replaces_the_response() {
    let mut cache = Cache::new(4);
    let key = fingerprint(&baseline());
    cache.put(key.clone(), response("first"));
    cache.put(key.clone(), response("second"));

    assert_eq!(cache.len(), 1);
    let hit = cache.get(&key).expect("the replacement");
    assert_eq!(
        hit.chunks,
        vec![
            StreamChunk::TextDelta {
                index: 0,
                text: "second".to_owned(),
            },
            StreamChunk::Finish {
                reason: FinishReason::Stop,
            },
        ]
    );
}

// ---- Cache: LRU eviction -----------------------------------------------------------------------

#[test]
fn the_least_recently_used_entry_is_evicted() {
    let mut cache = Cache::new(2);
    let a = fingerprint(&request_for("a"));
    let b = fingerprint(&request_for("b"));
    let c = fingerprint(&request_for("c"));

    cache.put(a.clone(), response("a"));
    cache.put(b.clone(), response("b"));
    // Reading `a` makes `b` the least recently used, even though `a` is older.
    assert!(cache.get(&a).is_some());

    cache.put(c.clone(), response("c"));

    assert!(cache.get(&b).is_none(), "b was the least recently used");
    assert!(cache.get(&a).is_some(), "a was used more recently than b");
    assert!(cache.get(&c).is_some(), "c was just stored");
    assert_eq!(cache.len(), 2);
}

#[test]
fn a_rewritten_entry_becomes_the_most_recently_used() {
    let mut cache = Cache::new(2);
    let a = fingerprint(&request_for("a"));
    let b = fingerprint(&request_for("b"));
    let c = fingerprint(&request_for("c"));

    cache.put(a.clone(), response("a"));
    cache.put(b.clone(), response("b"));
    // Rewriting `a` must count as a use; otherwise `a` would still be the eviction victim.
    cache.put(a.clone(), response("a again"));
    cache.put(c.clone(), response("c"));

    assert!(cache.get(&a).is_some());
    assert!(cache.get(&b).is_none());
    assert!(cache.get(&c).is_some());
}

#[test]
fn eviction_keeps_the_cache_at_capacity() {
    let mut cache = Cache::new(3);
    for index in 0..10 {
        let key = fingerprint(&request_for(&format!("prompt-{index}")));
        cache.put(key, response("x"));
        assert!(cache.len() <= 3);
    }
    assert_eq!(cache.len(), 3);
}

fn request_for(prompt: &str) -> LlmRequest {
    LlmRequest::new("deepseek-chat", vec![user_message(prompt)])
}

// ---- Cache: capacity zero ----------------------------------------------------------------------

#[test]
fn a_zero_capacity_cache_stores_nothing() {
    let mut cache = Cache::new(0);
    let key = fingerprint(&baseline());

    cache.put(key.clone(), response("first"));

    assert_eq!(cache.len(), 0);
    assert!(cache.is_empty());
    assert_eq!(cache.capacity(), 0);
    assert!(cache.get(&key).is_none());
    // A store into a disabled cache is not a hit; the lookup that follows is an honest miss.
    assert_eq!(cache.stats(), Stats { hits: 0, misses: 1 });
}

// ---- Stats -------------------------------------------------------------------------------------

#[test]
fn the_hit_rate_is_hits_over_requests() {
    let mut cache = Cache::new(4);
    let key = fingerprint(&baseline());
    cache.put(key.clone(), response("first"));

    assert!(cache.get(&key).is_some());
    assert!(cache.get(&fingerprint(&request_for("other"))).is_none());

    let stats = cache.stats();
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 1);
    assert_eq!(stats.requests(), 2);
    assert_eq!(stats.hit_rate(), 0.5);
}

#[test]
fn the_hit_rate_of_an_unused_cache_is_zero() {
    let cache = Cache::new(4);
    let stats = cache.stats();
    assert_eq!(stats, Stats { hits: 0, misses: 0 });
    assert_eq!(stats.requests(), 0);
    // Zero rather than NaN: a NaN here would poison any average it entered.
    assert_eq!(stats.hit_rate(), 0.0);
}

#[test]
fn the_hit_rate_is_one_when_every_lookup_hits() {
    let mut cache = Cache::new(4);
    let key = fingerprint(&baseline());
    cache.put(key.clone(), response("first"));
    assert!(cache.get(&key).is_some());
    assert!(cache.get(&key).is_some());
    assert_eq!(cache.stats().hit_rate(), 1.0);
}

#[test]
fn putting_without_looking_up_is_not_a_miss() {
    let mut cache = Cache::new(4);
    cache.put(fingerprint(&baseline()), response("first"));
    assert_eq!(cache.stats(), Stats { hits: 0, misses: 0 });
}

// ---- Cacheability ------------------------------------------------------------------------------

#[test]
fn a_zero_temperature_request_is_cacheable() {
    assert!(is_cacheable(Some(0.0)));
}

#[test]
fn a_sampled_request_is_not_cacheable() {
    // Serving one draw from a distribution back as if it were the only possible answer would change
    // what a test is testing.
    assert!(!is_cacheable(Some(0.7)));
    assert!(!is_cacheable(Some(1.0)));
}

#[test]
fn an_unspecified_temperature_is_not_cacheable() {
    // `None` defers to the provider's default, which this module cannot see, so the safe answer is
    // "do not cache" rather than "assume deterministic".
    assert!(!is_cacheable(None));
}

#[test]
fn a_non_finite_temperature_is_not_cacheable() {
    assert!(!is_cacheable(Some(f32::NAN)));
    assert!(!is_cacheable(Some(f32::INFINITY)));
}

// ---- CachedResponse ----------------------------------------------------------------------------

#[test]
fn a_stored_response_can_be_aged() {
    let stored = CachedResponse {
        chunks: Vec::new(),
        stored_ms: 1_000,
    };
    assert_eq!(stored.age_ms(1_500), 500);
    // A clock that moved backwards reads as "just stored" rather than wrapping to a huge age.
    assert_eq!(stored.age_ms(500), 0);
    assert_eq!(stored.age_ms(1_000), 0);
}

#[test]
fn a_new_response_is_stamped_with_the_current_time() {
    // `now_millis` is the process's wall clock; the assertion is only that the stamp is a real
    // timestamp, not that it equals a value captured before the call.
    let stored = CachedResponse::new(Vec::new());
    assert_eq!(stored.age_ms(stored.stored_ms), 0);
    assert!(stored.stored_ms > 0);
}
