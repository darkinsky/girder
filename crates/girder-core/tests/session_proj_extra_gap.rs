//! Tests for the `turnOutline` projection unit.
//!
//! Corresponds to upstream `packages/session/session-turn-outline/tests/projection.spec.ts`. The
//! module is included by path because `girder-core`'s crate root does not declare
//! `session_proj_extra_gap` yet — wiring `pub mod session_proj_extra_gap;` and its `pub use`
//! belongs to the integrator, the same arrangement every other `*_gap` test here uses.
//!
//! # Why these cases and not others
//!
//! The module exists because of one finding: the key `turnOutline` had exactly one occurrence in
//! `crates/`, and it was a comment in [`girder_core::session_outline`]. A suite that only checked
//! "the fold compiles and yields JSON" would leave that finding unverified, so the first cases here
//! are *end-to-end through the value the rail consumes*: fold real events, publish the wire view,
//! merge it with a loaded window, and assert the marks a client would render.
//!
//! Every remaining case pins one rule whose violation is expensive and silent:
//!
//! - **The anchor is `turn/start`'s seq, never the prompt's.** A rail mark that pages through the
//!   prompt's seq lands one event late and shows a turn whose opening line is missing. This build
//!   writes the prompt *before* the boundary, so the case that catches it is exactly that order.
//! - **A turn number that does not advance is ignored.** A retried boundary would otherwise append a
//!   duplicate row and break the "strictly increasing by turn" invariant the wire schema declares.
//! - **Steering keeps the first prompt.** A second human message in the same turn must not overwrite
//!   what the turn was opened with, or the rail would relabel a turn every time the user corrected
//!   themselves.
//! - **Only the person's own words open a turn.** A relay, an injection, and a system prompt all
//!   arrive on the user role. Taking any of them would make a turn's title the harness talking.
//! - **The draft commits at `turn/end` and nowhere else.** Committing on `assistant/message` would
//!   show the newest partial step as the turn's outcome.
//! - **Draft-only applies do not move the view.** Two consecutive assistant messages with no
//!   boundary between them must produce a view change of exactly zero after the first, which is what
//!   keeps the change feed quiet inside a turn.
//! - **Previews are budgeted and never split a code point.** A truncated preview ends with exactly
//!   one ellipsis and stays inside its limit in code points, so a CJK prompt renders the same width
//!   in this crate as upstream.
//! - **A malformed persisted state is refused, not replaced.** Substituting the empty state would
//!   delete every row a user was looking at.

#[path = "../src/session_proj_extra_gap.rs"]
mod session_proj_extra_gap;

use girder_core::{
    ContentBlock, MessageSource, Projection, ProjectionRegistry, SessionEvent, TurnEndReason,
    VersionedProjection,
};
use session_proj_extra_gap::{
    INJECT, KEY, LoadedRailItem, NAME, PROMPT_PREVIEW_LIMIT, RESPONSE_PREVIEW_LIMIT, RailAnchor,
    STATE_VERSION, TurnOutlineEntry, TurnOutlineProjection, TurnOutlineState, apply, fold,
    merge_rail_items, preview,
};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

fn text(value: &str) -> ContentBlock {
    ContentBlock::Text {
        text: value.to_owned(),
    }
}

fn human(text_blocks: &[&str]) -> SessionEvent {
    SessionEvent::UserMessage {
        message: girder_core::ChatMessage::new(
            girder_core::MessageId::new("m").expect("valid id"),
            girder_core::ChatRole::User,
            MessageSource::User { form: None },
            text_blocks.iter().map(|value| text(value)).collect(),
        ),
    }
}

fn relayed(value: &str) -> SessionEvent {
    SessionEvent::UserMessage {
        message: girder_core::ChatMessage::new(
            girder_core::MessageId::new("m").expect("valid id"),
            girder_core::ChatRole::User,
            MessageSource::Relay {
                from: girder_core::AgentId::new("parent").expect("valid agent id"),
            },
            vec![text(value)],
        ),
    }
}

fn injected(value: &str) -> SessionEvent {
    SessionEvent::UserMessage {
        message: girder_core::ChatMessage::new(
            girder_core::MessageId::new("m").expect("valid id"),
            girder_core::ChatRole::User,
            MessageSource::User {
                form: Some(girder_core::ContextForm::Notice),
            },
            vec![text(value)],
        ),
    }
}

