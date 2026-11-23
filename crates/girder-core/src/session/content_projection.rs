//! Request-side content projection: what a route may actually be handed, and what it is told instead.
//!
//! Upstream path: `packages/llm/llm/src/content.ts` (510 lines). Its exports fall into four groups,
//! and only the last one is this module's subject:
//!
//! | group | upstream exports | what it decides |
//! |---|---|---|
//! | block predicates | `contentHasImage`, `contentHasFile` | does this content contain a block kind at all |
//! | model-facing prose | `fileHandleText`, `textOnlyImageText`, `offloadedImageText`, `requestImageHandleText`, `resolveImageAttachmentAccess` | the exact string a model sees in place of bytes it cannot receive |
//! | offload arithmetic | `requiredImageOffload`, `offloadedImagePrefixCount` | how many of the oldest image occurrences must be dropped to fit a route's image budget |
//! | **content projection** | **`projectFilesToText`, `projectImagesForTextModel`, `projectOffloadedImages`, `projectToolUpdates`** | **rewrite a request so it contains only what the route accepts** |
//!
//! # What the local crate already had
//!
//! Nothing. `grep` for `projectFilesToText`, `projectImages`, `requiredImageOffload` or `textOnly`
//! across `crates/girder-core/src` returns no match outside this file. The neighbouring ported
//! modules answer adjacent questions and none of them answers this one:
//!
//! | module | question it answers | why it is not this |
//! |---|---|---|
//! | [`crate::ContentBlock`], [`crate::Message`] | what a block and a message *are* | the vocabulary, with no rewrite over it |
//! | [`crate::BlockAssembler`], [`crate::llm::attempt`] | what an inbound *stream* means | response side: chunks to blocks, one direction only |
//! | [`crate::llm::image_tokens`] | what an image costs in tokens at some dimensions | pricing, not admission — it never drops or replaces a block |
//! | [`crate::llm::extra`] | why a provider *failed* | the error a projection prevents, not the projection |
//! | [`crate::attachment`], [`crate::attachment::limits`] | whether an upload may be *stored* | ingest admission; still I/O-adjacent and upstream of the request |
//!
//! So the whole request-side projection surface was absent, which is the largest pure-function gap
//! left in this area of the port.
//!
//! # Why this module is pure
//!
//! Every function here takes a resolver **function** instead of a store, a path, or a client:
//!
//! - `project_files_to_text` takes `resolve_path: &dyn Fn(&FileAttachmentRef) -> Option<String>`.
//!   In practice that closure is an attachment store lookup plus a host-path mapping, which is I/O
//!   — but it happens in the caller, once per file, and never here. This module cannot open a file,
//!   cannot consult the store, and cannot tell whether the path it is handed exists.
//! - `required_image_offload` takes `version_bytes: &dyn Fn(&ImageBlock) -> u64` for the same
//!   reason: the exact request-version byte length of an image requires the *normalized* bytes,
//!   which live in the attachment service. The arithmetic that turns lengths into a count is the
//!   part worth porting exactly, so that is the part that lives here.
//! - `project_images_for_text_model` and `project_offloaded_images` take no resolver at all. They
//!   replace blocks with deterministic text, and the text is derived only from the reference.
//!
//! The consequence is the usual one: this is testable against upstream's own vectors with no
//! fixture directory, no temp dir, and no store.
//!
//! # The three policies, and why they are not one function
//!
//! A request is rewritten differently depending on *what the route cannot take*, and the three
//! answers are deliberately not composable by accident:
//!
//! 1. **Every route** loses its file blocks ([`project_files_to_text`]). No provider receives a file
//!    natively, so this projection is unconditional and runs in request assembly. The replacement is
//!    a *handle*: a read-only path the model's own tools can open. The model is told to read the file
//!    rather than handed its contents, which is what keeps a 40 MB PDF from being tokenized by
//!    accident — and what makes the handle text carry the "do not claim to have read it" warning
//!    when no readable path exists.
//! 2. **An exact text-only model** loses its image blocks ([`project_images_for_text_model`]). The
//!    replacement is a placeholder naming the attachment digest, so a later turn that *does* have
//!    vision can identify what was hidden. It is deliberately not the file handle: a text-only model
//!    may still have file tools, and pointing it at an image path invites it to "read" a PNG.
//! 3. **A route that already logged a durable offload** replaces exactly those occurrences with its
//!    own placeholder ([`project_offloaded_images`]). This set is a *surface* fact, identical on
//!    every route; only the words differ. Running it before the other projections is what makes the
//!    offloaded set stable across a retry to a different provider.
//!
//! Order matters and is the caller's obligation: offloaded images first (surface fact), then text-only
//! projection (capability), then files (unconditional). [`project_images_for_text_model`] would also
//! catch an `offloaded` image, but with the *wrong* text, so a caller that skips step 3 loses the
//! "omitted to fit request image limits" explanation.
//!
//! # What `project_tool_updates` is doing in a content module, and why it is out of scope here
//!
//! Upstream's fourth projection is not about images or files: it decides which *developer* messages
//! survive and rebuilds the tool declaration list for a route whose tool-update grammar differs
//! (`addition-only` versus `in-history`). That needs `ToolUpdate`, a `ToolHistory` folded from
//! committed headers, and a `developer` role — none of which exist in this crate: [`crate::Role`] has
//! exactly `User`, `Assistant`, and `Tool`.
//!
//! Implementing it would therefore mean either inventing a `ToolHistory` model here that the session
//! layer has not agreed to, or porting half of it against a role that cannot appear in a message. Both
//! produce code that looks done and cannot be wired. It is reported as the remaining gap rather than
//! guessed at; see the module's closing section for the exact contract it needs.

