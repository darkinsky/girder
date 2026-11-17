//! Tests for `api_controller_gap`: the non-streaming client logic of
//! `packages/api/{job-controller,workspace-files,terminal-controller}`.
//!
//! The properties pinned here are the ones upstream's `observe.ts`, `rows.ts`, `client/model.ts`,
//! `client/service.ts`, `client/inventory.ts`, `client/change-feed.ts` and `client/roster.ts` rely
//! on, restated as behaviour:
//!
//! - **A frame's cursor is its own last chunk.** A budgeted frame that reported the *read's* next
//!   offset would let the client skip bytes it never received, and the next poll would start past
//!   them.
//! - **Loss is announced once.** `lossy` rides the first frame a lossy read produces and no later
//!   one, and a lossy read with no chunks still produces a frame — otherwise the client keeps a
//!   position it can no longer trust.
//! - **The render tail never splits a surrogate pair.** The bound is UTF-16 code units because that
//!   is what a DOM string is measured in.
//! - **The roster wakes on lifecycle, not on output.** A settled projection already carries the
//!   final byte count.
//! - **A release binds the entry it was minted for.** A stale release must not tear down the
//!   successor that re-acquired the key.
//! - **A `list` response never resurrects a mutated row.** In-flight `update`/`remove` win, and a
//!   whole roster that arrived while loading wins over rows it does not list.
//! - **A follower filters at pull time, not push time.** Changes that arrive before `stat` binds the
//!   Host path are judged against the path bound afterwards.
//!
//! # Why the module is compiled by path
//!
//! The crate root's wiring is a separate change, so this compiles the exact source file rather than
//! a copy. Inside a `#[path]` module the `crate::` prefix would name *this* test crate, not
//! `girder-core`, so every reference to the library goes through `girder_core::{...}` — including
//! the ones the module under test makes, which is why the module itself spells them that way.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/api_controller_gap.rs"]
mod api_controller_gap;

use api_controller_gap::{
    Acquired, Applied, DEFAULT_OBSERVE_FLUSH_MS, DEFAULT_OBSERVE_MAX_FRAME_BYTES, Follower,
    Inventory, JobEvent, JobEventKind, JobOutput, JobView, JobsModel, Notice, OffsetError, Opened,
    OutputFrame, RENDER_TAIL_LIMIT, Read, Released, RosterBook, StreamBook, TerminalInfo, Watched,
    chunk_bytes, feed_key, is_terminal, path_key, plan_output_frames, utf16_len, utf16_tail,
    validate_resume_offset, visible_roster, wake_roster,
};
use girder_core::{JobChannel, JobChunk, JobStatus, MAX_SAFE_INTEGER};

/// One chunk at an absolute offset, on stdout, with no inherited gap mark.
fn chunk(at: u64, text: &str) -> JobChunk {
    JobChunk {
        at,
        text: text.to_owned(),
        channel: JobChannel::Stdout,
        gap: false,
    }
}

/// Three chunks of four bytes each, from offset zero.
fn three_chunks() -> Vec<JobChunk> {
    vec![chunk(0, "aaaa"), chunk(4, "bbbb"), chunk(8, "cccc")]
}

fn view(id: &str, owner: Option<&str>, status: JobStatus) -> JobView {
    JobView {
        id: id.to_owned(),
        kind: "bash".to_owned(),
        label: format!("job {id}"),
        owner: owner.map(str::to_owned),
        status,
        output: JobOutput {
            total: 0,
            earliest: 0,
        },
    }
}

fn opened(id: &str, from: u64) -> Opened {
    Opened {
        job: view(id, Some("s1"), JobStatus::Running),
        from,
    }
}

fn terminal(id: &str) -> TerminalInfo {
    TerminalInfo::new(id, format!("title {id}"), false)
}

// -------------------------------------------------------------------------------------------
// Offsets and framing
// -------------------------------------------------------------------------------------------

#[test]
fn a_resume_offset_must_be_a_safe_integer() {
    assert_eq!(validate_resume_offset(None), Ok(None));
    assert_eq!(validate_resume_offset(Some(0)), Ok(Some(0)));
    assert_eq!(
        validate_resume_offset(Some(MAX_SAFE_INTEGER)),
        Ok(Some(MAX_SAFE_INTEGER))
    );
    assert_eq!(
        validate_resume_offset(Some(MAX_SAFE_INTEGER + 1)),
        Err(OffsetError::Unsafe {
            from: MAX_SAFE_INTEGER + 1
        })
    );
}

#[test]
fn the_refusal_names_the_offset_it_refused() {
    // A bare "invalid offset" leaves a caller unable to tell a corrupt resume token from a client
    // bug, so the number itself is in the message.
    let error = validate_resume_offset(Some(1 << 60)).unwrap_err();
    assert!(error.to_string().contains("1152921504606846976"));
}