fn assistant(text_blocks: &[&str]) -> SessionEvent {
    SessionEvent::AssistantMessage {
        message: girder_core::ChatMessage::new(
            girder_core::MessageId::new("a").expect("valid id"),
            girder_core::ChatRole::Assistant,
            MessageSource::Model {
                provider: "deepseek".to_owned(),
                model: "deepseek-chat".to_owned(),
                replay_state: None,
            },
            text_blocks.iter().map(|value| text(value)).collect(),
        ),
    }
}

fn start(turn: u64) -> SessionEvent {
    SessionEvent::TurnStart { turn }
}

fn end(turn: u64) -> SessionEvent {
    SessionEvent::TurnEnd {
        turn,
        reason: TurnEndReason::Completed,
    }
}

/// Folds with explicit seqs, so an assertion can name the anchor it expects.
fn fold_at(events: &[(u64, SessionEvent)]) -> TurnOutlineState {
    let mut state = TurnOutlineState::new();
    for (seq, event) in events {
        state = apply(&state, event, *seq);
    }
    state
}

// ---------------------------------------------------------------------------------------------
// End-to-end: the value the rail consumes
// ---------------------------------------------------------------------------------------------

#[test]
fn a_two_turn_log_produces_the_rail_a_client_renders() {
    let events = [
        start(1),
        human(&["fix the retry"]),
        assistant(&["Looking at the retry loop now."]),
        end(1),
        start(2),
        human(&["and the tests"]),
        assistant(&["The tests pass."]),
        end(2),
    ];
    let state = fold(&events);
    assert_eq!(state.turns.len(), 2, "one row per started turn");
    assert_eq!(
        state.turns[0],
        TurnOutlineEntry {
            turn: 1,
            seq: 1,
            prompt: "fix the retry".to_owned(),
            response: "Looking at the retry loop now.".to_owned(),
        }
    );
    assert_eq!(
        state.turns[1],
        TurnOutlineEntry {
            turn: 2,
            seq: 5,
            prompt: "and the tests".to_owned(),
            response: "The tests pass.".to_owned(),
        }
    );
    assert_eq!(state.draft, "", "a closed turn leaves no draft");
    assert_eq!(
        state.to_wire(),
        serde_json::json!([
            { "turn": 1, "seq": 1, "prompt": "fix the retry", "response": "Looking at the retry loop now." },
            { "turn": 2, "seq": 5, "prompt": "and the tests", "response": "The tests pass." }
        ])
    );
}

#[test]
fn the_merged_ladder_lets_a_client_jump_to_an_unloaded_turn() {
    let state = fold(&[
        start(1),
        human(&["first question"]),
        assistant(&["first answer"]),
        end(1),
        start(2),
        human(&["second question"]),
        assistant(&["second answer"]),
        end(2),
    ]);
    // The client holds turn 1 only; turn 2 is outside its window.
    let loaded = [LoadedRailItem {
        turn: 1,
        anchor_key: "node-1".to_owned(),
        prompt: "first question".to_owned(),
        response: "first answer".to_owned(),
    }];
    let ladder = merge_rail_items(&loaded, &state);
    assert_eq!(ladder.len(), 2);
    assert_eq!(
        ladder[0].anchor,
        RailAnchor::Loaded {
            key: "node-1".to_owned()
        },
        "a loaded turn scrolls to its node"
    );
    assert_eq!(
        ladder[1].anchor,
        RailAnchor::Unloaded { seq: 5 },
        "an unloaded turn pages history through its turn/start seq"
    );
    assert_eq!(ladder[1].prompt, "second question");
    assert_eq!(ladder[1].response, "second answer");
}

#[test]
fn a_loaded_turn_keeps_its_own_previews_and_only_falls_back_where_it_has_none() {
    let state = fold(&[
        start(1),
        human(&["host preview"]),
        assistant(&["host response"]),
        end(1),
        start(2),
        human(&["host-only turn"]),
        assistant(&["host-only response"]),
        end(2),
    ]);
    let loaded = [
        LoadedRailItem {
            turn: 1,
            anchor_key: "k1".to_owned(),
            prompt: "window prompt".to_owned(),
            response: String::new(),
        },
        LoadedRailItem {
            turn: 2,
            anchor_key: "k2".to_owned(),
            prompt: String::new(),
            response: String::new(),
        },
    ];
    let ladder = merge_rail_items(&loaded, &state);
    assert_eq!(
        ladder[0].prompt, "window prompt",
        "the loaded window's richer preview wins"
    );
    assert_eq!(
        ladder[0].response, "host response",
        "an empty window preview falls back to the outline"
    );
    assert_eq!(
        ladder[1].prompt, "host-only turn",
        "a mid-window head can have no prompt preview at all"
    );
    assert_eq!(
        ladder[1].anchor,
        RailAnchor::Loaded {
            key: "k2".to_owned()
        }
    );
}

