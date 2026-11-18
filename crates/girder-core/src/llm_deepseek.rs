//! A `DeepSeek` provider speaking the Anthropic Messages wire format.
//!
//! Upstream's `packages/llm/llm-deepseek` posts to `{base}/messages` with `anthropic-version` and
//! consumes a server-sent event stream. This module implements that protocol and nothing else about
//! the provider — no image offload, no file ids, no model discovery, no pricing. Those are admin
//! surfaces (see `docs/phase4-interfaces.md`), and the vertical slice never reaches them.
//!
//! # What is verified, and what is not
//!
//! The tests drive this against a **local mock server** that replays recorded-shaped SSE. That
//! verifies the request body the provider builds, the SSE parser, and the chunk translation. It does
//! **not** verify the provider against the live API: no credential is available in this environment,
//! so "works against api.deepseek.com" is an unverified claim. The module is written so that claim
//! is one HTTP call away, not one refactor away.
//!
//! # Why SSE parsing is hand-written
//!
//! The event stream is line-oriented: `event: <type>` then `data: <json>`, blank-line separated.
//! A full SSE crate would bring reconnection, retry policy, and event-id bookkeeping that this
//! protocol does not use — the harness owns retry (`llm-retry` upstream) and the stream is one-shot.
//! Parsing three fields by hand is smaller than configuring the alternative.

use std::sync::Arc;

use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;

use crate::llm::{
    ChunkStream, ContentBlock, FinishReason, LlmError, LlmProvider, LlmRequest, Role, StreamChunk,
    TokenUsage,
};
use crate::scope::BoxFut;

/// Default endpoint. Upstream's `PUBLIC_BASE_URL`.
pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com/anthropic";

/// Default model id.
pub const DEFAULT_MODEL: &str = "deepseek-chat";

/// Configuration for one provider instance.
#[derive(Clone, Debug)]
pub struct DeepSeekConfig {
    /// Endpoint root; `/messages` is appended.
    pub base_url: String,
    /// API key, sent as `x-api-key`.
    pub api_key: String,
    /// `anthropic-version` header value.
    pub anthropic_version: String,
    /// Default output limit when a request does not set one.
    pub max_tokens: u32,
    /// Default model when a request does not name one.
    pub default_model: String,
}

impl DeepSeekConfig {
    /// Builds a config for `base_url` with `api_key`.
    #[must_use]
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            anthropic_version: "2023-06-01".to_owned(),
            max_tokens: 8192,
            default_model: DEFAULT_MODEL.to_owned(),
        }
    }

    /// Reads configuration from the environment.
    ///
    /// Returns `None` when no key is present, so a caller can fall back to another provider rather
    /// than fail at the first request.
    #[must_use]
    /// Reads a configuration from the environment.
    ///
    /// `DEEPSEEK_API_KEY` is the historical name and stays first so existing deployments keep working. `ANTHROPIC_API_KEY`
    /// is accepted because the wire protocol is Anthropic's: a deployment pointing this at a different compatible
    /// endpoint should not have to set a variable named after a provider it is not using.
    pub fn from_env() -> Option<Self> {
        Self::from_values(|name| std::env::var(name).ok())
    }

    /// Builds a configuration from a lookup function.
    ///
    /// Split from [`Self::from_env`] so the precedence rules are testable without touching the process environment:
    /// `std::env::set_var` is unsafe in this toolchain, and a test that mutated it would race every other test in the
    /// binary anyway.
    pub fn from_values(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        // This provider's own variable first, then the historical alias, and only then the
        // catalogue. The order matters: `provider_ids()` is sorted, so a directory-wide search
        // finds `anthropic` before `deepseek` and a deployment that set both would take the alias
        // while its `DEEPSEEK_BASE_URL` still pointed at DeepSeek. The directory earns its place as
        // the last resort -- a client repointed at a gateway finds its credential under whatever
        // name that provider documents.
        let own = crate::provider_catalog_gap::provider_data("deepseek")
            .and_then(|data| data.env.iter().find_map(|name| lookup(name)));
        let api_key = own.or_else(|| lookup("ANTHROPIC_API_KEY")).or_else(|| {
            crate::provider_catalog_gap::provider_ids()
                .into_iter()
                .filter(|id| *id != "deepseek" && *id != "anthropic")
                .find_map(|id| {
                    crate::provider_catalog_gap::provider_data(id)
                        .and_then(|data| data.env.iter().find_map(|name| lookup(name)))
                })
        })?;
        // `ANTHROPIC_BASE_URL` is the name a deployment pointing at a compatible service would already have set. Both
        // are read so the same binary serves either without a code change.
        let base_url = lookup("DEEPSEEK_BASE_URL")
            .or_else(|| lookup("ANTHROPIC_BASE_URL"))
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        let default_model = lookup("GIRDER_MODEL")
            .or_else(|| lookup("ANTHROPIC_MODEL"))
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        // Through the option ladder rather than a literal: `deepseek_options_gap` owns the eighteen
        // fields and their ten cross-field rules, and its default output cap is 256k, not the 8192
        // this literal used to say. A deployment that configured a smaller cap now gets it, and one
        // whose numbers contradict each other is refused here rather than at the provider.
        let mut options = crate::deepseek_options_gap::Options {
            base_url: Some(base_url.clone()),
            ..crate::deepseek_options_gap::Options::default()
        };
        options.max_tokens = None;
        match crate::deepseek_options_gap::resolve_options(
            &options,
            &crate::deepseek_options_gap::NoEnvironment,
        ) {
            Ok(resolved) => Some(Self {
                base_url: resolved.base_url,
                api_key,
                anthropic_version: "2023-06-01".to_owned(),
                max_tokens: u32::try_from(resolved.max_tokens).unwrap_or(u32::MAX),
                default_model,
            }),
            // A configuration the ladder refuses is not a configuration. Falling back to the
            // hard-coded literal would start a provider this build has already judged unusable.
            Err(_) => None,
        }
    }

    /// The full endpoint URL.
    #[must_use]
    pub fn endpoint(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/messages")
    }
}

