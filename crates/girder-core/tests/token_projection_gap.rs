//! Tests for the token accounting folds.
//!
//! The module is included by path because `girder-core`'s crate root does not declare it yet —
//! wiring `pub mod token_projection_gap;` and its `pub use` belongs to the integrator. Under that
//! inclusion the module is a child of *this* test crate, which is why it reaches the crate's types
//! through `girder_core::` re-exports rather than `crate::` paths: those resolve the same way in
//! both builds, so one source file is the single thing under test.
//!
//! # What these tests are for
//!
//! Three claims carry the weight, and each is checked from the side that can fail.
//!
//! The first is that a replacement is a *signed* transition. A fold that merely re-summed the
//! surviving nodes, and a fold that applied deltas correctly, agree on every append-only log; they
//! disagree exactly when a compaction shrinks the surface. So the tests assert the delta *and* the
//! total after a replacement, and one of them asserts a negative delta.
//!
//! The second is that a disclosure refuses rather than guesses. Upstream answers `undefined` for
//! every unprovable turn; the equivalent here is a named [`TurnUsageGap`], and the tests assert the
//! *specific* variant rather than merely that something failed. A test that only checked
//! `is_err()` would pass against a fold that refused every turn.
//!
//! The third is that a re-reported settlement does not double-count while a retried attempt does,
//! because both arrive as two `model/usage` events for one step and only the lifecycle between them
//! says which it is. That is the one rule a plain sum gets wrong, so it is tested in both
//! directions.

#[path = "../src/token_projection_gap.rs"]
mod token_projection_gap;

use girder_core::{
    ChatMessage, ChatRole, ContentBlock, Message, MessageId, MessageSource, SessionEvent,
    SurfaceEnvelope, ToolSchema, TurnEndReason,
};
use token_projection_gap::{
    ContextBreakdown, ImageOccurrence, ProjectionError, SurfaceTarget, TokenUsageProjection,
    TurnUsageGap, context_breakdown, context_pressure, derive_turn_usage, derive_turn_usages,
    estimate_tools, fold_token_usage, price_surface,
};

// ---- fixtures ----------------------------------------------------------------------------------

/// A user message that appends to the surface.
fn user(seq: u64, text: &str) -> SurfaceEnvelope {
    SurfaceEnvelope::appended(SessionEvent::user_text(&format!("m{seq}"), text), seq)
}

/// An assistant message that appends to the surface.
fn assistant(seq: u64, text: &str) -> SurfaceEnvelope {
    SurfaceEnvelope::appended(
        SessionEvent::assistant_text(&format!("m{seq}"), text, "provider"),
        seq,
    )
}

/// A logged message with an explicit role and content.
fn logged(id: &str, role: ChatRole, content: Vec<ContentBlock>) -> ChatMessage {
    let source = match role {
        ChatRole::Assistant => MessageSource::Model {
            provider: "p".to_owned(),
            model: "m".to_owned(),
            replay_state: None,
        },
        ChatRole::System => MessageSource::SystemPrompt,
        _ => MessageSource::User { form: None },
    };
    ChatMessage::new(
        MessageId::new(id).expect("test ids are legal"),
        role,
        source,
        content,
    )
}

/// A system prompt that appends to the surface.
fn system(seq: u64, text: &str) -> SurfaceEnvelope {
    let message = logged(
        &format!("m{seq}"),
        ChatRole::System,
        vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
    );
    SurfaceEnvelope::appended(
        SessionEvent::SystemMessage {
            turn: 1,
            step: 1,
            message,
        },
        seq,
    )
}

/// A tool result that appends to the surface.
fn tool_result(seq: u64, text: &str) -> SurfaceEnvelope {
    SurfaceEnvelope::appended(
        SessionEvent::ToolResult {
            id: girder_core::ToolCallId::new(format!("c{seq}")).expect("test ids are legal"),
            content: vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            is_error: false,
        },
        seq,
    )
}

/// An assistant message carrying one image reference.
fn assistant_with_image(seq: u64, reference: &str) -> SurfaceEnvelope {
    let message = logged(
        &format!("m{seq}"),
        ChatRole::Assistant,
        vec![
            ContentBlock::Text {
                text: "look".to_owned(),
            },
            ContentBlock::Image {
                attachment: reference.to_owned(),
            },
        ],
    );
    SurfaceEnvelope::appended(SessionEvent::AssistantMessage { message }, seq)
}

/// Replaces the surface range `start..=end` with a new user message at `seq`.
///
/// Provenance cites exactly the shadowed nodes, which the fold requires.
fn replace(seq: u64, text: &str, start: u64, end: u64) -> SurfaceEnvelope {
    let sources = (start..=end).collect();
    SurfaceEnvelope::replacing(
        SessionEvent::user_text(&format!("m{seq}"), text),
        seq,
        start,
        end,
        sources,
    )
}

/// A log-only step boundary.
fn step_start(seq: u64, turn: u64, step: u64) -> SurfaceEnvelope {
    SurfaceEnvelope::log_only(SessionEvent::StepStart { turn, step }, seq)
}