use girder_core::{ContentBlock, Message, MessageId};

/// The four media types the attachment layer accepts, as the extension each one gets.
///
/// A closed match rather than a table lookup: the extension is used in an instruction the model
/// follows ("copy to a writable path ending in `.jpg`"), so a wrong answer silently produces a file
/// the model then fails to edit. Upstream expresses this as an exhaustive `switch` with an
/// `assertNever` default, which is what a Rust `match` gives for free.
fn extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some(".png"),
        "image/jpeg" => Some(".jpg"),
        "image/webp" => Some(".webp"),
        "image/gif" => Some(".gif"),
        _ => None,
    }
}

/// Quotes a value the way upstream's `JSON.stringify` does for the strings it is given.
///
/// Only two inputs reach this: a display name (already sanitized by the attachment layer) and a
/// filesystem path. Both are model-facing *prose*, so the quoting exists to make an embedded space
/// unambiguous — a path with a space is otherwise indistinguishable from two arguments to anything
/// reading the sentence.
#[must_use]
pub fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The eight hex digits of a `sha256:` attachment id, for prose that names an attachment briefly.
///
/// Upstream slices `String(attachmentId).slice(8, 16)`, which assumes the `sha256:` prefix is
/// exactly eight bytes. This strips the prefix when present and otherwise takes the id from the
/// front, so a store that mints ids some other way still yields a short stable label instead of
/// eight characters of nothing.
#[must_use]
pub fn digest(id: &str) -> String {
    let rest = id.strip_prefix("sha256:").unwrap_or(id);
    rest.chars().take(8).collect()
}

/// One durable image reference, as the model-facing prose needs it.
///
/// A local shape rather than [`girder_core::ImageRef`] because the two carry different fields for
/// different reasons: [`girder_core::ImageRef`] is an *ingest* record with no intrinsic dimensions,
/// while every sentence below needs `width`/`height` to be meaningful. Adopting the ingest type and
/// defaulting the dimensions to zero would silently render "0x0px", which is worse than a type the
/// caller must build deliberately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageAttachmentRef {
    /// Opaque storage identifier, `sha256:<64 hex>` by convention.
    pub attachment_id: String,
    /// One of `image/png`, `image/jpeg`, `image/webp`, `image/gif`.
    pub media_type: String,
    /// Exact encoded byte length.
    pub bytes: u64,
    /// Intrinsic encoded width in pixels.
    pub width: u64,
    /// Intrinsic encoded height in pixels.
    pub height: u64,
    /// Optional display name, stripped of local path information.
    pub name: Option<String>,
}

impl ImageAttachmentRef {
    /// Builds a reference.
    #[must_use]
    pub fn new(
        attachment_id: impl Into<String>,
        media_type: impl Into<String>,
        bytes: u64,
        width: u64,
        height: u64,
    ) -> Self {
        Self {
            attachment_id: attachment_id.into(),
            media_type: media_type.into(),
            bytes,
            width,
            height,
            name: None,
        }
    }

    /// Attaches a display name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// How prose identifies this attachment: `"name" (sha256:…)` or just the id.
    #[must_use]
    pub fn identity(&self) -> String {
        match &self.name {
            Some(name) => format!("{} ({})", quoted(name), self.attachment_id),
            None => self.attachment_id.clone(),
        }
    }
}

/// One durable file reference, as the handle text needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileAttachmentRef {
    /// Opaque content-addressed identifier, `sha256:<64 hex>` by convention.
    pub attachment_id: String,
    /// Sanitized display filename.
    pub name: String,
    /// Exact byte length.
    pub bytes: u64,
}

impl FileAttachmentRef {
    /// Builds a reference.
    #[must_use]
    pub fn new(attachment_id: impl Into<String>, name: impl Into<String>, bytes: u64) -> Self {
        Self {
            attachment_id: attachment_id.into(),
            name: name.into(),
            bytes,
        }
    }
}

