//! Tests for request-side content projection.
//!
//! The reference vectors come from upstream `packages/llm/llm/tests/content.spec.ts` (656 lines) and
//! `packages/llm/llm-deepseek/tests/request-pricing.spec.ts`, which is what makes this a port rather
//! than an interpretation. Where this suite asserts an exact string, that string is the one upstream
//! asserts character for character: the model reads these sentences, so a missing space or a dropped
//! clause is a behavioural change even though nothing fails to compile.
//!
//! # Why the module is compiled by path
//!
//! `girder-core`'s crate root does not declare `content_projection_gap`; adding `pub mod` and its
//! `pub use` belongs to the integrator. Inside a `#[path]` module the `crate::` prefix would name
//! *this* test crate, so the module spells library types `girder_core::{…}` — which is why it compiles
//! unchanged in both places.
//!
//! # What each group protects
//!
//! - **Identity is preserved for untouched content.** A projection that returns equal-but-new messages
//!   makes every downstream cache (prompt cache, projection cache, replay fixtures) miss on a request
//!   that did not change. Upstream asserts `toBe` on the untouched entries; the equivalent here is that
//!   a file-free history comes back *equal*, and that the source history was never mutated.
//! - **The three policies stay distinguishable.** An offloaded image explained as "this model accepts
//!   text only" blames the model for a surface decision, and the difference is only visible in prose.
//! - **Offload arithmetic is exact.** The count becomes a durable `image/offload` event; recomputing it
//!   differently on a later turn re-offloads images that were already dropped, or leaves the request
//!   over budget.
//! - **The corpus is pure.** Every function takes its knowledge as a closure, so the whole suite runs
//!   with no store, no filesystem, and no fixture directory.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::float_cmp,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/session/content_projection.rs"]
pub mod content_projection_gap;

use content_projection_gap::{
    FileAttachmentRef, ImageAttachmentAccess, ImageAttachmentRef, ImageBlock, ImageRequestBudget,
    Representation, TOOL_UPDATE_GAP, base64_length, content_has_file, content_has_image, digest,
    file_handle_text, messages_have_file, messages_have_image, offloaded_image_prefix_count,
    offloaded_image_text, position_id, project_files_to_text, project_image_blocks_for_text_model,
    project_images_for_text_model, project_offloaded_images, project_request, quoted,
    request_image_handle_text, required_image_offload, required_image_offload_blocks,
    text_only_image_text, tool_message, user_message,
};
use girder_core::{ContentBlock, Message, Role};

const OMITTED: &str = "[omitted]";

fn png(name: &str) -> ImageAttachmentRef {
    ImageAttachmentRef::new(
        format!(
            "sha256:{}",
            name.chars().next().unwrap_or('a').to_string().repeat(64)
        ),
        "image/png",
        1024,
        800,
        600,
    )
    .with_name(name)
}

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text {
        text: value.to_owned(),
    }
}

fn image(id: &str) -> ContentBlock {
    ContentBlock::Image {
        attachment: id.to_owned(),
    }
}

fn file(id: &str) -> ContentBlock {
    ContentBlock::File {
        attachment: id.to_owned(),
    }
}

fn file_ref(name: &str) -> FileAttachmentRef {
    FileAttachmentRef::new(format!("sha256:{}", "ab".repeat(32)), name, 42)
}

fn text_of(block: &ContentBlock) -> &str {
    match block {
        ContentBlock::Text { text } => text,
        other => panic!("expected text, got {}", other.type_name()),
    }
}

// ---------------------------------------------------------------------------
// Exact prose: the sentences a model reads.
// ---------------------------------------------------------------------------

#[test]
fn the_text_only_placeholder_names_the_digest_only() {
    let reference =
        ImageAttachmentRef::new(format!("sha256:{}", "a".repeat(64)), "image/png", 1, 1, 1);
    assert_eq!(
        text_only_image_text(&reference),
        "[image omitted because this model accepts text only; attachment sha256:aaaaaaaa]"
    );
}