/// A log-only step close.
fn step_end(seq: u64, turn: u64, step: u64) -> SurfaceEnvelope {
    SurfaceEnvelope::log_only(SessionEvent::StepEnd { turn, step }, seq)
}

/// A log-only usage record.
fn usage(seq: u64, model: &str, input: u64, output: u64) -> SurfaceEnvelope {
    SurfaceEnvelope::log_only(
        SessionEvent::ModelUsage {
            model: model.to_owned(),
            input_tokens: input,
            output_tokens: output,
        },
        seq,
    )
}

/// A log-only retry marker.
fn retry(seq: u64) -> SurfaceEnvelope {
    SurfaceEnvelope::log_only(
        SessionEvent::LlmRetry {
            attempt: 2,
            reason: None,
        },
        seq,
    )
}

/// A complete one-step turn whose single attempt reports `input`/`output`.
fn one_step_turn(input: u64, output: u64) -> Vec<SurfaceEnvelope> {
    vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        usage(2, "deepseek-chat", input, output),
        step_end(3, 1, 1),
        SurfaceEnvelope::log_only(
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            4,
        ),
    ]
}

/// The heuristic price of a one-block user message of `text`.
fn user_price(text: &str) -> u64 {
    girder_core::estimate_blocks(&[ContentBlock::Text {
        text: text.to_owned(),
    }]) + token_projection_gap::ROLE_OVERHEAD
}

// ---- pricing a surface -------------------------------------------------------------------------

#[test]
fn every_appended_node_carries_its_own_price() {
    let log = vec![system(0, "sys"), user(1, "hi"), assistant(2, "hello")];

    let surface = price_surface(&log).expect("an append-only log is a valid surface");

    assert_eq!(surface.nodes().len(), 3);
    assert_eq!(surface.transitions().len(), 3);
    for transition in surface.transitions() {
        assert_eq!(transition.target, SurfaceTarget::Append);
        assert_eq!(
            transition.delta_tokens,
            i64::try_from(transition.tokens).unwrap()
        );
    }
    // The total is the sum of the node prices, not a separately tracked number that could drift.
    let summed: u64 = surface
        .nodes()
        .iter()
        .map(|node| node.heuristic_tokens)
        .sum();
    assert_eq!(surface.surface_tokens(), summed);
    assert_eq!(surface.heuristic_surface_tokens(), summed);
    assert!(summed > 0, "the estimator must charge something for text");
}

#[test]
fn a_system_node_is_flagged_and_an_ordinary_one_is_not() {
    // The breakdown picks the system prompt out of the surface, and the log is the only place that
    // fact lives, so the flag has to survive the fold.
    let surface = price_surface(&[system(0, "sys"), user(1, "hi"), tool_result(2, "out")])
        .expect("a valid surface");

    let flags: Vec<bool> = surface.nodes().iter().map(|node| node.is_system).collect();
    assert_eq!(flags, vec![true, false, false]);
}

#[test]
fn a_tool_result_is_priced_from_its_content() {
    // A tool result carries blocks inline rather than a message wrapper, so a fold that only read
    // `derive_event_message` would price every one of them at zero.
    let surface =
        price_surface(&[tool_result(0, "a result that costs something")]).expect("a valid surface");

    assert!(surface.nodes()[0].heuristic_tokens > 0);
}

#[test]
fn a_replacement_shrinks_the_total_by_the_price_it_shadows() {
    // The claim under test: the delta is signed and exact. A fold that re-summed the surviving
    // nodes would agree here; a fold that ignored the removal would not.
    let filler = "a".repeat(400);
    let log = vec![
        user(0, &filler),
        user(1, &filler),
        assistant(2, "short"),
        replace(3, "summarized", 0, 1),
    ];

    let surface = price_surface(&log).expect("a valid surface");

    let shadowed = price_surface(&log[..2])
        .expect("the prefix is valid")
        .surface_tokens();
    let replacement = surface
        .transitions()
        .last()
        .expect("the replacement has a transition");
    assert_eq!(
        replacement.target,
        SurfaceTarget::Replace {
            start_idx: 0,
            end_idx: 1
        }
    );
    assert_eq!(
        replacement.delta_tokens,
        i64::try_from(replacement.tokens).unwrap() - i64::try_from(shadowed).unwrap()
    );
    assert!(
        replacement.delta_tokens < 0,
        "summarizing two long messages must reduce the surface"
    );
    // Two long nodes collapsed into one short one, so the total is far below the pre-replacement one.
    let before = price_surface(&log[..3])
        .expect("the prefix is valid")
        .surface_tokens();
    assert!(surface.surface_tokens() < before);
}

#[test]
fn a_replacement_keeps_the_range_position_rather_than_moving_to_the_tail() {
    // A compaction summarizes where it happened. A fold that appended the replacement would report
    // the same total and the wrong order, which is why the position is asserted.
    let log = vec![
        user(0, "first"),
        user(1, "second"),
        user(2, "third"),
        replace(3, "second and third", 1, 2),
    ];

    let surface = price_surface(&log).expect("a valid surface");

    let sequences: Vec<u64> = surface.nodes().iter().map(|node| node.seq).collect();
    assert_eq!(sequences, vec![0, 3]);
}

