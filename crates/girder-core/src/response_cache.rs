//! A response cache keyed by a request fingerprint.
//!
//! Nothing in the harness reuses a provider response today: [`crate::agent_loop`] builds an
//! [`LlmRequest`] and awaits `provider.stream` on every turn, so a test environment that runs the
//! same prompt a hundred times pays for a hundred calls. This module is the memory such a caller
//! needs: a fingerprint that says "these two requests are the same question" and a bounded store of
//! the chunks that answered the first one.
//!
//! # The one thing this module cannot get wrong
//!
//! A fingerprint that omits a field which reaches the provider does not lose a cache hit — it
//! *returns the wrong answer*, silently, with the shape of a real response. Every field of
//! [`LlmRequest`] therefore contributes to the fingerprint, and the encoding is length-prefixed
//! rather than a digest so two different requests cannot collide at all. A 64-bit hash would halve
//! the stored bytes and, on a birthday-bound schedule, hand a caller someone else's answer; a
//! bounded cache is not worth that trade. A caller that wants compact keys can hash
//! [`Fingerprint::as_str`] itself and take responsibility for the collision.
//!
//! # Sampling is not in the request
//!
//! [`LlmRequest`] has no `temperature` field: sampling belongs to [`CallConfig`] and is applied by
//! the provider, so this module cannot read it off the request and the fingerprint does not include
//! it. That is exactly why [`is_cacheable`] exists as an explicit gate: a request sampled at a
//! non-zero temperature answers from a distribution, and serving one sample again because the
//! prompt repeated would turn "the model might say something else" into "the model always says
//! this". A caller must ask [`is_cacheable`] before either [`Cache::get`] or [`Cache::put`].
//!
//! [`CallConfig`]: crate::CallConfig

#![allow(dead_code)]
// The crate root is wired up separately, so until then nothing in the library references these
// items. The allow is scoped to this module and disappears in effect once it is exported.

use std::collections::HashMap;

use girder_core::{ContentBlock, LlmRequest, Message, Role, StreamChunk, ToolSchema};

/// How many responses a cache built by [`Cache::new`] callers should normally keep.
///
/// Small on purpose: an entry is a whole response, and a cache exists to make a repeated prompt
/// cheap rather than to hold a transcript. Sixty-four is enough for the "same few prompts over and
/// over" shape a test environment has, and small enough that a leak of stale answers stays bounded.
pub const DEFAULT_CAPACITY: usize = 64;

/// A request's identity, for deciding whether two requests are equivalent.
///
/// Two fingerprints are equal only when the requests they were built from would produce the same
/// provider call. The string is a canonical, self-delimiting encoding of every output-affecting
/// field (see [`Fingerprint::of`]) rather than a hash, because equality here decides which answer a
/// caller receives.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// Computes the fingerprint of a request.
    ///
    /// The encoding covers, and is only correct if it keeps covering:
    ///
    /// - `model` — a different model is a different answer;
    /// - `system_prompt` — including the distinction between "absent" and "present but empty";
    /// - `max_tokens` — a lower limit truncates a response that would otherwise be longer;
    /// - every message's `role`, `tool_call_id`, and content blocks in order — the conversation is
    ///   the question, so order and repetition both matter;
    /// - every tool's `name`, `description`, and `parameters` — the tool catalogue is part of the
    ///   prompt, and changing a schema changes what the model is told it may do.
    ///
    /// Each record is written as `tag:byte-len:value;`, so no value can be mistaken for a delimiter
    /// and the encoding is injective: distinct requests get distinct fingerprints. Records are
    /// emitted in a fixed order, and `serde_json`'s map ordering is sorted, so a `parameters` object
    /// built in a different order still fingerprints the same.
    ///
    /// # Panics
    ///
    /// Does not panic. Every value is written with `Display`/`len`, none of which can fail.
    #[must_use]
    pub fn of(request: &LlmRequest) -> Self {
        let mut out = String::new();
        out.push_str("llm-request/v1;");

        push_field(&mut out, "model", &request.model);
        push_optional(&mut out, "system", request.system_prompt.as_deref());
        push_field(
            &mut out,
            "max-tokens",
            &match request.max_tokens {
                // `-` cannot be the start of a decimal, so absent and present are distinct even
                // though both are one record.
                None => "-".to_owned(),
                Some(limit) => limit.to_string(),
            },
        );

        push_field(&mut out, "messages", &request.messages.len().to_string());
        for message in &request.messages {
            encode_message(&mut out, message);
        }

        push_field(&mut out, "tools", &request.tools.len().to_string());
        for tool in &request.tools {
            encode_tool(&mut out, tool);
        }

        Self(out)
    }

    /// The fingerprint's value.
    ///
    /// Stable for a given request shape and safe to use as a map key or a log line; it is not
    /// intended to be parsed back.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Appends one labelled, length-prefixed record.
fn push_field(out: &mut String, tag: &str, value: &str) {
    out.push_str(tag);
    out.push(':');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
    out.push(';');
}

