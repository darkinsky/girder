//! Tests for `workspace_watch_gap`: where workspace file changes come from, and which ones count.
//!
//! `packages/api/workspace-files/src/changes.ts` is 162 lines. Half of it is a notifier call
//! (`await this.ctx.fs.watch(target, ...)`) and half is the decision about what to do with what the
//! notifier said. Only the second half is portable, and these are its properties, in the order they
//! can hurt:
//!
//! - **The gate is type-aware, not position-aware.** A *directory* outside the workspace root is
//!   refused; an absolute file outside it is followed. Upstream's check is
//!   `info.type === 'directory' && !contains(root, target)`, and inverting or generalizing it either
//!   lets a tree walk escape or breaks the supported single-file mode.
//! - **An absent target is watchable.** Planning a path that does not exist must succeed, or the
//!   only thing a `changes` stream can never report is the creation it exists for.
//! - **A child event invalidates the target.** A directory's own version does not move when a
//!   child's metadata does, so a consumer that keyed on the child would wait forever. Only a
//!   notifier that genuinely reports descendants gets per-child frames.
//! - **A directory's repeat version is a change; a file's is not.** The same fact read both ways:
//!   suppressing a directory frame hides a child's edit, and echoing a file frame re-renders a file
//!   that did not change.
//! - **A burst coalesces to one frame and one stat.** A save is several events; a build is
//!   thousands. Coalescing is asserted by counting `stat` calls, because that is the cost that
//!   actually matters.
//! - **Overflow is announced, never silent.** Past the bound the queue reports `resync` rather than
//!   pretending it still knows what is dirty.
//! - **The wire shape is upstream's.** `{absolutePath, version}` and `{absolutePath, absent: true}`
//!   round-trip, and a payload carrying both or neither is rejected rather than guessed at.
//!
//! No test starts a watcher, touches a real file, or reads a clock: `stat` is injected, so every
//! property here is a plain assertion. The module is compiled by path, which is why it spells the
//! crate as `girder_core::`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/workspace/watch.rs"]
pub mod workspace_watch_gap;

use std::path::Path;

use girder_core::FsType;
use workspace_watch_gap::{
    Change, Changes, DEFAULT_DIRTY_LIMIT, Frame, Generation, OUTSIDE_WORKSPACE, Reach, Relevance,
    TargetState, WATCH_UNSUPPORTED, WatchError, info, plan_watch,
};

/// A workspace root that exists on every platform this builds on (`/w` on POSIX).
fn root() -> &'static Path {
    Path::new("/w")
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "the same helpers serve both call shapes: `Option<&FsInfo>` for the planning gate and the drain function's `Option<FsInfo>` answer"
)]
fn dir() -> Option<girder_core::FsInfo> {
    Some(info("d:1", FsType::Directory))
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "same as `dir`: the helpers answer drains as well as the planning gate"
)]
fn file(version: &str) -> Option<girder_core::FsInfo> {
    Some(info(version, FsType::File))
}

// -------------------------------------------------------------------------------------------
// The gate: which targets may be watched at all.
// -------------------------------------------------------------------------------------------

#[test]
fn a_directory_inside_the_workspace_is_watchable() {
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    assert_eq!(plan.kind, FsType::Directory);
    assert!(plan.existed);
    assert_eq!(plan.absolute_path, "/w/src");
}

#[test]
fn a_directory_outside_the_workspace_is_refused() {
    // Upstream's `stat` closure throws only for a directory that escapes the root. This is the one
    // case in the whole `changes` contract that is a hard refusal rather than an empty stream.
    let error = plan_watch(Some(root()), "/etc", dir().as_ref()).unwrap_err();
    assert_eq!(
        error,
        WatchError::OutsideWorkspace {
            path: "/etc".into()
        }
    );
    assert_eq!(error.code(), OUTSIDE_WORKSPACE);
    assert_eq!(error.code(), "workspace-file/outside-workspace");
}

#[test]
fn an_absolute_file_outside_the_workspace_is_followed() {
    // The asymmetry is deliberate: following one file by absolute path is a supported mode, and a
    // check that refused it would break a client that opened a file by its Host path.
    let plan = plan_watch(Some(root()), "/etc/hosts", file("v:1").as_ref()).unwrap();
    assert_eq!(plan.kind, FsType::File);
    assert_eq!(plan.absolute_path, "/etc/hosts");
}