#[test]
fn the_file_handle_with_a_path_states_the_copy_the_size_and_the_delegation_rule() {
    let handle = file_handle_text(
        &file_ref("notes.pdf"),
        Some("/home/.dsh/attachments/v1/files/ab/x/notes.pdf"),
    );
    assert!(handle.contains("\"notes.pdf\""), "{handle}");
    assert!(handle.contains("42 bytes"), "{handle}");
    assert!(handle.contains("sha256:abababab"), "{handle}");
    assert!(
        handle.contains("\"/home/.dsh/attachments/v1/files/ab/x/notes.pdf\""),
        "{handle}"
    );
    assert!(
        handle.contains("include this saved path in the delegation prompt"),
        "{handle}"
    );
    assert!(
        handle.contains("only subagents sharing this execution environment can read it"),
        "{handle}"
    );
}

#[test]
fn the_file_handle_without_a_path_removes_the_licence_to_pretend() {
    let handle = file_handle_text(&file_ref("notes.pdf"), None);
    assert!(
        handle.contains("current execution environment cannot access a readable path"),
        "{handle}"
    );
    assert!(handle.contains("do not claim to have read it"), "{handle}");
    // The no-path branch must not also claim a readable copy exists.
    assert!(
        !handle.contains("verbatim read-only copy saved at"),
        "{handle}"
    );
}

#[test]
fn the_offloaded_placeholder_matches_upstream_character_for_character() {
    let id = format!("sha256:{}", "d".repeat(64));
    let reference =
        ImageAttachmentRef::new(id.clone(), "image/jpeg", 10, 10, 5).with_name("photo.jpg");
    assert!(
        offloaded_image_text(&reference, None)
            .contains("No local normalized image path is available")
    );
    assert_eq!(
        offloaded_image_text(&reference, Some(&ImageAttachmentAccess::new("/tmp/object"))),
        format!(
            "[image omitted to fit request image limits; \"photo.jpg\" ({id}). Normalized copy \
             (read-only; may be resized or re-encoded): \"/tmp/object\" (10x5px, image/jpeg). Source \
             dimensions, format, and byte size may differ. Copy to a writable path ending in .jpg \
             before editing.]"
        )
    );
}

#[test]
fn every_accepted_media_type_names_its_own_writable_extension() {
    for (media_type, suffix) in [
        ("image/png", ".png"),
        ("image/jpeg", ".jpg"),
        ("image/webp", ".webp"),
        ("image/gif", ".gif"),
    ] {
        let reference =
            ImageAttachmentRef::new(format!("sha256:{}", "e".repeat(64)), media_type, 1, 1, 1);
        let placeholder =
            offloaded_image_text(&reference, Some(&ImageAttachmentAccess::new("/tmp/object")));
        assert!(
            placeholder.contains(&format!("writable path ending in {suffix}")),
            "{media_type}: {placeholder}"
        );
    }
}

#[test]
fn the_request_handle_describes_the_request_preview_not_the_source() {
    let id = format!("sha256:{}", "b".repeat(64));
    let reference = ImageAttachmentRef::new(id.clone(), "image/png", 4_000, 2048, 1536)
        .with_name("source \"map\".png");
    assert_eq!(
        request_image_handle_text(
            &reference,
            (923, 692),
            Some(&ImageAttachmentAccess::new("/tmp/object"))
        ),
        format!(
            "Image \"source \\\"map\\\".png\" ({id}); request preview 923x692px. Normalized copy \
             (read-only; may be resized or re-encoded): \"/tmp/object\" (2048x1536px, image/png). \
             Source dimensions, format, and byte size may differ. Copy to a writable path ending in \
             .png before editing."
        )
    );
}