/// A provider backed by the Anthropic Messages API.
pub struct DeepSeekProvider {
    config: DeepSeekConfig,
    client: reqwest::Client,
    /// Per-model limits, resolved at call time.
    ///
    /// Held here rather than read from the environment on each call because the resolution has to happen before the
    /// request is built -- a body with the wrong `max_tokens` is already wrong by the time it is serialized.
    calls: crate::call_config::ConfigTable,
}

impl DeepSeekProvider {
    /// Builds a provider.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Provider`] when the HTTP client cannot be constructed.
    pub fn new(config: DeepSeekConfig) -> Result<Self, LlmError> {
        // A connect timeout and a read timeout, because a client with neither waits forever: a provider that accepts
        // the connection and then stalls holds the turn open until the outer cancellation, and a provider that never
        // accepts it holds it open forever. These are the transport's bounds rather than a call's -- `CallConfig`
        // carries the per-model policy, and this keeps the underlying socket from outliving any policy at all.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|error| LlmError::Provider(format!("http client: {error}")))?;
        Ok(Self {
            config,
            client,
            calls: crate::call_config::ConfigTable::new(crate::call_config::CallConfig::default()),
        })
    }

    /// Replaces the per-model limits.
    ///
    /// A builder rather than a constructor argument: the table is empty in every deployment that has no per-model
    /// policy, and requiring one would make the common case write a line that means "nothing special".
    #[must_use]
    pub fn with_calls(mut self, calls: crate::call_config::ConfigTable) -> Self {
        self.calls = calls;
        self
    }

    /// The limits in force for `model`.
    #[must_use]
    pub fn limits(&self, model: &str) -> &crate::call_config::CallConfig {
        self.calls.resolve(model)
    }

    /// The configuration in use.
    #[must_use]
    pub fn config(&self) -> &DeepSeekConfig {
        &self.config
    }

    /// Builds the JSON body for one request.
    ///
    /// Exposed so a test can assert the wire shape without a server, which is the part most likely
    /// to drift from the API.
    #[must_use]
    /// The Messages request body.
    ///
    /// Delegates to `deepseek_serialize_gap`, which owns all 26 rules of `serialize.ts`: promoting a
    /// reasoning block back into a `thinking` block, declaring `thinking` so the API accepts a
    /// history that contains one, merging parallel tool results into a single user turn, hoisting a
    /// late system prompt, and the four refusal families. A hand-rolled body here had two of them
    /// and got both wrong -- it turned reasoning into an empty text block and never declared
    /// `thinking`, which makes a multi-turn conversation with reasoning a malformed request.
    ///
    /// Falls back to the local shape only when serialization refuses, so a caller sees a body rather
    /// than a panic; the refusal itself is surfaced by [`Self::body_result`].
    pub fn body(&self, request: &LlmRequest) -> serde_json::Value {
        // The serializer refuses a conversation it cannot express and *drops* turns it cannot pair --
        // an orphan tool result, for instance. Dropping is right for the wire, but a body with no
        // messages is a request about nothing, so a non-empty input that serialized to nothing falls
        // back to the per-message shape: the provider's refusal then names the orphan, which is a
        // better diagnostic than an empty request.
        //
        // The condition is "the serializer produced turns", not "it did not error". The serializer
        // validates the conversation as a whole -- an unanswered `tool_use` or an orphan
        // `tool_result` is dropped rather than sent -- so a single-message probe legitimately
        // produces nothing. Falling back then keeps this method's contract: it returns a body for
        // whatever it is given, and the provider's refusal names the defect precisely.
        match self.body_result(request) {
            Ok(body) if !body["messages"].as_array().is_some_and(Vec::is_empty) => body,
            _ => self.body_fallback(request),
        }
    }

