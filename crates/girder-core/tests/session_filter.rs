//! Tests for filtering a session list by structural properties.
//!
//! Two properties carry the weight. An unset constraint must constrain nothing, because the empty
//! filter is the documented "match all" and every list view starts from it. And the order must be
//! total, not merely sorted: an order that left ties in input order would make the same request
//! return differently-ordered rows depending on what the directory walk happened to yield.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in.

#[path = "../src/session_filter.rs"]
mod session_filter;

use session_filter::{Filter, Order, SessionSummary, apply};

/// A summary with the fields a case does not care about filled in.
fn summary(id: &str, title: Option<&str>, events: usize, modified_ms: u64) -> SessionSummary {
    SessionSummary {
        id: id.to_owned(),
        title: title.map(str::to_owned),
        events,
        modified_ms,
        tool_calls: 0,
    }
}

/// The ids of a result, in order.
fn ids(summaries: &[SessionSummary]) -> Vec<&str> {
    summaries
        .iter()
        .map(|summary| summary.id.as_str())
        .collect()
}

// ---- the empty filter matches everything -------------------------------------------------------

#[test]
fn an_empty_filter_matches_every_summary() {
    let filter = Filter::default();
    let summaries = vec![
        summary("a", Some("retry policy"), 12, 1_000),
        summary("b", None, 0, 0),
        summary("c", Some(""), 1, 5),
    ];

    let kept = apply(&filter, summaries.clone(), Order::RecentFirst);

    assert_eq!(kept.len(), 3);
    assert_eq!(ids(&kept), vec!["a", "c", "b"]);
}

#[test]
fn an_empty_filter_matches_a_summary_with_nothing_in_it() {
    assert!(Filter::default().matches(&SessionSummary::default()));
}

#[test]
fn an_empty_list_yields_an_empty_result_for_every_order() {
    for order in [Order::RecentFirst, Order::OldestFirst, Order::MostEvents] {
        assert!(apply(&Filter::default(), Vec::new(), order).is_empty());
    }
}

// ---- each condition on its own -----------------------------------------------------------------

#[test]
fn title_contains_is_case_insensitive() {
    let filter = Filter {
        title_contains: Some("RETRY".to_owned()),
        ..Filter::default()
    };

    assert!(filter.matches(&summary("a", Some("retry policy"), 3, 1)));
    assert!(filter.matches(&summary("b", Some("the Retry Policy"), 3, 1)));
    assert!(!filter.matches(&summary("c", Some("rollback policy"), 3, 1)));
}

#[test]
fn title_contains_does_not_match_a_session_with_no_title() {
    let filter = Filter {
        title_contains: Some("anything".to_owned()),
        ..Filter::default()
    };

    // The absence of a title is not a title containing the needle: a caller filtering by title is
    // trying to exclude rows it cannot name, so an untitled row is not a match.
    assert!(!filter.matches(&summary("a", None, 3, 1)));
}

#[test]
fn title_contains_finds_a_substring_anywhere_in_the_title() {
    let filter = Filter {
        title_contains: Some("middle".to_owned()),
        ..Filter::default()
    };

    assert!(filter.matches(&summary("a", Some("start middle end"), 1, 0)));
}

#[test]
fn min_events_is_inclusive() {
    let filter = Filter {
        min_events: Some(5),
        ..Filter::default()
    };

    assert!(!filter.matches(&summary("a", None, 4, 0)));
    assert!(filter.matches(&summary("b", None, 5, 0)));
    assert!(filter.matches(&summary("c", None, 6, 0)));
}

#[test]
fn modified_after_is_inclusive_and_rejects_older_sessions() {
    let filter = Filter {
        modified_after: Some(2_000),
        ..Filter::default()
    };

    assert!(!filter.matches(&summary("a", None, 0, 1_999)));
    assert!(filter.matches(&summary("b", None, 0, 2_000)));
    assert!(filter.matches(&summary("c", None, 0, 2_001)));
}