#[test]
fn a_replacement_range_off_the_surface_is_refused() {
    // Both folds validate placement; a price attached to a log that is not a surface is a number
    // about nothing, so this must fail rather than silently skip the event.
    let log = vec![user(0, "first"), replace(1, "x", 0, 5)];

    let error = price_surface(&log).expect_err("range 0-5 is not on the surface");

    assert!(
        matches!(error, ProjectionError::Surface(_)),
        "got {error:?}"
    );
}

#[test]
fn a_non_contiguous_log_is_refused() {
    let log = vec![user(0, "first"), user(2, "gap")];

    let error = price_surface(&log).expect_err("seq 1 is missing");

    assert!(
        matches!(error, ProjectionError::Surface(_)),
        "got {error:?}"
    );
}

#[test]
fn an_empty_log_prices_at_zero() {
    let surface = price_surface(&[]).expect("an empty log is trivially a surface");

    assert!(surface.nodes().is_empty());
    assert!(surface.transitions().is_empty());
    assert_eq!(surface.surface_tokens(), 0);
}

// ---- route pricing -----------------------------------------------------------------------------

#[test]
fn image_occurrences_are_collected_with_their_structural_price() {
    let surface = price_surface(&[assistant_with_image(0, "att-1")]).expect("a valid surface");

    let images = &surface.nodes()[0].images;
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].reference, "att-1");
    assert!(
        images[0].structural_tokens > 0,
        "the reference itself costs structural tokens under the heuristic"
    );
}

#[test]
fn a_route_price_replaces_the_structural_price_rather_than_adding_to_it() {
    // The reference never reaches the model, so charging both the structural price and the visual
    // one would count the same block twice. The expected total is therefore
    // `heuristic - structural + visual`, not `heuristic + visual`.
    let log = vec![assistant_with_image(0, "att-1")];
    let heuristic = price_surface(&log).expect("a valid surface");
    let structural = heuristic.nodes()[0].images[0].structural_tokens;
    let visual = 900u64;

    let priced = heuristic
        .with_image_prices(Some(&[visual]))
        .expect("one price for one occurrence");

    assert_eq!(
        priced.nodes()[0].tokens,
        heuristic.nodes()[0].heuristic_tokens - structural + visual
    );
    assert_eq!(priced.surface_tokens(), priced.nodes()[0].tokens);
    // The heuristic price is still reported, so a caller can compare a route against it.
    assert_eq!(
        priced.nodes()[0].heuristic_tokens,
        heuristic.nodes()[0].heuristic_tokens
    );
    assert_eq!(
        priced.heuristic_surface_tokens(),
        heuristic.surface_tokens()
    );
    // The transitions are unaffected: a route changes prices, not the log's shape.
    assert_eq!(priced.transitions(), heuristic.transitions());
}

#[test]
fn a_text_only_route_keeps_the_heuristic_price() {
    let surface = price_surface(&[assistant_with_image(0, "att-1")]).expect("a valid surface");

    let priced = surface
        .with_image_prices(None)
        .expect("None never misaligns");

    assert_eq!(priced, surface);
}

#[test]
fn a_misaligned_price_vector_is_refused() {
    // Upstream throws here for the same reason: misalignment would silently price node *n* with
    // occurrence *n-1*'s image, which is a wrong number rather than a missing one.
    let log = vec![
        assistant_with_image(0, "att-1"),
        assistant_with_image(1, "att-2"),
    ];
    let surface = price_surface(&log).expect("a valid surface");

    let error = surface
        .with_image_prices(Some(&[100]))
        .expect_err("two occurrences need two prices");

    assert_eq!(
        error,
        ProjectionError::ImagePriceArity {
            given: 1,
            needed: 2
        }
    );
}

#[test]
fn image_prices_are_consumed_in_surface_order() {
    let log = vec![
        assistant_with_image(0, "att-1"),
        assistant_with_image(1, "att-2"),
    ];
    let surface = price_surface(&log).expect("a valid surface");
    let structural: Vec<u64> = surface
        .nodes()
        .iter()
        .map(|node| node.images[0].structural_tokens)
        .collect();
    let heuristic: Vec<u64> = surface
        .nodes()
        .iter()
        .map(|node| node.heuristic_tokens)
        .collect();

    let priced = surface
        .with_image_prices(Some(&[111, 222]))
        .expect("two prices for two occurrences");

    // Reversing the inputs must change the result, or order is not being honoured.
    assert_eq!(priced.nodes()[0].tokens, heuristic[0] - structural[0] + 111);
    assert_eq!(priced.nodes()[1].tokens, heuristic[1] - structural[1] + 222);
    let reversed = surface
        .with_image_prices(Some(&[222, 111]))
        .expect("two prices for two occurrences");
    assert_ne!(priced.nodes(), reversed.nodes());
}