#[test]
fn an_absent_target_is_watchable() {
    // `None` from `stat` is the normal answer for a path that does not exist yet, and watching one
    // to learn when it appears is the reason the stream exists.
    let plan = plan_watch(Some(root()), "src/new.rs", None).unwrap();
    assert!(!plan.existed);
    assert_eq!(plan.kind, FsType::Other);
    assert_eq!(plan.absolute_path, "/w/src/new.rs");
}

#[test]
fn an_absent_target_outside_the_workspace_is_still_watchable() {
    // The type is what makes a tree dangerous, and an absent path has no type. Refusing here would
    // make "watch for this file to appear" impossible outside the root.
    let plan = plan_watch(Some(root()), "/elsewhere/x", None).unwrap();
    assert_eq!(plan.kind, FsType::Other);
}

#[test]
fn a_root_escape_through_dot_dot_is_refused() {
    // The containment check normalizes lexically, so `..` cannot be used to name a directory that
    // looks inside and is not.
    let error = plan_watch(Some(root()), "../etc", dir().as_ref()).unwrap_err();
    assert_eq!(error.code(), OUTSIDE_WORKSPACE);
}

#[test]
fn a_directory_with_no_root_is_refused() {
    // Upstream returns early when the workspace root does not resolve, so a directory target cannot
    // be proved inside anything and must not be watched.
    let error = plan_watch(None, "/w/src", dir().as_ref()).unwrap_err();
    assert_eq!(error.code(), OUTSIDE_WORKSPACE);
}

#[test]
fn the_unsupported_code_matches_upstream() {
    let error = WatchError::NotSupported {
        path: "src".into(),
        message: "no watcher".into(),
    };
    assert_eq!(error.code(), WATCH_UNSUPPORTED);
    assert_eq!(error.code(), "workspace-file/watch-unsupported");
}

#[test]
fn a_windows_path_normalizes_so_the_filter_can_compare_it() {
    // `\\` normalized to `/` is not cosmetic: a Host that reports `C:\\w\\src` against a configured
    // `C:/w/src` would otherwise filter out every change.
    let plan = plan_watch(Some(Path::new("C:/w")), "C:\\w\\src", dir().as_ref()).unwrap();
    assert_eq!(plan.absolute_path, "C:/w/src");
}

// -------------------------------------------------------------------------------------------
// Relevance: which events belong to the target.
// -------------------------------------------------------------------------------------------

#[test]
fn an_event_without_a_path_is_the_target() {
    // `fs.watch(target, cb)`'s callback carries no path at all. That is the only shape every
    // platform can deliver, so it must be the one that always works.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let changes = Changes::new(&plan, Reach::Target);
    assert_eq!(changes.classify(None), Relevance::SelfTarget);
}

#[test]
fn an_event_naming_the_target_is_the_target() {
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let changes = Changes::new(&plan, Reach::Recursive);
    assert_eq!(changes.classify(Some("/w/src")), Relevance::SelfTarget);
}

#[test]
fn a_direct_child_is_a_child_and_a_grandchild_is_not() {
    // `watch` observes "a directory's direct entries", so a grandchild is not this target's event.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let changes = Changes::new(&plan, Reach::Recursive);
    assert_eq!(changes.classify(Some("/w/src/a.rs")), Relevance::Child);
    assert_eq!(
        changes.classify(Some("/w/src/a/b.rs")),
        Relevance::Unrelated
    );
    assert_eq!(
        changes.classify(Some("/w/other/a.rs")),
        Relevance::Unrelated
    );
}

#[test]
fn a_file_target_has_no_children() {
    let plan = plan_watch(Some(root()), "src/a.rs", file("v:1").as_ref()).unwrap();
    let changes = Changes::new(&plan, Reach::Recursive);
    assert_eq!(changes.classify(Some("/w/src/a.rs")), Relevance::SelfTarget);
    assert_eq!(
        changes.classify(Some("/w/src/a.rs/x")),
        Relevance::Unrelated
    );
}

#[test]
fn a_child_event_dirties_the_target_not_the_child() {
    // The rule that makes a directory watch useful: the directory's own version does not move when
    // a child's metadata does, so a consumer waiting on the child would wait forever.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Target);
    assert_eq!(changes.observe(Some("/w/src/a.rs")), Relevance::Child);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes.target(), "/w/src");

    let flushed = changes.drain(|path| {
        assert_eq!(path, "/w/src", "the target is what gets restatted");
        dir()
    });
    assert_eq!(flushed.changes.len(), 1);
    assert_eq!(flushed.changes[0].absolute_path, "/w/src");
}

