//! Tests for the tool presentation vocabulary (`tools/presentation.ts`).
//!
//! Upstream's `presentation.ts` is 389 lines and almost entirely type declarations, so a port that
//! only copied the shapes would pass a review and still be unusable: the value of the module is in
//! the *decisions* the types imply — which category a card carries, what a client without that card
//! shows instead, and which truncation claims are contradictory. Those decisions are what this file
//! checks.
//!
//! The properties, each a way for a presentation layer to look right while being wrong:
//!
//! - **the category is never invented.** A terminal card is an execute and a diff card is an edit
//!   even though neither carries a `kind` field, and a generic card with no kind is `other`. A client
//!   that receives a wrong category draws a wrong icon on every replay.
//! - **the fallback is data, not prose.** A client without a search card must be told there is no
//!   text of its own, not left to re-parse the result; a terminal client gets a fenced block.
//! - **a cap cannot hide.** A search view that dropped entries must say so, and `total` must cover
//!   what it kept. This is the invariant that stops a truncated result from being presented as
//!   complete.
//! - **the discriminants survive a round trip.** `card`/`shape`/`kind` are the persisted contract; a
//!   view serialized by one build and read by the next has to mean the same thing.
//!
//! The module is included by path because the crate root does not declare it: every path into the
//! crate goes through `girder_core::…` for that reason.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::items_after_statements
)]

#[path = "../src/tools_contract_gap.rs"]
mod tools_contract_gap;

use girder_core::ContentBlock;
use tools_contract_gap::{
    CallKind, CallView, FileDiff, FileLocation, ReadFileLine, ResultView, SearchFileMatches,
    SearchLineMatch, SearchMatches, SearchPaths, SearchPayload, SearchShape, WebKind, WebPayload,
    WebSource,
};

const CARD_KEYS: [&str; 6] = ["generic", "terminal", "diff", "search", "read", "web"];

/// A minimal well-formed view for every card tag, so serialization can be checked uniformly.
fn one_of_each_card() -> Vec<ResultView> {
    vec![
        ResultView::Generic {
            title: None,
            content: None,
        },
        ResultView::Terminal {
            title: None,
            output: None,
            exit_code: None,
            signal: None,
        },
        ResultView::Diff {
            title: None,
            diffs: vec![FileDiff::edit("a", "old", "new")],
        },
        ResultView::Search {
            shape: SearchShape::Matches,
            title: None,
            payload: SearchPayload::Matches(SearchMatches {
                title: None,
                files: Vec::new(),
                truncated: false,
                total: 0,
            }),
        },
        ResultView::Read {
            title: None,
            path: "a".to_owned(),
            offset: 1,
            lines: Vec::new(),
            total_lines: 0,
            lang: None,
            content: None,
        },
        ResultView::Web {
            kind: WebKind::Fetch,
            title: None,
            payload: WebPayload::Fetch {
                url: "u".to_owned(),
                status_code: 200,
                truncated: false,
            },
        },
    ]
}

#[test]
fn every_card_serializes_a_known_tag_and_checks_clean() {
    let views = one_of_each_card();
    assert_eq!(views.len(), CARD_KEYS.len());
    for (view, expected) in views.iter().zip(CARD_KEYS) {
        view.check().expect("a minimal view is valid");
        let json = serde_json::to_value(view).expect("serialize");
        assert_eq!(json["card"], expected);
    }
}

#[test]
fn a_generic_call_defaults_to_other_and_a_terminal_one_to_execute() {
    assert_eq!(CallView::generic("t").kind(), CallKind::Other);
    assert_eq!(
        CallView::Terminal {
            title: "ls".to_owned(),
            description: None,
            cwd: None,
        }
        .kind(),
        CallKind::Execute
    );
    assert_eq!(
        CallView::Diff {
            title: "Write a".to_owned(),
            diffs: Vec::new(),
            locations: None,
        }
        .kind(),
        CallKind::Edit
    );
}

