//! Tests for session history search.
//!
//! The properties that matter are about what a search can claim: that a truncated result is labelled, and
//! that the text searched is the content rather than the debug shape of the event.

use girder_core::{
    Candidate, ContentBlock, SearchKind, SearchQuery, SessionEvent, run_query, search_files,
    search_matches, searchable_text, session_excerpt,
};

fn message(id: &str, text: &str) -> SessionEvent {
    SessionEvent::user_text(id, text)
}

fn candidate(id: &str, events: Vec<SessionEvent>) -> Candidate {
    Candidate {
        id: id.to_owned(),
        events,
    }
}

// ---- the searchable text ----------------------------------------------------------------------

#[test]
fn a_message_searches_its_text() {
    let event = message("m1", "hello world");
    assert_eq!(searchable_text(&event), "hello world");
}

#[test]
fn a_tool_call_searches_its_name_and_arguments() {
    let event = SessionEvent::ToolCall {
        id: girder_core::ToolCallId::new("c1").expect("valid"),
        name: "bash".to_owned(),
        arguments: r#"{"command":"ls"}"#.to_owned(),
    };

    let text = searchable_text(&event);
    assert!(text.contains("bash"));
    assert!(text.contains("ls"));
}

#[test]
fn a_tool_result_searches_only_its_text_blocks() {
    let event = SessionEvent::ToolResult {
        id: girder_core::ToolCallId::new("c1").expect("valid"),
        content: vec![ContentBlock::Text {
            text: "the answer".to_owned(),
        }],
        is_error: false,
    };

    assert_eq!(searchable_text(&event), "the answer");
}

#[test]
fn a_boundary_event_searches_nothing() {
    // Searching its debug form would make a query for "turn" match every boundary.
    assert!(searchable_text(&SessionEvent::TurnStart { turn: 1 }).is_empty());
}

#[test]
fn a_boundary_event_does_not_match_its_own_field_names() {
    // The debug shape contains field names, which would make a search for "turn" match every boundary.
    assert!(!search_matches(
        &SessionEvent::TurnStart { turn: 1 },
        &SearchQuery::text("turn")
    ));
}

// ---- matching ---------------------------------------------------------------------------------

#[test]
fn a_search_is_case_insensitive() {
    // A caller searching for "BASH" expects to find `bash`.
    let event = message("m1", "Running Bash now");

    assert!(search_matches(&event, &SearchQuery::text("bash")));
    assert!(search_matches(&event, &SearchQuery::text("BASH")));
    assert!(search_matches(&event, &SearchQuery::text("Bash")));
}

#[test]
fn an_empty_query_matches_everything() {
    assert!(search_matches(&message("m1", "x"), &SearchQuery::default()));
}

#[test]
fn a_kind_filter_excludes_other_kinds() {
    let messages = message("m1", "bash");
    let tools = SessionEvent::ToolCall {
        id: girder_core::ToolCallId::new("c1").expect("valid"),
        name: "bash".to_owned(),
        arguments: "{}".to_owned(),
    };

    let only_tools = SearchQuery::text("bash").of_kind(SearchKind::Tools);

    assert!(!search_matches(&messages, &only_tools));
    assert!(search_matches(&tools, &only_tools));
}

#[test]
fn the_kinds_partition_the_conversation_events() {
    let message_event = message("m1", "x");
    let tool_event = SessionEvent::ToolCall {
        id: girder_core::ToolCallId::new("c1").expect("valid"),
        name: "n".to_owned(),
        arguments: "{}".to_owned(),
    };
    let boundary = SessionEvent::TurnStart { turn: 1 };

    assert!(SearchKind::Messages.accepts(&message_event));
    assert!(!SearchKind::Messages.accepts(&tool_event));
    assert!(SearchKind::Tools.accepts(&tool_event));
    assert!(SearchKind::Boundaries.accepts(&boundary));
    assert!(SearchKind::Any.accepts(&boundary));
    assert!(!SearchKind::Boundaries.accepts(&message_event));
}

// ---- running a query --------------------------------------------------------------------------

#[test]
fn a_query_returns_matches_with_their_positions() {
    let results = run_query(
        &SearchQuery::text("needle"),
        vec![candidate(
            "s1",
            vec![message("m1", "first"), message("m2", "a needle here")],
        )],
    );

    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].session, "s1");
    assert_eq!(
        results.hits[0].seq, 2,
        "1-based, so a caller can page from it"
    );
    assert_eq!(results.hits[0].kind, "user/message");
}

#[test]
fn a_query_can_restrict_to_one_session() {
    let results = run_query(
        &SearchQuery::text("x").in_session("s2"),
        vec![
            candidate("s1", vec![message("m1", "x")]),
            candidate("s2", vec![message("m2", "x")]),
        ],
    );

    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].session, "s2");
    assert_eq!(results.sessions_scanned, 1, "the other was never scanned");
}