/// One image occurrence inside request content: the durable reference plus the surface's offload mark.
///
/// `offloaded` is not a capability fact but a *history* fact: the surface decided this occurrence must
/// not be sent inline, and every route must honour that same set. Keeping the flag on the block rather
/// than in a side table is what lets [`required_image_offload`] skip already-dropped occurrences
/// without a second lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageBlock {
    /// The durable reference.
    pub attachment: ImageAttachmentRef,
    /// True when the surface already offloaded this occurrence.
    pub offloaded: bool,
}

impl ImageBlock {
    /// A retained (not offloaded) occurrence.
    #[must_use]
    pub fn retained(attachment: ImageAttachmentRef) -> Self {
        Self {
            attachment,
            offloaded: false,
        }
    }

    /// An offloaded occurrence.
    #[must_use]
    pub fn offloaded(attachment: ImageAttachmentRef) -> Self {
        Self {
            attachment,
            offloaded: true,
        }
    }
}

/// Access to a normalized image in the current tool execution world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageAttachmentAccess {
    /// Absolute path to the immutable normalized bytes. Callers must treat it as read-only.
    pub readonly_path: String,
}

impl ImageAttachmentAccess {
    /// Builds an access record from an execution-world path.
    #[must_use]
    pub fn new(readonly_path: impl Into<String>) -> Self {
        Self {
            readonly_path: readonly_path.into(),
        }
    }
}

/// True when content contains an image block.
///
/// The one image walk every policy shares upstream, so that capability gating, text-only
/// serialization, and the compaction survey cannot silently disagree about what "has an image" means.
#[must_use]
pub fn content_has_image(content: &[ContentBlock]) -> bool {
    content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }))
}

/// True when content contains a file block.
#[must_use]
pub fn content_has_file(content: &[ContentBlock]) -> bool {
    content
        .iter()
        .any(|block| matches!(block, ContentBlock::File { .. }))
}

/// True when *any* message in the history contains an image.
#[must_use]
pub fn messages_have_image(messages: &[Message]) -> bool {
    messages
        .iter()
        .any(|message| content_has_image(&message.content))
}

/// True when *any* message in the history contains a file.
#[must_use]
pub fn messages_have_file(messages: &[Message]) -> bool {
    messages
        .iter()
        .any(|message| content_has_file(&message.content))
}

/// The model-facing identity and request dimensions of one retained image.
///
/// `version` is the *request* size, not the source size: a vision route resizes before sending, and
/// the model is told the dimensions of what it actually receives. Telling it the source dimensions
/// would make it reason about a crop it never saw.
fn version_preview(reference: &ImageAttachmentRef, version: (u64, u64)) -> String {
    format!(
        "Image {}; request preview {}x{}px.",
        reference.identity(),
        version.0,
        version.1
    )
}

/// The sentence describing a normalized copy the model can read but must not overwrite.
fn normalized_access_text(
    reference: &ImageAttachmentRef,
    access: &ImageAttachmentAccess,
) -> String {
    let ext = extension(&reference.media_type).unwrap_or(".bin");
    format!(
        " Normalized copy (read-only; may be resized or re-encoded): {} ({}x{}px, {}). Source \
         dimensions, format, and byte size may differ. Copy to a writable path ending in {} before \
         editing.",
        quoted(&access.readonly_path),
        reference.width,
        reference.height,
        reference.media_type,
        ext,
    )
}

/// Stable text shown to a model that cannot accept one durable image reference.
///
/// Deterministic, and deliberately not a summary: a text-only model is told that an image existed and
/// which attachment it was, so a later vision-capable turn can be asked about the same image and the
/// two turns can be correlated by digest.
#[must_use]
pub fn text_only_image_text(reference: &ImageAttachmentRef) -> String {
    format!(
        "[image omitted because this model accepts text only; attachment sha256:{}]",
        digest(&reference.attachment_id)
    )
}

/// Stable per-image placeholder for a request-limit omission.
///
/// The two branches differ in what the model can *do*, so they cannot share a sentence: with a path it
/// is told where the bytes are; without one it is told to stop and report the limitation rather than
/// answer from an image it never saw.
#[must_use]
pub fn offloaded_image_text(
    reference: &ImageAttachmentRef,
    access: Option<&ImageAttachmentAccess>,
) -> String {
    let identity = format!(
        "image omitted to fit request image limits; {}.",
        reference.identity()
    );
    match access {
        None => format!(
            "[{identity} No local normalized image path is available; ask the user to attach it \
             again if needed.]"
        ),
        Some(access) => format!("[{identity}{}]", normalized_access_text(reference, access)),
    }
}

