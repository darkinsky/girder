//! Tests for tool-result pruning.
//!
//! The property that matters most here is not "the result got smaller" — any prefix cut achieves
//! that — but "the part that explains the failure survived". A build log, a stack trace and a test
//! summary all put their answer at the *end*, and a pruner that keeps only the opening lines turns
//! a failed command into a confident-sounding header. So the load-bearing test is
//! [`an_error_at_the_end_survives_pruning`]: it asserts the `error[E0308]` line is still readable
//! after the middle of a long log was thrown away.
//!
//! The second property is honesty: a shortened result must say how much is missing, in lines,
//! because a model that is not told will answer about data it never saw.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests
//! compile against it as written even before the crate root wires it in. That is why nothing here
//! reaches for anything else in `girder_core`.

#[path = "../src/tool_result_pruning.rs"]
mod tool_result_pruning;

use std::fmt::Write as _;

use tool_result_pruning::{Presentation, apply, decide};

/// A log of `lines` compile-looking lines, ending in a real error.
///
/// Shape taken from a failing `cargo build`: warnings and context first, the diagnostic last.
fn compile_log(lines: usize) -> String {
    let mut out = String::new();
    for index in 0..lines {
        let _ = writeln!(
            out,
            "warning: unused variable `value_{index}` in `crates/girder-core/src/file_{index}.rs`"
        );
    }
    out.push_str("error[E0308]: mismatched types\n");
    out.push_str("  --> crates/girder-core/src/agent_loop.rs:412:9\n");
    out.push_str("expected `usize`, found `&str`\n");
    out
}

// ---- the decision -------------------------------------------------------------------------------

#[test]
fn short_content_is_kept_whole() {
    let content = "line one\nline two\nline three";
    let presentation = decide(content, 1024);
    assert_eq!(presentation, Presentation::Full);
    assert_eq!(apply(content, &presentation), content);
}

#[test]
fn empty_content_is_kept_whole() {
    let presentation = decide("", 0);
    assert_eq!(presentation, Presentation::Full);
    assert_eq!(apply("", &presentation), "");
}

#[test]
fn long_content_keeps_both_ends() {
    let content = compile_log(400);
    let presentation = decide(&content, 600);

    let (head_lines, tail_lines) = match presentation {
        Presentation::HeadTail {
            head_lines,
            tail_lines,
        } => (head_lines, tail_lines),
        other => panic!("expected a head/tail presentation, got {other:?}"),
    };
    assert!(
        head_lines > 0,
        "the head must survive to identify the output"
    );
    assert!(
        tail_lines > 0,
        "the tail must survive; it is where the answer usually is"
    );

    let rendered = apply(&content, &presentation);
    assert!(rendered.contains("warning: unused variable `value_0`"));
    assert!(
        rendered.contains("expected `usize`, found `&str`"),
        "the final line of the result must survive:\n{rendered}"
    );
    assert!(
        rendered.chars().count() < content.chars().count(),
        "pruning must shrink the result"
    );
}

#[test]
fn an_error_at_the_end_survives_pruning() {
    // The case the module exists for: a long log whose only useful line is its last. Keeping the
    // head alone would show 400 warnings and no reason the build failed.
    let content = compile_log(1000);
    let presentation = decide(&content, 400);
    let rendered = apply(&content, &presentation);

    assert!(
        rendered.contains("error[E0308]: mismatched types"),
        "the error line must survive a mid-result cut:\n{rendered}"
    );
    assert!(
        rendered.contains("expected `usize`, found `&str`"),
        "the diagnostic's explanation must survive too:\n{rendered}"
    );
    assert!(
        !rendered.contains("warning: unused variable `value_500`"),
        "the middle really was removed"
    );
}

#[test]
fn a_stack_trace_root_cause_survives_pruning() {
    // The second shape of the same problem: a trace's useful part is the *bottom*, where the cause
    // is, and above it a wall of frames a reader skips.
    let mut content = String::new();
    for index in 0..300 {
        let _ = writeln!(
            content,
            "    at frame_{index} (crates/girder-core/src/module_{index}.rs:{index})"
        );
    }
    content.push_str("Caused by: connection refused (os error 111)\n");

    let rendered = apply(&content, &decide(&content, 300));
    assert!(
        rendered.contains("Caused by: connection refused"),
        "the root cause at the bottom must survive:\n{rendered}"
    );
}

#[test]
fn a_single_line_gets_a_summary_rather_than_a_guess() {
    // No line boundary to cut on, so a head/tail line cut is impossible. The honest presentation is
    // a flattened excerpt that says how much was dropped.
    let content = "x".repeat(10_000);
    let presentation = decide(&content, 500);

    match &presentation {
        Presentation::Summary(_) => {}
        other => panic!("expected a summary for one huge line, got {other:?}"),
    }
    let rendered = apply(&content, &presentation);
    assert!(!rendered.contains('\n'), "a summary is one line");
    assert!(rendered.chars().count() < content.chars().count());
    assert!(rendered.contains("characters pruned"));
}

#[test]
fn one_short_line_is_kept_whole() {
    let content = "only one line here";
    let presentation = decide(content, content.chars().count());
    assert_eq!(presentation, Presentation::Full);
    assert_eq!(apply(content, &presentation), content);
}