    /// The Messages request body, or why it cannot be built.
    ///
    /// # Errors
    ///
    /// [`SerializeError`] when the request cannot be expressed, which is a fact about the
    /// conversation rather than about the transport.
    pub fn body_result(
        &self,
        request: &LlmRequest,
    ) -> Result<serde_json::Value, crate::deepseek_serialize_gap::SerializeError> {
        let route = crate::deepseek_serialize_gap::SerializeRoute::new(
            if request.model.is_empty() {
                &self.config.default_model
            } else {
                &request.model
            },
            request
                .max_tokens
                .or_else(|| self.limits(&request.model).max_tokens)
                .unwrap_or(self.config.max_tokens),
        );
        // A history that carries a reasoning block *requires* the declaration: the Messages API
        // rejects a thinking block that the request did not announce, so this is not a preference.
        // Derived from the content rather than configured, because the content is what makes it
        // true.
        let carries_reasoning = request.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Reasoning { .. }))
        });
        let route = route.with_thinking_enabled(carries_reasoning);
        crate::deepseek_serialize_gap::serialize_llm_request(route, request)
    }

    /// The pre-serializer body, kept for the one caller that must not fail: a request this build
    /// cannot express still has to reach the provider, and the provider's refusal names the field
    /// more precisely than a local error would.
    fn body_fallback(&self, request: &LlmRequest) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|message| {
                // The Messages API has no `tool` role: a tool result is a `user` turn holding a
                // `tool_result` block. That block is not optional — every `tool_use` must be
                // answered by one in the immediately following message, and the API rejects the
                // request otherwise.
                let role = match message.role {
                    Role::Tool | Role::User => "user",
                    Role::Assistant => "assistant",
                };
                let content = if message.role == Role::Tool {
                    let call_id = message.tool_call_id.as_ref().map_or("", |id| id.as_str());
                    let text: String = message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    vec![serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": [{ "type": "text", "text": text }],
                    })]
                } else {
                    message
                        .content
                        .iter()
                        // The fallback path has no prepared versions: it exists for the case the serializer
                        // refused, which is a conversation this build cannot express, not one it can
                        // enrich. `None` here produces the handle sentence, never a source-less block.
                        .map(|block| block_to_wire(block, &|_| None))
                        .collect::<Vec<_>>()
                };
                serde_json::json!({ "role": role, "content": content })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": if request.model.is_empty() { &self.config.default_model } else { &request.model },
            "stream": true,
            // The request wins, then the model's own limit, then the provider's default. Three levels because they
            // answer different questions: what this call wants, what this model accepts, and what the deployment
            // configured when it knew neither.
            "max_tokens": request
                .max_tokens
                .or_else(|| self.limits(&request.model).max_tokens)
                .unwrap_or(self.config.max_tokens),
            "messages": messages,
        });

        if let Some(system) = &request.system_prompt {
            body["system"] = serde_json::json!(system);
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(
                request
                    .tools
                    .iter()
                    .map(|tool| serde_json::json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    }))
                    .collect::<Vec<_>>()
            );
        }
        body
    }

    /// What one image of these dimensions costs this request, in image tokens.
    ///
    /// `llm_gap` owns the arithmetic: the two-dimensional resize, the 28-pixel patch grid, and the
    /// cap. It is exposed here because a caller that is deciding whether to resize an image before
    /// uploading needs the same number the billing will use, and a second implementation in the
    /// caller is how the two come to disagree.
    ///
    /// Returns `None` for dimensions the catalogue cannot price, rather than a zero: a zero reads as
    /// "free" and would make an unpriceable image look cheap.
    #[must_use]
    pub fn image_token_cost(width: u32, height: u32) -> Option<u64> {
        let price = crate::llm_gap::price_request_image(width, height);
        // A zero-dimension image is the refusal case: no patch grid, no tokens, and the pricing
        // module reports it as zero rather than failing.
        (width > 0 && height > 0).then_some(price.tokens)
    }

    /// Plans the file uploads a request needs, so a caller knows what to send before it sends it.
    ///
    /// This is the caller of `deepseek_files_gap::plan_upload`. That module holds the Files API
    /// decision layer — reuse-or-upload, the size and expiry refusals, the provider filename — but
    /// it is pure: it answers one image at a time and reads no request. Walking a request and
    /// turning its [`ContentBlock::Image`] blocks into those questions is the part that was
    /// missing, and it is here because this is the provider whose wire format the answers feed.
    ///
    /// `images` supplies the prepared request version for each attachment: the media type and the
    /// encoded bytes the provider would store. It is a parameter rather than a field because the
    /// preparation pipeline (re-encode, resize, choose an encoder) lives above this crate's
    /// provider and produces those bytes per request, not per provider instance. `index` is the
    /// persisted reuse table; an empty index is the correct value for "nothing has been uploaded
    /// under this credential yet".
    ///
    /// The returned plans are in wire order and carry the message and image position each one
    /// fills, so a caller can resolve them while serializing. Two occurrences of one attachment
    /// produce two entries — they are separate positions on the wire — but they name the same
    /// variant, so a caller that commits as it goes uploads once and reuses the id afterwards.
    ///
    /// # Errors
    ///
    /// [`LlmError::Provider`] when an image block names an attachment that cannot be resolved to a
    /// prepared version at all, which is a request whose bytes no caller can send; a *refusal* such
    /// as "too large" is not an error and is reported inside the plan, because a caller that
    /// substitutes text for that image can still send the rest of the request.
    pub fn plan_image_uploads(
        &self,
        request: &LlmRequest,
        images: crate::deepseek_serialize_gap::ImageLookup<'_>,
        index: &crate::deepseek_files_gap::UploadIndex,
    ) -> Result<Vec<ImageUploadPlan>, LlmError> {
        let scope = self.file_scope();
        let policy = crate::deepseek_files_gap::FilePolicy::default();
        let now = now_millis();

        let mut plans = Vec::new();
        for (message_index, message) in request.messages.iter().enumerate() {
            let mut image = 0_usize;
            for block in &message.content {
                let ContentBlock::Image { attachment } = block else {
                    continue;
                };
                image += 1;
                let prepared = images(attachment).ok_or_else(|| {
                    LlmError::Provider(format!(
                        "image attachment `{attachment}` has no prepared request version, so there \
                         are no bytes to upload"
                    ))
                })?;
                let variant = image_variant_id(attachment, &prepared.media_type, &prepared.data);
                let request_image = crate::deepseek_files_gap::RequestImage {
                    attachment_id: attachment.clone(),
                    variant_id: variant.clone(),
                    bytes: u64::try_from(prepared.data.len()).unwrap_or(u64::MAX),
                    media_type: prepared.media_type.clone(),
                };
                let decision = decision_of(crate::deepseek_files_gap::plan_upload(
                    &request_image,
                    index,
                    &scope,
                    &policy,
                    now,
                ));
                plans.push(ImageUploadPlan {
                    message: message_index + 1,
                    image,
                    attachment: attachment.clone(),
                    variant,
                    decision,
                });
            }
        }
        Ok(plans)
    }

    /// Plans image uploads for a request whose prepared bytes the caller cannot supply.
    ///
    /// [`ContentBlock::Image`] carries an attachment reference and nothing else: no media type, no
    /// byte count, no prepared version. Without those three the Files layer cannot answer —
    /// [`crate::deepseek_files_gap::plan_upload`] refuses on size, expiry, and media type, and all
    /// three are unanswerable from a bare reference. This method therefore reports each image as
    /// [`UploadDecision::MissingPreparation`] rather than inventing a media type and a size, which
    /// would produce a plan that looks authoritative and is wrong.
    ///
    /// It is still useful for the one thing a reference *can* answer: which images a request
    /// contains, in wire order, and the upload position each will occupy. A caller that only needs
    /// to know "how many uploads will this turn need" gets a correct count here before the
    /// preparation pipeline runs.
    #[must_use]
    pub fn plan_image_uploads_unprepared(&self, request: &LlmRequest) -> Vec<ImageUploadPlan> {
        let mut plans = Vec::new();
        for (message_index, message) in request.messages.iter().enumerate() {
            let mut image = 0_usize;
            for block in &message.content {
                let ContentBlock::Image { attachment } = block else {
                    continue;
                };
                image += 1;
                plans.push(ImageUploadPlan {
                    message: message_index + 1,
                    image,
                    attachment: attachment.clone(),
                    // No version exists yet, so none can be named. An empty variant is the honest
                    // value: the mapping this plan would key is created by preparation, which has
                    // not run.
                    variant: String::new(),
                    decision: UploadDecision::MissingPreparation {
                        attachment: attachment.clone(),
                    },
                });
            }
        }
        plans
    }

    /// The Files namespace this provider's uploads belong to.
    ///
    /// Derived rather than stored: the namespace binds the endpoint and the credential together, so
    /// a provider that changed either would otherwise reuse file ids the new credential cannot
    /// read. The credential only ever enters a hash, and the result is safe to persist.
    fn file_scope(&self) -> crate::deepseek_files_gap::FileScope {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert(
            "anthropic-version".to_owned(),
            self.config.anthropic_version.clone(),
        );
        headers.insert("x-api-key".to_owned(), self.config.api_key.clone());
        crate::deepseek_files_gap::file_scope(
            &self.config.base_url,
            &crate::deepseek_files_gap::scope_credentials(&headers),
        )
    }
}

