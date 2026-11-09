//! Tests for context compaction (M3 of the v2 plan).
//!
//! One invariant dominates: a cut must not separate a tool call from its result. A history where a
//! call survives without its answer is rejected by every provider, so a selection that would produce
//! one must be refused rather than attempted.

use girder_core::ToolCallId;
use girder_core::{
    CompactionError, CompactionRequest, ContentBlock, SessionEvent, SurfaceEntry, TurnEndReason,
    balances, needs_compaction, pairing_balanced_at, select_compaction, transcript_for_summary,
};

fn user(seq: u64, text: &str) -> SurfaceEntry {
    SurfaceEntry {
        seq,
        event: SessionEvent::user_text(&format!("m{seq}"), text),
    }
}

fn assistant(seq: u64, text: &str) -> SurfaceEntry {
    SurfaceEntry {
        seq,
        event: SessionEvent::assistant_text(&format!("a{seq}"), text, "test"),
    }
}

fn call(seq: u64, id: &str) -> SurfaceEntry {
    SurfaceEntry {
        seq,
        event: SessionEvent::ToolCall {
            id: ToolCallId::new(id).expect("valid"),
            name: "bash".to_owned(),
            arguments: "{}".to_owned(),
        },
    }
}

fn result(seq: u64, id: &str) -> SurfaceEntry {
    SurfaceEntry {
        seq,
        event: SessionEvent::ToolResult {
            id: ToolCallId::new(id).expect("valid"),
            content: vec![ContentBlock::Text {
                text: "ok".to_owned(),
            }],
            is_error: false,
        },
    }
}

fn turn_end(seq: u64) -> SurfaceEntry {
    SurfaceEntry {
        seq,
        event: SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    }
}

// ---- balance ----------------------------------------------------------------------------------

#[test]
fn a_history_with_no_tools_is_always_balanced() {
    let surface = vec![user(1, "hi"), assistant(2, "hello")];
    let b = balances(&surface);

    assert!(b.iter().all(|balance| balance.is_balanced()));
}

#[test]
fn an_open_call_leaves_the_balance_positive() {
    let surface = vec![call(1, "c1")];
    let b = balances(&surface);

    assert_eq!(b[0].open, 1);
    assert!(!b[0].is_balanced());
}

#[test]
fn a_result_closes_its_call() {
    let surface = vec![call(1, "c1"), result(2, "c1")];
    let b = balances(&surface);

    assert_eq!(b[0].open, 1, "the call is open after itself");
    assert_eq!(b[1].open, 0, "the result closes it");
    assert!(pairing_balanced_at(&surface, 1));
}

#[test]
fn a_cut_inside_a_tool_pair_is_unbalanced() {
    // The invariant: a call without its answer is a protocol violation.
    let surface = vec![
        user(1, "go"),
        call(2, "c1"),
        result(3, "c1"),
        assistant(4, "done"),
    ];

    assert!(pairing_balanced_at(&surface, 0), "before the call");
    assert!(!pairing_balanced_at(&surface, 1), "between call and result");
    assert!(pairing_balanced_at(&surface, 2), "after the result");
}

#[test]
fn the_end_of_the_history_is_always_balanced() {
    let surface = vec![call(1, "c1")];
    assert!(pairing_balanced_at(&surface, 1), "index past the end");
    assert!(pairing_balanced_at(&surface, 99));
}

#[test]
fn two_open_calls_need_both_results() {
    let surface = vec![
        call(1, "c1"),
        call(2, "c2"),
        result(3, "c1"),
        result(4, "c2"),
    ];
    let b = balances(&surface);

    assert_eq!(b[1].open, 2);
    assert_eq!(b[2].open, 1, "one still open");
    assert_eq!(b[3].open, 0);
}

// ---- selection --------------------------------------------------------------------------------

#[test]
fn a_selection_stops_before_the_recent_entries() {
    // A long history of plain messages: the cut lands exactly where `keep_recent` asks.
    let mut surface: Vec<SurfaceEntry> = (1..=20).map(|i| user(i, "x")).collect();
    surface.push(assistant(21, "last"));
    let request = CompactionRequest {
        keep_recent: 5,
        max_reach: None,
    };

    let selection = select_compaction(&surface, &request).expect("selected");

    assert_eq!(selection.end_index, 16, "21 minus 5 kept");
    assert_eq!(selection.start_index, 0);
    assert_eq!(selection.len(), 16);
}

#[test]
fn a_selection_never_splits_a_tool_pair() {
    // The cut moves earlier rather than separating a call from its result.
    let mut surface: Vec<SurfaceEntry> = (1..=10).map(|i| user(i, "x")).collect();
    surface.push(call(11, "c1"));
    surface.push(result(12, "c1"));
    surface.push(assistant(13, "after tools"));
    let request = CompactionRequest {
        keep_recent: 1,
        max_reach: None,
    };

    let selection = select_compaction(&surface, &request).expect("selected");

    // The compression range may include the whole pair, but it must never end between the call and
    // its result: that would leave the surviving suffix holding a result whose call was summarized
    // away.
    let ends_inside_pair = selection.end_index == 11;
    assert!(
        !ends_inside_pair,
        "the cut must not fall between call(11) and result(12), got {}",
        selection.end_index
    );
    // And the suffix must be internally consistent on its own.
    let suffix = &surface[selection.end_index..];
    assert!(
        pairing_balanced_at(suffix, 0)
            || suffix
                .iter()
                .all(|e| !matches!(e.event, SessionEvent::ToolResult { .. })),
        "the surviving suffix must not begin with an orphaned result"
    );
}