#[test]
fn a_surface_with_no_images_refuses_a_nonempty_price_vector() {
    let surface = price_surface(&[user(0, "no images here")]).expect("a valid surface");

    let error = surface
        .with_image_prices(Some(&[7]))
        .expect_err("there is no occurrence to price");

    assert_eq!(
        error,
        ProjectionError::ImagePriceArity {
            given: 1,
            needed: 0
        }
    );
    // An empty vector is the correct way to say "price nothing".
    assert_eq!(
        surface.with_image_prices(Some(&[])).expect("zero for zero"),
        surface
    );
}

// ---- context breakdown -------------------------------------------------------------------------

#[test]
fn the_breakdown_classifies_the_last_nonempty_system_prompt() {
    // Two surviving system prompts: the later one is the one a request would send, so the earlier
    // one is counted as message content. Upstream's rule, and the reason it keeps a list.
    let log = vec![
        system(0, "first prompt"),
        system(1, "second prompt"),
        user(2, "hi"),
    ];

    let breakdown = context_breakdown(&log, &[]).expect("a valid surface");

    let surface = price_surface(&log).expect("a valid surface");
    let second = surface.nodes()[1].heuristic_tokens;
    assert_eq!(breakdown.system_tokens, second);
    assert_eq!(
        breakdown.message_tokens,
        surface.surface_tokens() - second,
        "every other node, including the superseded prompt, is message content"
    );
    assert_eq!(breakdown.tools_tokens, 0);
}

#[test]
fn a_zero_priced_system_node_does_not_become_the_system_figure() {
    // A zero-price system node is not "the system prompt": upstream takes the last system node with
    // `tokens > 0`, because reporting 0 would be indistinguishable from a surface with no prompt.
    //
    // The node has to be genuinely empty to price at zero. A system message carrying one *empty*
    // text block still pays role framing — its content vector is not empty — and that is upstream's
    // rule too (`content.length === 0`), so it is asserted rather than worked around.
    let genuine_empty = SurfaceEnvelope::appended(
        SessionEvent::SystemMessage {
            turn: 1,
            step: 1,
            message: logged("m-empty", ChatRole::System, Vec::new()),
        },
        1,
    );
    let log = vec![system(0, "real prompt"), genuine_empty];

    let breakdown = context_breakdown(&log, &[]).expect("a valid surface");

    let only_first = price_surface(&log[..1]).expect("valid").surface_tokens();
    assert!(breakdown.system_tokens > 0);
    assert_eq!(
        breakdown.system_tokens, only_first,
        "the empty node prices at 0, so the last nonempty prompt is still the first one"
    );
    assert_eq!(
        price_surface(&log).expect("valid").surface_tokens(),
        only_first,
        "and the empty node adds nothing to the message figure either"
    );
}

#[test]
fn a_system_prompt_with_an_empty_text_block_still_pays_role_framing() {
    // The counterpart rule, asserted so the distinction above is not mistaken for an accident: an
    // empty *block* is not an empty *message*, and only the latter is free.
    let log = vec![system(0, "")];

    let surface = price_surface(&log).expect("a valid surface");

    assert_eq!(
        surface.nodes()[0].heuristic_tokens,
        token_projection_gap::ROLE_OVERHEAD
    );
}

#[test]
fn a_compaction_removes_the_surviving_system_prompt_it_shadows() {
    // The bounded-state version of this fold cannot see which system prompts a claim removes; this
    // one subtracts the exact nodes, so the figures follow the surface.
    //
    // The prompt is the *second* node here rather than the first, because `fold_surface` protects
    // node zero when it holds the session's opening prompt: only another system message may rewrite
    // that contract. A later re-assembled prompt is ordinary compaction fodder.
    let log = vec![
        user(0, "hello"),
        system(1, "prompt"),
        replace(2, "summary", 0, 1),
    ];

    let breakdown = context_breakdown(&log, &[]).expect("a valid surface");

    assert_eq!(
        breakdown.system_tokens, 0,
        "no system node survives the compaction"
    );
    assert_eq!(
        breakdown.message_tokens,
        price_surface(&log).expect("valid").surface_tokens()
    );
}

#[test]
fn tools_are_priced_from_the_catalogue_and_zero_when_absent() {
    let schema = ToolSchema {
        name: "bash".to_owned(),
        description: "run a command".to_owned(),
        parameters: serde_json::json!({"type": "object"}),
    };
    let log = vec![user(0, "hi")];

    let without = context_breakdown(&log, &[]).expect("a valid surface");
    let with = context_breakdown(&log, &[schema]).expect("a valid surface");

    assert_eq!(without.tools_tokens, 0);
    assert!(
        with.tools_tokens > token_projection_gap::BLOCK_OVERHEAD,
        "a non-empty catalogue costs its serialized text plus framing, never just the framing"
    );
    // Tools are a separate figure: they must not leak into the message total.
    assert_eq!(with.message_tokens, without.message_tokens);
    assert_eq!(with.system_tokens, without.system_tokens);
}