#[test]
fn the_request_handle_without_access_still_warns_about_re_encoding() {
    let reference = png("photo");
    let handle = request_image_handle_text(&reference, (800, 800), None);
    assert!(
        handle.starts_with(&format!(
            "Image \"photo\" ({}); request preview 800x800px.",
            reference.attachment_id
        )),
        "{handle}"
    );
    assert!(
        handle.contains("source dimensions, format, and byte size may differ"),
        "{handle}"
    );
    assert!(!handle.contains("Normalized copy"), "{handle}");
}

#[test]
fn an_occurrence_is_named_from_its_own_reference_not_the_shared_version() {
    // One prepared version can serve occurrences whose display names differ; naming the version would
    // attribute the wrong file.
    let version_attachment = png("second");
    let first = png("first");
    let handle = request_image_handle_text(&first, (8, 8), None);
    assert!(
        handle.contains("\"first.png\"") || handle.contains("\"first\""),
        "{handle}"
    );
    assert!(!handle.contains("\"second\""), "{handle}");
    assert_eq!(version_attachment.width, 800);
}

#[test]
fn quoting_escapes_what_json_stringify_escapes() {
    assert_eq!(quoted("plain"), "\"plain\"");
    assert_eq!(quoted("a\"b"), "\"a\\\"b\"");
    assert_eq!(quoted("a\\b"), "\"a\\\\b\"");
    assert_eq!(quoted("a\nb"), "\"a\\nb\"");
}

#[test]
fn a_digest_is_eight_hex_digits_with_or_without_the_prefix() {
    assert_eq!(
        digest(&format!("sha256:{}", "0123456789abcdef".repeat(4))),
        "01234567"
    );
    // A store that mints ids some other way still yields a label, not eight characters of nothing.
    assert_eq!(digest("0123456789abcdef"), "01234567");
    assert_eq!(digest("ab"), "ab");
}

// ---------------------------------------------------------------------------
// Block predicates.
// ---------------------------------------------------------------------------

#[test]
fn predicates_see_every_occurrence_not_just_the_first() {
    assert!(!content_has_image(&[text("x")]));
    assert!(!content_has_file(&[text("x")]));
    assert!(content_has_image(&[text("x"), image("a"), text("y")]));
    assert!(content_has_file(&[file("a"), text("y")]));
    assert!(!content_has_image(&[]));
}

#[test]
fn message_predicates_cover_tool_results_as_well_as_user_turns() {
    let tool = tool_message("call-1", vec![text("x"), image("a")]);
    assert_eq!(tool.role, Role::Tool);
    assert!(messages_have_image(std::slice::from_ref(&tool)));
    let user = user_message(vec![text("x"), file("a")]);
    assert!(messages_have_file(std::slice::from_ref(&user)));
    assert!(!messages_have_image(std::slice::from_ref(&user)));
}

// ---------------------------------------------------------------------------
// file projection.
// ---------------------------------------------------------------------------

#[test]
fn a_file_free_history_is_returned_unchanged_in_content() {
    let plain = user_message(vec![text("hi")]);
    let projected = project_files_to_text(std::slice::from_ref(&plain), &|_| None);
    assert_eq!(projected, vec![plain.clone()]);
    let projected = project_files_to_text(std::slice::from_ref(&plain), &|_| None);
    assert_eq!(projected, vec![plain]);
}

#[test]
fn every_file_occurrence_becomes_handle_text_and_the_source_is_untouched() {
    let plain = user_message(vec![text("hi")]);
    let top = user_message(vec![file("f1"), text("keep")]);
    let nested = tool_message("call-3", vec![file("f2")]);
    let messages = vec![plain.clone(), top.clone(), nested.clone()];

    let projected = project_files_to_text(&messages, &|id| {
        Some((
            FileAttachmentRef::new(
                "sha256:",
                if id == "f1" { "top.csv" } else { "nested.csv" },
                42,
            ),
            Some(format!(
                "/copies/{}",
                if id == "f1" { "top.csv" } else { "nested.csv" }
            )),
        ))
    });

    assert_eq!(projected[0], plain);
    let first = &projected[1].content;
    assert!(text_of(&first[0]).contains("\"top.csv\""));
    assert!(text_of(&first[0]).contains("\"/copies/top.csv\""));
    assert_eq!(text_of(&first[1]), "keep");
    assert!(text_of(&projected[2].content[0]).contains("\"nested.csv\""));

    // The durable messages still carry file blocks: projection returns copies.
    assert_eq!(top.content[0], file("f1"));
    assert_eq!(nested.content[0], file("f2"));
}

