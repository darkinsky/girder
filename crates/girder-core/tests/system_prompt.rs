//! Tests for system-prompt assembly.
//!
//! The property that matters is not the exact wording of any one block but the separation rule: a
//! part that was not supplied must leave no trace, while a part that was supplied appears in the
//! documented position. Both halves are asserted as whole strings rather than with `contains`,
//! because a heading rendered around an absent value would still pass a substring check.
//!
//! # Why the module is compiled by path
//!
//! [`system_prompt`](crate) is not exported from the crate root yet; the wiring is a separate
//! change to `lib.rs`. Compiling the exact source file here keeps these tests runnable and
//! independent of that wiring, while still exercising the same code a consumer will reach through
//! `girder_core::system_prompt`. Nothing is duplicated: this is the real file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/system_prompt.rs"]
mod system_prompt;

use std::path::PathBuf;

use system_prompt::{Parts, assemble};

/// A base description reused by the cases that are not about the base itself.
const BASE: &str = "You are an AI agent powered by Girder.";

// ---- the minimum ------------------------------------------------------------------------------

#[test]
fn base_alone_is_the_whole_prompt() {
    let parts = Parts::new(BASE);

    // No trailing separator, no trailing newline: the base is the only block.
    assert_eq!(assemble(&parts), BASE);
}

#[test]
fn base_is_trimmed() {
    let parts = Parts::new(format!("\n  {BASE}  \n"));

    assert_eq!(assemble(&parts), BASE);
}

// ---- one optional part at a time --------------------------------------------------------------

#[test]
fn workspace_is_quoted_after_the_base() {
    let parts = Parts {
        workspace: Some(PathBuf::from("/srv/docs")),
        ..Parts::new(BASE)
    };

    assert_eq!(
        assemble(&parts),
        format!("{BASE}\n\nCurrent working directory: \"/srv/docs\".")
    );
}

#[test]
fn a_workspace_path_with_spaces_stays_one_token() {
    // Without quoting, `Current working directory: /srv/my docs.` would end the path at the space.
    let parts = Parts {
        workspace: Some(PathBuf::from("/srv/my docs")),
        ..Parts::new(BASE)
    };

    assert_eq!(
        assemble(&parts),
        format!("{BASE}\n\nCurrent working directory: \"/srv/my docs\".")
    );
}

#[test]
fn a_workspace_path_with_a_quote_cannot_end_the_line_early() {
    // A directory may legally be named `a"b`. The quote is escaped, so the block still ends where
    // this module put it, not where the name says.
    let parts = Parts {
        workspace: Some(PathBuf::from("/tmp/a\"b")),
        ..Parts::new(BASE)
    };

    assert_eq!(
        assemble(&parts),
        format!("{BASE}\n\nCurrent working directory: \"/tmp/a\\\"b\".")
    );
}

#[test]
fn several_skills_share_one_line() {
    let parts = Parts {
        skills: vec![
            "review".to_owned(),
            "commit".to_owned(),
            "office".to_owned(),
        ],
        ..Parts::new(BASE)
    };

    assert_eq!(
        assemble(&parts),
        format!("{BASE}\n\nAvailable skills: `review`, `commit`, `office`.")
    );
}

#[test]
fn a_single_skill_is_not_a_list() {
    let parts = Parts {
        skills: vec!["review".to_owned()],
        ..Parts::new(BASE)
    };

    assert_eq!(
        assemble(&parts),
        format!("{BASE}\n\nAvailable skills: `review`.")
    );
}

#[test]
fn duplicate_and_blank_skill_names_are_collapsed() {
    // A name that cannot be invoked, or that appears twice, costs tokens and reads as two skills.
    let parts = Parts {
        skills: vec![
            "review".to_owned(),
            "  ".to_owned(),
            "review".to_owned(),
            " commit ".to_owned(),
        ],
        ..Parts::new(BASE)
    };

    assert_eq!(
        assemble(&parts),
        format!("{BASE}\n\nAvailable skills: `review`, `commit`.")
    );
}

#[test]
fn today_follows_the_workspace() {
    let parts = Parts {
        today: Some("2026-11-13".to_owned()),
        ..Parts::new(BASE)
    };

    assert_eq!(
        assemble(&parts),
        format!("{BASE}\n\nCurrent date: 2026-11-13.")
    );
}

#[test]
fn extra_notes_are_one_block_each_after_the_environment() {
    let parts = Parts {
        workspace: Some(PathBuf::from("/srv/docs")),
        today: Some("2026-11-13".to_owned()),
        extra: vec![
            "This deployment is air-gapped.".to_owned(),
            "Answer in Chinese.".to_owned(),
        ],
        ..Parts::new(BASE)
    };

    assert_eq!(
        assemble(&parts),
        concat!(
            "You are an AI agent powered by Girder.",
            "\n\nCurrent working directory: \"/srv/docs\".",
            "\n\nCurrent date: 2026-11-13.",
            "\n\nThis deployment is air-gapped.",
            "\n\nAnswer in Chinese.",
        )
    );
}