#[test]
fn stopping_is_not_terminal_but_the_three_settled_states_are() {
    assert!(!is_terminal(JobStatus::Running));
    // A terminal frame at `stopping` closes the stream a beat before the last line arrives.
    assert!(!is_terminal(JobStatus::Stopping));
    assert!(is_terminal(JobStatus::Completed));
    assert!(is_terminal(JobStatus::Killed));
    assert!(is_terminal(JobStatus::Failed));
}

#[test]
fn an_unbudgeted_read_is_one_frame_carrying_the_reads_next() {
    let frames = plan_output_frames(&three_chunks(), 12, false, DEFAULT_OBSERVE_MAX_FRAME_BYTES);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].chunks.len(), 3);
    assert_eq!(frames[0].next, 12);
    assert!(!frames[0].lossy);
}

#[test]
fn a_budgeted_frame_reports_its_own_last_chunk_not_the_reads_next() {
    // The read's `next` is 12, but the first frame only carries two chunks and so ends at 8. A
    // frame that reported 12 would make the client skip `cccc` forever.
    let frames = plan_output_frames(&three_chunks(), 12, false, 8);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].next, 8);
    assert_eq!(
        frames[0]
            .chunks
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>(),
        ["aaaa", "bbbb"]
    );
    assert_eq!(frames[1].next, 12);
    assert_eq!(frames[1].chunks.len(), 1);
}

#[test]
fn one_chunk_larger_than_the_budget_ships_whole() {
    // Retention's cap is the hard bound on a chunk; splitting inside one would cut a UTF-8
    // sequence, so the soft budget yields to it.
    let chunks = vec![chunk(0, &"x".repeat(4096))];
    let frames = plan_output_frames(&chunks, 4096, false, 64);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].chunks[0].text.len(), 4096);
    assert_eq!(frames[0].next, 4096);
}

#[test]
fn loss_rides_the_first_frame_only() {
    let frames = plan_output_frames(&three_chunks(), 12, true, 8);
    assert_eq!(frames.len(), 2);
    assert!(frames[0].lossy, "the first frame announces the loss");
    assert!(
        !frames[1].lossy,
        "repeating it would say the position kept breaking"
    );
}

#[test]
fn a_lossy_read_with_no_chunks_still_yields_a_frame() {
    // Everything was evicted. Returning nothing would leave the client at a position it can no
    // longer read, with nothing telling it so.
    let frames = plan_output_frames(&[], 99, true, DEFAULT_OBSERVE_MAX_FRAME_BYTES);
    assert_eq!(frames.len(), 1);
    assert!(frames[0].chunks.is_empty());
    assert!(frames[0].lossy);
    assert_eq!(frames[0].next, 99);
}

#[test]
fn a_clean_read_with_no_chunks_yields_nothing() {
    assert!(plan_output_frames(&[], 99, false, 1024).is_empty());
}

#[test]
fn a_zero_budget_flushes_every_chunk() {
    // Unreachable through the service (its config floor is 1) but defined rather than panicking.
    let frames = plan_output_frames(&three_chunks(), 12, false, 0);
    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].next, 4);
    assert_eq!(frames[2].next, 12);
}

#[test]
fn frame_offsets_are_byte_offsets_not_char_counts() {
    let chunk = chunk(0, "héllo");
    assert_eq!(chunk_bytes(&chunk), 6, "é is two UTF-8 bytes");
    let frames = plan_output_frames(&[chunk], 6, false, 1);
    assert_eq!(frames[0].next, 6);
}

// -------------------------------------------------------------------------------------------
// Roster predicate and visible set
// -------------------------------------------------------------------------------------------

#[test]
fn output_never_wakes_the_roster() {
    let event = JobEvent {
        kind: JobEventKind::Output,
        job_id: "j1".to_owned(),
        owner: Some("s1".to_owned()),
    };
    assert!(!wake_roster(&event, "s1"));
    // Not even for an unowned job: a settled projection already carries the final byte count, so a
    // frame per append would be a frame per line of a build log.
    let unowned = JobEvent {
        owner: None,
        ..event
    };
    assert!(!wake_roster(&unowned, "s1"));
}

#[test]
fn every_lifecycle_commit_wakes_the_roster_for_a_visible_job() {
    for kind in [
        JobEventKind::Registered,
        JobEventKind::Progress,
        JobEventKind::Stopping,
        JobEventKind::Settled,
        JobEventKind::Removed,
    ] {
        let mine = JobEvent {
            kind,
            job_id: "j1".to_owned(),
            owner: Some("s1".to_owned()),
        };
        assert!(wake_roster(&mine, "s1"), "{kind:?} on my own job");
        // An unowned job is visible to every session, so every session's roster must refresh.
        let unowned = JobEvent {
            owner: None,
            ..mine.clone()
        };
        assert!(wake_roster(&unowned, "s1"), "{kind:?} on an unowned job");
        // Another session's job is not in this session's frame; waking would emit a frame that
        // differs from its predecessor in nothing.
        let theirs = JobEvent {
            owner: Some("s2".to_owned()),
            ..mine
        };
        assert!(
            !wake_roster(&theirs, "s1"),
            "{kind:?} on another session's job"
        );
    }
}