#[test]
fn an_unresolvable_file_still_reports_that_it_was_uploaded() {
    // A caller that cannot resolve the reference must not silently drop the occurrence: the model has to
    // know a file existed.
    let messages = vec![user_message(vec![file("f1")])];
    let projected = project_files_to_text(&messages, &|_| None);
    let rendered = text_of(&projected[0].content[0]);
    assert!(rendered.contains("was uploaded"), "{rendered}");
    assert!(
        rendered.contains("do not claim to have read it"),
        "{rendered}"
    );
}

#[test]
fn a_resolver_that_cannot_name_a_file_still_yields_a_handle() {
    // A caller with only an id supplies an empty name rather than inventing one; the handle stops
    // describing a copy it cannot vouch for.
    let messages = vec![user_message(vec![file("f1")])];
    let projected = project_files_to_text(&messages, &|id| {
        Some((FileAttachmentRef::new(id, "", 0), None))
    });
    let rendered = text_of(&projected[0].content[0]);
    assert!(rendered.contains("File \"\" "), "{rendered}");
    assert!(rendered.contains("0 bytes"), "{rendered}");
    assert!(
        !rendered.contains("verbatim read-only copy saved at"),
        "{rendered}"
    );
}

// ---------------------------------------------------------------------------
// text-only image projection.
// ---------------------------------------------------------------------------

#[test]
fn an_image_free_history_is_untouched() {
    let plain = user_message(vec![text("plain")]);
    let result = tool_message("plain-result", vec![text("plain")]);
    let messages = vec![plain.clone(), result.clone()];
    assert_eq!(project_images_for_text_model(&messages), messages);
    assert_eq!(
        project_image_blocks_for_text_model(&messages, &|_| None),
        messages
    );
}

#[test]
fn direct_and_nested_images_become_placeholders_in_place() {
    let plain = user_message(vec![text("plain")]);
    let unchanged_tool = tool_message("text-only", vec![text("unchanged")]);
    let id = format!("sha256:{}", "a".repeat(64));
    let visual = user_message(vec![text("lead"), image(&id)]);
    let visual_tool = tool_message(
        "nested-image",
        vec![text("before"), image(&id), text("after")],
    );

    let projected = project_images_for_text_model(&[
        plain.clone(),
        visual,
        unchanged_tool.clone(),
        visual_tool,
    ]);

    assert_eq!(projected[0], plain);
    let placeholder =
        "[image omitted because this model accepts text only; attachment sha256:aaaaaaaa]";
    assert_eq!(projected[1].content, vec![text("lead"), text(placeholder)]);
    assert_eq!(projected[2], unchanged_tool);
    assert_eq!(
        projected[3].content,
        vec![text("before"), text(placeholder), text("after")]
    );
}

#[test]
fn a_placeholder_takes_the_digest_from_the_resolved_reference() {
    let id = format!("sha256:{}", "z".repeat(64));
    let messages = vec![user_message(vec![image(&id)])];
    let projected = project_image_blocks_for_text_model(&messages, &|resolved| {
        Some(ImageAttachmentRef::new(
            resolved.to_owned(),
            "image/png",
            1,
            1,
            1,
        ))
    });
    assert!(text_of(&projected[0].content[0]).contains("sha256:zzzzzzzz"));
}

// ---------------------------------------------------------------------------
// offloaded image projection.
// ---------------------------------------------------------------------------