#[test]
fn the_breakdown_figures_are_heuristic_and_need_not_sum_to_a_pressure_total() {
    // The documented trap: composition and occupancy are different questions priced by different
    // methods, so a caller must not present one as the other. This asserts the two disagree, which
    // is what makes the warning load-bearing rather than decorative.
    let log = vec![
        system(0, "prompt"),
        user(1, "a rather long user message with words in it"),
    ];
    let mut log = log;
    log.push(SurfaceEnvelope::log_only(
        SessionEvent::ModelUsage {
            model: "deepseek-chat".to_owned(),
            input_tokens: 5,
            output_tokens: 2,
        },
        2,
    ));

    let breakdown = context_breakdown(&log, &[]).expect("a valid surface");
    let pressure = context_pressure(&log).expect("a valid surface");

    let composed = breakdown.system_tokens + breakdown.tools_tokens + breakdown.message_tokens;
    assert!(composed > 0);
    // The anchor is the provider's own figure and only the *delta* is heuristic, so the projection
    // is not the composed total: the two answer different questions and a caller must not present
    // one as the other. Here the surface has not moved since the sample, so the projection is just
    // the provider figure — while the composition is a nonzero heuristic figure that does not
    // equal it.
    assert_eq!(pressure.pressure_tokens, Some(5));
    assert_eq!(pressure.projected_tokens, Some(5));
    assert_ne!(composed, 5);
}

// ---- context pressure --------------------------------------------------------------------------

#[test]
fn pressure_is_absent_before_any_provider_reports_usage() {
    let pressure =
        context_pressure(&[system(0, "prompt"), user(1, "hi")]).expect("a valid surface");

    assert_eq!(pressure.pressure_tokens, None);
    assert_eq!(pressure.projected_tokens, None);
    assert_eq!(pressure.context_window, None);
}

#[test]
fn the_projected_figure_follows_the_surface_after_the_sample() {
    let mut log = vec![user(0, "first")];
    log.push(SurfaceEnvelope::log_only(
        SessionEvent::ModelUsage {
            model: "deepseek-chat".to_owned(),
            input_tokens: 100,
            output_tokens: 20,
        },
        1,
    ));
    let sampled = context_pressure(&log).expect("a valid surface");
    assert_eq!(sampled.projected_tokens, Some(100));

    // A message admitted after the sample moves the projection but not the provider's figure.
    log.push(user(2, "second"));
    let after = context_pressure(&log).expect("a valid surface");
    let added = price_surface(&log).expect("valid").nodes()[1].heuristic_tokens;

    assert_eq!(after.pressure_tokens, Some(100));
    assert_eq!(after.projected_tokens, Some(100 + added));
}

#[test]
fn a_compaction_moves_the_projection_which_the_provider_figure_cannot() {
    // The whole reason the projection exists: a compaction reports no usage of its own, so only a
    // surface-aware delta can show the prompt getting smaller.
    let filler = "a".repeat(400);
    let mut log = vec![user(0, &filler), user(1, &filler)];
    log.push(SurfaceEnvelope::log_only(
        SessionEvent::ModelUsage {
            model: "deepseek-chat".to_owned(),
            input_tokens: 500,
            output_tokens: 10,
        },
        2,
    ));
    log.push(replace(3, "summary", 0, 1));

    let pressure = context_pressure(&log).expect("a valid surface");

    assert_eq!(
        pressure.pressure_tokens,
        Some(500),
        "the provider figure is untouched"
    );
    let projected = pressure.projected_tokens.expect("a sample exists");
    assert!(
        projected < 500,
        "the shadowed span lowers the next prompt's cost"
    );
}

#[test]
fn the_projection_adds_back_the_response_the_next_prompt_will_carry() {
    // The anchor excludes the settled response, because the prompt that produced it did not contain
    // it. But the *next* request's prompt will contain it, so the growth since the sample is exactly
    // that response's price. A fold that anchored on the surface total as it stood after the
    // assistant node would report no growth at all and understate the next prompt by one response.
    let log = vec![
        user(0, "hi"),
        assistant(1, "a long answer"),
        SurfaceEnvelope::log_only(
            SessionEvent::ModelUsage {
                model: "deepseek-chat".to_owned(),
                input_tokens: 100,
                output_tokens: 60,
            },
            2,
        ),
    ];

    let pressure = context_pressure(&log).expect("a valid surface");
    let response = price_surface(&log).expect("valid").nodes()[1].heuristic_tokens;

    assert_eq!(pressure.pressure_tokens, Some(100));
    assert_eq!(pressure.projected_tokens, Some(100 + response));
}

#[test]
fn a_later_prompt_does_not_double_count_the_settled_response() {
    // The mirror of the test above: a response that settled an *earlier* sample is part of the
    // prompt already measured, so it must not be added a second time.
    let log = vec![
        user(0, "hi"),
        assistant(1, "answer"),
        SurfaceEnvelope::log_only(
            SessionEvent::ModelUsage {
                model: "deepseek-chat".to_owned(),
                input_tokens: 100,
                output_tokens: 10,
            },
            2,
        ),
        user(3, "next question"),
        SurfaceEnvelope::log_only(
            SessionEvent::ModelUsage {
                model: "deepseek-chat".to_owned(),
                input_tokens: 150,
                output_tokens: 5,
            },
            4,
        ),
    ];

    let pressure = context_pressure(&log).expect("a valid surface");

    // The second sample is the newest one, and nothing was placed after it, so the projection is
    // the provider's own figure — the earlier response is inside it, not on top of it.
    assert_eq!(pressure.pressure_tokens, Some(150));
    assert_eq!(pressure.projected_tokens, Some(150));
}