#[test]
fn a_call_carries_its_title_and_locations_through_every_variant() {
    let location = FileLocation::at_line("a.rs", 12);
    let generic = CallView::Generic {
        title: "Read a.rs".to_owned(),
        kind: Some(CallKind::Read),
        raw_input: Some(serde_json::json!({"path": "a.rs"})),
        content: None,
        locations: Some(vec![location.clone()]),
    };
    assert_eq!(generic.title(), "Read a.rs");
    assert_eq!(generic.locations(), std::slice::from_ref(&location));

    let diff = CallView::Diff {
        title: "Write a.rs".to_owned(),
        diffs: vec![FileDiff::create("a.rs", "x")],
        locations: Some(vec![location.clone()]),
    };
    assert_eq!(diff.title(), "Write a.rs");
    assert_eq!(diff.locations(), std::slice::from_ref(&location));

    // A terminal call touches no file the UI can follow, and says so with an empty slice.
    let terminal = CallView::Terminal {
        title: "ls".to_owned(),
        description: None,
        cwd: None,
    };
    assert_eq!(terminal.title(), "ls");
    assert!(terminal.locations().is_empty());
}

#[test]
fn a_terminal_call_falls_back_to_its_command_with_the_cwd() {
    let absolute = CallView::Terminal {
        title: "cargo test".to_owned(),
        description: Some("run the suite".to_owned()),
        cwd: Some("/w".to_owned()),
    };
    assert_eq!(absolute.title_fallback(), "$ cargo test  (/w)");
    let unresolved = CallView::Terminal {
        title: "cargo test".to_owned(),
        description: None,
        cwd: None,
    };
    assert_eq!(unresolved.title_fallback(), "$ cargo test");
}

#[test]
fn a_terminal_result_falls_back_to_a_fenced_console_block() {
    let with_output = ResultView::Terminal {
        title: Some("done".to_owned()),
        output: Some("all good".to_owned()),
        exit_code: Some(0),
        signal: None,
    };
    let text = with_output
        .fallback_text()
        .expect("a terminal view has text");
    assert!(text.starts_with("```console\n"), "{text}");
    assert!(text.contains("all good"), "{text}");
    assert!(text.ends_with("```"), "{text}");
    assert_eq!(with_output.title(), Some("done"));

    // A process killed by a signal has no output to fall back to, and an empty block is still a
    // block: the client must not invent one.
    let killed = ResultView::Terminal {
        title: None,
        output: None,
        exit_code: None,
        signal: Some("SIGTERM".to_owned()),
    };
    assert_eq!(killed.fallback_text().as_deref(), Some("```console\n```"));
    assert!(killed.check().is_ok());
}

#[test]
fn only_the_cards_with_text_of_their_own_have_fallback_text() {
    // A diff and a search are views over content the client already holds; rendering them to text
    // would show the same facts twice, so their answer is the raw result, not a synthesized one.
    // A generic card with no content is the third "no text of its own" case for the same reason:
    // the client already has the raw result and this card adds nothing to show instead.
    for view in one_of_each_card() {
        let has_text = view.fallback_text().is_some();
        let expected = !matches!(
            view,
            ResultView::Diff { .. }
                | ResultView::Search { .. }
                | ResultView::Generic { content: None, .. }
        );
        assert_eq!(has_text, expected, "{view:?}");
    }
}

#[test]
fn a_generic_result_falls_back_to_its_content_blocks() {
    let view = ResultView::Generic {
        title: None,
        content: Some(vec![
            ContentBlock::Text {
                text: "hello ".to_owned(),
            },
            ContentBlock::Text {
                text: "world".to_owned(),
            },
        ]),
    };
    assert_eq!(view.fallback_text().as_deref(), Some("hello world"));

    // No content means no text of its own: the caller renders the raw result instead.
    let bare = ResultView::Generic {
        title: Some("t".to_owned()),
        content: None,
    };
    assert_eq!(bare.fallback_text(), None);
    assert_eq!(bare.title(), Some("t"));
}