#[test]
fn decide_never_chooses_head_only() {
    // `Head` exists for callers that know their own output is front-loaded. `decide` sees text and
    // nothing else, so it must not silently drop the tail — that is the failure this module guards
    // against.
    let mut seed = 7u64;
    for limit in [1usize, 8, 40, 200, 1000] {
        for line_count in [2usize, 3, 10, 50] {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let filler = usize::try_from(seed % 60).unwrap_or(0);
            let content = compile_log(line_count).replace('x', &"y".repeat(filler + 1));
            assert!(
                !matches!(decide(&content, limit), Presentation::Head { .. }),
                "decide must not drop the tail (limit {limit}, {line_count} lines)"
            );
        }
    }
}

#[test]
fn pruning_never_makes_a_result_larger() {
    // A budget smaller than the notice itself cannot pay for a cut. Returning a longer string would
    // spend the context the caller was trying to save.
    for limit in [0usize, 1, 4, 20] {
        let content = "alpha\nbeta\ngamma";
        let presentation = decide(content, limit);
        let rendered = apply(content, &presentation);
        assert!(
            rendered.chars().count() <= content.chars().count(),
            "limit {limit} grew the result: {rendered:?}"
        );
    }
}

// ---- the notice ---------------------------------------------------------------------------------

#[test]
fn the_notice_reports_how_many_lines_were_pruned() {
    let content = compile_log(200);
    let presentation = decide(&content, 500);
    let rendered = apply(&content, &presentation);

    let total_lines = content.lines().count();
    match presentation {
        Presentation::HeadTail {
            head_lines,
            tail_lines,
        } => {
            let expected = total_lines - head_lines - tail_lines;
            assert!(
                rendered.contains(&format!("{expected} of {total_lines} lines pruned")),
                "the model must be told how much it cannot see:\n{rendered}"
            );
        }
        other => panic!("expected a head/tail presentation, got {other:?}"),
    }
}

#[test]
fn the_notice_does_not_claim_a_cut_that_did_not_happen() {
    // A marker reporting "0 lines pruned" teaches the model to distrust every other marker.
    let content = "alpha\nbeta";
    let rendered = apply(
        content,
        &Presentation::HeadTail {
            head_lines: 1,
            tail_lines: 1,
        },
    );
    assert_eq!(rendered, content);
}

#[test]
fn head_only_presentation_reports_its_own_loss() {
    let content = compile_log(50);
    let rendered = apply(&content, &Presentation::Head { lines: 2 });
    assert!(rendered.starts_with("warning: unused variable `value_0`"));
    assert!(rendered.contains("only the beginning is shown"));
    assert!(!rendered.contains("error[E0308]"));
    assert!(rendered.len() < content.len());
}

// ---- character handling -------------------------------------------------------------------------

#[test]
fn chinese_text_is_cut_by_characters_not_bytes() {
    // Counted in characters because that is what the tokenizer sees. Counting bytes would prune
    // Chinese text roughly three times too eagerly; slicing by byte would split a character.
    let mut content = String::new();
    for index in 0..200 {
        let _ = writeln!(content, "第 {index} 行：这是一条用于测试的中文日志内容");
    }
    content.push_str("错误：类型不匹配\n");

    let presentation = decide(&content, 500);
    let rendered = apply(&content, &presentation);

    assert!(
        rendered.contains("错误：类型不匹配"),
        "the Chinese error line at the end must survive:\n{rendered}"
    );
    assert!(
        !rendered.contains('\u{fffd}'),
        "no character may be split into a replacement character"
    );
    assert!(
        rendered.chars().count() < content.chars().count(),
        "the result must shrink"
    );
}

#[test]
fn a_wide_single_character_line_is_not_split_into_garbage() {
    // Emoji are multi-byte and, in some cases, multi-code-point. Whatever the cut, the output must
    // still be valid text the model can read.
    let content = "🎉".repeat(500);
    let presentation = decide(&content, 100);
    let rendered = apply(&content, &presentation);

    assert!(!rendered.contains('\u{fffd}'));
    assert!(rendered.chars().count() < content.chars().count());
}

// ---- rendering ----------------------------------------------------------------------------------

#[test]
fn full_is_verbatim_even_for_whitespace() {
    let content = "  indented\n\n\ttabbed  \n";
    assert_eq!(apply(content, &Presentation::Full), content);
}

#[test]
fn summary_is_returned_verbatim() {
    // A summary is prose about the result; only the code that wrote it knows what it says.
    let presentation =
        Presentation::Summary("the command printed 12 warnings and no errors".into());
    assert_eq!(
        apply("ignored", &presentation),
        "the command printed 12 warnings and no errors"
    );
}

#[test]
fn head_tail_keeps_lines_in_order() {
    let content = "one\ntwo\nthree\nfour\nfive\nsix";
    let rendered = apply(
        content,
        &Presentation::HeadTail {
            head_lines: 2,
            tail_lines: 2,
        },
    );

    let lines: Vec<&str> = rendered.lines().collect();
    assert_eq!(lines.first(), Some(&"one"));
    assert_eq!(lines.get(1), Some(&"two"));
    assert_eq!(lines.last(), Some(&"six"));
    assert_eq!(lines.get(lines.len() - 2), Some(&"five"));
    assert!(
        lines.iter().any(|line| line.contains("pruned")),
        "the middle is a notice, not a gap:\n{rendered}"
    );
}

#[test]
fn head_tail_of_more_lines_than_exist_is_a_no_op() {
    // A presentation decided against one version of a result may be applied to a shorter one; it
    // must degrade to the full text rather than panic or invent a cut.
    let content = "one\ntwo";
    let rendered = apply(
        content,
        &Presentation::HeadTail {
            head_lines: 40,
            tail_lines: 40,
        },
    );
    assert_eq!(rendered, content);
}