#[test]
fn the_visible_set_is_my_jobs_plus_every_unowned_one_in_id_order() {
    let jobs = vec![
        view("j3", Some("s2"), JobStatus::Running),
        view("j1", Some("s1"), JobStatus::Running),
        view("j0", None, JobStatus::Running),
        view("j2", None, JobStatus::Completed),
    ];
    let mine = visible_roster(&jobs, "s1");
    let ids: Vec<&str> = mine.iter().map(|job| job.id.as_str()).collect();
    assert_eq!(ids, ["j0", "j1", "j2"]);
    // A different session sees a different set from the same registry.
    let theirs = visible_roster(&jobs, "s2");
    let ids: Vec<&str> = theirs.iter().map(|job| job.id.as_str()).collect();
    assert_eq!(ids, ["j0", "j2", "j3"]);
}

// -------------------------------------------------------------------------------------------
// The render tail
// -------------------------------------------------------------------------------------------

#[test]
fn a_tail_shorter_than_the_limit_is_unchanged() {
    assert_eq!(utf16_tail("abc", 8), "abc");
    assert_eq!(utf16_tail("", 8), "");
}

#[test]
fn a_tail_keeps_the_newest_code_units() {
    assert_eq!(utf16_tail("abcdef", 2), "ef");
    assert_eq!(utf16_tail("abcdef", 0), "");
}

#[test]
fn a_cut_never_splits_a_surrogate_pair() {
    // U+1F600 is one code point, two UTF-16 units. Cutting at a low surrogate would render a
    // replacement glyph, so the cut advances by one and the surviving tail is one unit short.
    let emoji = "\u{1F600}ab";
    assert_eq!(utf16_len(emoji), 4);
    assert_eq!(utf16_tail(emoji, 3), "ab");
    // The tail starts on a whole code point, never mid-pair.
    let tail = utf16_tail(emoji, 3);
    assert!(tail.is_char_boundary(0));
    assert_eq!(tail, "ab");
}

#[test]
fn a_surrogate_pair_is_not_dropped_when_the_cut_lands_on_its_high_half() {
    // "a" + emoji + "b" = 4 units; a cut at index 1 is a high surrogate, which is fine, and the
    // pair survives whole.
    let text = "a\u{1F600}b";
    assert_eq!(utf16_len(text), 4);
    assert_eq!(utf16_tail(text, 3), "\u{1F600}b");
}

#[test]
fn the_render_bound_counts_utf16_units_not_bytes() {
    assert_eq!(RENDER_TAIL_LIMIT, 128 * 1024);
    // A 1000-character CJK string is 1000 units and 3000 bytes; a byte bound would cut it early.
    let text = "漢".repeat(1000);
    assert_eq!(utf16_len(&text), 1000);
    assert_eq!(utf16_tail(&text, RENDER_TAIL_LIMIT).len(), text.len());
}

// -------------------------------------------------------------------------------------------
// The client model
// -------------------------------------------------------------------------------------------

#[test]
fn a_rows_frame_replaces_the_whole_set_and_an_empty_one_removes_the_key() {
    let mut model = JobsModel::new();
    assert!(model.rows_replaced("s1", vec![view("j1", Some("s1"), JobStatus::Running)]));
    assert_eq!(model.revision(), 1);

    // A repeated identical frame changes nothing, so a client store is not asked to re-render.
    assert!(!model.rows_replaced("s1", vec![view("j1", Some("s1"), JobStatus::Running)]));
    assert_eq!(model.revision(), 1);

    // Whole-set replacement: the second frame drops `j1` without any delta saying so.
    assert!(model.rows_replaced("s1", vec![view("j2", Some("s1"), JobStatus::Running)]));
    assert_eq!(model.snapshot().rows["s1"].len(), 1);
    assert_eq!(model.snapshot().rows["s1"][0].id, "j2");

    // An empty set is an absent key, so consumers read absence rather than a sentinel.
    assert!(model.rows_replaced("s1", Vec::new()));
    assert!(!model.snapshot().rows.contains_key("s1"));
    // And an empty frame over an absent key is a no-op.
    assert!(!model.rows_replaced("s1", Vec::new()));
}

#[test]
fn dropping_a_roster_clears_only_that_session() {
    let mut model = JobsModel::new();
    model.rows_replaced("s1", vec![view("j1", Some("s1"), JobStatus::Running)]);
    model.rows_replaced("s2", vec![view("j2", Some("s2"), JobStatus::Running)]);
    assert!(model.rows_dropped("s1"));
    assert!(!model.rows_dropped("s1"));
    let snapshot = model.snapshot();
    assert!(!snapshot.rows.contains_key("s1"));
    assert!(snapshot.rows.contains_key("s2"));
}