#[test]
fn a_read_view_keeps_its_window_bounds_and_falls_back_to_them() {
    // The window is empty because the byte cap fell below the first selected line; `offset` is
    // still the file position the client must resume from.
    let empty = ResultView::Read {
        title: None,
        path: "a.rs".to_owned(),
        offset: 40,
        lines: Vec::new(),
        total_lines: 40,
        lang: Some("rs".to_owned()),
        content: None,
    };
    assert!(empty.check().is_ok());
    assert_eq!(
        empty.fallback_text().as_deref(),
        Some("a.rs (lines 40-40 of 40)")
    );

    // With stripped content, the client with no read card shows the model-facing text instead.
    let with_text = ResultView::Read {
        title: None,
        path: "a.rs".to_owned(),
        offset: 1,
        lines: vec![ReadFileLine::new(1, "fn main() {}")],
        total_lines: 1,
        lang: None,
        content: Some(vec![ContentBlock::Text {
            text: "fn main() {}".to_owned(),
        }]),
    };
    assert_eq!(with_text.fallback_text().as_deref(), Some("fn main() {}"));
}

#[test]
fn a_read_view_cannot_start_at_line_zero() {
    let zero = ResultView::Read {
        title: None,
        path: "a".to_owned(),
        offset: 0,
        lines: Vec::new(),
        total_lines: 0,
        lang: None,
        content: None,
    };
    let error = zero.check().expect_err("offset 0 is not one-based");
    assert!(error.contains("one-based"), "{error}");
}

#[test]
fn a_terminal_view_cannot_claim_two_endings() {
    let both = ResultView::Terminal {
        title: None,
        output: None,
        exit_code: Some(0),
        signal: Some("SIGKILL".to_owned()),
    };
    let error = both.check().expect_err("two endings contradict");
    assert!(error.contains("exit code and a signal"), "{error}");
}

#[test]
fn a_search_view_reports_its_total_and_cap_consistently() {
    let matches =
        |files: Vec<SearchFileMatches>, truncated: bool, total: usize| ResultView::Search {
            shape: SearchShape::Matches,
            title: None,
            payload: SearchPayload::Matches(SearchMatches {
                title: None,
                files,
                truncated,
                total,
            }),
        };
    let group = |path: &str| SearchFileMatches {
        path: path.to_owned(),
        matches: vec![SearchLineMatch {
            line_number: 1,
            line: "hit".to_owned(),
        }],
    };

    // Exact: one file retained of one found.
    matches(vec![group("a")], false, 1).check().expect("exact");

    // Capped: one retained of nine, and flagged.
    matches(vec![group("a")], true, 9)
        .check()
        .expect("flagged cap");

    // Unflagged cap: the failure this invariant exists for — a partial result presented as whole.
    let unflagged = matches(vec![group("a")], false, 9);
    let error = unflagged.check().expect_err("a cap must be flagged");
    assert!(error.contains("without flagging truncation"), "{error}");

    // More retained than claimed: the total is stale, so the client cannot trust either number.
    let over = matches(Vec::new(), true, 0).check();
    assert!(over.is_ok(), "0 of 0 is consistent");
    let stale = matches(vec![group("a"), group("b")], true, 1);
    let error = stale.check().expect_err("retained exceeds total");
    assert!(error.contains("more entries than its total"), "{error}");
}