#[test]
fn the_newest_context_window_wins_and_a_clearing_record_removes_it() {
    let window = |seq: u64, context: Option<u64>| {
        SurfaceEnvelope::log_only(
            SessionEvent::RequestContext {
                provider: "p".to_owned(),
                model: "m".to_owned(),
                context_window: context,
            },
            seq,
        )
    };
    let log = vec![
        window(0, Some(1000)),
        window(1, Some(2000)),
        window(2, None),
    ];

    assert_eq!(
        context_pressure(&log).expect("valid").context_window,
        None,
        "a record that clears the window must clear it, not leave the previous one"
    );
    assert_eq!(
        context_pressure(&log[..2]).expect("valid").context_window,
        Some(2000)
    );
}

// ---- session usage totals ----------------------------------------------------------------------

#[test]
fn usage_totals_sum_across_steps() {
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        usage(2, "deepseek-chat", 100, 20),
        step_end(3, 1, 1),
        step_start(4, 1, 2),
        usage(5, "deepseek-chat", 300, 40),
        step_end(6, 1, 2),
        SurfaceEnvelope::log_only(
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            7,
        ),
    ];

    let totals = fold_token_usage(&log);

    assert_eq!(totals.uncached_input_tokens, 400);
    assert_eq!(totals.output_tokens, 60);
}

#[test]
fn a_reported_settlement_replaces_rather_than_double_counting() {
    // Two records for one step with no retry between them are the same attempt reported twice. A
    // plain sum would double the spend; this is the rule that makes the fold a fold.
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        usage(2, "deepseek-chat", 100, 20),
        usage(3, "deepseek-chat", 110, 25),
        step_end(4, 1, 1),
    ];

    let totals = fold_token_usage(&log);

    assert_eq!(totals.uncached_input_tokens, 110);
    assert_eq!(totals.output_tokens, 25);
}

#[test]
fn a_retried_attempt_adds_because_it_was_billed_twice() {
    // The other side of the same rule: one step, two attempts, a retry between them. The provider
    // billed both, so both count.
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        usage(2, "deepseek-chat", 100, 0),
        retry(3),
        usage(4, "deepseek-chat", 120, 30),
        step_end(5, 1, 1),
    ];

    let totals = fold_token_usage(&log);

    assert_eq!(totals.uncached_input_tokens, 220);
    assert_eq!(totals.output_tokens, 30);
}

#[test]
fn a_usage_record_outside_a_step_is_counted() {
    // It names no attempt, so two such records cannot be proven to be the same one. Counting them
    // is the reading that does not hide spend.
    let log = vec![usage(0, "a", 10, 1), usage(1, "b", 20, 2)];

    let totals = fold_token_usage(&log);

    assert_eq!(totals.uncached_input_tokens, 30);
    assert_eq!(totals.output_tokens, 3);
}

// ---- per-turn accounting -----------------------------------------------------------------------

#[test]
fn one_step_turn_reports_its_attempt_exactly() {
    let usage = derive_turn_usage(&one_step_turn(100, 25)).expect("a complete turn is provable");

    assert_eq!(usage.uncached_input_tokens, 100);
    assert_eq!(usage.output_tokens, 25);
    assert_eq!(usage.total_tokens, 125);
    assert_eq!(usage.models, vec!["deepseek-chat".to_owned()]);
}

#[test]
fn every_step_of_a_multi_step_turn_contributes() {
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        usage(2, "deepseek-chat", 100, 10),
        step_end(3, 1, 1),
        step_start(4, 1, 2),
        usage(5, "deepseek-chat", 200, 20),
        step_end(6, 1, 2),
        SurfaceEnvelope::log_only(
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            7,
        ),
    ];

    let usage = derive_turn_usage(&log).expect("the turn is complete");

    assert_eq!(usage.uncached_input_tokens, 300);
    assert_eq!(usage.output_tokens, 30);
    assert_eq!(usage.total_tokens, 330);
}

#[test]
fn a_retried_attempt_inside_one_turn_adds() {
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        usage(2, "deepseek-chat", 100, 0),
        retry(3),
        usage(4, "deepseek-chat", 120, 30),
        step_end(5, 1, 1),
        SurfaceEnvelope::log_only(
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            6,
        ),
    ];

    let usage = derive_turn_usage(&log).expect("the turn is complete");

    assert_eq!(usage.uncached_input_tokens, 220);
    assert_eq!(usage.output_tokens, 30);
}

#[test]
fn a_re_reported_settlement_inside_one_turn_replaces() {
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        usage(2, "deepseek-chat", 100, 10),
        usage(3, "deepseek-chat", 110, 15),
        step_end(4, 1, 1),
        SurfaceEnvelope::log_only(
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            5,
        ),
    ];

    let usage = derive_turn_usage(&log).expect("the turn is complete");

    assert_eq!(usage.uncached_input_tokens, 110);
    assert_eq!(usage.output_tokens, 15);
}