#[test]
fn a_turn_the_window_has_and_the_outline_does_not_still_appears() {
    // A window can hold a turn whose boundary the outline has not folded yet — a client opening a
    // deep page before the projection arrives. The merge must not drop it.
    let state = TurnOutlineState::new();
    let ladder = merge_rail_items(
        &[LoadedRailItem {
            turn: 7,
            anchor_key: "deep".to_owned(),
            prompt: "p".to_owned(),
            response: "r".to_owned(),
        }],
        &state,
    );
    assert_eq!(ladder.len(), 1);
    assert_eq!(ladder[0].turn, 7);
    assert_eq!(
        ladder[0].anchor,
        RailAnchor::Loaded {
            key: "deep".to_owned()
        }
    );
}

#[test]
fn the_merged_ladder_ascends_by_turn_even_when_the_sides_disagree_on_order() {
    let state = fold(&[start(2), human(&["two"]), start(4), human(&["four"])]);
    let loaded = [
        LoadedRailItem {
            turn: 4,
            anchor_key: "k4".to_owned(),
            prompt: "four".to_owned(),
            response: String::new(),
        },
        LoadedRailItem {
            turn: 3,
            anchor_key: "k3".to_owned(),
            prompt: "three".to_owned(),
            response: String::new(),
        },
    ];
    let turns: Vec<u64> = merge_rail_items(&loaded, &state)
        .iter()
        .map(|row| row.turn)
        .collect();
    assert_eq!(turns, [2, 3, 4], "ascending, one row per known turn");
}

// ---------------------------------------------------------------------------------------------
// The anchor
// ---------------------------------------------------------------------------------------------

#[test]
fn the_anchor_is_the_turn_start_seq_even_when_the_prompt_was_written_first() {
    // This build appends the prompt before its boundary so a crash before the model answers still
    // leaves the person's words in the log. Anchoring on the prompt would page one event late.
    let state = fold(&[
        human(&["written first"]),
        start(1),
        assistant(&["answer"]),
        end(1),
    ]);
    assert_eq!(state.turns.len(), 1);
    assert_eq!(
        state.turns[0].seq, 2,
        "the anchor is turn/start's seq, not the prompt's"
    );
    assert_eq!(
        state.turns[0].prompt, "written first",
        "a prompt that arrived before its boundary still opens the turn"
    );
}

#[test]
fn a_prompt_with_no_turn_yet_is_not_attributed_to_anything() {
    // A log that ends between the prompt and its boundary has no turn to open, so the prompt is
    // dropped rather than pre-creating a row whose seq nothing would anchor.
    let state = fold(&[human(&["orphan"])]);
    assert!(state.turns.is_empty());
    assert_eq!(state.draft, "");
}

#[test]
fn a_boundary_that_does_not_advance_the_turn_number_is_ignored() {
    let state = fold(&[start(1), human(&["one"]), start(1), start(0)]);
    assert_eq!(
        state.turns.len(),
        1,
        "a retried boundary keeps the outline strictly increasing"
    );
    assert_eq!(state.turns[0].turn, 1);
    assert_eq!(state.turns[0].prompt, "one");
}

#[test]
fn a_retried_turn_lands_its_previews_on_the_standing_entry() {
    let state = fold(&[
        start(1),
        human(&["first prompt"]),
        start(1),
        assistant(&["answer"]),
        end(1),
    ]);
    assert_eq!(state.turns.len(), 1);
    assert_eq!(
        state.turns[0].prompt, "first prompt",
        "the duplicate boundary must not reset the preview"
    );
    assert_eq!(state.turns[0].response, "answer");
}

// ---------------------------------------------------------------------------------------------
// Who opens a turn
// ---------------------------------------------------------------------------------------------