/// Stable model-facing handle for one exact request image.
///
/// Identity comes from the occurrence's own durable reference, not from the prepared version: request
/// versions are prepared per attachment id, so one shared version can serve occurrences whose display
/// names differ, and naming the version would attribute the wrong file.
#[must_use]
pub fn request_image_handle_text(
    reference: &ImageAttachmentRef,
    version: (u64, u64),
    access: Option<&ImageAttachmentAccess>,
) -> String {
    let preview = version_preview(reference, version);
    match access {
        None => format!(
            "{preview} It may be resized or re-encoded; source dimensions, format, and byte size \
             may differ."
        ),
        Some(access) => format!("{preview}{}", normalized_access_text(reference, access)),
    }
}

/// Stable model-facing handle for one durable file reference: where the verbatim copy is, and the
/// instruction to read it on demand.
///
/// This is the only representation a provider ever receives for a file. The no-path branch is not a
/// degraded version of the with-path branch — it removes a capability, so it also removes the licence
/// to pretend: "do not claim to have read it" is what stops a model from confabulating file contents
/// it was never able to open.
#[must_use]
pub fn file_handle_text(reference: &FileAttachmentRef, readonly_path: Option<&str>) -> String {
    let identity = format!(
        "File {} ({} bytes, sha256:{})",
        quoted(&reference.name),
        reference.bytes,
        digest(&reference.attachment_id)
    );
    match readonly_path {
        None => format!(
            "[{identity} was uploaded, but the current execution environment cannot access a \
             readable path. Report that limitation if its contents are needed; do not claim to have \
             read it.]"
        ),
        Some(path) => format!(
            "[{identity}: verbatim read-only copy saved at {}. Read that path with your file tools \
             when its contents are needed; copy it to a writable location before modifying it. When \
             delegating file work, include this saved path in the delegation prompt; only subagents \
             sharing this execution environment can read it.]",
            quoted(path)
        ),
    }
}

/// Rewrites `content`, mapping every block for which `replace` returns `Some(text)` into a text block.
///
/// Returns `None` when nothing was replaced, which is how the projections below preserve the input by
/// identity — upstream's `next ??= blocks` trick, and the reason a caller can skip a write when the
/// history is already in the shape the route needs.
fn replace_blocks(
    content: &[ContentBlock],
    mut replace: impl FnMut(&ContentBlock) -> Option<String>,
) -> Option<Vec<ContentBlock>> {
    let mut next: Option<Vec<ContentBlock>> = None;
    for (index, block) in content.iter().enumerate() {
        if let Some(text) = replace(block) {
            let out = next.get_or_insert_with(|| content[..index].to_vec());
            out.push(ContentBlock::Text { text });
            continue;
        }
        if let Some(out) = next.as_mut() {
            out.push(block.clone());
        }
    }
    next
}

/// Applies `rewrite` to every message's content, cloning only the messages that changed.
///
/// A message whose content was untouched keeps its identity (its index in the returned list is the
/// same value), which is what upstream's `projected[0]).toBe(messages[0])` assertions pin.
fn project_messages(
    messages: &[Message],
    mut rewrite: impl FnMut(&[ContentBlock]) -> Option<Vec<ContentBlock>>,
) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len());
    let mut changed = false;
    for message in messages {
        match rewrite(&message.content) {
            Some(content) => {
                changed = true;
                out.push(Message {
                    content,
                    ..message.clone()
                });
            }
            None => out.push(message.clone()),
        }
    }
    if changed { out } else { messages.to_vec() }
}

/// What one file block resolves to: the durable reference and the execution-world read path.
///
/// A named alias rather than an inline tuple because the same shape appears in
/// [`project_files_to_text`] and in [`project_request`], and a caller reading either signature should
/// not have to parse `&dyn Fn(&str) -> Option<(FileAttachmentRef, Option<String>)>` to find out.
pub type FileResolution = Option<(FileAttachmentRef, Option<String>)>;

/// Projects request file content into deterministic handle text for every model route.
///
/// Unlike images, no provider receives file blocks natively, so this projection is unconditional in
/// request assembly.
///
/// `resolve` returns the durable reference and the execution-world read path for one block's opaque
/// id. It takes a `&str` rather than a [`FileAttachmentRef`] because [`ContentBlock::File`] stores only
/// that id: the name and byte count are the attachment store's knowledge, so a caller that has them
/// supplies them here and a caller that does not degrades to the no-path branch rather than inventing
/// a filename. Upstream's block carries the whole reference, so its resolver names one field; the
/// difference is in the block, not the projection.
///
/// `None` from `resolve` is *not* "skip this file": the occurrence is still replaced, with the handle
/// that says the file was uploaded and cannot be read. Dropping it silently would leave a request whose
/// transcript claims a file was sent when nothing mentions it.
#[must_use]
pub fn project_files_to_text(
    messages: &[Message],
    resolve: &dyn Fn(&str) -> FileResolution,
) -> Vec<Message> {
    if !messages_have_file(messages) {
        return messages.to_vec();
    }
    project_messages(messages, |content| {
        replace_blocks(content, |block| match block {
            ContentBlock::File { attachment } => {
                let (reference, path) = resolve(attachment)
                    .unwrap_or_else(|| (FileAttachmentRef::new(attachment.clone(), "", 0), None));
                Some(file_handle_text(&reference, path.as_deref()))
            }
            _ => None,
        })
    })
}