#[test]
fn an_anchor_installs_the_resume_cursor() {
    let mut model = JobsModel::new();
    model.observe_opened("j1", &opened("j1", 7));
    assert_eq!(model.cursor_of("j1"), Some(7));
    let observed = model.observed("j1").unwrap();
    assert!(observed.streaming);
    assert_eq!(observed.text, "");
    // Anchored at 7 with nothing accumulated: the head was evicted before anyone looked.
    assert!(observed.gap_before);
}

#[test]
fn a_fresh_anchor_at_zero_owes_no_gap() {
    let mut model = JobsModel::new();
    model.observe_opened("j1", &opened("j1", 0));
    assert!(!model.observed("j1").unwrap().gap_before);
}

#[test]
fn an_anchor_below_the_earliest_retained_byte_owes_a_gap() {
    let mut model = JobsModel::new();
    let mut anchor = opened("j1", 4);
    anchor.job.output = JobOutput {
        total: 40,
        earliest: 10,
    };
    model.observe_opened("j1", &anchor);
    let observed = model.observed("j1").unwrap();
    assert!(observed.gap_before);
    // The cursor is still what the caller asked for: the ring clamps, the client does not lie.
    assert_eq!(model.cursor_of("j1"), Some(4));
}

#[test]
fn a_resume_that_already_accumulated_text_keeps_its_recorded_gap() {
    let mut model = JobsModel::new();
    model.observe_opened("j1", &opened("j1", 0));
    assert!(!model.observed("j1").unwrap().gap_before);
    model.observe_output(
        "j1",
        &OutputFrame {
            chunks: vec![chunk(0, "line\n")],
            next: 5,
            lossy: false,
        },
    );
    // A reconnect anchors at 5 — past zero, but this view is not fresh, so the anchor alone must
    // not manufacture a gap the reader never suffered.
    model.observe_opened("j1", &opened("j1", 5));
    let observed = model.observed("j1").unwrap();
    assert_eq!(
        observed.text, "line\n",
        "accumulated text survives a reconnect"
    );
    assert!(!observed.gap_before);
}

#[test]
fn output_appends_and_moves_the_cursor_to_the_frames_next() {
    let mut model = JobsModel::new();
    model.observe_opened("j1", &opened("j1", 0));
    model.observe_output(
        "j1",
        &OutputFrame {
            chunks: vec![chunk(0, "one\n"), chunk(4, "two\n")],
            next: 8,
            lossy: false,
        },
    );
    assert_eq!(model.observed("j1").unwrap().text, "one\ntwo\n");
    assert_eq!(model.cursor_of("j1"), Some(8));
}

#[test]
fn a_lossy_frame_marks_the_gap() {
    let mut model = JobsModel::new();
    model.observe_opened("j1", &opened("j1", 0));
    model.observe_output(
        "j1",
        &OutputFrame {
            chunks: vec![chunk(10, "tail")],
            next: 14,
            lossy: true,
        },
    );
    assert!(model.observed("j1").unwrap().gap_before);
}

#[test]
fn a_chunks_own_gap_mark_propagates_to_the_view() {
    let mut model = JobsModel::new();
    model.observe_opened("j1", &opened("j1", 0));
    let mut marked = chunk(0, "after the hole");
    marked.gap = true;
    model.observe_output(
        "j1",
        &OutputFrame {
            chunks: vec![marked],
            next: 14,
            lossy: false,
        },
    );
    assert!(model.observed("j1").unwrap().gap_before);
}

#[test]
fn the_render_tail_is_bounded_and_marks_the_bound_as_a_gap() {
    let mut model = JobsModel::new();
    model.observe_opened("j1", &opened("j1", 0));
    assert!(!model.observed("j1").unwrap().gap_before);
    let big = "x".repeat(RENDER_TAIL_LIMIT + 100);
    model.observe_output(
        "j1",
        &OutputFrame {
            chunks: vec![chunk(0, &big)],
            next: u64::try_from(big.len()).unwrap(),
            lossy: false,
        },
    );
    let observed = model.observed("j1").unwrap();
    assert_eq!(utf16_len(&observed.text), RENDER_TAIL_LIMIT);
    // The bound dropped the head, and a client that kept `gap_before` false would render the
    // remainder as if it were the whole log.
    assert!(observed.gap_before);
}

#[test]
fn an_output_frame_for_an_untracked_job_is_ignored() {
    let mut model = JobsModel::new();
    model.observe_output(
        "ghost",
        &OutputFrame {
            chunks: vec![chunk(0, "x")],
            next: 1,
            lossy: false,
        },
    );
    assert!(model.observed("ghost").is_none());
    assert_eq!(
        model.revision(),
        0,
        "nothing changed, so nothing was announced"
    );
}

#[test]
fn settlement_stops_streaming_but_keeps_the_text() {
    let mut model = JobsModel::new();
    model.observe_opened("j1", &opened("j1", 0));
    model.observe_output(
        "j1",
        &OutputFrame {
            chunks: vec![chunk(0, "final\n")],
            next: 6,
            lossy: false,
        },
    );
    model.observe_settled("j1");
    let observed = model.observed("j1").unwrap();
    assert!(!observed.streaming);
    assert_eq!(
        observed.text, "final\n",
        "a panel that blanked would lose what was read"
    );
    assert_eq!(observed.error, None);
}