#[test]
fn only_the_persons_own_words_open_a_turn() {
    for (label, event) in [
        ("a relayed peer message", relayed("relayed")),
        ("an injected notice", injected("a notice the harness wrote")),
        (
            "the system prompt",
            SessionEvent::UserMessage {
                message: girder_core::ChatMessage::new(
                    girder_core::MessageId::new("s").expect("valid id"),
                    girder_core::ChatRole::User,
                    MessageSource::SystemPrompt,
                    vec![text("you are a helpful agent")],
                ),
            },
        ),
    ] {
        let state = fold(&[start(1), event]);
        assert_eq!(
            state.turns[0].prompt, "",
            "{label} must not become the turn's title"
        );
    }
}

#[test]
fn steering_keeps_the_first_prompt() {
    let state = fold(&[
        start(1),
        human(&["the original ask"]),
        human(&["actually, do the other thing"]),
    ]);
    assert_eq!(state.turns.len(), 1);
    assert_eq!(
        state.turns[0].prompt, "the original ask",
        "a later human message in the same turn is steering, not the title"
    );
}

#[test]
fn a_blank_or_non_text_prompt_leaves_the_row_empty() {
    let state = fold(&[
        start(1),
        SessionEvent::UserMessage {
            message: girder_core::ChatMessage::new(
                girder_core::MessageId::new("m").expect("valid id"),
                girder_core::ChatRole::User,
                MessageSource::User { form: None },
                vec![ContentBlock::Reasoning {
                    text: "hidden".to_owned(),
                }],
            ),
        },
    ]);
    assert_eq!(
        state.turns[0].prompt, "",
        "a message with no text block has no preview"
    );
}

#[test]
fn a_second_turn_gets_its_own_prompt_after_the_first_one_closed() {
    let state = fold(&[start(1), human(&["one"]), end(1), start(2), human(&["two"])]);
    assert_eq!(state.turns[0].prompt, "one");
    assert_eq!(state.turns[1].prompt, "two");
}

// ---------------------------------------------------------------------------------------------
// The draft
// ---------------------------------------------------------------------------------------------

#[test]
fn the_draft_commits_only_at_turn_end() {
    let after_message = fold(&[start(1), human(&["q"]), assistant(&["partial"])]);
    assert_eq!(
        after_message.turns[0].response, "",
        "an assistant message must not become the turn's outcome on its own"
    );
    assert_eq!(after_message.draft, "partial");

    let committed = fold(&[start(1), human(&["q"]), assistant(&["partial"]), end(1)]);
    assert_eq!(committed.turns[0].response, "partial");
    assert_eq!(committed.draft, "", "committing clears the buffer");
}

#[test]
fn the_newest_text_bearing_assistant_message_wins_the_draft() {
    let state = fold(&[
        start(1),
        assistant(&["first"]),
        assistant(&["second"]),
        assistant(&["third"]),
    ]);
    assert_eq!(state.draft, "third");
}

#[test]
fn a_draft_only_apply_does_not_move_the_published_view() {
    let state = fold(&[start(1), human(&["q"]), assistant(&["one"])]);
    let view = state.to_wire();
    let next = apply(&state, &assistant(&["two"]), 20);
    assert_eq!(
        next.to_wire(),
        view,
        "the view is turns alone, so a draft update publishes nothing"
    );
    assert_eq!(next.draft, "two", "the buffer still advances");
}

#[test]
fn a_repeated_draft_is_the_same_state() {
    let state = fold(&[start(1), assistant(&["same"])]);
    let next = apply(&state, &assistant(&["same"]), 9);
    assert_eq!(next, state, "an identical preview is not a change at all");
}

#[test]
fn a_turn_that_ends_with_no_draft_keeps_its_previous_response() {
    let state = fold(&[
        start(1),
        assistant(&["answer"]),
        end(1),
        start(2),
        human(&["next"]),
        end(2),
    ]);
    assert_eq!(state.turns[0].response, "answer");
    assert_eq!(
        state.turns[1].response, "",
        "a turn with no assistant text has no preview, not a stale one"
    );
}

#[test]
fn a_turn_end_with_no_open_turn_clears_the_draft_without_inventing_a_row() {
    let state = fold(&[assistant(&["orphan answer"]), end(1)]);
    assert!(state.turns.is_empty(), "no boundary means no row");
    assert_eq!(state.draft, "", "the stale buffer is still cleared");
}