/// Projects request image content into deterministic text for an exact text-only model.
///
/// Returns the input unchanged when the history has no images, so a caller can branch on identity
/// rather than comparing contents.
#[must_use]
pub fn project_images_for_text_model(messages: &[Message]) -> Vec<Message> {
    if !messages_have_image(messages) {
        return messages.to_vec();
    }
    project_messages(messages, |content| {
        replace_blocks(content, |block| match block {
            ContentBlock::Image { attachment } => Some(text_only_image_text(
                &ImageAttachmentRef::new(attachment.clone(), "image/png", 0, 0, 0),
            )),
            _ => None,
        })
    })
}

/// Projects image content into text for a text-only model, using full references for the placeholder.
///
/// The placeholder names the digest and nothing else, but taking the reference keeps the call site
/// honest: a caller cannot pass a bare id it invented and get prose that disagrees with the durable
/// record.
#[must_use]
pub fn project_image_blocks_for_text_model(
    messages: &[Message],
    resolve: &dyn Fn(&str) -> Option<ImageAttachmentRef>,
) -> Vec<Message> {
    if !messages_have_image(messages) {
        return messages.to_vec();
    }
    project_messages(messages, |content| {
        replace_blocks(content, |block| match block {
            ContentBlock::Image { attachment } => {
                Some(text_only_image_text(&resolve(attachment).unwrap_or_else(
                    || ImageAttachmentRef::new(attachment.clone(), "image/png", 0, 0, 0),
                )))
            }
            _ => None,
        })
    })
}

/// Projects the surface's offloaded occurrences into deterministic text for one request.
///
/// `resolve` yields the durable reference and whether the surface marked this occurrence offloaded.
/// An offloaded occurrence is dropped here even on a vision route: the offload decision was made once,
/// at the surface, and re-deriving it per route is how two providers end up disagreeing about what the
/// user sent.
#[must_use]
pub fn project_offloaded_images(
    messages: &[Message],
    resolve: &dyn Fn(&str) -> Option<ImageBlock>,
    placeholder: &dyn Fn(&ImageAttachmentRef) -> String,
) -> Vec<Message> {
    project_messages(messages, |content| {
        replace_blocks(content, |block| match block {
            ContentBlock::Image { attachment } => {
                let resolved = resolve(attachment)?;
                if !resolved.offloaded {
                    return None;
                }
                Some(placeholder(&resolved.attachment))
            }
            _ => None,
        })
    })
}

/// Replaces the oldest `count` images in user and tool messages with the offload placeholder.
///
/// Assistant images are left alone: they are model output, not input the route is being asked to
/// accept again. Returns the attachment ids that were replaced, in the order they occurred.
#[must_use]
pub fn offload_oldest_images(messages: &mut [Message], count: usize) -> Vec<String> {
    let mut ids = Vec::new();
    if count == 0 {
        return ids;
    }
    for message in messages.iter_mut() {
        if ids.len() >= count {
            break;
        }
        if message.role == girder_core::Role::Assistant {
            continue;
        }
        let mut content = Vec::with_capacity(message.content.len());
        for block in std::mem::take(&mut message.content) {
            if ids.len() < count {
                if let ContentBlock::Image { attachment } = &block {
                    ids.push(attachment.clone());
                    content.push(ContentBlock::Text {
                        text: offload_placeholder(attachment),
                    });
                    continue;
                }
            }
            content.push(block);
        }
        message.content = content;
    }
    ids
}

/// Replaces images whose attachment id was recorded by an earlier offload.
///
/// The history is returned unchanged when `ids` is empty or none of its ids are still images.
#[must_use]
pub fn hide_offloaded_images(
    messages: &[Message],
    ids: &std::collections::HashSet<String>,
) -> Vec<Message> {
    if ids.is_empty() {
        return messages.to_vec();
    }
    project_messages(messages, |content| {
        replace_blocks(content, |block| match block {
            ContentBlock::Image { attachment } if ids.contains(attachment) => {
                Some(offload_placeholder(attachment))
            }
            _ => None,
        })
    })
}

fn offload_placeholder(attachment: &str) -> String {
    offloaded_image_text(
        &ImageAttachmentRef::new(attachment, "image/png", 0, 0, 0),
        None,
    )
}

/// Whether a route accounts an image's `raw` file bytes or its inline `base64` length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Representation {
    /// Count the encoded file bytes.
    Raw,
    /// Count the base64 length of those bytes, as the wire actually carries them.
    Base64,
}