#[test]
fn a_failure_before_the_anchor_still_creates_a_view() {
    let mut model = JobsModel::new();
    model.observe_failed("j9", "job/not-found");
    let observed = model.observed("j9").unwrap();
    assert_eq!(observed.error.as_deref(), Some("job/not-found"));
    assert!(!observed.streaming);
    assert!(observed.text.is_empty());
    // No cursor: a later successful anchor resumes from nothing rather than from a guessed offset.
    assert_eq!(model.cursor_of("j9"), None);
}

#[test]
fn a_failure_after_the_anchor_keeps_the_cursor_and_the_text() {
    let mut model = JobsModel::new();
    model.observe_opened("j1", &opened("j1", 3));
    model.observe_output(
        "j1",
        &OutputFrame {
            chunks: vec![chunk(3, "abc")],
            next: 6,
            lossy: false,
        },
    );
    model.observe_failed("j1", "transport closed");
    let observed = model.observed("j1").unwrap();
    assert!(!observed.streaming);
    assert_eq!(observed.text, "abc");
    assert_eq!(observed.error.as_deref(), Some("transport closed"));
    assert_eq!(
        model.cursor_of("j1"),
        Some(6),
        "a reconnect resumes where it stopped"
    );
}

#[test]
fn stopping_an_observation_drops_its_state() {
    let mut model = JobsModel::new();
    model.observe_opened("j1", &opened("j1", 0));
    assert!(model.observe_stopped("j1"));
    assert!(!model.observe_stopped("j1"));
    assert!(model.observed("j1").is_none());
    assert_eq!(model.cursor_of("j1"), None);
}

#[test]
fn a_reopened_generation_clears_a_previous_error() {
    let mut model = JobsModel::new();
    model.observe_failed("j1", "gone");
    model.observe_opened("j1", &opened("j1", 0));
    let observed = model.observed("j1").unwrap();
    assert_eq!(observed.error, None);
    assert!(observed.streaming);
}

// -------------------------------------------------------------------------------------------
// Reference-counted streams
// -------------------------------------------------------------------------------------------

#[test]
fn two_watchers_share_one_entry_and_the_first_release_keeps_it_open() {
    let mut book = StreamBook::new();
    let first = book.acquire("s1");
    assert!(first.started);
    assert_eq!(first.refs, 1);
    let second = book.acquire("s1");
    assert!(!second.started, "the second watcher joins the live stream");
    assert_eq!(second.refs, 2);
    assert_eq!(second.lease, first.lease, "one entry, one lease identity");
    assert_eq!(book.refs_of("s1"), 2);
    assert_eq!(first.lease.entry(), second.lease.entry());

    assert_eq!(
        book.release(first.lease),
        Released::RefCounted { remaining: 1 }
    );
    assert!(book.is_open("s1"));
    assert_eq!(
        book.release(second.lease),
        Released::Disposed {
            cleared_state: true
        }
    );
    assert!(!book.is_open("s1"));
}

#[test]
fn different_keys_get_different_entries() {
    let mut book = StreamBook::new();
    let rows = book.acquire("s1");
    let observation = book.acquire("j1");
    assert_ne!(rows.lease, observation.lease);
    assert_eq!(book.keys(), ["j1", "s1"]);
}

#[test]
fn a_stale_release_does_not_tear_down_its_successor() {
    // The bug this pins: two acquires of the same key, the first stopping, a third acquiring, and
    // then the *first's* release arriving late. Releasing through the key alone would dispose the
    // third's stream and leave it with a subscription that never delivers.
    let mut book = StreamBook::new();
    let first = book.acquire("s1");
    book.release(first.lease);
    let third = book.acquire("s1");
    assert!(third.started, "the stopped entry is not joined");
    assert_ne!(third.lease, first.lease);

    assert_eq!(book.release(first.lease), Released::Stale);
    assert!(book.is_open("s1"), "the successor's stream survives");
    assert_eq!(book.refs_of("s1"), 1);
}

#[test]
fn a_release_that_races_a_successor_reports_the_state_must_survive() {
    let mut book = StreamBook::new();
    let first = book.acquire("s1");
    let release = book.release(first.lease);
    assert_eq!(
        release,
        Released::Disposed {
            cleared_state: true
        }
    );
    // Re-acquire before the caller's dispose round-trip finished: the entry is back, so a clear
    // now would blank a refilled model for good.
    let again = book.acquire("s1");
    assert!(again.started);
    assert!(book.is_open("s1"));
}

#[test]
fn a_stopped_entry_is_not_joined_even_before_its_release() {
    let mut book = StreamBook::new();
    let first = book.acquire("s1");
    // A successor acquire arrives while the first entry is still tracked but already released.
    book.release(first.lease);
    let second: Acquired = book.acquire("s1");
    assert!(second.started);
    assert_eq!(second.refs, 1);
}