#[test]
fn only_marked_occurrences_are_replaced() {
    let messages = vec![
        tool_message("shot", vec![image("marked")]),
        user_message(vec![image("marked"), image("retained")]),
    ];
    let projected = project_offloaded_images(
        &messages,
        &|id| {
            Some(ImageBlock {
                attachment: ImageAttachmentRef::new(id.to_owned(), "image/png", 3, 1, 1),
                offloaded: id == "marked",
            })
        },
        &|_| OMITTED.to_owned(),
    );
    assert_eq!(projected[0].content, vec![text(OMITTED)]);
    assert_eq!(projected[1].content, vec![text(OMITTED), image("retained")]);
    // The source still has its marks.
    assert_eq!(messages[1].content[0], image("marked"));
}

#[test]
fn an_unresolvable_image_is_left_for_the_next_policy() {
    // A route that cannot resolve a reference must not claim it was offloaded.
    let messages = vec![user_message(vec![image("unknown")])];
    let projected = project_offloaded_images(&messages, &|_| None, &|_| OMITTED.to_owned());
    assert_eq!(projected[0].content, vec![image("unknown")]);
}

#[test]
fn the_placeholder_text_is_route_owned_while_the_set_is_not() {
    let messages = vec![user_message(
        vec![
            ImageBlock::offloaded(ImageAttachmentRef::new("a", "image/png", 3, 1, 1)).attachment,
            ImageBlock::retained(ImageAttachmentRef::new("b", "image/png", 3, 1, 1)).attachment,
        ]
        .into_iter()
        .map(|reference| image(&reference.attachment_id))
        .collect(),
    )];
    let projected = project_offloaded_images(
        &messages,
        &|id| {
            Some(ImageBlock {
                attachment: ImageAttachmentRef::new(id.to_owned(), "image/png", 3, 1, 1),
                offloaded: id == "a",
            })
        },
        &|reference| format!("{OMITTED}:{}", reference.attachment_id),
    );
    assert_eq!(
        projected[0].content,
        vec![text(&format!("{OMITTED}:a")), image("b")]
    );
}

// ---------------------------------------------------------------------------
// offload arithmetic.
// ---------------------------------------------------------------------------

#[test]
fn base64_length_counts_padding() {
    assert_eq!(base64_length(0), 0);
    assert_eq!(base64_length(1), 4);
    assert_eq!(base64_length(2), 4);
    assert_eq!(base64_length(3), 4);
    assert_eq!(base64_length(4), 8);
    assert_eq!(base64_length(1024), 1368);
}

#[test]
fn nothing_is_removed_under_unbounded_budgets() {
    let lengths = [4_u64, 4, 4, 4];
    assert_eq!(
        offloaded_image_prefix_count(&lengths, &ImageRequestBudget::unbounded()),
        0
    );
    let exact_bytes = ImageRequestBudget::unbounded().with_max_bytes(16);
    assert_eq!(offloaded_image_prefix_count(&lengths, &exact_bytes), 0);
    let exact_count = ImageRequestBudget::unbounded().with_max_images(4);
    assert_eq!(offloaded_image_prefix_count(&lengths, &exact_count), 0);
}

#[test]
fn one_excess_image_rounds_up_to_the_whole_count_quantum() {
    let lengths = [4_u64, 4, 4, 4, 4];
    let budget = ImageRequestBudget::unbounded()
        .with_max_images(4)
        .with_count_quantum(2);
    assert_eq!(offloaded_image_prefix_count(&lengths, &budget), 2);
}

#[test]
fn one_excess_byte_removes_a_whole_byte_quantum_crossing_the_second_image() {
    let lengths = [4_u64, 4, 4, 4, 1];
    let budget = ImageRequestBudget::unbounded()
        .with_max_bytes(16)
        .with_byte_quantum(5);
    assert_eq!(offloaded_image_prefix_count(&lengths, &budget), 2);
}