#[test]
fn the_anchor_follows_the_seq_the_caller_supplies() {
    // A caller replaying a stored log passes the row's own seq, which need not be the row's index:
    // a fork's inherited prefix or a resumed suffix starts at an offset. The anchor is whatever the
    // caller passes, and the fold must not silently renumber.
    let state = fold_at(&[
        (40, start(1)),
        (41, human(&["offset prompt"])),
        (42, end(1)),
        (43, start(2)),
    ]);
    assert_eq!(state.turns[0].seq, 40);
    assert_eq!(state.turns[0].prompt, "offset prompt");
    assert_eq!(state.turns[1].seq, 43);
}

#[test]
fn a_repeated_response_is_idempotent_across_a_duplicate_turn_end() {
    // Upstream commits to the newest row and only skips the write when the two are already equal.
    // That makes a duplicated `turn/end` (a retried write) a no-op rather than a second change, which
    // is the case that matters: the boundary is the one event a log writer can plausibly repeat.
    let state = fold_at(&[
        (1, start(1)),
        (2, assistant(&["answer"])),
        (3, end(1)),
        (4, assistant(&["answer"])),
        (5, end(1)),
    ]);
    assert_eq!(state.turns.len(), 1, "the repeated boundary adds no row");
    assert_eq!(state.turns[0].response, "answer");
    assert_eq!(state.draft, "", "the buffer is cleared either way");
}

#[test]
fn the_draft_commits_to_the_newest_row_exactly_as_upstream_does() {
    // Upstream's arm is `const last = state.turns.at(-1)` with no turn-number check, and this port
    // keeps that rule deliberately: a `turn/end` names a turn the outline may not have folded (a
    // window that begins mid-turn), and refusing the commit there would drop the only response
    // preview that turn will ever get.
    //
    // The consequence is stated rather than hidden: a malformed log whose `turn/end` names a turn it
    // never started attributes its draft to the standing row. A stricter rule was tried and rejected
    // — it silently discarded the response of every turn whose boundary the fold had not seen.
    let state = fold_at(&[
        (1, start(1)),
        (2, assistant(&["answer for turn one"])),
        (3, end(1)),
        (4, assistant(&["a draft with no open turn"])),
        (5, end(2)),
    ]);
    assert_eq!(state.turns.len(), 1, "no row is invented for turn 2");
    assert_eq!(
        state.turns[0].response, "a draft with no open turn",
        "upstream's newest-row rule, not a turn-number match"
    );
    assert_eq!(state.draft, "");
}

#[test]
fn an_open_turn_is_visible_before_it_ends() {
    // A running turn must appear in the rail, or the sidebar would show nothing exactly while the
    // user is watching.
    let state = fold(&[start(1), human(&["in flight"]), assistant(&["thinking"])]);
    assert_eq!(state.turns.len(), 1);
    assert_eq!(state.turns[0].prompt, "in flight");
    assert_eq!(
        state.turns[0].response, "",
        "the response commits at the boundary, so an open turn shows none yet"
    );
}

// ---------------------------------------------------------------------------------------------
// Previews
// ---------------------------------------------------------------------------------------------

#[test]
fn a_preview_collapses_whitespace_into_one_line() {
    assert_eq!(
        preview(&[text("  fix   the\n\nretry\tloop  ")], 50),
        "fix the retry loop"
    );
}

#[test]
fn a_preview_space_joins_text_blocks_and_skips_non_text_ones() {
    let content = [
        text("one"),
        ContentBlock::Reasoning {
            text: "not visible".to_owned(),
        },
        text("two"),
    ];
    assert_eq!(preview(&content, 50), "one two");
}

#[test]
fn a_preview_past_its_budget_ends_with_one_ellipsis_and_fits() {
    let long = "word ".repeat(40);
    let clipped = preview(&[text(&long)], PROMPT_PREVIEW_LIMIT);
    assert!(
        clipped.chars().count() <= PROMPT_PREVIEW_LIMIT,
        "a clipped preview stays inside its limit: {} > {}",
        clipped.chars().count(),
        PROMPT_PREVIEW_LIMIT
    );
    assert!(clipped.ends_with('…'), "a clipped preview says so");
    assert_eq!(
        clipped.matches('…').count(),
        1,
        "exactly one marker, never two"
    );
}