#[test]
fn disposing_the_book_stops_everything() {
    let mut book = StreamBook::new();
    book.acquire("s1");
    book.acquire("j1");
    let mut keys = book.dispose_all();
    keys.sort();
    assert_eq!(keys, ["j1", "s1"]);
    assert!(book.keys().is_empty());
    // A lease minted before the teardown is stale rather than resurrecting the stream.
    let stale = BookStale::lease();
    assert_eq!(book.release(stale), Released::Stale);
}

/// Builds a lease that no live book knows, without exposing `Lease`'s private field.
struct BookStale;

impl BookStale {
    fn lease() -> api_controller_gap::Lease {
        let mut scratch = StreamBook::new();
        let acquired = scratch.acquire("scratch");
        scratch.dispose_all();
        acquired.lease
    }
}

// -------------------------------------------------------------------------------------------
// Terminal inventory
// -------------------------------------------------------------------------------------------

#[test]
fn the_first_read_starts_a_request_and_a_second_joins_it() {
    let mut inventory = Inventory::new();
    let Read::Start { request } = inventory.read(false) else {
        panic!("the first read must start a request");
    };
    assert!(inventory.is_loading());
    assert_eq!(inventory.read(false), Read::Pending { request });
    // An explicit refresh while loading joins rather than starting a second request.
    assert_eq!(inventory.read(true), Read::Pending { request });
}

#[test]
fn a_settled_list_is_cached_until_an_explicit_refresh() {
    let mut inventory = Inventory::new();
    let Read::Start { request } = inventory.read(false) else {
        panic!("expected a request");
    };
    assert_eq!(
        inventory.complete(request, vec![terminal("t1")]),
        Applied::Applied
    );
    assert_eq!(inventory.read(false), Read::Cached);
    assert!(matches!(inventory.read(true), Read::Start { .. }));
}

#[test]
fn a_failure_is_retained_until_a_refresh() {
    let mut inventory = Inventory::new();
    let Read::Start { request } = inventory.read(false) else {
        panic!("expected a request");
    };
    assert_eq!(
        inventory.fail(request, "host unreachable"),
        Applied::Applied
    );
    // Every pane that mounts must not re-hammer a dead Host.
    assert_eq!(inventory.read(false), Read::Failed);
    assert_eq!(inventory.error(), Some("host unreachable"));
    assert!(matches!(inventory.read(true), Read::Start { .. }));
}

#[test]
fn a_response_that_lost_a_race_is_ignored() {
    let mut inventory = Inventory::new();
    let Read::Start { request: first } = inventory.read(false) else {
        panic!("expected a request");
    };
    inventory.fail(first, "boom");
    let Read::Start { request: second } = inventory.read(true) else {
        panic!("expected a refresh request");
    };
    assert_ne!(first.get(), second.get());
    // The first request's late success must not publish over the refresh in flight.
    assert_eq!(
        inventory.complete(first, vec![terminal("stale")]),
        Applied::Ignored
    );
    assert!(inventory.items().is_empty());
    assert_eq!(
        inventory.complete(second, vec![terminal("t1")]),
        Applied::Applied
    );
    assert_eq!(inventory.items().len(), 1);
}

#[test]
fn a_response_does_not_resurrect_a_row_that_was_removed_while_loading() {
    // The user closes a terminal while the list request is in flight. The response still names it.
    let mut inventory = Inventory::new();
    let Read::Start { request } = inventory.read(false) else {
        panic!("expected a request");
    };
    inventory.remove("t2");
    inventory.complete(request, vec![terminal("t1"), terminal("t2")]);
    assert!(inventory.items().contains_key("t1"));
    assert!(
        !inventory.items().contains_key("t2"),
        "the confirmed close wins over the stale list"
    );
}

#[test]
fn a_response_does_not_revert_a_row_that_was_updated_while_loading() {
    let mut inventory = Inventory::new();
    let Read::Start { request } = inventory.read(false) else {
        panic!("expected a request");
    };
    inventory.update(TerminalInfo::new("t1", "renamed", true));
    inventory.complete(request, vec![TerminalInfo::new("t1", "stale title", false)]);
    let info = &inventory.items()["t1"];
    assert_eq!(info.title, "renamed");
    assert!(info.attached);
}

#[test]
fn a_roster_that_arrived_while_loading_filters_the_response() {
    // A whole roster is authoritative about *membership*: a terminal it does not list is gone. A
    // mutation recorded after it still wins, which is why the filter spares `updates` keys.
    let mut inventory = Inventory::new();
    let Read::Start { request } = inventory.read(false) else {
        panic!("expected a request");
    };
    inventory.reconcile(vec![terminal("t1")]);
    inventory.update(TerminalInfo::new("t9", "just created", false));
    inventory.complete(
        request,
        vec![terminal("t1"), terminal("t2"), terminal("t9")],
    );
    assert!(inventory.items().contains_key("t1"));
    assert!(!inventory.items().contains_key("t2"), "not in the roster");
    assert!(
        inventory.items().contains_key("t9"),
        "created after the roster"
    );
}