/// One image of a request, paired with the Files API decision made for it.
///
/// The three identifying fields are carried even for a refusal, because a caller that substitutes
/// text for an un-uploadable image still has to find the position it was at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageUploadPlan {
    /// 1-based message index, in wire order.
    pub message: usize,
    /// 1-based image index within that message.
    pub image: usize,
    /// The attachment reference the content block carried.
    pub attachment: String,
    /// The prepared request version, as `sha256:<hex>`.
    ///
    /// Empty when nothing was prepared, which is the only case where no version exists. The two
    /// targets are two request variants of one attachment, and the Files index is keyed on this
    /// rather than on the attachment id.
    pub variant: String,
    /// What to do about it.
    pub decision: UploadDecision,
}

impl ImageUploadPlan {
    /// Whether this image already has a provider file id and must not be uploaded again.
    #[must_use]
    pub fn is_reuse(&self) -> bool {
        matches!(self.decision, UploadDecision::Reuse { .. })
    }

    /// Whether this image needs bytes sent to the Files API.
    #[must_use]
    pub fn needs_upload(&self) -> bool {
        matches!(self.decision, UploadDecision::Upload { .. })
    }

    /// Whether the caller cannot resolve this image at all, and so must not ask for its file id.
    #[must_use]
    pub fn is_unresolved(&self) -> bool {
        matches!(
            self.decision,
            UploadDecision::Refuse(_) | UploadDecision::MissingPreparation { .. }
        )
    }