#[test]
fn a_preview_never_splits_a_code_point() {
    // Each of these is one code point and several bytes; a byte-sliced preview would be mojibake.
    let cjk = "故障".repeat(60);
    let clipped = preview(&[text(&cjk)], PROMPT_PREVIEW_LIMIT);
    assert!(
        clipped
            .chars()
            .all(|character| character == '故' || character == '障' || character == '…')
    );
    assert!(clipped.chars().count() <= PROMPT_PREVIEW_LIMIT);

    let emoji = "🚀".repeat(40);
    let clipped = preview(&[text(&emoji)], PROMPT_PREVIEW_LIMIT);
    assert!(
        clipped
            .chars()
            .all(|character| character == '🚀' || character == '…'),
        "an emoji is never cut in half"
    );
}

#[test]
fn a_preview_that_reads_only_part_of_its_input_still_says_so() {
    // A single block longer than twice the budget is clipped per block, which is what stops a
    // multi-megabyte block from being concatenated whole for a 50-character preview.
    let huge = "x".repeat(RESPONSE_PREVIEW_LIMIT * 10);
    let clipped = preview(&[text(&huge)], RESPONSE_PREVIEW_LIMIT);
    assert!(clipped.ends_with('…'));
    assert!(clipped.chars().count() <= RESPONSE_PREVIEW_LIMIT);
}

#[test]
fn a_preview_of_empty_content_is_empty() {
    assert_eq!(preview(&[], 50), "");
    assert_eq!(preview(&[text("   \n\t ")], 50), "");
    assert_eq!(preview(&[text("")], 50), "");
}

#[test]
fn a_preview_shorter_than_its_budget_is_returned_whole() {
    assert_eq!(preview(&[text("short")], 50), "short");
}

// ---------------------------------------------------------------------------------------------
// The registry adapter
// ---------------------------------------------------------------------------------------------

#[test]
fn the_unit_declares_upstreams_key_and_a_version_this_reduced_state_needs() {
    let projection = TurnOutlineProjection::new();
    assert_eq!(projection.key(), KEY);
    assert_eq!(KEY, "turnOutline");
    assert_eq!(projection.state_version(), STATE_VERSION);
    assert_eq!(
        STATE_VERSION, 3,
        "upstream writes 2; this port's state carries `pending` as well, so a v2 row must be \
         discarded rather than read as a v3 state with that field silently missing"
    );
    assert_eq!(NAME, "session-turn-outline");
    assert_eq!(INJECT, ["sessionProjections"]);
    assert!(
        !projection.in_summary(),
        "upstream declares summary: false — the outline grows with the log and no list row renders it"
    );
}