#[test]
fn several_models_are_listed_in_first_use_order_without_duplicates() {
    // One step per attempt: records inside a single step are re-reports of one attempt, so each
    // model needs its own step to be counted at all.
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        usage(2, "beta", 10, 1),
        step_end(3, 1, 1),
        step_start(4, 1, 2),
        usage(5, "alpha", 10, 1),
        step_end(6, 1, 2),
        step_start(7, 1, 3),
        usage(8, "beta", 10, 1),
        step_end(9, 1, 3),
        SurfaceEnvelope::log_only(
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            10,
        ),
    ];

    let usage = derive_turn_usage(&log).expect("the turn is complete");

    assert_eq!(
        usage.models,
        vec!["beta".to_owned(), "alpha".to_owned()],
        "first-use order, and beta appears once despite answering twice"
    );
}

#[test]
fn a_turn_without_a_provider_report_is_refused_rather_than_priced_at_zero() {
    // A zero would claim the turn was free; refusing says the number is unavailable.
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        step_end(2, 1, 1),
        SurfaceEnvelope::log_only(
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            3,
        ),
    ];

    let error = derive_turn_usage(&log).expect_err("nothing settled an attempt");

    assert_eq!(error, TurnUsageGap::NoBillingAttempt);
}

#[test]
fn an_unclosed_turn_is_refused() {
    let mut log = one_step_turn(10, 2);
    log.pop();

    assert_eq!(
        derive_turn_usage(&log).expect_err("no turn/end"),
        TurnUsageGap::MissingTurnEnd
    );
}

#[test]
fn a_turn_end_for_another_turn_is_refused() {
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        SurfaceEnvelope::log_only(
            SessionEvent::TurnEnd {
                turn: 2,
                reason: TurnEndReason::Completed,
            },
            1,
        ),
    ];

    assert_eq!(
        derive_turn_usage(&log).expect_err("wrong turn"),
        TurnUsageGap::TurnEndMismatch {
            expected: 1,
            found: 2
        }
    );
}

#[test]
fn a_nested_step_is_refused() {
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        step_start(2, 1, 2),
    ];

    assert_eq!(
        derive_turn_usage(&log).expect_err("a step cannot open before the last one closed"),
        TurnUsageGap::NestedStepStart { index: 2 }
    );
}

#[test]
fn an_unmatched_step_end_is_refused() {
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        step_end(2, 1, 2),
    ];

    assert_eq!(
        derive_turn_usage(&log).expect_err("step 2 was never opened"),
        TurnUsageGap::UnmatchedStepEnd { index: 2 }
    );
}

#[test]
fn usage_outside_a_step_is_refused_even_though_the_session_wide_fold_counts_it() {
    // The two folds deliberately disagree: a turn's accounting must be provable attempt by attempt,
    // while a session total only has to not hide spend. Asserting both pins that the difference is
    // intentional rather than an inconsistency.
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        usage(1, "deepseek-chat", 100, 10),
    ];

    assert_eq!(
        derive_turn_usage(&log).expect_err("no step is open"),
        TurnUsageGap::UsageOutsideStep { index: 1 }
    );
    assert_eq!(fold_token_usage(&log).uncached_input_tokens, 100);
}

#[test]
fn an_event_after_turn_end_is_refused() {
    let mut log = one_step_turn(10, 2);
    log.push(usage(5, "deepseek-chat", 99, 9));

    assert_eq!(
        derive_turn_usage(&log).expect_err("the turn already closed"),
        TurnUsageGap::TrailingEvent { index: 5 }
    );
}

#[test]
fn a_step_naming_another_turn_is_refused() {
    let log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 2, 1),
    ];

    assert_eq!(
        derive_turn_usage(&log).expect_err("turn 2 is not open"),
        TurnUsageGap::StepTurnMismatch {
            event: "step/start",
            expected: 1,
            found: 2
        }
    );
}

#[test]
fn a_slice_that_does_not_open_with_turn_start_is_refused() {
    let log = vec![step_start(0, 1, 1), usage(1, "m", 5, 1)];

    assert_eq!(
        derive_turn_usage(&log).expect_err("no turn/start"),
        TurnUsageGap::MissingTurnStart
    );
}

// ---- whole-log turn splitting ------------------------------------------------------------------

#[test]
fn the_log_is_split_at_its_turn_boundaries() {
    let mut log = vec![
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 1 }, 0),
        step_start(1, 1, 1),
        usage(2, "deepseek-chat", 10, 1),
        step_end(3, 1, 1),
        SurfaceEnvelope::log_only(
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            4,
        ),
        SurfaceEnvelope::log_only(SessionEvent::TurnStart { turn: 2 }, 5),
        step_start(6, 2, 1),
        usage(7, "deepseek-chat", 20, 2),
        step_end(8, 2, 1),
        SurfaceEnvelope::log_only(
            SessionEvent::TurnEnd {
                turn: 2,
                reason: TurnEndReason::Completed,
            },
            9,
        ),
    ];
    let _ = &mut log;

    let entries = derive_turn_usages(&log);

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].turn, 1);
    assert_eq!(
        entries[0]
            .usage
            .as_ref()
            .expect("provable")
            .uncached_input_tokens,
        10
    );
    assert_eq!(entries[1].turn, 2);
    assert_eq!(
        entries[1]
            .usage
            .as_ref()
            .expect("provable")
            .uncached_input_tokens,
        20
    );
}