/// Appends an optional string, distinguishing absent from empty.
fn push_optional(out: &mut String, tag: &str, value: Option<&str>) {
    match value {
        None => push_field(out, tag, "-"),
        Some(text) => {
            push_field(out, tag, "+");
            push_field(out, tag, text);
        }
    }
}

/// Appends everything about one message that reaches the provider.
fn encode_message(out: &mut String, message: &Message) {
    push_field(out, "msg-role", role_name(message.role));
    // The provider turns a `tool` message into a `tool_result` block keyed by this id, so two
    // messages that answer different calls are different requests to the API even when their text
    // matches.
    push_optional(
        out,
        "msg-call",
        message.tool_call_id.as_ref().map(girder_core::Id::as_str),
    );
    push_field(out, "msg-blocks", &message.content.len().to_string());
    for block in &message.content {
        encode_block(out, block);
    }
}

/// Appends one content block.
fn encode_block(out: &mut String, block: &ContentBlock) {
    match block {
        ContentBlock::Text { text } => {
            push_field(out, "block", "text");
            push_field(out, "text", text);
        }
        ContentBlock::Reasoning { text } => {
            // Reasoning is not visible output but providers do feed it back on later turns, so a
            // request carrying different reasoning is a different request.
            push_field(out, "block", "reasoning");
            push_field(out, "text", text);
        }
        ContentBlock::ToolCall {
            id,
            name,
            arguments,
        } => {
            push_field(out, "block", "tool-call");
            push_field(out, "call-id", id.as_str());
            push_field(out, "tool-name", name);
            // The raw JSON string, not a parsed value: a parse would normalize whitespace and
            // argument order, and the model saw the original bytes.
            push_field(out, "arguments", arguments);
        }
        ContentBlock::Image { attachment } => {
            push_field(out, "block", "image");
            push_field(out, "attachment", attachment);
        }
        ContentBlock::File { attachment } => {
            push_field(out, "block", "file");
            push_field(out, "attachment", attachment);
        }
    }
}

/// Appends one tool definition.
fn encode_tool(out: &mut String, tool: &ToolSchema) {
    push_field(out, "tool", &tool.name);
    push_field(out, "tool-description", &tool.description);
    // `Display` on a JSON value writes object keys in the map's own order, which `serde_json` keeps
    // sorted without the `preserve_order` feature. Two equal schemas therefore encode identically
    // even if they were constructed in a different order.
    push_field(out, "tool-parameters", &tool.parameters.to_string());
}

/// The name a role has on the wire, matching the provider's mapping.
fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Whether a response produced at `temperature` may be cached.
///
/// [`LlmRequest`] carries no temperature — it lives in `CallConfig` and is applied by the provider —
/// so the cache cannot inspect a request to answer this and the caller must pass what it used.
///
/// - `Some(t)` with `t <= 0.0` is cacheable: sampling is pinned to one point, so the same request
///   has one answer and replaying it is the answer the provider would have produced.
/// - `Some(t)` with `t > 0.0` is not: the provider draws from a distribution, and a hit would
///   freeze one draw as if it were the only possible one. That silently changes what a test is
///   testing.
/// - `None` is not cacheable either: it defers to the provider's default, which this module cannot
///   see. Assuming determinism on an unknown default is the unsafe direction — a caller that has
///   configured its provider to default to zero can say so by passing `Some(0.0)`.
/// - A `NaN` temperature fails the comparison and is therefore not cacheable, which matches
///   [`crate::CallConfig`] refusing non-finite values.
#[must_use]
pub fn is_cacheable(temperature: Option<f32>) -> bool {
    // `<=` also rejects negatives, which no provider accepts anyway; comparing with `==` would
    // treat `-0.0` and `0.0` differently for no reason and trip `clippy::float_cmp`.
    temperature.is_some_and(|value| value <= 0.0)
}

/// One cached response, stored as the chunks the provider streamed.
///
/// Chunks rather than an assembled [`crate::AssistantMessage`] because a hit has to be
/// indistinguishable from a miss to the caller above: the loop consumes a chunk stream, so replaying
/// the recorded chunks produces the same blocks, the same usage accounting, and the same timing
/// hooks without the cache knowing how to assemble anything.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedResponse {
    /// The chunks, in the order the provider delivered them.
    pub chunks: Vec<StreamChunk>,
    /// When the response was stored, in milliseconds since the Unix epoch.
    ///
    /// The cache imposes no expiry: only the caller knows how stale an answer may be, so this is the
    /// raw fact it needs to decide rather than a policy baked in here.
    pub stored_ms: u64,
}

impl CachedResponse {
    /// Wraps a chunk stream, stamping it with the current time.
    #[must_use]
    pub fn new(chunks: Vec<StreamChunk>) -> Self {
        Self {
            chunks,
            stored_ms: girder_core::now_millis(),
        }
    }

    /// How old the entry is at `now_ms`.
    ///
    /// Saturates at zero rather than wrapping, so a clock that moved backwards reads as "just
    /// stored" instead of as an entry from the far future.
    #[must_use]
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.stored_ms)
    }
}