#[test]
fn has_tool_calls_true_keeps_only_sessions_that_ran_a_tool() {
    let filter = Filter {
        has_tool_calls: Some(true),
        ..Filter::default()
    };
    let mut with_tool = summary("a", None, 3, 0);
    with_tool.tool_calls = 2;
    let without_tool = summary("b", None, 3, 0);

    assert!(filter.matches(&with_tool));
    assert!(!filter.matches(&without_tool));
}

#[test]
fn has_tool_calls_false_keeps_only_sessions_that_ran_none() {
    let filter = Filter {
        has_tool_calls: Some(false),
        ..Filter::default()
    };
    let mut with_tool = summary("a", None, 3, 0);
    with_tool.tool_calls = 1;
    let without_tool = summary("b", None, 3, 0);

    assert!(!filter.matches(&with_tool));
    assert!(filter.matches(&without_tool));
}

#[test]
fn apply_reports_whether_a_summary_had_tool_calls() {
    let mut summary = summary("a", None, 1, 0);
    assert!(!summary.has_tool_calls());
    summary.tool_calls = 1;
    assert!(summary.has_tool_calls());
}

// ---- combinations ------------------------------------------------------------------------------

#[test]
fn conditions_are_anded() {
    let filter = Filter {
        title_contains: Some("retry".to_owned()),
        min_events: Some(5),
        modified_after: Some(1_000),
        has_tool_calls: Some(true),
    };
    let mut both = summary("a", Some("retry budget"), 5, 1_000);
    both.tool_calls = 1;

    // Each of these fails exactly one clause, so none of them may survive.
    let mut too_short = summary("b", Some("retry budget"), 4, 1_000);
    too_short.tool_calls = 1;
    let mut too_old = summary("c", Some("retry budget"), 5, 999);
    too_old.tool_calls = 1;
    let mut no_tool = summary("d", Some("retry budget"), 5, 1_000);
    no_tool.tool_calls = 0;
    let mut wrong_title = summary("e", Some("rollback budget"), 5, 1_000);
    wrong_title.tool_calls = 1;

    let kept = apply(
        &filter,
        vec![both, too_short, too_old, no_tool, wrong_title],
        Order::OldestFirst,
    );

    assert_eq!(ids(&kept), vec!["a"]);
}

#[test]
fn a_filter_that_matches_nothing_yields_an_empty_result() {
    let filter = Filter {
        title_contains: Some("absent".to_owned()),
        ..Filter::default()
    };
    let kept = apply(
        &filter,
        vec![summary("a", Some("present"), 1, 0)],
        Order::MostEvents,
    );

    assert!(kept.is_empty());
}

// ---- describe ----------------------------------------------------------------------------------

#[test]
fn describe_says_when_there_is_no_filter() {
    assert_eq!(Filter::default().describe(), "no filter");
}

#[test]
fn describe_reads_as_one_sentence_for_a_combination() {
    let filter = Filter {
        title_contains: Some("retry".to_owned()),
        min_events: Some(5),
        ..Filter::default()
    };

    assert_eq!(
        filter.describe(),
        "title contains 'retry' and at least 5 events"
    );
}

#[test]
fn describe_names_each_clause_once() {
    let filter = Filter {
        title_contains: Some("retry".to_owned()),
        min_events: Some(5),
        modified_after: Some(1_700_000_000_000),
        has_tool_calls: Some(true),
    };

    assert_eq!(
        filter.describe(),
        "title contains 'retry' and at least 5 events and modified at or after 1700000000000ms and has tool calls"
    );
}

#[test]
fn describe_uses_the_singular_for_one_event() {
    let filter = Filter {
        min_events: Some(1),
        ..Filter::default()
    };

    assert_eq!(filter.describe(), "at least 1 event");
}

#[test]
fn describe_states_the_absence_of_tool_calls() {
    let filter = Filter {
        has_tool_calls: Some(false),
        ..Filter::default()
    };

    assert_eq!(filter.describe(), "has no tool calls");
}

#[test]
fn describe_of_each_single_condition_is_readable() {
    assert_eq!(
        Filter {
            title_contains: Some("x".to_owned()),
            ..Filter::default()
        }
        .describe(),
        "title contains 'x'"
    );
    assert_eq!(
        Filter {
            modified_after: Some(42),
            ..Filter::default()
        }
        .describe(),
        "modified at or after 42ms"
    );
}