#[test]
fn a_recursive_notifier_reports_the_child_itself() {
    // A facility that can name the descendant is strictly more useful, so it is honoured — but it is
    // offered, never assumed.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Recursive);
    changes.observe(Some("/w/src/a.rs"));
    let flushed = changes.drain(|_| file("v:1"));
    assert_eq!(flushed.changes.len(), 1);
    assert_eq!(flushed.changes[0].absolute_path, "/w/src/a.rs");
}

// -------------------------------------------------------------------------------------------
// Coalescing and dedupe: the cost that actually matters is the stat.
// -------------------------------------------------------------------------------------------

#[test]
fn a_burst_on_one_path_costs_one_stat_and_one_frame() {
    let plan = plan_watch(Some(root()), "src/a.rs", file("v:1").as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Target);
    for _ in 0..500 {
        changes.observe(Some("/w/src/a.rs"));
    }
    assert_eq!(changes.len(), 1);

    let mut stats = 0;
    let flushed = changes.drain(|_| {
        stats += 1;
        file("v:2")
    });
    assert_eq!(stats, 1, "one stat per dirty path per drain, not per event");
    assert_eq!(flushed.changes.len(), 1);
    assert_eq!(
        flushed.changes[0].state().version().map(|v| v.0.as_str()),
        Some("v:2")
    );
}

#[test]
fn dirty_paths_are_reported_in_first_dirty_order() {
    // Order is the consumer's only signal about causality, and a `BTreeMap` would reorder it.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Recursive);
    changes.observe(Some("/w/src/z.rs"));
    changes.observe(Some("/w/src/a.rs"));
    changes.observe(Some("/w/src/z.rs"));
    let flushed = changes.drain(|_| file("v:1"));
    let paths: Vec<&str> = flushed
        .changes
        .iter()
        .map(|change| change.absolute_path.as_str())
        .collect();
    assert_eq!(paths, vec!["/w/src/z.rs", "/w/src/a.rs"]);
}

#[test]
fn an_unchanged_file_version_is_not_a_second_change() {
    // A `write` plus a `chmod` fires twice with the same content. Echoing the second frame would
    // re-render a file that did not change.
    let plan = plan_watch(Some(root()), "src/a.rs", file("v:1").as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Target);

    changes.observe(Some("/w/src/a.rs"));
    let first = changes.drain(|_| file("v:1"));
    assert_eq!(first.changes.len(), 1);

    changes.observe(Some("/w/src/a.rs"));
    let second = changes.drain(|_| file("v:1"));
    assert!(
        second.changes.is_empty(),
        "same version, same file: nothing to say"
    );
}

#[test]
fn a_changed_file_version_is_a_second_change() {
    let plan = plan_watch(Some(root()), "src/a.rs", file("v:1").as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Target);
    changes.observe(Some("/w/src/a.rs"));
    changes.drain(|_| file("v:1"));
    changes.observe(Some("/w/src/a.rs"));
    let second = changes.drain(|_| file("v:2"));
    assert_eq!(second.changes.len(), 1);
}

#[test]
fn an_unchanged_directory_version_is_still_a_change() {
    // The other half of the same rule. A directory's version does not move when a child's metadata
    // changes, so deduping it would hide the edit the watch exists to report.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Target);
    changes.observe(None);
    assert_eq!(changes.drain(|_| dir()).changes.len(), 1);
    changes.observe(None);
    assert_eq!(
        changes.drain(|_| dir()).changes.len(),
        1,
        "a directory consumer relists on every frame"
    );
}

#[test]
fn a_target_that_disappears_reports_absence() {
    // `{absolutePath, absent: true}` rather than a stale version: a client that kept the last
    // version would render a file that is gone.
    let plan = plan_watch(Some(root()), "src/a.rs", file("v:1").as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Target);
    changes.observe(None);
    let flushed = changes.drain(|_| None);
    assert_eq!(flushed.changes.len(), 1);
    assert!(flushed.changes[0].is_absent());
    assert_eq!(*flushed.changes[0].state(), TargetState::Absent);
}