#[test]
fn reconcile_drops_unlisted_terminals_and_updates_listed_ones() {
    let mut inventory = Inventory::new();
    let Read::Start { request } = inventory.read(false) else {
        panic!("expected a request");
    };
    inventory.complete(
        request,
        vec![
            TerminalInfo::new("t1", "one", false),
            TerminalInfo::new("t2", "two", false),
        ],
    );
    inventory.reconcile(vec![TerminalInfo::new("t1", "one renamed", true)]);
    assert_eq!(inventory.items().len(), 1);
    assert_eq!(inventory.items()["t1"].title, "one renamed");
    assert!(inventory.items()["t1"].attached);
}

#[test]
fn a_reset_only_restarts_an_inventory_that_was_used() {
    let mut unused = Inventory::new();
    assert_eq!(
        unused.reset(),
        None,
        "an unused inventory must not start listing"
    );
    assert!(!unused.is_loading());

    let mut used = Inventory::new();
    let Read::Start { .. } = used.read(false) else {
        panic!("expected a request");
    };
    assert!(used.reset().is_some());
    assert!(used.is_loading());
}

#[test]
fn a_disposed_inventory_refuses_reads_and_late_results() {
    let mut inventory = Inventory::new();
    let Read::Start { request } = inventory.read(false) else {
        panic!("expected a request");
    };
    inventory.update(terminal("t1"));
    inventory.dispose();
    assert!(inventory.is_disposed());
    assert!(inventory.items().is_empty());
    assert_eq!(inventory.read(false), Read::Disposed);
    assert_eq!(inventory.read(true), Read::Disposed);
    // A late response must not publish, and a late mutation must not re-add.
    assert_eq!(
        inventory.complete(request, vec![terminal("t2")]),
        Applied::Ignored
    );
    inventory.update(terminal("t3"));
    inventory.remove("t3");
    inventory.reconcile(vec![terminal("t4")]);
    assert!(inventory.items().is_empty());
    assert_eq!(inventory.reset(), None);
}

// -------------------------------------------------------------------------------------------
// Change-feed followers
// -------------------------------------------------------------------------------------------

#[test]
fn a_path_key_normalizes_windows_separators() {
    assert_eq!(path_key(r"c:\work\a.txt"), "c:/work/a.txt");
    assert_eq!(path_key("/work/a.txt"), "/work/a.txt");
    // Without normalization a Windows Host path and a configured path never compare equal, and
    // every change is filtered out.
    assert_eq!(path_key(r"c:\work\a.txt"), path_key("c:/work/a.txt"));
}