// ---- ordering ------------------------------------------------------------------------------

#[test]
fn recent_first_puts_the_newest_modification_first() {
    let summaries = vec![
        summary("old", None, 1, 100),
        summary("new", None, 1, 300),
        summary("mid", None, 1, 200),
    ];

    let kept = apply(&Filter::default(), summaries, Order::RecentFirst);

    assert_eq!(ids(&kept), vec!["new", "mid", "old"]);
}

#[test]
fn oldest_first_puts_the_oldest_modification_first() {
    let summaries = vec![
        summary("mid", None, 1, 200),
        summary("old", None, 1, 100),
        summary("new", None, 1, 300),
    ];

    let kept = apply(&Filter::default(), summaries, Order::OldestFirst);

    assert_eq!(ids(&kept), vec!["old", "mid", "new"]);
}

#[test]
fn most_events_puts_the_longest_conversation_first() {
    let summaries = vec![
        summary("short", None, 2, 100),
        summary("long", None, 40, 200),
        summary("medium", None, 9, 300),
    ];

    let kept = apply(&Filter::default(), summaries, Order::MostEvents);

    assert_eq!(ids(&kept), vec!["long", "medium", "short"]);
}

#[test]
fn ordering_by_events_ignores_modification_time() {
    // A row that is long but old still sorts first under `MostEvents`: the order is the field the
    // caller asked for, not a recency hint hidden behind it.
    let summaries = vec![
        summary("fresh", None, 1, 9_000),
        summary("long-ago", None, 50, 1),
    ];

    let kept = apply(&Filter::default(), summaries, Order::MostEvents);

    assert_eq!(ids(&kept), vec!["long-ago", "fresh"]);
}

// ---- stability ------------------------------------------------------------------------------

#[test]
fn ties_are_broken_by_id_so_equal_rows_are_ordered_predictably() {
    let summaries = vec![
        summary("c", None, 3, 500),
        summary("a", None, 3, 500),
        summary("b", None, 3, 500),
    ];

    let kept = apply(&Filter::default(), summaries, Order::RecentFirst);

    assert_eq!(ids(&kept), vec!["a", "b", "c"]);
}

#[test]
fn every_order_breaks_ties_by_id() {
    for order in [Order::RecentFirst, Order::OldestFirst, Order::MostEvents] {
        let summaries = vec![
            summary("b", None, 3, 500),
            summary("a", None, 3, 500),
            summary("c", None, 3, 500),
        ];

        let kept = apply(&Filter::default(), summaries, order);

        assert_eq!(ids(&kept), vec!["a", "b", "c"], "order {order:?}");
    }
}

#[test]
fn the_order_does_not_depend_on_the_input_order() {
    // The same three rows in two different arrival orders must produce one output order, which is
    // the property a list view relies on when its catalog walks a directory.
    let forwards = vec![
        summary("a", None, 3, 500),
        summary("b", None, 1, 900),
        summary("c", None, 7, 500),
    ];
    let backwards = vec![
        summary("c", None, 7, 500),
        summary("b", None, 1, 900),
        summary("a", None, 3, 500),
    ];

    for order in [Order::RecentFirst, Order::OldestFirst, Order::MostEvents] {
        assert_eq!(
            ids(&apply(&Filter::default(), forwards.clone(), order)),
            ids(&apply(&Filter::default(), backwards.clone(), order)),
            "order {order:?}"
        );
    }
}

#[test]
fn ties_within_most_events_are_broken_by_id() {
    let summaries = vec![
        summary("z", None, 5, 1),
        summary("m", None, 5, 2),
        summary("a", None, 5, 3),
    ];

    let kept = apply(&Filter::default(), summaries, Order::MostEvents);

    assert_eq!(ids(&kept), vec!["a", "m", "z"]);
}

// ---- apply keeps the rows it selected -----------------------------------------------------------

#[test]
fn apply_preserves_the_selected_values() {
    let mut row = summary("a", Some("Retry Policy"), 12, 1_500);
    row.tool_calls = 3;

    let kept = apply(&Filter::default(), vec![row.clone()], Order::OldestFirst);

    assert_eq!(kept, vec![row]);
}