#[test]
fn the_wire_view_is_turns_alone_so_no_client_sees_the_extra_state() {
    // The version bump is about the durable row; the published value is upstream's exact shape.
    let state = fold(&[
        human(&["a prompt with no boundary yet"]),
        start(1),
        assistant(&["a draft"]),
    ]);
    assert!(!state.pending.is_empty() || !state.draft.is_empty());
    let wire = state.to_wire();
    let array = wire.as_array().expect("the view is an array of entries");
    assert_eq!(array.len(), 1);
    let entry = array[0].as_object().expect("an object per entry");
    assert_eq!(
        entry
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        ["prompt", "response", "seq", "turn"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        "neither `draft` nor `pending` may reach a client"
    );
}

#[test]
fn driving_the_unit_through_a_registry_folds_the_outline() {
    let mut registry = ProjectionRegistry::new();
    registry
        .register(std::sync::Arc::new(TurnOutlineProjection::new()))
        .expect("registered");
    let events = [
        start(1),
        human(&["hello"]),
        assistant(&["hi there"]),
        end(1),
    ];
    registry.replay(
        &events
            .iter()
            .enumerate()
            .map(|(index, event)| (index as u64 + 1, event.clone()))
            .collect::<Vec<_>>(),
    );
    let value = registry.read(KEY).expect("the key is registered");
    let state = TurnOutlineState::from_json(&value).expect("the state round-trips");
    assert_eq!(state.turns.len(), 1);
    assert_eq!(state.turns[0].prompt, "hello");
    assert_eq!(state.turns[0].response, "hi there");
}

#[test]
fn the_units_init_is_the_empty_outline() {
    let value = TurnOutlineProjection::new().init();
    assert_eq!(value, serde_json::json!({ "turns": [], "draft": "" }));
    let state = TurnOutlineState::from_json(&value).expect("parses");
    assert_eq!(state, TurnOutlineState::new());
}

#[test]
fn applying_to_an_unparseable_state_leaves_it_untouched() {
    // Rewriting a malformed state to `init` would silently delete a fold that was almost readable.
    let projection = TurnOutlineProjection::new();
    let broken = serde_json::json!({ "turns": "not an array" });
    let next = projection.apply(broken.clone(), &start(1), 1);
    assert_eq!(next, broken);
}

#[test]
fn a_response_preview_is_budgeted_independently_of_a_prompt_preview() {
    let long = "y".repeat(RESPONSE_PREVIEW_LIMIT * 2);
    let state = fold(&[start(1), human(&[&long]), assistant(&[&long]), end(1)]);
    assert!(state.turns[0].prompt.chars().count() <= PROMPT_PREVIEW_LIMIT);
    assert!(
        state.turns[0].response.chars().count() <= RESPONSE_PREVIEW_LIMIT,
        "the response budget is not the prompt budget"
    );
}

#[test]
fn a_persisted_state_that_breaks_the_schema_is_refused() {
    // The wire schema is `strict()` plus strictly-increasing turns plus bounded previews. Each of
    // these violates exactly one clause.
    let cases = [
        serde_json::json!({ "turns": [], "draft": "", "extra": 1 }),
        serde_json::json!({ "turns": [] }),
        serde_json::json!({ "turns": [], "draft": "x".repeat(RESPONSE_PREVIEW_LIMIT + 1) }),
        serde_json::json!({
            "turns": [
                { "turn": 2, "seq": 1, "prompt": "", "response": "" },
                { "turn": 1, "seq": 2, "prompt": "", "response": "" }
            ],
            "draft": ""
        }),
        serde_json::json!({
            "turns": [{ "turn": 1, "seq": 1, "prompt": "", "response": "" }, { "turn": 1, "seq": 2, "prompt": "", "response": "" }],
            "draft": ""
        }),
        serde_json::json!({
            "turns": [{ "turn": 1, "seq": 1, "prompt": "p".repeat(PROMPT_PREVIEW_LIMIT + 1), "response": "" }],
            "draft": ""
        }),
        serde_json::json!({
            "turns": [{ "turn": 1, "seq": 1, "prompt": "", "response": "r".repeat(RESPONSE_PREVIEW_LIMIT + 1) }],
            "draft": ""
        }),
        serde_json::json!({ "turns": [{ "turn": -1, "seq": 1, "prompt": "", "response": "" }], "draft": "" }),
        serde_json::json!(null),
    ];
    for (index, case) in cases.iter().enumerate() {
        assert!(
            TurnOutlineState::from_json(case).is_none(),
            "case {index} must be refused: {case}"
        );
    }
}

#[test]
fn a_folded_state_always_round_trips_through_its_persisted_shape() {
    let state = fold(&[
        start(1),
        human(&["a question with spaces"]),
        assistant(&["an answer"]),
        end(1),
        start(2),
        human(&["second"]),
    ]);
    let json = state.to_json();
    assert_eq!(TurnOutlineState::from_json(&json), Some(state));
}

// ---------------------------------------------------------------------------------------------
// The fold ignores what it does not use
// ---------------------------------------------------------------------------------------------

#[test]
fn an_uninteresting_event_leaves_the_outline_unchanged() {
    let state = fold(&[start(1), human(&["q"]), assistant(&["a"]), end(1)]);
    for event in [
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::ModelUsage {
            model: "deepseek-chat".to_owned(),
            input_tokens: 10,
            output_tokens: 5,
        },
        SessionEvent::PlanMode { active: true },
        SessionEvent::SessionTitle {
            title: "a title".to_owned(),
            message_seqs: vec![1],
            source: "human".to_owned(),
        },
    ] {
        assert_eq!(
            apply(&state, &event, 99),
            state,
            "{} must not move the outline",
            event.type_name()
        );
    }
}

#[test]
fn the_fold_assigns_one_based_seqs() {
    let state = fold(&[start(1), start(2)]);
    assert_eq!(state.turns[0].seq, 1, "the first row of a log is seq 1");
    assert_eq!(state.turns[1].seq, 2);
}

#[test]
fn an_empty_log_has_an_empty_outline() {
    let state = fold(&[]);
    assert!(state.turns.is_empty());
    assert_eq!(state.draft, "");
    assert_eq!(state.to_wire(), serde_json::json!([]));
    assert!(state.last_turn().is_none());
    assert!(state.entry(1).is_none());
}