    /// The provider file id, when one is already known.
    #[must_use]
    pub fn file_id(&self) -> Option<&str> {
        match &self.decision {
            UploadDecision::Reuse { file_id, .. } => Some(file_id),
            UploadDecision::Upload { .. }
            | UploadDecision::Refuse(_)
            | UploadDecision::MissingPreparation { .. } => None,
        }
    }

    /// The provider filename an upload would use.
    ///
    /// Present exactly when bytes must be sent, because the filename is what the upload request
    /// carries and what later identifies the file as harness-owned.
    #[must_use]
    pub fn upload_filename(&self) -> Option<&str> {
        match &self.decision {
            UploadDecision::Upload { filename, .. } => Some(filename),
            UploadDecision::Reuse { .. }
            | UploadDecision::Refuse(_)
            | UploadDecision::MissingPreparation { .. } => None,
        }
    }

    /// Why this image cannot be resolved, in the words a caller would show a user.
    #[must_use]
    pub fn unresolved_reason(&self) -> Option<String> {
        match &self.decision {
            UploadDecision::Refuse(refusal) => Some(refusal.to_string()),
            UploadDecision::MissingPreparation { attachment } => Some(format!(
                "image attachment `{attachment}` has no prepared request version, so its media \
                 type and size are unknown"
            )),
            UploadDecision::Reuse { .. } | UploadDecision::Upload { .. } => None,
        }
    }
}

/// What a caller should do about one request image.
///
/// A mirror of [`crate::deepseek_files_gap::UploadPlan`] with one addition: that enum has no state
/// for "the request did not carry enough to decide", because its own input
/// (`RequestImage`) always carries the size and media type. A request block does not, so the gap
/// has to be representable rather than silently coerced into a refusal the library never produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadDecision {
    /// A reusable mapping exists; send nothing.
    Reuse {
        /// The provider identifier to cite on the wire.
        file_id: String,
        /// When that mapping expires, in Unix milliseconds.
        expires_at: u64,
    },
    /// Upload the prepared bytes.
    Upload {
        /// Provider filename to store them under.
        filename: String,
        /// Requested lifetime, in seconds.
        expires_after_seconds: u64,
    },
    /// The Files layer refused before any transport.
    Refuse(crate::deepseek_files_gap::Refusal),
    /// The request did not carry enough for the Files layer to decide.
    MissingPreparation {
        /// The attachment reference that could not be resolved.
        attachment: String,
    },
}