#[test]
fn a_limit_stops_the_search_and_is_reported() {
    // A caller that received exactly the limit cannot tell whether more existed, and a tool that said
    // "these are all the matches" would be drawing a conclusion the evidence does not support.
    let results = run_query(
        &SearchQuery::text("x").with_limit(2),
        vec![candidate(
            "s1",
            vec![message("m1", "x"), message("m2", "x"), message("m3", "x")],
        )],
    );

    assert_eq!(results.hits.len(), 2);
    assert!(results.truncated);
}

#[test]
fn a_query_under_its_limit_is_not_truncated() {
    let results = run_query(
        &SearchQuery::text("x").with_limit(10),
        vec![candidate("s1", vec![message("m1", "x")])],
    );

    assert_eq!(results.hits.len(), 1);
    assert!(!results.truncated);
}

#[test]
fn a_zero_limit_returns_nothing_and_says_so() {
    let results = run_query(
        &SearchQuery::text("x").with_limit(0),
        vec![candidate("s1", vec![message("m1", "x")])],
    );

    assert!(results.hits.is_empty());
    assert!(results.truncated);
}

#[test]
fn an_empty_catalogue_returns_nothing() {
    let results = run_query(&SearchQuery::text("x"), Vec::new());

    assert!(results.hits.is_empty());
    assert_eq!(results.sessions_scanned, 0);
    assert!(!results.truncated);
}

// ---- searching files --------------------------------------------------------------------------

#[test]
fn a_file_search_reads_every_session() {
    let dir = std::env::temp_dir().join(format!("girder-search-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for (id, text) in [
        ("a", "first needle"),
        ("b", "no match"),
        ("c", "second needle"),
    ] {
        let store = girder_core::SessionStore::new(dir.join(format!("{id}.jsonl.zst")));
        store.touch().expect("created");
        store
            .append(&message(&format!("m-{id}"), text))
            .expect("appended");
    }

    let results = search_files(&dir, &SearchQuery::text("needle"));

    assert_eq!(results.hits.len(), 2);
    assert_eq!(results.sessions_scanned, 3);
    // Sorted by id, so two runs agree.
    assert_eq!(results.hits[0].session, "a");
    assert_eq!(results.hits[1].session, "c");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_search_of_a_missing_directory_is_empty_not_an_error() {
    let results = search_files(
        std::path::Path::new("/nonexistent/girder-search"),
        &SearchQuery::text("x"),
    );

    assert!(results.hits.is_empty());
}

#[test]
fn a_file_search_honours_a_session_restriction_without_reading_the_others() {
    let dir = std::env::temp_dir().join(format!("girder-search-one-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for id in ["a", "b"] {
        let store = girder_core::SessionStore::new(dir.join(format!("{id}.jsonl.zst")));
        store.touch().expect("created");
        store
            .append(&message(&format!("m-{id}"), "needle"))
            .expect("appended");
    }

    let results = search_files(&dir, &SearchQuery::text("needle").in_session("b"));

    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].session, "b");
    assert_eq!(results.sessions_scanned, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- excerpt ----------------------------------------------------------------------------------

#[test]
fn a_short_excerpt_is_returned_whole() {
    assert_eq!(session_excerpt("short", 10, false), "short");
}

#[test]
fn a_long_excerpt_is_cut_and_marked() {
    // Without the ellipsis a reader cannot tell a complete message from the head of a longer one.
    let text = "x".repeat(50);

    let excerpt = session_excerpt(&text, 10, false);

    assert_eq!(
        excerpt.chars().count(),
        11,
        "ten characters plus the ellipsis"
    );
    assert!(excerpt.ends_with('…'));
}

#[test]
fn folding_replaces_newlines_with_spaces() {
    // A tool result is rendered one line per hit, so a newline inside an excerpt would break the row.
    assert_eq!(session_excerpt("a\nb", 10, true), "a b");
}

#[test]
fn not_folding_keeps_newlines() {
    // A client renders each hit as a block, so a newline is the message's shape rather than a row separator.
    assert_eq!(session_excerpt("a\nb", 10, false), "a\nb");
}

#[test]
fn a_cut_lands_on_a_character_boundary() {
    // A byte offset would split a multi-byte character and the excerpt would carry a replacement character, which
    // reads as a corrupt session rather than a shortened one.
    let text = "字".repeat(100);

    let excerpt = session_excerpt(&text, 5, false);

    assert_eq!(
        excerpt.chars().count(),
        6,
        "five characters plus the ellipsis"
    );
    assert!(!excerpt.contains('\u{fffd}'));
}

#[test]
fn an_emoji_counts_as_one_character() {
    let excerpt = session_excerpt("😀😀😀😀", 2, false);

    assert_eq!(excerpt.chars().count(), 3);
    assert!(!excerpt.contains('\u{fffd}'));
}