#[test]
fn an_unclosed_final_turn_is_reported_rather_than_dropped() {
    // A crash mid-turn left real spend behind. Omitting the entry would make the log look shorter
    // than it was, and the named gap says why the figure is missing.
    let mut log = one_step_turn(10, 2);
    let closed = log.split_off(4);
    assert_eq!(closed.len(), 1, "the turn/end is the only event removed");
    log.push(SurfaceEnvelope::log_only(
        SessionEvent::TurnStart { turn: 2 },
        4,
    ));
    log.push(step_start(5, 2, 1));
    log.push(usage(6, "deepseek-chat", 30, 3));

    let entries = derive_turn_usages(&log);

    // Turn 1 is reported from its `turn/start` to the *next* one, and turn 2 to the end of the log.
    // Both are present and both name the same gap: neither was closed. Dropping turn 1 instead
    // would hide real spend behind a boundary the crash removed.
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].turn, 1);
    assert_eq!(entries[1].turn, 2);
    for entry in &entries {
        assert_eq!(
            entry.usage.as_ref().expect_err("neither turn closed"),
            &TurnUsageGap::MissingTurnEnd
        );
    }
}

#[test]
fn events_before_the_first_turn_start_belong_to_no_turn() {
    let mut log = vec![usage(0, "orphan", 5, 1)];
    log.extend(
        one_step_turn(10, 2)
            .into_iter()
            .enumerate()
            .map(|(offset, mut envelope)| {
                envelope.seq = u64::try_from(offset).unwrap() + 1;
                envelope
            }),
    );

    let entries = derive_turn_usages(&log);

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].turn, 1);
    assert_eq!(
        entries[0]
            .usage
            .as_ref()
            .expect("provable")
            .uncached_input_tokens,
        10,
        "the orphan record is not attributed to the turn"
    );
}

#[test]
fn an_empty_log_has_no_turns() {
    assert!(derive_turn_usages(&[]).is_empty());
    assert_eq!(fold_token_usage(&[]), TokenUsageProjection::default());
}

// ---- what the disclosure does not cover --------------------------------------------------------

#[test]
fn the_cache_bucket_is_absent_rather_than_zero() {
    // The log records no cache split, so reporting a cache bucket would be reporting a measurement
    // nothing took. This test pins the documented limitation so a later producer change is noticed.
    let usage = derive_turn_usage(&one_step_turn(100, 25)).expect("provable");

    assert_eq!(
        usage.total_tokens,
        usage.uncached_input_tokens + usage.output_tokens
    );
    assert!(!usage.is_empty());
    assert!(TurnTokenUsageFields::all().len() == 4);
}

/// The fields a disclosure publishes, as a checklist.
///
/// A helper rather than four assertions scattered through the test: the point is the *set*, and a
/// new field should make someone come back here and decide whether it can be populated.
struct TurnTokenUsageFields;

impl TurnTokenUsageFields {
    fn all() -> [&'static str; 4] {
        [
            "uncached_input_tokens",
            "output_tokens",
            "total_tokens",
            "models",
        ]
    }
}

// ---- the estimates the folds are built on ------------------------------------------------------

#[test]
fn the_repeated_overhead_constants_agree_with_the_public_estimator() {
    // `BLOCK_OVERHEAD` and `ROLE_OVERHEAD` are repeated here because the crate-private
    // `token_meter` module is not reachable from a `#[path]`-included build. This pins them against
    // the public estimator, so a change there fails a test instead of drifting silently.
    let blocks = [ContentBlock::Text {
        text: "abcd".to_owned(),
    }];
    let priced = girder_core::estimate_blocks(&blocks);
    assert_eq!(
        priced,
        token_projection_gap::BLOCK_OVERHEAD + 1,
        "one four-character text block is one density token plus framing"
    );
    // A message price is that plus one role framing charge, which is the value the folds use.
    assert_eq!(
        user_price("abcd"),
        girder_core::estimate_message(&Message::new(girder_core::Role::User, blocks.to_vec()))
    );
    assert_eq!(token_projection_gap::ROLE_OVERHEAD, 4);
}

#[test]
fn an_empty_tool_catalogue_is_free() {
    assert_eq!(estimate_tools(&[]), 0);
}

#[test]
fn the_breakdown_of_an_empty_surface_is_all_zeroes() {
    assert_eq!(
        context_breakdown(&[], &[]).expect("an empty log is valid"),
        ContextBreakdown::default()
    );
}

#[test]
fn an_image_occurrence_records_its_reference_verbatim() {
    let surface = price_surface(&[assistant_with_image(0, "sha256:abc")]).expect("valid");

    assert_eq!(
        surface.nodes()[0].images,
        vec![ImageOccurrence {
            reference: "sha256:abc".to_owned(),
            structural_tokens: surface.nodes()[0].images[0].structural_tokens,
        }]
    );
}