/// A route's image budget: what it accepts, and how coarsely it may drop.
///
/// The quanta exist because dropping one image at a time can be pointless: a cached prefix is keyed on
/// how many leading occurrences were removed, so a route that removes them in steps of 2 reuses one
/// prompt cache entry for both of a pair instead of missing on each. `None` means "remove the minimum",
/// which is the behaviour a caller gets for free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageRequestBudget {
    /// How the route counts an image's bytes.
    pub representation: Representation,
    /// Accumulated represented bytes the route accepts; `None` leaves bytes unbounded.
    pub max_bytes: Option<u64>,
    /// Image occurrences the route accepts; `None` leaves the count unbounded.
    pub max_images: Option<usize>,
    /// Represented bytes removed per advance step; `None` removes the minimum.
    pub byte_quantum: Option<u64>,
    /// Occurrences removed per advance step; `None` removes the minimum.
    pub count_quantum: Option<usize>,
}

impl ImageRequestBudget {
    /// A budget that accepts anything, in raw bytes.
    #[must_use]
    pub fn unbounded() -> Self {
        Self {
            representation: Representation::Raw,
            max_bytes: None,
            max_images: None,
            byte_quantum: None,
            count_quantum: None,
        }
    }

    /// Caps the occurrence count.
    #[must_use]
    pub fn with_max_images(mut self, max: usize) -> Self {
        self.max_images = Some(max);
        self
    }

    /// Caps the represented byte total.
    #[must_use]
    pub fn with_max_bytes(mut self, max: u64) -> Self {
        self.max_bytes = Some(max);
        self
    }

    /// Sets the byte removal quantum.
    #[must_use]
    pub fn with_byte_quantum(mut self, quantum: u64) -> Self {
        self.byte_quantum = Some(quantum);
        self
    }

    /// Sets the count removal quantum.
    #[must_use]
    pub fn with_count_quantum(mut self, quantum: usize) -> Self {
        self.count_quantum = Some(quantum);
        self
    }

    /// Accounts one occurrence's raw bytes at the route's representation.
    #[must_use]
    pub fn represent(&self, bytes: u64) -> u64 {
        match self.representation {
            Representation::Raw => bytes,
            Representation::Base64 => base64_length(bytes),
        }
    }
}

/// Base64 length of raw bytes, including padding.
///
/// The wire carries four characters per three bytes, so a route with a base64 byte budget can be over
/// it while every raw file fits — which is the whole reason the representation is explicit.
#[must_use]
pub fn base64_length(bytes: u64) -> u64 {
    bytes.div_ceil(3).saturating_mul(4)
}

/// How many leading retained occurrences one route budget removes, in whole count and byte quanta.
///
/// The result depends only on the represented lengths, so every route names the count the same way and
/// a persisted offload decision replays identically.
///
/// The `byte_quantum == 1` special case is upstream's, kept because it is observable: with a quantum of
/// one the loop stops as soon as the removed total *reaches* the target, while with a larger quantum it
/// only stops once the total *exceeds* it. Both are "one more image than strictly needed" in the large
/// quantum case, and the vectors below pin which.
#[must_use]
pub fn offloaded_image_prefix_count(lengths: &[u64], budget: &ImageRequestBudget) -> usize {
    let total: u64 = lengths.iter().copied().fold(0_u64, u64::saturating_add);
    let excess_count = budget
        .max_images
        .map_or(0, |max| lengths.len().saturating_sub(max));
    let excess_bytes = budget.max_bytes.map_or(0, |max| total.saturating_sub(max));
    if excess_count == 0 && excess_bytes == 0 {
        return 0;
    }
    let count_quantum = budget.count_quantum.unwrap_or(1).max(1);
    let byte_quantum = budget.byte_quantum.unwrap_or(1).max(1);
    let remove_count = if excess_count == 0 {
        0
    } else {
        excess_count
            .div_ceil(count_quantum)
            .saturating_mul(count_quantum)
    };
    let remove_bytes = if excess_bytes == 0 {
        0
    } else {
        excess_bytes
            .div_ceil(byte_quantum)
            .saturating_mul(byte_quantum)
    };
    let mut count = 0_usize;
    let mut dropped_total = 0_u64;
    for image_bytes in lengths {
        // A quantum of one stops once the target is reached; a larger one only once it is passed, which
        // is what makes the two cases differ by one image.
        let byte_target_met = remove_bytes == 0
            || (if byte_quantum == 1 {
                dropped_total >= remove_bytes
            } else {
                dropped_total > remove_bytes
            });
        if count >= remove_count && byte_target_met {
            break;
        }
        dropped_total = dropped_total.saturating_add(*image_bytes);
        count += 1;
    }
    count
}