#[test]
fn repeated_absence_is_reported_once_for_a_file() {
    let plan = plan_watch(Some(root()), "src/a.rs", file("v:1").as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Target);
    changes.observe(None);
    assert_eq!(changes.drain(|_| None).changes.len(), 1);
    changes.observe(None);
    assert!(changes.drain(|_| None).changes.is_empty());
}

#[test]
fn an_unrelated_event_dirties_nothing() {
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Target);
    assert_eq!(changes.observe(Some("/w/other/x")), Relevance::Unrelated);
    assert!(changes.is_empty());
    let flushed = changes.drain(|_| panic!("an unrelated event must not cost a stat"));
    assert!(flushed.is_empty());
}

// -------------------------------------------------------------------------------------------
// Overflow: bounded memory, truthful answer.
// -------------------------------------------------------------------------------------------

#[test]
fn a_queue_past_its_bound_announces_a_resync() {
    // A stalled consumer must not be able to grow the queue without bound, and must not be told a
    // partial dirty set is the whole truth.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut changes = Changes::with_limit(&plan, Reach::Recursive, 4);
    for index in 0..10 {
        changes.observe(Some(&format!("/w/src/{index}.rs")));
    }
    assert!(changes.overflowed());

    let flushed = changes.drain(|_| file("v:1"));
    assert!(flushed.resync, "the consumer is told to distrust its state");
    assert!(
        flushed.changes.is_empty(),
        "the dropped detail is not invented back"
    );

    // The flag is one-shot: the next drain is an ordinary one.
    assert!(!changes.overflowed());
    assert!(!changes.drain(|_| file("v:1")).resync);
}

#[test]
fn the_default_bound_is_the_advertised_one() {
    // The constant is part of the module's contract, so a test pins it rather than trusting a comment.
    assert_eq!(DEFAULT_DIRTY_LIMIT, 4096);
}

#[test]
fn the_dedupe_memory_is_bounded_too() {
    // The delivered map is memory as well; a watch on a busy directory must not retain every path
    // it has ever seen.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut changes = Changes::with_limit(&plan, Reach::Recursive, 2);
    for index in 0..5 {
        changes.observe(Some(&format!("/w/src/{index}.rs")));
        let _ = changes.drain(|_| file("v:1"));
    }
    // Evicting the oldest only risks one redundant frame, which is the harmless direction.
    changes.observe(Some("/w/src/4.rs"));
    let flushed = changes.drain(|_| file("v:1"));
    assert!(flushed.changes.len() <= 1);
}

// -------------------------------------------------------------------------------------------
// The generation: the ready handshake and the end.
// -------------------------------------------------------------------------------------------

#[test]
fn ready_is_sent_exactly_once() {
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut generation = Generation::open(plan, Reach::Target);
    assert_eq!(generation.ready(), Some(Frame::Ready));
    assert_eq!(
        generation.ready(),
        None,
        "a second ready would reset a client's state"
    );
    assert!(generation.is_started());
}

#[test]
fn a_generation_queues_before_ready_and_still_reports_it() {
    // Events can arrive before the notifier acknowledges; upstream queues them for exactly this
    // reason, and dropping them would lose the change that made the client subscribe.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut generation = Generation::open(plan, Reach::Target);
    generation.observe(Some("/w/src/a.rs"));
    assert_eq!(generation.ready(), Some(Frame::Ready));
    let flushed = generation.drain(|_| file("v:1"));
    assert_eq!(flushed.changes.len(), 1);
}

#[test]
fn nothing_is_delivered_after_the_generation_ends() {
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut generation = Generation::open(plan, Reach::Target);
    generation.observe(Some("/w/src/a.rs"));
    generation.end();
    assert!(generation.is_ended());
    assert_eq!(
        generation.observe(Some("/w/src/a.rs")),
        Relevance::Unrelated
    );
    let flushed = generation.drain(|_| file("v:1"));
    assert!(
        flushed.is_empty(),
        "a teardown in flight must not emit frames"
    );
}

#[test]
fn a_frame_carries_the_wire_shape_upstream_sends() {
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut generation = Generation::open(plan, Reach::Target);
    assert_eq!(
        serde_json::to_value(generation.ready().unwrap()).unwrap(),
        serde_json::json!({ "kind": "ready" })
    );

    generation.observe(None);
    let frames = generation.drain(|_| file("v:7")).frames();
    assert_eq!(
        serde_json::to_value(&frames[0]).unwrap(),
        serde_json::json!({
            "kind": "change",
            "change": { "absolutePath": "/w/src", "version": "v:7" },
        })
    );
}