#[test]
fn a_quantum_of_one_stops_at_the_target_while_a_larger_one_crosses_it() {
    // The asymmetry is upstream's and is observable: with a quantum of 1 the loop stops once the removed
    // total *reaches* the target; above 1 it stops only once the total *exceeds* it.
    let lengths = [4_u64, 4, 4, 4];
    let reaches = ImageRequestBudget::unbounded()
        .with_max_bytes(12)
        .with_byte_quantum(1);
    assert_eq!(offloaded_image_prefix_count(&lengths, &reaches), 1);
    let crosses = ImageRequestBudget::unbounded()
        .with_max_bytes(12)
        .with_byte_quantum(2);
    assert_eq!(offloaded_image_prefix_count(&lengths, &crosses), 2);
}

#[test]
fn a_hundred_and_twenty_nine_mebibytes_offload_sixty_five() {
    // The published vector: 129 one-mebibyte images under a 128 MiB bound with a 64 MiB quantum
    // offload the oldest 65.
    let mib = 1024 * 1024;
    let lengths = vec![mib; 129];
    let budget = ImageRequestBudget::unbounded()
        .with_max_bytes(128 * mib)
        .with_byte_quantum(64 * mib);
    assert_eq!(offloaded_image_prefix_count(&lengths, &budget), 65);
}

#[test]
fn a_base64_route_budget_is_measured_in_encoded_bytes() {
    // Three three-byte images are four base64 characters each: twelve represented bytes against an
    // eight-byte budget removes the oldest one.
    let mut budget = ImageRequestBudget::unbounded().with_max_bytes(8);
    budget.representation = Representation::Base64;
    assert_eq!(budget.represent(3), 4);
    let lengths: Vec<u64> = [3_u64, 3, 3]
        .iter()
        .map(|bytes| budget.represent(*bytes))
        .collect();
    assert_eq!(offloaded_image_prefix_count(&lengths, &budget), 1);
}

#[test]
fn traversal_visits_images_in_message_order_across_roles() {
    let messages = vec![
        user_message(vec![text("before"), image("a")]),
        tool_message("shot", vec![text("x"), image("b")]),
        tool_message("second", vec![image("c")]),
        user_message(vec![image("d")]),
    ];
    let seen = std::cell::RefCell::new(Vec::new());
    let budget = ImageRequestBudget::unbounded().with_max_images(0);
    let count = required_image_offload(&messages, &budget, &|block| {
        let bytes = match block.attachment.attachment_id.as_str() {
            "a" => 1_u64,
            "b" => 2,
            "c" => 4,
            _ => 3,
        };
        seen.borrow_mut().push(bytes);
        bytes
    });
    // Message order, not role order: a tool result between two user turns is visited where it sits.
    assert_eq!(seen.borrow().clone(), vec![1, 2, 4, 3]);
    assert_eq!(count, 4);
}

#[test]
fn an_already_offloaded_occurrence_is_not_counted_again() {
    let occurrences = vec![
        ImageBlock::offloaded(ImageAttachmentRef::new("a", "image/png", 3, 1, 1)),
        ImageBlock::retained(ImageAttachmentRef::new("b", "image/png", 3, 1, 1)),
    ];
    let budget = ImageRequestBudget::unbounded().with_max_images(1);
    // One retained occurrence, bound of one: nothing to do.
    assert_eq!(
        required_image_offload_blocks(&occurrences, &budget, &|_| 3),
        0
    );
    let tight = ImageRequestBudget::unbounded().with_max_images(0);
    assert_eq!(
        required_image_offload_blocks(&occurrences, &tight, &|_| 3),
        1
    );
}

// ---------------------------------------------------------------------------
// the whole-request projection, and the order of its policies.
// ---------------------------------------------------------------------------