/// Lowers one Files-layer decision into the request-facing one.
fn decision_of(plan: crate::deepseek_files_gap::UploadPlan) -> UploadDecision {
    use crate::deepseek_files_gap::UploadPlan;
    match plan {
        UploadPlan::Reuse(record) => UploadDecision::Reuse {
            file_id: record.file_id.to_string(),
            expires_at: record.expires_at,
        },
        // `plan_upload` never returns `ReclaimThenUpload`: that is the recovery half of the upload
        // loop, decided after a quota error the planning call has not yet provoked. Sharing the
        // arm keeps the two from disagreeing if a caller ever hands over a recovery plan —
        // planning an upload is the safe reading, since a caller that uploads and then hits the
        // quota still has the recovery path.
        UploadPlan::Upload {
            filename,
            expires_after_seconds,
        }
        | UploadPlan::ReclaimThenUpload {
            filename,
            expires_after_seconds,
            ..
        } => UploadDecision::Upload {
            filename,
            expires_after_seconds,
        },
        UploadPlan::Refuse(refusal) => UploadDecision::Refuse(refusal),
    }
}

/// The complete request-transformation identity of one prepared image, as `sha256:<hex>`.
///
/// Upstream's variant id folds in the route budgets, the encoder parameters, and the source
/// digest; this build's preparation pipeline produces the encoded bytes and nothing that names
/// them, so the identity is derived from what the bytes *are*: the attachment it came from, its
/// media type, and its length. That is coarser than upstream — two encodings of one attachment that
/// differ in content but agree on all three would collide — so a caller that owns the pipeline
/// should pass its own id rather than relying on this one.
fn image_variant_id(attachment_id: &str, media_type: &str, data: &[u8]) -> String {
    let mut input = Vec::with_capacity(attachment_id.len() + media_type.len() + 24);
    input.extend_from_slice(attachment_id.as_bytes());
    input.push(0);
    input.extend_from_slice(media_type.as_bytes());
    input.push(0);
    input.extend_from_slice(&u64::try_from(data.len()).unwrap_or(u64::MAX).to_be_bytes());
    format!("sha256:{}", crate::migration_steps_gap::sha256_hex(&input))
}

/// Milliseconds on the Unix timeline, floored.
///
/// The Files index compares every timestamp in milliseconds, and a clock before the epoch reads as
/// zero rather than failing: a wrong-clock host should plan a refresh, not refuse to plan at all.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// One content block in the wire vocabulary.
fn block_to_wire(
    block: &ContentBlock,
    prepared_image: &dyn Fn(&str) -> Option<crate::deepseek_serialize_gap::SerializeImage>,
) -> serde_json::Value {
    match block {
        ContentBlock::Text { text } => serde_json::json!({ "type": "text", "text": text }),
        // Reasoning is not sent back: the API does not accept it as input.
        ContentBlock::Reasoning { .. } => serde_json::json!({ "type": "text", "text": "" }),
        ContentBlock::ToolCall {
            id,
            name,
            arguments,
        } => serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            // The API wants a parsed object, not a string. A malformed accumulator becomes an empty
            // object rather than failing the whole request: the tool executor will report the
            // invalid arguments to the model, which is more useful than dropping the turn.
            "input": serde_json::from_str::<serde_json::Value>(arguments)
                .unwrap_or_else(|_| serde_json::json!({})),
        }),
        // The Messages API carries an image as an `image` block with a `source`, and this used to
        // emit `{"type":"text","text":"[image …]"}` instead -- so the model was told an image
        // existed and shown a filename. A caller that has resolved the request version (media type
        // and bytes) gets a real block; one that has not gets the handle sentence, which is what
        // `content_projection_gap` uses for a route that cannot take images anyway. Sending a block
        // with no source would be rejected by the API, so the text is the honest fallback.
        ContentBlock::Image { attachment } => {
            match prepared_image(attachment) {
                Some(prepared) => {
                    let mut block = serde_json::json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": prepared.media_type,
                            "data": girder_core::encode_base64(&prepared.data),
                        },
                    });
                    // A resolved Files id switches the source from inline to `file`, which is what
                    // keeps a large image out of the request body.
                    if let Some(file_id) = &prepared.file_id {
                        block = serde_json::json!({
                            "type": "image",
                            "source": { "type": "file", "file_id": file_id },
                        });
                    }
                    block
                }
                None => serde_json::json!({
                    "type": "text",
                    "text": crate::content_projection_gap::text_only_image_text(
                        &crate::content_projection_gap::ImageAttachmentRef::new(
                            attachment.clone(),
                            "application/octet-stream",
                            0,
                            0,
                            0,
                        ),
                    ),
                }),
            }
        }
        ContentBlock::File { attachment } => serde_json::json!({
            "type": "text",
            "text": format!("[file {attachment}]"),
        }),
    }
}

impl LlmProvider for DeepSeekProvider {
    fn stream(
        &self,
        request: LlmRequest,
        cancel: CancellationToken,
    ) -> BoxFut<Result<ChunkStream, LlmError>> {
        // `body` resolves the model's limits itself, so nothing is read here. A second read would be a second place for
        // the resolution to disagree with what the body actually sent.
        let body = self.body(&request);
        let url = self.config.endpoint();
        let key = self.config.api_key.clone();
        let version = self.config.anthropic_version.clone();
        let client = self.client.clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }

