//! Tests for planning image uploads on the `DeepSeek` provider.
//!
//! The subject is the seam that was missing: `deepseek_files_gap` decides what to do with one
//! prepared image, but nothing walked a `LlmRequest` and asked it. These tests drive the walk.
//!
//! # What can and cannot be verified here
//!
//! No credential is available, so nothing asserts that an upload reaches `api.deepseek.com`. What
//! is asserted is the planning decision: which images a request contains, whether each is a reuse
//! or an upload, what filename it would take, and — the part most likely to be quietly wrong —
//! what happens when the request does not carry enough to decide at all.
//!
//! # The gap this test file documents
//!
//! `ContentBlock::Image` carries an `attachment` string and nothing else. No media type, no byte
//! count, no prepared version. `deepseek_files_gap::plan_upload` needs all three. That is why the
//! provider has two entry points and why one of them reports a *missing preparation* instead of
//! guessing a media type and a size.

use girder_core::deepseek_files_gap::{
    FilePolicy, RequestImage, UploadIndex, UploadRecord, file_scope,
};
use girder_core::{ContentBlock, DeepSeekConfig, DeepSeekProvider, LlmRequest, Message, Role};

const PNG: &str = "image/png";

/// A tiny valid-looking PNG. The bytes are never sent; only their length and media type matter to
/// the decision layer.
fn png_bytes() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(b"\0\0\0\rIHDR");
    bytes
}

fn provider() -> DeepSeekProvider {
    DeepSeekProvider::new(DeepSeekConfig::new("http://localhost:1", "test-key")).expect("provider")
}

fn image(id: &str) -> ContentBlock {
    ContentBlock::Image {
        attachment: id.to_owned(),
    }
}

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text {
        text: value.to_owned(),
    }
}

fn request(messages: Vec<Message>) -> LlmRequest {
    LlmRequest::new("deepseek-chat", messages)
}

/// A request that names `ids` as images, one per user turn.
fn request_with_images(ids: &[&str]) -> LlmRequest {
    request(
        ids.iter()
            .map(|id| Message::new(Role::User, vec![image(id)]))
            .collect(),
    )
}

/// The scope the provider derives, so a test can seed an index it will actually be judged against.
fn scope() -> girder_core::deepseek_files_gap::FileScope {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("anthropic-version".to_owned(), "2023-06-01".to_owned());
    headers.insert("x-api-key".to_owned(), "test-key".to_owned());
    file_scope(
        "http://localhost:1",
        &girder_core::deepseek_files_gap::scope_credentials(&headers),
    )
}

/// What the provider's own variant id for one prepared image is.
///
/// The provider derives it from the attachment, the media type, and the encoded length. The test
/// recomputes it so a seeded index can be keyed the way the provider will look it up.
fn variant_of(attachment: &str, media_type: &str, data: &[u8]) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(attachment.as_bytes());
    input.push(0);
    input.extend_from_slice(media_type.as_bytes());
    input.push(0);
    input.extend_from_slice(&(data.len() as u64).to_be_bytes());
    format!(
        "sha256:{}",
        girder_core::migration_steps_gap::sha256_hex(&input)
    )
}

/// An index holding one reusable mapping for `attachment`'s prepared version.
fn seeded_index(attachment: &str, data: &[u8], file_id: &str) -> UploadIndex {
    let variant = variant_of(attachment, PNG, data);
    let scope = scope();
    let record = UploadRecord {
        scope: scope.clone(),
        attachment_id: attachment.to_owned(),
        variant_id: variant,
        file_id: girder_core::deepseek_files_gap::FileId::new(file_id).expect("valid id"),
        bytes: data.len() as u64,
        created_at: 1_000,
        // Far enough out that the default one-hour refresh margin cannot make it read as absent.
        expires_at: now_millis() + 86_400_000,
    };
    UploadIndex::new(vec![record]).expect("index")
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// The full prepared bytes for one attachment.
fn prepared<'a>(
    data: &'a [u8],
) -> impl Fn(&str) -> Option<girder_core::deepseek_serialize_gap::SerializeImage> + 'a {
    move |attachment: &str| {
        Some(girder_core::deepseek_serialize_gap::SerializeImage::inline(
            attachment,
            format!("[image {attachment}]"),
            PNG,
            data.to_vec(),
        ))
    }
}