#[test]
fn a_feed_key_is_two_components_so_a_separator_cannot_collide() {
    assert_ne!(feed_key("a", "b,c"), feed_key("a,b", "c"));
    assert_eq!(feed_key("s1", "/a"), r#"["s1","/a"]"#);
}

#[test]
fn a_refresh_is_always_visible() {
    let mut follower = Follower::new();
    // Nothing bound yet, and the notice is not about a path.
    follower.push(Notice::Refresh, "/a/one");
    follower.push(Notice::Refresh, "/a/two");
    assert_eq!(follower.drain(), [Notice::Refresh, Notice::Refresh]);
}

#[test]
fn an_unbound_follower_queues_and_then_filters_against_the_path_it_binds() {
    // The whole point: the follower is registered before `stat` supplies the Host path, so a
    // change arriving during the wait is judged afterwards, not dropped at push time.
    let mut follower = Follower::new();
    follower.push(Notice::Changed { version: 1 }, "/w/other.txt");
    follower.push(Notice::Changed { version: 2 }, "/w/target.txt");
    // After binding, only the target's change survives; the other was for a different file that
    // shares the stream.
    follower.bind("/w/target.txt");
    assert_eq!(follower.drain(), [Notice::Changed { version: 2 }]);
}

#[test]
fn a_bound_follower_passes_matching_and_drops_other_paths() {
    let mut follower = Follower::new();
    follower.bind("/w/a.txt");
    follower.push(Notice::Changed { version: 3 }, "/w/a.txt");
    follower.push(Notice::Absent, "/w/b.txt");
    follower.push(Notice::Absent, "/w/a.txt");
    assert_eq!(
        follower.drain(),
        [Notice::Changed { version: 3 }, Notice::Absent]
    );
}

#[test]
fn a_follower_keeps_queue_order_across_kinds() {
    let mut follower = Follower::new();
    follower.bind("/w/a.txt");
    follower.push(Notice::Refresh, "");
    follower.push(Notice::Changed { version: 1 }, "/w/a.txt");
    follower.push(Notice::Absent, "/w/a.txt");
    assert_eq!(
        follower.drain(),
        [
            Notice::Refresh,
            Notice::Changed { version: 1 },
            Notice::Absent
        ]
    );
}

#[test]
fn draining_an_empty_follower_yields_nothing_and_keeps_it_open() {
    let mut follower = Follower::new();
    follower.start();
    follower.bind("/w/a.txt");
    assert!(follower.drain().is_empty());
    assert!(follower.is_started());
    assert!(!follower.is_ended());
    follower.end();
    assert!(follower.is_ended());
    assert!(follower.drain().is_empty());
}

#[test]
fn ending_a_follower_still_delivers_what_was_queued() {
    // Upstream's `end()` sets the flag and wakes; a queued notice is delivered before the iterator
    // returns. Dropping the backlog would lose the last change before a teardown.
    let mut follower = Follower::new();
    follower.bind("/w/a.txt");
    follower.push(Notice::Changed { version: 9 }, "/w/a.txt");
    follower.end();
    assert_eq!(follower.drain(), [Notice::Changed { version: 9 }]);
}

// -------------------------------------------------------------------------------------------
// Terminal rosters
// -------------------------------------------------------------------------------------------

#[test]
fn two_watchers_of_a_session_share_one_stream() {
    let mut book = RosterBook::new();
    let first = book.watch("s1");
    assert!(first.opened, "the first watcher opens the stream");
    let second = book.watch("s1");
    assert!(!second.opened, "the second joins it");
    assert_eq!(book.watchers_of("s1"), 2);
    assert_ne!(first.watch.listener(), second.watch.listener());
    assert_eq!(book.release(first.watch), Watched::Shared);
    assert_eq!(book.watchers_of("s1"), 1);
    assert_eq!(book.release(second.watch), Watched::Disposed);
    assert_eq!(book.watchers_of("s1"), 0);
    assert!(book.sessions().is_empty());
}

#[test]
fn a_late_watcher_is_replayed_the_latest_roster() {
    let mut book = RosterBook::new();
    let first = book.watch("s1");
    assert!(book.received("s1", vec![terminal("t1")]));
    let late = book.watch("s1");
    assert!(!late.opened);
    assert_eq!(
        late.replay,
        Some(vec![terminal("t1")]),
        "a listener added to a live stream first receives its latest roster"
    );
    assert_eq!(book.latest_of("s1"), Some(&[terminal("t1")][..]));
    book.release(first.watch);
}

#[test]
fn different_sessions_get_different_streams() {
    let mut book = RosterBook::new();
    let one = book.watch("s1");
    let two = book.watch("s2");
    assert!(one.opened && two.opened);
    assert_eq!(book.sessions(), ["s1", "s2"]);
    assert_ne!(one.watch.session(), two.watch.session());
}

#[test]
fn a_failed_stream_keeps_its_watchers_and_reopens_on_resume() {
    let mut book = RosterBook::new();
    let first = book.watch("s1");
    assert!(book.stream_failed("s1"));
    // The watcher never released, so its entry must survive; dropping it would silently stop
    // delivering rosters to a listener that is still on screen.
    assert_eq!(book.watchers_of("s1"), 1);
    assert_eq!(book.resume(), ["s1"]);
    // A second resume has nothing to do: the stream is live again.
    assert!(book.resume().is_empty());
    // The next watch joins the reopened stream rather than opening a third.
    let second = book.watch("s1");
    assert!(!second.opened);
    book.release(first.watch);
    book.release(second.watch);
}

#[test]
fn a_new_watch_reopens_a_failed_stream_without_resume() {
    let mut book = RosterBook::new();
    let first = book.watch("s1");
    book.stream_failed("s1");
    let second = book.watch("s1");
    assert!(
        second.opened,
        "a watcher must not be handed a dead subscription"
    );
    assert!(book.resume().is_empty());
    book.release(first.watch);
    book.release(second.watch);
}

#[test]
fn a_released_watcher_cannot_be_released_twice() {
    let mut book = RosterBook::new();
    let only = book.watch("s1");
    assert_eq!(book.release(only.watch), Watched::Disposed);
    assert_eq!(book.release(only.watch), Watched::Stale);
}

#[test]
fn a_roster_for_an_unwatched_session_is_dropped() {
    let mut book = RosterBook::new();
    assert!(!book.received("s9", vec![terminal("t1")]));
    assert_eq!(book.latest_of("s9"), None);
    assert!(!book.stream_failed("s9"));
}

#[test]
fn disposing_the_roster_book_stops_every_session() {
    let mut book = RosterBook::new();
    let first = book.watch("s1");
    book.watch("s2");
    let mut sessions = book.dispose();
    sessions.sort();
    assert_eq!(sessions, ["s1", "s2"]);
    assert!(book.sessions().is_empty());
    assert_eq!(book.release(first.watch), Watched::Stale);
    assert!(book.resume().is_empty());
}

// -------------------------------------------------------------------------------------------
// The advertised defaults
// -------------------------------------------------------------------------------------------

#[test]
fn the_defaults_match_upstream() {
    assert_eq!(DEFAULT_OBSERVE_FLUSH_MS, 100);
    assert_eq!(DEFAULT_OBSERVE_MAX_FRAME_BYTES, 64 * 1024);
}