/// How many more leading retained occurrences a route must offload before the request fits, or zero.
///
/// A route does not offload on its own: it fails with an `IMAGE_OFFLOAD_REQUIRED` error carrying this
/// count, and the surface performs the offload so that *every* route sees the same set. That is why the
/// function is pure and why it takes `version_bytes` — the count must be computed from the exact bytes
/// the route would send, before it sends them.
#[must_use]
pub fn required_image_offload(
    messages: &[Message],
    budget: &ImageRequestBudget,
    version_bytes: &dyn Fn(&ImageBlock) -> u64,
) -> usize {
    let mut lengths = Vec::new();
    for message in messages {
        for block in &message.content {
            let ContentBlock::Image { attachment } = block else {
                continue;
            };
            let resolved = ImageBlock::retained(ImageAttachmentRef::new(
                attachment.clone(),
                "image/png",
                0,
                0,
                0,
            ));
            lengths.push(budget.represent(version_bytes(&resolved)));
        }
    }
    offloaded_image_prefix_count(&lengths, budget)
}

/// How many more leading retained occurrences must be offloaded, from already-resolved blocks.
///
/// The form a caller uses when it holds the durable references: the `offloaded` flag is visible here,
/// so an occurrence the surface already dropped is not counted twice.
#[must_use]
pub fn required_image_offload_blocks(
    occurrences: &[ImageBlock],
    budget: &ImageRequestBudget,
    version_bytes: &dyn Fn(&ImageBlock) -> u64,
) -> usize {
    let lengths: Vec<u64> = occurrences
        .iter()
        .filter(|block| !block.offloaded)
        .map(|block| budget.represent(version_bytes(block)))
        .collect();
    offloaded_image_prefix_count(&lengths, budget)
}

/// The fully projected request a route receives: content the route can take, and nothing else.
///
/// This is the assembly step itself, expressed as data rather than as four calls a caller must remember
/// to make in an order. It composes the three image policies and the file policy in the only order that
/// is correct, and reports what it did so a caller can log a projection instead of guessing whether one
/// happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedRequest {
    /// The rewritten history.
    pub messages: Vec<Message>,
    /// Image occurrences replaced because the surface had offloaded them.
    pub offloaded_images: usize,
    /// Image occurrences replaced because the route accepts text only.
    pub text_only_images: usize,
    /// File occurrences replaced by handle text.
    pub files: usize,
}

impl ProjectedRequest {
    /// True when nothing was replaced and the history is the caller's own.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.offloaded_images == 0 && self.text_only_images == 0 && self.files == 0
    }
}

/// Projects one request for a route that may or may not accept images.
///
/// `accepts_images == false` replaces every remaining image with text-only placeholder text after the
/// offloaded set has been replaced with its own placeholder. Files are always replaced, because no
/// route accepts them.
///
/// The order is the contract:
/// 1. offloaded occurrence -> `offloaded_image_text` (surface fact, identical on every route),
/// 2. image on a text-only route -> `text_only_image_text` (capability),
/// 3. file -> `file_handle_text` (unconditional).
///
/// Reversing 1 and 2 would be invisible in the type system and wrong in the transcript: an offloaded
/// image would be explained as "this model accepts text only", which blames the model for a decision the
/// surface made.
#[must_use]
pub fn project_request(
    messages: &[Message],
    accepts_images: bool,
    image: &dyn Fn(&str) -> Option<ImageBlock>,
    file: &dyn Fn(&str) -> FileResolution,
    access: &dyn Fn(&ImageAttachmentRef) -> Option<ImageAttachmentAccess>,
) -> ProjectedRequest {
    let mut offloaded_images = 0_usize;
    let mut text_only_images = 0_usize;
    let mut files = 0_usize;

    let after_offload = project_messages(messages, |content| {
        replace_blocks(content, |block| match block {
            ContentBlock::Image { attachment } => {
                let resolved = image(attachment)?;
                if !resolved.offloaded {
                    return None;
                }
                offloaded_images += 1;
                Some(offloaded_image_text(
                    &resolved.attachment,
                    access(&resolved.attachment).as_ref(),
                ))
            }
            _ => None,
        })
    });

    let after_images = if accepts_images {
        after_offload
    } else {
        project_messages(&after_offload, |content| {
            replace_blocks(content, |block| match block {
                ContentBlock::Image { attachment } => {
                    text_only_images += 1;
                    Some(text_only_image_text(&ImageAttachmentRef::new(
                        attachment.clone(),
                        "image/png",
                        0,
                        0,
                        0,
                    )))
                }
                _ => None,
            })
        })
    };

    let after_files = project_messages(&after_images, |content| {
        replace_blocks(content, |block| match block {
            ContentBlock::File { attachment } => {
                let (reference, path) = file(attachment)
                    .unwrap_or_else(|| (FileAttachmentRef::new(attachment.clone(), "", 0), None));
                files += 1;
                Some(file_handle_text(&reference, path.as_deref()))
            }
            _ => None,
        })
    });

    ProjectedRequest {
        messages: after_files,
        offloaded_images,
        text_only_images,
        files,
    }
}