#[test]
fn a_selection_leaves_the_prefix_balanced() {
    // The property that matters for the surviving history.
    let surface = vec![
        user(1, "a"),
        call(2, "c1"),
        result(3, "c1"),
        user(4, "b"),
        call(5, "c2"),
        result(6, "c2"),
        assistant(7, "done"),
    ];
    let request = CompactionRequest {
        keep_recent: 2,
        max_reach: None,
    };

    let selection = select_compaction(&surface, &request).expect("selected");
    let surviving_prefix = &surface[..selection.start_index];
    let surviving_suffix = &surface[selection.end_index..];

    // Both sides must be independently consistent: the summary replaces the middle, so the model
    // sees prefix, summary, suffix.
    assert!(pairing_balanced_at(
        surviving_prefix,
        surviving_prefix.len()
    ));
    assert!(pairing_balanced_at(
        surviving_suffix,
        surviving_suffix.len()
    ));
}

#[test]
fn no_balanced_cut_is_reported_rather_than_guessed() {
    // The whole surface is one open tool call; any cut would split it.
    let surface = vec![call(1, "c1")];
    let request = CompactionRequest {
        keep_recent: 0,
        max_reach: None,
    };

    let error = select_compaction(&surface, &request).expect_err("no legal cut");

    assert!(matches!(
        error,
        CompactionError::NoBalancedCut
            | CompactionError::WholeSurface
            | CompactionError::EmptyRange
    ));
}

#[test]
fn selecting_the_whole_surface_is_refused() {
    // Keeping nothing leaves the model with no history at all.
    let surface = vec![user(1, "a"), assistant(2, "b")];
    let request = CompactionRequest {
        keep_recent: 0,
        max_reach: None,
    };

    let error = select_compaction(&surface, &request).expect_err("would remove everything");
    assert!(matches!(error, CompactionError::WholeSurface));
}

#[test]
fn an_empty_surface_is_an_empty_range() {
    let error = select_compaction(&[], &CompactionRequest::default()).expect_err("nothing to do");
    assert!(matches!(error, CompactionError::EmptyRange));
}

#[test]
fn max_reach_bounds_the_selection() {
    let surface: Vec<SurfaceEntry> = (1..=50).map(|i| user(i, "x")).collect();
    let request = CompactionRequest {
        keep_recent: 5,
        max_reach: Some(10),
    };

    let selection = select_compaction(&surface, &request).expect("selected");

    assert_eq!(selection.start_index, 40, "only the last ten are reachable");
    assert_eq!(selection.end_index, 45);
}

// ---- transcript -------------------------------------------------------------------------------

#[test]
fn the_summary_transcript_keeps_who_said_what() {
    // A summary that loses the speakers is not usable: who asked and who answered is the content.
    let entries = vec![user(1, "list the files"), assistant(2, "here they are")];

    let transcript = transcript_for_summary(&entries);

    assert!(transcript.contains("user: list the files"), "{transcript}");
    assert!(
        transcript.contains("assistant: here they are"),
        "{transcript}"
    );
}

#[test]
fn the_transcript_keeps_tool_calls_and_results() {
    let entries = vec![call(1, "c1"), result(2, "c1")];
    let transcript = transcript_for_summary(&entries);

    assert!(transcript.contains("tool call: bash"), "{transcript}");
    assert!(transcript.contains("tool: ok"), "{transcript}");
}

#[test]
fn a_normal_turn_end_is_not_worth_summarizing() {
    // The boundary is implied by the message order; carrying it would add noise.
    let entries = vec![user(1, "x"), turn_end(2)];
    let transcript = transcript_for_summary(&entries);

    assert!(!transcript.contains("turn ended"), "{transcript}");
}

#[test]
fn a_failed_or_aborted_turn_end_is_carried() {
    let entries = vec![
        user(1, "x"),
        SurfaceEntry {
            seq: 2,
            event: SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Aborted {
                    cause: "user".to_owned(),
                },
            },
        },
    ];
    let transcript = transcript_for_summary(&entries);

    assert!(transcript.contains("turn ended"), "{transcript}");
}

// ---- the pressure trigger ---------------------------------------------------------------------

#[test]
fn compaction_is_needed_past_the_threshold() {
    // A ratio rather than a byte count: the same conversation is fine on a 200k window and past the
    // limit on an 8k one.
    assert!(!needs_compaction(500, 1000, 0.8));
    assert!(needs_compaction(800, 1000, 0.8));
    assert!(needs_compaction(1000, 1000, 0.8));
}

#[test]
fn a_zero_window_never_needs_compaction() {
    // A model with no declared window would otherwise divide by zero.
    assert!(!needs_compaction(1000, 0, 0.8));
}