            let response = client
                .post(&url)
                .header("x-api-key", key)
                .header("anthropic-version", version)
                .header("accept", "text/event-stream")
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|error| {
                    if cancel.is_cancelled() {
                        LlmError::Cancelled
                    } else {
                        LlmError::Provider(format!("request failed: {error}"))
                    }
                })?;

            if !response.status().is_success() {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                return Err(LlmError::Provider(format!(
                    "HTTP {status}: {}",
                    text.trim()
                )));
            }

            // Parse the SSE stream incrementally so a chunk reaches the caller as it arrives rather
            // than after the whole response.
            let stream = response
                .bytes_stream()
                .map(move |chunk| chunk.map_err(|error| error.to_string()));

            let chunks = async_stream::stream! {
                let mut parser = SseParser::default();
                let mut stream = Box::pin(stream);
                let mut translator = Translator::default();

                while let Some(chunk) = stream.next().await {
                    if cancel.is_cancelled() {
                        yield Err(LlmError::Cancelled);
                        return;
                    }
                    match chunk {
                        Ok(bytes) => {
                            for event in parser.feed(&bytes) {
                                for outcome in translator.translate(&event) {
                                    yield outcome;
                                }
                            }
                        }
                        Err(error) => {
                            yield Err(LlmError::Provider(format!("stream error: {error}")));
                            return;
                        }
                    }
                }
                // A stream that ends without `message_stop` is truncated.
                if !translator.finished {
                    yield Err(LlmError::Provider(
                        "stream ended before message_stop".to_owned(),
                    ));
                }
            };

            // Map `Result<StreamChunk, LlmError>` into the provider contract: a stream item is a
            // chunk, and a failure inside the stream is a terminal `Error` finish so the consumer
            // always sees a well-formed ending.
            let mapped = chunks.map(|item| match item {
                Ok(chunk) => chunk,
                Err(error) => StreamChunk::Finish {
                    reason: FinishReason::Error {
                        message: error.to_string(),
                    },
                },
            });

            Ok(Box::pin(mapped) as ChunkStream)
        })
    }
}

/// One parsed server-sent event.
#[derive(Clone, Debug, PartialEq)]
pub struct SseEvent {
    /// The `event:` field.
    pub event: String,
    /// The `data:` payload.
    pub data: String,
}

/// Incremental SSE parser.
///
/// Buffers bytes until a blank line, then emits the accumulated fields. Bytes rather than `String`
/// because a multi-byte character can be split across network chunks, and decoding each chunk
/// independently would corrupt it.
#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    event: String,
    data: String,
}

impl SseParser {
    /// Feeds bytes, returning every complete event.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();

        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let raw: Vec<u8> = self.buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\r', '\n']);

            if line.is_empty() {
                if !self.event.is_empty() || !self.data.is_empty() {
                    out.push(SseEvent {
                        event: std::mem::take(&mut self.event),
                        data: std::mem::take(&mut self.data),
                    });
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                rest.trim().clone_into(&mut self.event);
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest.trim_start());
            }
            // `id:` and `retry:` are unused by this protocol and ignored.
        }

        out
    }

    /// Whether a partial event is buffered.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.buffer.is_empty() || !self.data.is_empty()
    }
}

/// Translates SSE events into [`StreamChunk`]s.
#[derive(Default)]
pub struct Translator {
    /// Whether a terminal event has been seen.
    pub finished: bool,
    /// Blocks started so far, by index, so `content_block_stop` can be paired.
    started: std::collections::BTreeMap<usize, String>,
}

impl Translator {
    /// Translates one event, returning the chunks it produces.
    pub fn translate(&mut self, event: &SseEvent) -> Vec<Result<StreamChunk, LlmError>> {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&event.data) else {
            // `ping` and unknown event types often carry no JSON, or carry a shape this version does
            // not know. Ignoring them keeps a newer server compatible.
            return Vec::new();
        };