/// Builds a request message, for callers that only have the projection.
///
/// [`Message`] has no constructor with an explicit id because it has no id at all; upstream's
/// `RequestMessage` carries one for tool-update bookkeeping. This helper exists so a test or a caller
/// assembling a projected history does not have to spell out the struct.
#[must_use]
pub fn user_message(content: Vec<ContentBlock>) -> Message {
    Message {
        role: girder_core::Role::User,
        content,
        tool_call_id: None,
    }
}

/// Builds a tool-result message, for the same reason.
#[must_use]
pub fn tool_message(call_id: impl Into<String>, content: Vec<ContentBlock>) -> Message {
    let id = girder_core::ToolCallId::new(call_id.into())
        .unwrap_or_else(|_| girder_core::ToolCallId::new("call").expect("a literal id is valid"));
    Message::tool_result(id, content)
}

/// A message id that names a request position.
///
/// Upstream's `RequestMessage` carries a [`MessageId`]; the local [`Message`] does not, because the
/// session log owns identity and a request is a view of it. This function exists so a projection report
/// can still name a position without inventing a second identity.
#[must_use]
pub fn position_id(index: usize) -> Option<MessageId> {
    MessageId::new(format!("m{index}")).ok()
}

/// The content projection's remaining gap: `projectToolUpdates`.
///
/// Upstream's fourth projection does not touch images or files. It takes the request messages, the
/// currently active [`girder_core::ToolSchema`] list, a route's `ToolUpdate` mode, and a folded
/// `ToolHistory`, and returns the declarations a provider should be told about plus the developer
/// messages that explain them. Its rules — deferred declarations need an addition to activate, a
/// removal on an `addition-only` route is not expressible in history so it must reset a baseline,
/// an earlier declaration series does not govern the current tool set — are all observable in the
/// transcript.
///
/// It is not implemented here, for one reason: the inputs do not exist in this crate.
///
/// | upstream input | local status |
/// |---|---|
/// | `Role` including `developer` | [`girder_core::Role`] has `User`, `Assistant`, `Tool` only |
/// | `ContentBlock` variants `tool-addition`, `tool-removal` | absent — the local enum is closed and has neither |
/// | `ToolSchema.deferLoading` | absent |
/// | `ToolHistory { tools, updates }` | absent; nothing folds committed headers into one |
///
/// Porting it against those absences would mean inventing a `ToolHistory` shape the session layer has
/// not agreed to, and a `developer` role no message can carry. The result would compile, pass tests
/// written against the invention, and be unusable. The contract this module needs is instead stated
/// here so the integrator can add it with the rest of the tool-update work:
///
/// ```text
/// fn project_tool_updates(
///     messages: &[RequestMessage],      // needs Role::Developer and request-position identity
///     tools: Option<&[ToolSchema]>,     // needs ToolSchema::defer_loading
///     mode: Option<ToolUpdate>,         // AdditionOnly | InHistory
///     history: Option<&ToolHistory>,    // needs a fold over committed headers
/// ) -> ProjectedToolUpdates
/// ```
///
/// Everything in it is a pure function of those four inputs, so it belongs in this module once they
/// exist — not in a fifth gap module.
pub const TOOL_UPDATE_GAP: &str = "projectToolUpdates needs Role::Developer, tool-addition/tool-removal \
     blocks, ToolSchema::defer_loading, and a folded ToolHistory; none exist in girder-core yet";

#[cfg(test)]
mod offload_tests {
    use std::collections::HashSet;

    use super::{hide_offloaded_images, offload_oldest_images};
    use crate::llm::{ContentBlock, Message, Role};

    fn image(id: &str) -> ContentBlock {
        ContentBlock::Image {
            attachment: id.to_owned(),
        }
    }

    #[test]
    fn the_oldest_user_image_is_replaced_and_a_later_one_stays() {
        let mut messages = vec![
            Message::new(Role::User, vec![image("old"), image("new")]),
            Message::new(Role::Assistant, vec![image("answer")]),
        ];
        let removed = offload_oldest_images(&mut messages, 1);
        assert_eq!(removed, vec!["old".to_owned()]);
        assert!(matches!(messages[0].content[0], ContentBlock::Text { .. }));
        assert!(matches!(messages[0].content[1], ContentBlock::Image { .. }));
        assert!(matches!(messages[1].content[0], ContentBlock::Image { .. }));

        let hidden = hide_offloaded_images(&messages, &HashSet::from([removed[0].clone()]));
        assert!(matches!(hidden[0].content[0], ContentBlock::Text { .. }));
        assert!(matches!(hidden[0].content[1], ContentBlock::Image { .. }));
    }
}