// ---- the fields the task asked about ------------------------------------------------------------

#[test]
fn an_image_block_carries_only_an_attachment_reference() {
    // This is the information gap, pinned as a test so it cannot be fixed silently in one place and
    // left broken in the other: the block has no media type, no size, and no prepared version, and
    // `ContentBlock::Image { attachment }` is an exhaustive pattern — a new field would fail to
    // compile here, which is the point.
    let block = image("att-1");
    let ContentBlock::Image { attachment } = &block else {
        panic!("built an image block");
    };
    assert_eq!(attachment, "att-1");
    // Named explicitly: there is nothing else to read.
    assert!(matches!(block, ContentBlock::Image { .. }));
}

// ---- the unprepared path ------------------------------------------------------------------------

#[test]
fn an_unprepared_request_reports_every_image_and_refuses_to_guess() {
    let plans = provider().plan_image_uploads_unprepared(&request_with_images(&["a", "b"]));

    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].attachment, "a");
    assert_eq!(plans[0].message, 1);
    assert_eq!(plans[0].image, 1);
    assert_eq!(plans[1].attachment, "b");
    assert_eq!(plans[1].message, 2);
    assert_eq!(plans[1].image, 1);

    // Nothing was invented: no variant exists, and neither a reuse nor an upload was claimed.
    for plan in &plans {
        assert!(
            plan.variant.is_empty(),
            "no prepared version exists, so no variant can be named"
        );
        assert!(plan.is_unresolved());
        assert!(!plan.is_reuse());
        assert!(!plan.needs_upload());
        assert_eq!(plan.file_id(), None);
        assert_eq!(plan.upload_filename(), None);
    }
    // A missing preparation is not a refusal: no decision was reached, so nothing is claimed.
    assert!(!plans[0].needs_upload());
    assert_eq!(plans[0].upload_filename(), None);
}

#[test]
fn an_unprepared_plan_explains_what_is_missing_rather_than_calling_it_a_refusal() {
    // A refusal is a decision the Files layer reached. This is not one, and saying so matters: a
    // caller that treated it as a refusal would report a size or type problem to the user that
    // nothing checked.
    let plans = provider().plan_image_uploads_unprepared(&request_with_images(&["a"]));
    let reason = plans[0].unresolved_reason().expect("explains itself");
    assert!(
        reason.contains("prepared request version"),
        "the reason names the missing step: {reason}"
    );
}

#[test]
fn a_request_with_no_images_plans_nothing() {
    let request = request(vec![Message::new(
        Role::User,
        vec![text("hello"), text("world")],
    )]);
    assert!(
        provider()
            .plan_image_uploads_unprepared(&request)
            .is_empty()
    );
    assert!(
        provider()
            .plan_image_uploads(&request, &|_| None, &UploadIndex::empty())
            .expect("no images, no error")
            .is_empty()
    );
}

#[test]
fn images_are_indexed_positionally_within_their_message() {
    // A message holding two images and text between them: the image counter counts images, not
    // blocks, so the second image is 2 rather than 3.
    let request = request(vec![Message::new(
        Role::User,
        vec![text("look:"), image("a"), text("and:"), image("b")],
    )]);
    let plans = provider().plan_image_uploads_unprepared(&request);

    assert_eq!(plans.len(), 2);
    assert_eq!((plans[0].message, plans[0].image), (1, 1));
    assert_eq!((plans[1].message, plans[1].image), (1, 2));
}

// ---- the prepared path --------------------------------------------------------------------------

#[test]
fn a_prepared_image_with_no_cached_mapping_is_an_upload() {
    let data = png_bytes();
    let plans = provider()
        .plan_image_uploads(
            &request_with_images(&["a"]),
            &prepared(&data),
            &UploadIndex::empty(),
        )
        .expect("plans");

    assert_eq!(plans.len(), 1);
    assert!(plans[0].needs_upload());
    assert!(!plans[0].is_reuse());
    assert!(!plans[0].is_unresolved());
    assert_eq!(plans[0].file_id(), None);

    let filename = plans[0]
        .upload_filename()
        .expect("an upload names its file");
    assert!(
        filename.starts_with("dsh-"),
        "a harness-owned file must be recognizable later: {filename}"
    );
    assert!(
        filename.ends_with(".png"),
        "the extension comes from the media type: {filename}"
    );
    assert!(plans[0].variant.starts_with("sha256:"));
    assert_eq!(plans[0].variant.len(), "sha256:".len() + 64);
}