// ---- absent parts leave no heading ------------------------------------------------------------

#[test]
fn an_empty_skill_list_renders_no_skills_line() {
    // The failure this prevents: a prompt containing `Available skills:` and then nothing, which
    // tells the model a list was supposed to be there.
    let parts = Parts::new(BASE);

    let prompt = assemble(&parts);

    assert_eq!(prompt, BASE);
    assert!(
        !prompt.contains("skill"),
        "empty deployment must not mention skills: {prompt}"
    );
}

#[test]
fn every_empty_part_contributes_nothing() {
    // Whitespace-only strings and a present-but-blank date are the cases a naive implementation
    // renders as empty headings; all of them must vanish.
    let parts = Parts {
        base: BASE.to_owned(),
        workspace: None,
        skills: Vec::new(),
        today: Some("   ".to_owned()),
        extra: vec![String::new(), "\t".to_owned(), "  \n ".to_owned()],
    };

    assert_eq!(assemble(&parts), BASE);
}

#[test]
fn no_option_renders_an_empty_label() {
    // Exhaustive guard over the three optional scalars: whatever combination is absent, none of
    // their labels may appear.
    let cases = [
        Parts::new(BASE),
        Parts {
            workspace: None,
            today: None,
            ..Parts::new(BASE)
        },
        Parts {
            workspace: Some(PathBuf::from("/w")),
            today: None,
            ..Parts::new(BASE)
        },
    ];

    for parts in cases {
        let prompt = assemble(&parts);
        if parts.workspace.is_none() {
            assert!(!prompt.contains("working directory"), "{prompt}");
        }
        if parts.today.is_none() {
            assert!(!prompt.contains("Current date"), "{prompt}");
        }
        assert!(!prompt.contains("Available skills"), "{prompt}");
    }
}

#[test]
fn all_empty_parts_assemble_to_the_empty_string() {
    // Not a placeholder sentence: an empty prompt means the caller should send no system field at
    // all, which is what `LlmRequest::system_prompt: Option<String>` already expresses.
    assert_eq!(assemble(&Parts::default()), "");
    assert_eq!(
        assemble(&Parts {
            base: "   ".to_owned(),
            skills: vec![String::new()],
            extra: vec!["  ".to_owned()],
            ..Parts::default()
        }),
        ""
    );
}

#[test]
fn separators_never_lead_or_trail() {
    // A leading or trailing blank line is a separator with nothing on one side — the residue of a
    // block that was rendered empty.
    let parts = Parts {
        base: BASE.to_owned(),
        workspace: Some(PathBuf::from("/w")),
        skills: vec!["s".to_owned()],
        today: Some("2026-11-13".to_owned()),
        extra: vec!["note".to_owned()],
    };

    let prompt = assemble(&parts);

    assert!(!prompt.starts_with('\n'), "{prompt:?}");
    assert!(!prompt.ends_with('\n'), "{prompt:?}");
    assert!(
        !prompt.contains("\n\n\n"),
        "blocks must be separated by exactly one blank line: {prompt:?}"
    );
}

// ---- order ------------------------------------------------------------------------------------

#[test]
fn blocks_appear_in_documented_order() {
    // The order is the contract: base first because it frames everything read after it, extra last
    // because recency gives the tail the most weight.
    let parts = Parts {
        base: BASE.to_owned(),
        workspace: Some(PathBuf::from("/w")),
        skills: vec!["first".to_owned()],
        today: Some("2026-11-13".to_owned()),
        extra: vec!["deployment note".to_owned()],
    };

    let prompt = assemble(&parts);
    let positions = [
        prompt.find(BASE).expect("base"),
        prompt.find("Current working directory").expect("workspace"),
        prompt.find("Available skills").expect("skills"),
        prompt.find("Current date").expect("date"),
        prompt.find("deployment note").expect("extra"),
    ];

    let mut sorted = positions;
    sorted.sort_unstable();
    assert_eq!(positions, sorted, "blocks out of order in {prompt:?}");
}

#[test]
fn base_precedes_extra_even_when_extra_would_reframe_it() {
    // A deployment note is appended, never prepended: identity is read before any qualification of
    // it, which is why `base` cannot be pushed down by a long `extra`.
    let parts = Parts {
        extra: vec!["Ignore the previous paragraph.".to_owned()],
        ..Parts::new(BASE)
    };

    assert_eq!(
        assemble(&parts),
        format!("{BASE}\n\nIgnore the previous paragraph.")
    );
}