/// One entry plus the evidence of when it was last read.
struct Entry {
    response: CachedResponse,
    /// A logical clock rather than a wall clock: monotonic, unaffected by the system clock moving,
    /// and unique per access, which is what makes "least recently used" unambiguous.
    last_used: u64,
}

/// Hit and miss counts.
///
/// Kept as raw counters, with the rate derived, because a rate alone cannot distinguish "one hit out
/// of two" from "a thousand out of two thousand" — and only the second is evidence a cache is
/// working.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Lookups that found an entry.
    pub hits: u64,
    /// Lookups that did not.
    pub misses: u64,
}

impl Stats {
    /// Lookups served, hit or miss.
    ///
    /// Saturating because a counter overflow must not wrap a huge total down to a small one, which
    /// would make the hit rate look better than it is.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.hits.saturating_add(self.misses)
    }

    /// Hits divided by lookups, or `0.0` when nothing was looked up.
    ///
    /// Zero rather than `NaN` for the empty case: a cache that has not been used yet has no rate,
    /// and a `NaN` propagating into a metric poisons every average it enters.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // A hit count above 2^53 cannot be reached by a counter in a process lifetime; the ratio only needs display precision.
    pub fn hit_rate(&self) -> f64 {
        let requests = self.requests();
        if requests == 0 {
            return 0.0;
        }
        self.hits as f64 / requests as f64
    }
}

/// A bounded least-recently-used cache of provider responses.
///
/// Eviction is by *last use*, not by insertion order: a prompt asked on every run has to survive
/// while prompts asked once age out, and an insertion-ordered map would evict the hot entry purely
/// because it is old. [`Cache::get`] therefore takes `&mut self` — a read is a write to the recency
/// order.
pub struct Cache {
    capacity: usize,
    /// Logical clock; see [`Entry::last_used`].
    clock: u64,
    hits: u64,
    misses: u64,
    entries: HashMap<Fingerprint, Entry>,
}

impl Cache {
    /// Builds a cache holding at most `capacity` responses.
    ///
    /// `0` is legal and makes every [`Cache::put`] a no-op, which is how a caller disables caching
    /// without branching at every call site.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            clock: 0,
            hits: 0,
            misses: 0,
            entries: HashMap::new(),
        }
    }

    /// Looks up a response, marking it most recently used.
    ///
    /// Counts a hit or a miss; a caller that skips [`Cache::put`] for an uncacheable response still
    /// gets an honest miss count, because the lookup is what failed.
    pub fn get(&mut self, key: &Fingerprint) -> Option<&CachedResponse> {
        self.clock = self.clock.wrapping_add(1);
        let tick = self.clock;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_used = tick;
            self.hits = self.hits.saturating_add(1);
            return Some(&entry.response);
        }
        self.misses = self.misses.saturating_add(1);
        None
    }

    /// Stores a response, replacing an existing entry for the same key and evicting the least
    /// recently used entry when the cache is full.
    ///
    /// A `put` is not a lookup, so it changes no counter: counting a store as a miss would make a
    /// caller that looks up first and then stores look twice as cold as it is.
    ///
    /// The caller decides cacheability — see [`is_cacheable`]. Storing is otherwise unconditional,
    /// including for a `capacity` of zero, where the entry is simply dropped.
    pub fn put(&mut self, key: Fingerprint, response: CachedResponse) {
        if self.capacity == 0 {
            return;
        }
        self.clock = self.clock.wrapping_add(1);
        let tick = self.clock;

        if let Some(entry) = self.entries.get_mut(&key) {
            entry.response = response;
            entry.last_used = tick;
            return;
        }
        if self.entries.len() >= self.capacity {
            self.evict_one();
        }
        self.entries.insert(
            key,
            Entry {
                response,
                last_used: tick,
            },
        );
    }

    /// Removes the entry whose last use is furthest in the past.
    ///
    /// A linear scan rather than a linked list: the capacity here is small enough that the scan costs
    /// less than maintaining an intrusive list's bookkeeping, and a scan cannot desynchronize from
    /// the map the way a second index can. The tick is unique per access, so the minimum is
    /// unambiguous.
    fn evict_one(&mut self) {
        let victim = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone());
        if let Some(key) = victim {
            self.entries.remove(&key);
        }
    }

    /// Hit and miss counts so far.
    #[must_use]
    pub fn stats(&self) -> Stats {
        Stats {
            hits: self.hits,
            misses: self.misses,
        }
    }

    /// How many responses are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache holds nothing.
    ///
    /// Present because a `len` without it forces each caller to write `== 0` and to remember that a
    /// zero-capacity cache is a valid, permanently empty one.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The most responses this cache will hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// `clock` is omitted rather than printed: it is a bookkeeping counter whose absolute value says
/// nothing a reader can use, and `len` already answers the question it would be consulted for.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately not derived: printing every cached chunk would put whole responses in a log
        // line. The counters and the size are what a diagnostic needs.
        f.debug_struct("Cache")
            .field("capacity", &self.capacity)
            .field("len", &self.entries.len())
            .field("hits", &self.hits)
            .field("misses", &self.misses)
            .finish()
    }
}