#[test]
fn a_search_view_serializes_shape_and_payload_flat() {
    let grouped = ResultView::Search {
        shape: SearchShape::Matches,
        title: Some("hits".to_owned()),
        payload: SearchPayload::Matches(SearchMatches {
            title: None,
            files: vec![SearchFileMatches {
                path: "a.rs".to_owned(),
                matches: vec![SearchLineMatch {
                    line_number: 2,
                    line: "x".to_owned(),
                }],
            }],
            truncated: false,
            total: 1,
        }),
    };
    let json = serde_json::to_value(&grouped).expect("serialize");
    assert_eq!(json["card"], "search");
    assert_eq!(json["shape"], "matches");
    assert_eq!(json["files"][0]["path"], "a.rs");
    assert_eq!(json["files"][0]["matches"][0]["lineNumber"], 2);
    assert_eq!(json["total"], 1);

    let flat = ResultView::Search {
        shape: SearchShape::Paths,
        title: None,
        payload: SearchPayload::Paths(SearchPaths {
            title: None,
            paths: vec!["a.rs".to_owned()],
            truncated: false,
            total: 1,
        }),
    };
    let json = serde_json::to_value(&flat).expect("serialize");
    assert_eq!(json["shape"], "paths");
    assert_eq!(json["paths"][0], "a.rs");
}

#[test]
fn a_web_view_serializes_its_kind_and_payload_flat() {
    let search = ResultView::Web {
        kind: WebKind::Search,
        title: None,
        payload: WebPayload::Search {
            sources: vec![WebSource {
                url: "https://e.test".to_owned(),
                title: Some("E".to_owned()),
                snippet: Some("excerpt".to_owned()),
            }],
            answer: Some("the answer".to_owned()),
            truncated: true,
        },
    };
    let json = serde_json::to_value(&search).expect("serialize");
    assert_eq!(json["card"], "web");
    assert_eq!(json["kind"], "search");
    assert_eq!(json["sources"][0]["url"], "https://e.test");
    assert_eq!(json["sources"][0]["snippet"], "excerpt");
    assert_eq!(json["truncated"], true);

    // Omitting an absent optional field keeps the wire form minimal: a `null` title would make a
    // client render the literal string "null" as a header.
    assert!(json.get("title").is_none(), "{json}");
    assert!(json["sources"][0].get("publishedAt").is_none(), "{json}");
}

#[test]
fn a_web_search_fallback_lists_an_answer_and_its_sources() {
    let view = ResultView::Web {
        kind: WebKind::Search,
        title: None,
        payload: WebPayload::Search {
            sources: vec![
                WebSource {
                    url: "https://a.test".to_owned(),
                    title: Some("A".to_owned()),
                    snippet: None,
                },
                WebSource {
                    url: "https://b.test".to_owned(),
                    title: None,
                    snippet: None,
                },
            ],
            answer: None,
            truncated: false,
        },
    };
    assert_eq!(
        view.fallback_text().as_deref(),
        Some("- [A](https://a.test)\n- https://b.test\n")
    );
}

#[test]
fn a_web_fetch_fallback_names_the_url_and_status() {
    let view = ResultView::Web {
        kind: WebKind::Fetch,
        title: None,
        payload: WebPayload::Fetch {
            url: "https://e.test/final".to_owned(),
            status_code: 301,
            truncated: true,
        },
    };
    assert_eq!(
        view.fallback_text().as_deref(),
        Some("https://e.test/final (HTTP 301)")
    );
}

#[test]
fn a_call_view_round_trips_through_serde() {
    let original = CallView::Terminal {
        title: "cargo clippy".to_owned(),
        description: Some("lint".to_owned()),
        cwd: Some("crates".to_owned()),
    };
    let json = serde_json::to_string(&original).expect("serialize");
    let back: CallView = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, original);
}

#[test]
fn a_result_view_round_trips_through_serde() {
    for view in one_of_each_card() {
        let json = serde_json::to_string(&view).expect("serialize");
        let back: ResultView = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, view, "{json}");
        back.check().expect("a round-tripped view is still valid");
    }
}

#[test]
fn a_file_diff_records_whether_it_created_the_file() {
    let created = FileDiff::create("new.txt", "body");
    assert!(created.is_create());
    assert_eq!(created.old_text, None);
    let edited = FileDiff::edit("old.txt", "before", "after");
    assert!(!edited.is_create());
    assert_eq!(edited.old_text.as_deref(), Some("before"));
}