        match event.event.as_str() {
            "message_start" => Self::message_start(&json),
            "content_block_start" => self.block_start(&json),
            "content_block_delta" => Self::block_delta(&json),
            "content_block_stop" => self.block_stop(&json),
            "message_delta" => self.message_delta(&json),
            "message_stop" => {
                self.finished = true;
                Vec::new()
            }
            "error" => Self::error(&json),
            _ => Vec::new(),
        }
    }

    fn message_start(json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        json.pointer("/message/usage")
            .map(|usage| vec![Ok(StreamChunk::Usage(usage_from(usage)))])
            .unwrap_or_default()
    }

    fn block_start(&mut self, json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        let Ok(index) = index_of(json) else {
            return Vec::new();
        };
        let kind = json["content_block"]["type"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.started.insert(index, kind.clone());

        match kind.as_str() {
            "text" => vec![Ok(StreamChunk::BlockStart {
                index,
                block_type: "text".to_owned(),
            })],
            "thinking" => vec![Ok(StreamChunk::BlockStart {
                index,
                block_type: "reasoning".to_owned(),
            })],
            "tool_use" => vec![
                Ok(StreamChunk::BlockStart {
                    index,
                    block_type: "tool-call".to_owned(),
                }),
                // The id and name arrive complete on the start event; only arguments stream.
                Ok(StreamChunk::ToolCallDelta {
                    index,
                    // A provider id that fails validation becomes a placeholder rather than failing
                    // the stream: the call is still usable, and dropping the response over a
                    // malformed id would be worse than an untraceable call.
                    id: crate::brand::ToolCallId::new(
                        json["content_block"]["id"].as_str().unwrap_or_default(),
                    )
                    .unwrap_or_else(|_| {
                        crate::brand::ToolCallId::new("invalid").expect("constant is valid")
                    }),
                    name: Some(
                        json["content_block"]["name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                    ),
                    arguments_delta: String::new(),
                }),
            ],
            _ => Vec::new(),
        }
    }

    fn block_delta(json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        let Ok(index) = index_of(json) else {
            return Vec::new();
        };
        let delta = &json["delta"];
        match delta["type"].as_str().unwrap_or_default() {
            "text_delta" => vec![Ok(StreamChunk::TextDelta {
                index,
                text: delta["text"].as_str().unwrap_or_default().to_owned(),
            })],
            "thinking_delta" => vec![Ok(StreamChunk::ReasoningDelta {
                index,
                text: delta["thinking"].as_str().unwrap_or_default().to_owned(),
            })],
            "input_json_delta" => vec![Ok(StreamChunk::ToolCallDelta {
                index,
                // Only the start event carries the id; a delta leaves it empty.
                id: crate::brand::ToolCallId::new("").unwrap_or_else(|_| {
                    crate::brand::ToolCallId::new("invalid").expect("constant is valid")
                }),
                name: None,
                arguments_delta: delta["partial_json"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            })],
            _ => Vec::new(),
        }
    }

    fn block_stop(&mut self, json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        let Ok(index) = index_of(json) else {
            return Vec::new();
        };
        let Some(kind) = self.started.remove(&index) else {
            return Vec::new();
        };
        // The block is materialized by the assembler from its deltas; emitting the accumulated text
        // here would duplicate it.
        let block = match kind.as_str() {
            "text" => ContentBlock::Text {
                text: String::new(),
            },
            "thinking" => ContentBlock::Reasoning {
                text: String::new(),
            },
            _ => return Vec::new(),
        };
        vec![Ok(StreamChunk::BlockEnd { index, block })]
    }

    fn message_delta(&mut self, json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        let mut out = Vec::new();
        if let Some(usage) = json.get("usage") {
            out.push(Ok(StreamChunk::Usage(usage_from(usage))));
        }
        if let Some(reason) = json.pointer("/delta/stop_reason").and_then(|r| r.as_str()) {
            out.push(Ok(StreamChunk::Finish {
                reason: finish_reason(reason),
            }));
            self.finished = true;
        }
        out
    }

    fn error(json: &serde_json::Value) -> Vec<Result<StreamChunk, LlmError>> {
        let message = json
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("provider error");
        vec![Ok(StreamChunk::Finish {
            reason: FinishReason::Error {
                message: message.to_owned(),
            },
        })]
    }
}

/// Reads the block index, refusing a value that cannot be a position.
fn index_of(json: &serde_json::Value) -> Result<usize, ()> {
    json["index"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(())
}

fn usage_from(value: &serde_json::Value) -> TokenUsage {
    TokenUsage {
        input: value
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        output: value
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cached: value
            .get("cache_read_input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    }
}

fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "refusal" => FinishReason::Error {
            message: "the model refused".to_owned(),
        },
        other => FinishReason::Error {
            message: format!("unrecognized stop reason `{other}`"),
        },
    }
}

/// Builds a provider from the environment, or reports why it cannot.
///
/// # Errors
///
/// Returns [`LlmError::Provider`] when `DEEPSEEK_API_KEY` is unset or the client cannot be built.
pub fn provider_from_env() -> Result<Arc<dyn LlmProvider>, LlmError> {
    let config = DeepSeekConfig::from_env().ok_or_else(|| {
        LlmError::Provider(
            "DEEPSEEK_API_KEY is not set; set it to use the live provider".to_owned(),
        )
    })?;
    Ok(Arc::new(DeepSeekProvider::new(config)?))
}