fn resolving(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn an_offloaded_image_is_explained_as_a_limit_not_as_a_capability() {
    let messages = vec![user_message(vec![image("shot")])];
    let projected = project_request(
        &messages,
        false,
        &|id| {
            Some(ImageBlock {
                attachment: ImageAttachmentRef::new(id.to_owned(), "image/png", 3, 1, 1),
                offloaded: true,
            })
        },
        &|_| None,
        &|_| None,
    );
    let rendered = resolving(&projected.messages);
    assert_eq!(rendered.len(), 1);
    assert!(
        rendered[0].contains("image omitted to fit request image limits"),
        "{rendered:?}"
    );
    assert!(!rendered[0].contains("accepts text only"), "{rendered:?}");
    assert_eq!(projected.offloaded_images, 1);
    assert_eq!(projected.text_only_images, 0);
    assert!(!projected.is_identity());
}

#[test]
fn a_retained_image_on_a_text_only_route_is_explained_as_a_capability() {
    let messages = vec![user_message(vec![image(&format!(
        "sha256:{}",
        "a".repeat(64)
    ))])];
    let projected = project_request(
        &messages,
        false,
        &|id| {
            Some(ImageBlock {
                attachment: ImageAttachmentRef::new(id.to_owned(), "image/png", 3, 1, 1),
                offloaded: false,
            })
        },
        &|_| None,
        &|_| None,
    );
    assert_eq!(
        projected.messages[0].content,
        vec![text(
            "[image omitted because this model accepts text only; attachment sha256:aaaaaaaa]"
        )]
    );
    assert_eq!(projected.text_only_images, 1);
    assert_eq!(projected.offloaded_images, 0);
}

#[test]
fn a_vision_route_keeps_its_retained_images_and_still_loses_its_files() {
    let messages = vec![user_message(vec![image("keep"), file("f1")])];
    let projected = project_request(
        &messages,
        true,
        &|id| {
            Some(ImageBlock {
                attachment: ImageAttachmentRef::new(id.to_owned(), "image/png", 3, 1, 1),
                offloaded: false,
            })
        },
        &|_| {
            Some((
                FileAttachmentRef::new("sha256:", "a.txt", 1),
                Some("/copies/a.txt".to_owned()),
            ))
        },
        &|_| None,
    );
    assert_eq!(projected.messages[0].content[0], image("keep"));
    assert!(text_of(&projected.messages[0].content[1]).contains("\"a.txt\""));
    assert_eq!(projected.files, 1);
    assert_eq!(projected.text_only_images, 0);
}

#[test]
fn an_untouched_request_reports_identity() {
    let messages = vec![user_message(vec![text("plain")])];
    let projected = project_request(&messages, true, &|_| None, &|_| None, &|_| None);
    assert_eq!(projected.messages, messages);
    assert!(projected.is_identity());
}

#[test]
fn every_message_role_is_projected_not_only_user_turns() {
    let messages = vec![
        user_message(vec![file("f1")]),
        tool_message("call-9", vec![file("f2")]),
    ];
    let projected = project_request(
        &messages,
        true,
        &|_| None,
        &|id| Some((FileAttachmentRef::new("sha256:", id, 7), None)),
        &|_| None,
    );
    assert_eq!(projected.files, 2);
    for message in &projected.messages {
        assert!(!messages_have_file(std::slice::from_ref(message)));
        assert!(
            message
                .content
                .iter()
                .all(|block| matches!(block, ContentBlock::Text { .. }))
        );
    }
}

// ---------------------------------------------------------------------------
// the declared gap.
// ---------------------------------------------------------------------------

#[test]
fn the_tool_update_gap_names_its_missing_inputs() {
    // `projectToolUpdates` is the one upstream projection this module does not port; the reason is that
    // its inputs do not exist in this crate yet, and that reason is pinned so the gap cannot be quietly
    // forgotten or quietly "fixed" against an invented history model.
    assert!(TOOL_UPDATE_GAP.contains("projectToolUpdates"));
    assert!(TOOL_UPDATE_GAP.contains("Developer"));
    assert!(TOOL_UPDATE_GAP.contains("ToolHistory"));
}

#[test]
fn a_request_position_can_be_named_without_inventing_an_identity() {
    let id = position_id(3).expect("a short digit string is a valid id");
    assert_eq!(id.as_str(), "m3");
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod session {
    pub use super::content_projection_gap as content_projection;
}