#[test]
fn a_prepared_image_with_a_cached_mapping_is_a_reuse() {
    let data = png_bytes();
    let index = seeded_index("a", &data, "file-abc");
    let plans = provider()
        .plan_image_uploads(&request_with_images(&["a"]), &prepared(&data), &index)
        .expect("plans");

    assert_eq!(plans.len(), 1);
    assert!(plans[0].is_reuse(), "a reusable mapping means send nothing");
    assert!(!plans[0].needs_upload());
    assert_eq!(plans[0].file_id(), Some("file-abc"));
    assert_eq!(
        plans[0].upload_filename(),
        None,
        "a reuse uploads nothing, so it names no file"
    );
}

#[test]
fn an_expired_mapping_is_planned_as_a_fresh_upload() {
    let data = png_bytes();
    let variant = variant_of("a", PNG, &data);
    let record = UploadRecord {
        scope: scope(),
        attachment_id: "a".to_owned(),
        variant_id: variant,
        file_id: girder_core::deepseek_files_gap::FileId::new("file-old").expect("valid"),
        bytes: data.len() as u64,
        created_at: 1_000,
        // Already past: the provider must not cite an id the API is about to forget.
        expires_at: now_millis().saturating_sub(1),
    };
    let index = UploadIndex::new(vec![record]).expect("index");

    let plans = provider()
        .plan_image_uploads(&request_with_images(&["a"]), &prepared(&data), &index)
        .expect("plans");

    assert!(
        plans[0].needs_upload(),
        "an expired mapping is not reusable"
    );
    assert_eq!(plans[0].file_id(), None);
}

#[test]
fn an_unsupported_media_type_is_refused_before_any_transport() {
    let data = png_bytes();
    let lookup = |attachment: &str| {
        Some(girder_core::deepseek_serialize_gap::SerializeImage::inline(
            attachment,
            "handle",
            "image/bmp",
            data.clone(),
        ))
    };
    let plans = provider()
        .plan_image_uploads(&request_with_images(&["a"]), &lookup, &UploadIndex::empty())
        .expect("a refusal is a plan, not an error");

    assert!(plans[0].is_unresolved());
    assert!(!plans[0].needs_upload());
    assert_eq!(plans[0].upload_filename(), None);
    let reason = plans[0].unresolved_reason().expect("explains itself");
    assert!(
        reason.contains("bmp") || reason.contains("PNG"),
        "the reason names the types: {reason}"
    );
}

#[test]
fn an_image_above_the_size_limit_is_refused_rather_than_uploaded() {
    // Exactly one byte over the cap: the boundary the Files layer cares about, because an upload
    // that starts and is then refused has already spent the bytes.
    let oversized = vec![0_u8; (girder_core::deepseek_files_gap::MAX_IMAGE_BYTES + 1) as usize];
    let plans = provider()
        .plan_image_uploads(
            &request_with_images(&["a"]),
            &prepared(&oversized),
            &UploadIndex::empty(),
        )
        .expect("a refusal is a plan, not an error");

    assert!(plans[0].is_unresolved());
    assert!(!plans[0].needs_upload());
    let reason = plans[0].unresolved_reason().expect("explains itself");
    assert!(
        reason.contains("32 MiB") || reason.contains("above the"),
        "the refusal names the limit: {reason}"
    );
}