#[test]
fn an_absent_change_serializes_as_absent_true() {
    // Upstream's union is `version` XOR `absent: true`, not a tagged variant with a null version.
    let change = Change::absent("/w/src/a.rs");
    assert_eq!(
        serde_json::to_value(&change).unwrap(),
        serde_json::json!({ "absolutePath": "/w/src/a.rs", "absent": true })
    );
}

#[test]
fn a_generation_exposes_the_plan_it_actually_watches() {
    // The plan is what every frame's path is built from, so a caller must be able to read the
    // normalized form rather than reconstruct it from the request.
    let plan = plan_watch(Some(Path::new("C:/w")), "C:\\w\\src", dir().as_ref()).unwrap();
    let generation = Generation::open(plan, Reach::Target);
    assert_eq!(generation.plan().absolute_path, "C:/w/src");
    // The execution-world path keeps the host's spelling: it is opened, not compared.
    assert_eq!(generation.plan().process_path, "C:\\w\\src");
}

#[test]
fn a_generation_exposes_its_pending_dirty_set() {
    // Coalescing is observable before the stat, which is what lets a caller see a burst collapse
    // rather than infer it from a frame count after the fact.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut generation = Generation::open(plan, Reach::Target);
    generation.observe(None);
    generation.observe(Some("/w/src/a.rs"));
    assert_eq!(generation.changes().len(), 1);
    assert_eq!(generation.changes().target(), "/w/src");
}

#[test]
fn a_ready_frame_carries_no_change() {
    // `Frame::change` is the accessor a consumer uses to skip the handshake without matching.
    assert!(Frame::Ready.change().is_none());
    let frame = Frame::Change {
        change: Change::absent("/w/src/a.rs"),
    };
    assert!(frame.change().is_some_and(Change::is_absent));
}

#[test]
fn a_present_state_says_whether_it_is_a_directory() {
    // `TargetState::is_directory` is how a consumer picks relist over re-read without parsing a
    // version it is told never to parse.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Target);
    changes.observe(None);
    let flushed = changes.drain(|_| dir());
    assert!(flushed.changes[0].state().is_directory());
    assert!(!TargetState::Absent.is_directory());
}

#[test]
fn a_wire_change_round_trips() {
    let present = Change::present(
        "/w/src/a.rs",
        girder_core::FsVersion("v:1".into()),
        FsType::File,
    );
    let encoded = serde_json::to_value(&present).unwrap();
    let decoded: Change = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded.absolute_path, "/w/src/a.rs");
    assert_eq!(decoded.state().version().map(|v| v.0.as_str()), Some("v:1"));
    assert!(!decoded.is_absent());

    let absent = Change::absent("/w/src/a.rs");
    let decoded: Change = serde_json::from_value(serde_json::to_value(&absent).unwrap()).unwrap();
    assert!(decoded.is_absent());
}

#[test]
fn a_change_carrying_both_shapes_is_rejected() {
    // A payload with both is a producer bug, and guessing which half it meant hides it.
    let error = serde_json::from_value::<Change>(serde_json::json!({
        "absolutePath": "/w/src/a.rs",
        "version": "v:1",
        "absent": true,
    }))
    .unwrap_err();
    assert!(error.to_string().contains("not both"), "{error}");
}

#[test]
fn a_change_carrying_neither_shape_is_rejected() {
    let error =
        serde_json::from_value::<Change>(serde_json::json!({ "absolutePath": "/w/src/a.rs" }))
            .unwrap_err();
    assert!(error.to_string().contains("carries"), "{error}");
}

#[test]
fn a_flushed_drain_reports_emptiness_honestly() {
    // `is_empty` folds the resync flag in: a resync with no changes is not nothing to send.
    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut changes = Changes::with_limit(&plan, Reach::Recursive, 1);
    changes.observe(Some("/w/src/a.rs"));
    changes.observe(Some("/w/src/b.rs"));
    let flushed = changes.drain(|_| file("v:1"));
    assert!(!flushed.is_empty(), "a resync is still something to send");
    assert!(flushed.frames().is_empty());

    let plan = plan_watch(Some(root()), "src", dir().as_ref()).unwrap();
    let mut changes = Changes::new(&plan, Reach::Target);
    assert!(changes.drain(|_| file("v:1")).is_empty());
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod workspace {
    pub use super::workspace_watch_gap as watch;
}