#[test]
fn two_positions_of_one_attachment_share_a_variant_so_the_caller_uploads_once() {
    // The index is keyed on the variant, not on the position. Two occurrences are two wire
    // positions — both must be planned — but one upload serves both, which is what makes the second
    // entry a reuse as soon as the caller commits the first.
    let data = png_bytes();
    let request = request(vec![Message::new(
        Role::User,
        vec![image("a"), text("again:"), image("a")],
    )]);
    let plans = provider()
        .plan_image_uploads(&request, &prepared(&data), &UploadIndex::empty())
        .expect("plans");

    assert_eq!(plans.len(), 2, "both positions are planned");
    assert_eq!(
        plans[0].variant, plans[1].variant,
        "one variant, one upload"
    );
    assert!(plans[0].needs_upload() && plans[1].needs_upload());

    // Now simulate the commit: the mapping exists, so both positions read as reuse.
    let index = seeded_index("a", &data, "file-abc");
    let after = provider()
        .plan_image_uploads(&request, &prepared(&data), &index)
        .expect("plans");
    assert!(after.iter().all(|plan| plan.is_reuse()));
    assert!(after.iter().all(|plan| plan.file_id() == Some("file-abc")));
}

#[test]
fn different_media_types_of_one_attachment_are_different_variants() {
    // The variant is the upload identity, so a re-encoding that changes the type must not reuse the
    // old file id: the provider keys its own file on that type.
    let data = png_bytes();
    let as_jpeg = |attachment: &str| {
        Some(girder_core::deepseek_serialize_gap::SerializeImage::inline(
            attachment,
            "handle",
            "image/jpeg",
            data.clone(),
        ))
    };
    let request = request_with_images(&["a"]);
    let png_variant = provider()
        .plan_image_uploads(&request, &prepared(&data), &UploadIndex::empty())
        .expect("plans")[0]
        .variant
        .clone();
    let jpeg_variant = provider()
        .plan_image_uploads(&request, &as_jpeg, &UploadIndex::empty())
        .expect("plans")[0]
        .variant
        .clone();

    assert_ne!(png_variant, jpeg_variant);
}

#[test]
fn an_unresolvable_attachment_is_an_error_not_a_plan() {
    // The Files layer cannot be asked about an image with no bytes at all, and a plan claiming one
    // would be a plan to upload nothing. The error names the attachment so the caller can find it.
    let error = provider()
        .plan_image_uploads(
            &request_with_images(&["ghost"]),
            &|_| None,
            &UploadIndex::empty(),
        )
        .expect_err("no prepared version");
    let message = error.to_string();
    assert!(
        message.contains("ghost"),
        "the error names the attachment: {message}"
    );
}

#[test]
fn a_request_whose_first_image_resolves_and_second_does_not_fails_whole() {
    // Partial plans are worse than none: a caller that uploaded the first image and then errored
    // would spend an upload for a request it cannot send.
    let data = png_bytes();
    let lookup = |attachment: &str| {
        if attachment == "ok" {
            Some(girder_core::deepseek_serialize_gap::SerializeImage::inline(
                attachment,
                "handle",
                PNG,
                data.clone(),
            ))
        } else {
            None
        }
    };
    let request = request(vec![
        Message::new(Role::User, vec![image("ok")]),
        Message::new(Role::User, vec![image("missing")]),
    ]);

    let error = provider()
        .plan_image_uploads(&request, &lookup, &UploadIndex::empty())
        .expect_err("the second image cannot be planned");
    assert!(error.to_string().contains("missing"));
}

// ---- the decision layer against its own contract ------------------------------------------------

#[test]
fn the_plan_agrees_with_plan_upload_called_directly() {
    // The provider's job is to ask the same question the library answers, so the two must agree on
    // one image. If they ever diverge, the walk is wrong, not the library.
    let data = png_bytes();
    let plans = provider()
        .plan_image_uploads(
            &request_with_images(&["a"]),
            &prepared(&data),
            &UploadIndex::empty(),
        )
        .expect("plans");

    let request_image = RequestImage {
        attachment_id: "a".to_owned(),
        variant_id: plans[0].variant.clone(),
        bytes: data.len() as u64,
        media_type: PNG.to_owned(),
    };
    let direct = girder_core::deepseek_files_gap::plan_upload(
        &request_image,
        &UploadIndex::empty(),
        &scope(),
        &FilePolicy::default(),
        now_millis(),
    );

    assert_eq!(
        plans[0].upload_filename(),
        match &direct {
            girder_core::deepseek_files_gap::UploadPlan::Upload { filename, .. } => {
                Some(filename.as_str())
            }
            _ => None,
        }
    );
}
