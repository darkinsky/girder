//! End-to-end tests for the four artifacts this crate keeps on disk, driven across a *simulated
//! process restart*.
//!
//! Every other persistence test in this crate writes and reads through one store. That proves a
//! round trip inside one process, which is a weaker claim than the one the modules actually make:
//! `checkpoint_store`, `terminal_persist`, and `search_persist` all have read caches or in-memory
//! state in front of them somewhere, and `session::SessionStore` keeps a decoded copy of the log
//! keyed on its size. A read through the same handle can therefore be answered from memory while the
//! bytes on disk are wrong, and the test still passes.
//!
//! So each test below writes, **drops every object it wrote through**, constructs a new store (or a
//! new `DirectoryCatalog`) over the same directory, and then asserts that the recovered value equals
//! the value that was written — field by field, byte by byte, event by event. "It could still be
//! read" is not the property under test; "it is the same content" is.
//!
//! # What the layout being verified actually is
//!
//! Confirmed by reading `src/session_layout.rs` before writing these tests, because every path below
//! depends on it:
//!
//! - **sessions are flat.** `root/{id}.jsonl.zst` is the event log, and its presence is what makes an
//!   id a session. `{id}.title` and `{id}.archived` are sidecars in the same directory, not events.
//!   There is no per-session directory and no generation number in the name, unlike upstream.
//! - **checkpoints are one JSON file per session**, `{id}.checkpoints.json`, holding a sorted array.
//! - **terminal output is one plain-text file per terminal**, `{id}.log`.
//! - **the search cache is one file**, `.search-index.json`, sitting beside the logs it describes,
//!   with the log's *length* as the invalidation key.
//!
//! # Why the invalidation test mutates the log through a real `SessionStore`
//!
//! `search_persist::load_for` decides a record is still true by comparing the recorded size with a
//! `stat` of `{id}.jsonl.zst`. Appending garbage bytes would exercise the same branch, but it would
//! leave a log that cannot be rebuilt from — and the interesting half of the contract is that the
//! stale entry is not merely dropped, it is *rebuilt* from the log on the next query. So the test
//! appends a real event through a second `SessionStore`, which is exactly what another process
//! writing to the same session looks like from the restarting one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting, and \
              the indexes are over fixed-size literals defined in the test itself"
)]

use std::path::{Path, PathBuf};

use girder_core::{
    CheckpointKind, CheckpointStore, ContentBlock, DirectoryCatalog, SearchRecord, SessionCatalog,
    SessionEvent, SessionLayout, SessionStore, StoredCheckpoint, TerminalPersistStore, ToolCallId,
    TurnEndReason, load_search_index, read_session_file, save_search_index, search_index_path,
    searchable_text,
};

/// A phrase that exists only in the conversation written before the restart, so finding it after a
/// reload cannot be a coincidence of the fixture text.
const NEEDLE: &str = "needle-42";

// ---- fixtures ----------------------------------------------------------------------------------

/// A fresh, empty directory per test, under the temp directory.
///
/// The counter is what makes two tests in this file independent: the process id alone would let the
/// second run of one test observe the first run's leftover files, which is precisely the state that
/// would make a broken restart look like a working one.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-restart-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A conversation that exercises every field a restart could lose.
///
/// The mixture is deliberate rather than convenient: a plain user/assistant pair would round-trip
/// through almost any format, while a tool call id, a nested content block, an enum with data in it
/// (`TurnEndReason::Aborted`), a token count, and non-ASCII text are each a place where a serialiser
/// can drop or reshape something without failing.
fn sample_events() -> Vec<SessionEvent> {
    let call = ToolCallId::new("call-1").expect("a valid tool call id");
    vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::user_text("m1", format!("please remember the secret phrase: {NEEDLE}")),
        SessionEvent::assistant_text(
            "m2",
            format!("I will remember {NEEDLE} — héllo 世界 🌍"),
            "test-provider",
        ),
        SessionEvent::ModelUsage {
            model: "test-model".to_owned(),
            input_tokens: 1234,
            output_tokens: 56,
        },
        SessionEvent::ToolCall {
            id: call.clone(),
            name: "read".to_owned(),
            arguments: r#"{"path":"notes.txt"}"#.to_owned(),
        },
        SessionEvent::ToolResult {
            id: call,
            content: vec![
                ContentBlock::Text {
                    text: format!("the file mentions {NEEDLE}"),
                },
                ContentBlock::Reasoning {
                    text: "the command output is small".to_owned(),
                },
            ],
            is_error: false,
        },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Aborted {
                cause: "the user pressed ctrl-c".to_owned(),
            },
        },
    ]
}

/// Writes `events` as the session `id` under `root`, returning the log's path.
///
/// The path is built through [`SessionLayout`] rather than by formatting `{id}.jsonl.zst` here: the
/// layout is the module every other reader agrees with, and a test that respelt the name would keep
/// passing after the layout changed.
fn write_session(root: &Path, id: &str, events: &[SessionEvent]) -> PathBuf {
    let files = SessionLayout::new(root)
        .files(id)
        .expect("a usable session id");
    let store = SessionStore::new(&files.log);
    store.touch().expect("the log was created");
    for event in events {
        store.append(event).expect("the event was appended");
    }
    files.log
}

/// The searchable text of a whole conversation, in log order.
///
/// Built with the crate's own `searchable_text` because that is what the index stores: a fixture that
/// invented its own strings would not prove the persisted file holds what a query would search.
fn searchable_lines(events: &[SessionEvent]) -> Vec<String> {
    events.iter().map(searchable_text).collect()
}

// ---- 1. session restart ------------------------------------------------------------------------

#[tokio::test]
async fn a_session_written_before_a_restart_reads_back_event_for_event() {
    let dir = scratch("session");
    let id = "restart-session";
    let written = sample_events();
    let files = SessionLayout::new(&dir)
        .files(id)
        .expect("a usable session id");

    {
        let store = SessionStore::new(&files.log);
        store.touch().expect("the log was created");
        for event in &written {
            store.append(event).expect("the event was appended");
        }
        // Dropped explicitly: this store holds a decoded copy of the log keyed on the file's size, so
        // keeping it alive would let the read below be answered from memory instead of from disk.
    }

    // The bytes on disk are the first claim: a fresh store over the same path must decode to the same
    // events, which is what a restarting process does before it knows anything about the session.
    let reread = read_session_file(&files.log).expect("the log decodes");
    assert_eq!(
        reread, written,
        "the log on disk must hold the conversation that was written to it"
    );

    // A brand-new catalog has never seen this session, so anything it reports comes from the
    // directory. This is the read path a restarted harness serves `session.read` from.
    let catalog = DirectoryCatalog::new(&dir);
    let record = catalog
        .read(id)
        .await
        .expect("the restarted catalog finds the session it never created");

    assert_eq!(record.id, id, "the record is addressed by the id on disk");
    assert_eq!(
        record.events.len(),
        written.len(),
        "every event must survive, not merely the last one"
    );
    // One assertion per event with the index in the message: a whole-vector mismatch says *that*
    // something differs, while this says *which* event and in what role it came back, which is what
    // distinguishes a dropped `turn/end` from a reordered tool pair.
    for (index, (recovered, original)) in record.events.iter().zip(&written).enumerate() {
        assert_eq!(
            recovered,
            original,
            "event {index} came back as `{}` instead of `{}`",
            recovered.type_name(),
            original.type_name()
        );
    }
    assert_eq!(
        record.events, written,
        "the recovered conversation must equal the written one exactly"
    );
    // The user message is what makes a session not blank; a restart that lost it would render a
    // resumed conversation as an empty one in every list view.
    assert!(
        !record.blank,
        "a session holding a user message must not come back blank"
    );

    let listed = catalog.list().await;
    let ids: Vec<&str> = listed.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(ids, vec![id], "the restarted catalog lists the session");
    assert!(
        listed[0].events.is_empty(),
        "`list` deliberately omits events; the log read above is what carries them"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- 2. checkpoint restart ---------------------------------------------------------------------

#[test]
fn a_checkpoint_written_before_a_restart_is_still_the_latest_after_a_reload() {
    let dir = scratch("checkpoint");
    let session = "restart-checkpoint";
    let manual = StoredCheckpoint {
        session: session.to_owned(),
        seq: 3,
        kind: CheckpointKind::Manual,
        summary: None,
        at_ms: 1_700_000_000_003,
    };
    let compaction = StoredCheckpoint {
        session: session.to_owned(),
        seq: 2,
        kind: CheckpointKind::Compaction,
        // The summary of a dropped span exists nowhere else — the log no longer holds those
        // messages — so it is the field whose loss a restart could never repair.
        summary: Some("## Next Step\n- continue from the compacted span".to_owned()),
        at_ms: 1_700_000_000_002,
    };
    let automatic = StoredCheckpoint {
        session: session.to_owned(),
        seq: 1,
        kind: CheckpointKind::Automatic,
        summary: None,
        at_ms: 1_700_000_000_001,
    };

    {
        let store = CheckpointStore::new(&dir);
        // Written highest-seq-first, so a reader that trusted insertion order instead of sorting would
        // report `automatic` as the latest and send a rewind to the start of the conversation.
        store.save(manual.clone()).expect("the manual mark saved");
        store
            .save(automatic.clone())
            .expect("the automatic mark saved");
        store
            .save(compaction.clone())
            .expect("the compaction mark saved");
    }

    let reopened = CheckpointStore::new(&dir);

    // The reopened store must address the same file the writer did; a layout drift here would make a
    // restarted harness report "no checkpoints" while the file sat right beside it.
    let path = reopened.path_for(session).expect("a usable session id");
    assert!(
        path.ends_with(format!("{session}.checkpoints.json")),
        "the checkpoint file is `{{id}}.checkpoints.json`, got {}",
        path.display()
    );
    assert!(path.is_file(), "saving must have created the file on disk");

    let loaded = reopened.load(session).expect("the checkpoints were read");
    assert_eq!(
        loaded,
        vec![automatic.clone(), compaction.clone(), manual.clone()],
        "the reloaded file must hold every record, ordered by sequence"
    );

    let latest = reopened
        .latest(session)
        .expect("the store reopened")
        .expect("a session that was checkpointed has a latest point");
    // Field-by-field equality, not just the sequence: `kind` decides whether a pruning policy may
    // drop the record, and `at_ms` orders two marks taken at the same position.
    assert_eq!(
        latest, manual,
        "the latest point is the highest sequence, not the last one written"
    );
    assert_eq!(latest.kind, CheckpointKind::Manual);
    assert_eq!(
        latest.summary, None,
        "`None` and `Some(\"\")` are different statements and must not be conflated"
    );

    let compaction_reloaded = loaded
        .iter()
        .find(|checkpoint| checkpoint.seq == compaction.seq)
        .expect("the compaction mark survived");
    assert_eq!(
        compaction_reloaded.summary, compaction.summary,
        "the summary is the only copy of what a compaction removed, so it must round-trip exactly"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- 3. search index restart ----------------------------------------------------------------

#[test]
fn a_search_index_built_before_a_restart_still_finds_its_text_after_a_reload() {
    let dir = scratch("search");
    let id = "restart-search";
    let written = sample_events();
    let log = write_session(&dir, id, &written);
    let size = std::fs::metadata(&log)
        .expect("the log can be measured")
        .len();
    let lines = searchable_lines(&written);

    {
        // Built the way a real caller must: the index type exposes no iterator, so the records come
        // from the searchable text of each event, and the size comes from the log they came from.
        let records = vec![SearchRecord {
            session: id.to_owned(),
            size,
            lines: lines.clone(),
        }];
        save_search_index(&search_index_path(&dir), &records).expect("the index was saved");
    }

    let mut loaded = load_search_index(&search_index_path(&dir)).expect("the index was loaded");

    assert_eq!(
        loaded.len(),
        1,
        "a record whose log still has the recorded size must be kept"
    );
    // The text is what the file exists to avoid recomputing, so the assertion is on the strings — a
    // count of one would pass even if the lines came back empty.
    let cached = loaded
        .text_of(&dir, id)
        .expect("the loaded index answers without decoding the log")
        .to_vec();
    assert_eq!(
        cached, lines,
        "the persisted searchable text must be what the conversation produced"
    );

    let entry = loaded
        .get(id, size)
        .expect("the entry is present and valid for the log's current size");
    assert_eq!(entry.size, size, "the invalidation key round-trips");
    assert_eq!(entry.events, lines);
    // The point of persisting the text: a query for a phrase from the pre-restart conversation is
    // answered by the reloaded index, with no log decode in between.
    assert!(
        entry.events.iter().any(|line| line.contains(NEEDLE)),
        "the reloaded index must still find a phrase from before the restart"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- 4. terminal output restart -------------------------------------------------------------

#[test]
fn terminal_output_written_before_a_restart_reads_back_byte_for_byte() {
    let root = scratch("terminal").join("terminals");
    // Escape sequences, a trailing blank line, and multi-byte characters: a transcript is bytes a
    // user reads, so any normalisation (trimming, newline translation, lossy UTF-8) is a corruption
    // that a "did it load?" assertion cannot see.
    let transcript = "user@host:~$ ls\r\n\x1b[31mred\x1b[0m\nhéllo 世界 🌍\n\n";
    let other = "a different terminal's output";

    {
        let store = TerminalPersistStore::new(&root);
        let written = store
            .save("term-1", transcript)
            .expect("the transcript saved");
        assert_eq!(
            written,
            u64::try_from(transcript.len()).expect("the length fits in u64"),
            "`save` reports the bytes it wrote, which is the length a caller can verify"
        );
        store
            .save("term-2", other)
            .expect("the second transcript saved");
        // Dropped so the read below cannot be served by anything the writer still holds.
    }

    let reopened = TerminalPersistStore::new(&root);

    let loaded = reopened
        .load("term-1")
        .expect("the store reopened")
        .expect("a saved transcript is still there");
    assert_eq!(
        loaded.as_bytes(),
        transcript.as_bytes(),
        "the transcript must be identical at the byte level, including escapes and multi-byte text"
    );
    assert_eq!(loaded, transcript, "and identical as text");
    // Two transcripts in one directory, each read by id: a store that answered with "the file that is
    // there" would pass a single-terminal test and fail this one.
    assert_eq!(
        reopened.load("term-2").expect("read").as_deref(),
        Some(other),
        "each id must read its own transcript"
    );
    assert_eq!(
        reopened.list().expect("listed"),
        vec!["term-1".to_owned(), "term-2".to_owned()],
        "both terminals are listed after the restart, sorted"
    );

    let _ = std::fs::remove_dir_all(root.parent().expect("the scratch parent"));
}

// ---- 5. external modification invalidates the index ------------------------------------------

#[test]
fn an_index_entry_whose_log_changed_after_a_restart_is_dropped_and_rebuilt() {
    let dir = scratch("search-stale");
    let kept_events = vec![
        SessionEvent::user_text("k1", "the untouched session keeps its cached text"),
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];
    let stale_events = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::user_text("s1", format!("the stale session said {NEEDLE}")),
        SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ];

    let kept_log = write_session(&dir, "kept", &kept_events);
    let stale_log = write_session(&dir, "stale", &stale_events);
    let kept_size = std::fs::metadata(&kept_log).expect("stat").len();
    let stale_size = std::fs::metadata(&stale_log).expect("stat").len();
    let kept_lines = searchable_lines(&kept_events);
    let stale_lines = searchable_lines(&stale_events);

    save_search_index(
        &search_index_path(&dir),
        &[
            SearchRecord {
                session: "kept".to_owned(),
                size: kept_size,
                lines: kept_lines.clone(),
            },
            SearchRecord {
                session: "stale".to_owned(),
                size: stale_size,
                // Cloned because the tests below compare the rebuilt text against what was written;
                // the record itself has to own its copy.
                lines: stale_lines.clone(),
            },
        ],
    )
    .expect("the index was saved");

    // Another process appends a turn while this one is down. A session log is append-only, so the
    // file grows and every byte the index recorded now describes a prefix of the conversation.
    let appended = SessionEvent::user_text("s2", "a line written after the index was saved");
    SessionStore::new(&stale_log)
        .append(&appended)
        .expect("the log grew");
    let grown_size = std::fs::metadata(&stale_log).expect("stat").len();
    assert!(
        grown_size > stale_size,
        "the log must actually have changed, or the invalidation below proves nothing"
    );

    let mut loaded = load_search_index(&search_index_path(&dir)).expect("the index was loaded");

    assert_eq!(
        loaded.len(),
        1,
        "only the record whose log is unchanged may be reused"
    );
    // Both spellings are asserted: the entry must not be served under the old size, and it must not
    // have been silently re-keyed to the new one either — that would claim the cached text describes
    // events it never saw.
    assert!(
        loaded.get("stale", stale_size).is_none(),
        "a record whose log grew must not be served at its old size"
    );
    assert!(
        loaded.get("stale", grown_size).is_none(),
        "and must not be re-keyed to the new size"
    );
    let kept_entry = loaded
        .get("kept", kept_size)
        .expect("the untouched session is still cached");
    assert_eq!(kept_entry.events, kept_lines);

    // Dropping the stale entry is only half the contract: the next query rebuilds it from the log,
    // which is what makes a persisted cache safe without a whole-file rebuild.
    let rebuilt = loaded
        .text_of(&dir, "stale")
        .expect("the entry was rebuilt from the grown log")
        .to_vec();
    assert_eq!(
        rebuilt.len(),
        stale_lines.len() + 1,
        "the rebuilt text must cover every event in the grown log"
    );
    assert_eq!(
        rebuilt.last().map(String::as_str),
        Some("a line written after the index was saved"),
        "and must end with the event that invalidated the old entry"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---- 6. two catalogs over one directory ------------------------------------------------------

#[tokio::test]
async fn two_catalogs_over_one_directory_each_create_a_session_and_both_see_both() {
    let dir = scratch("two-catalogs");
    // Two instances, no shared state: this is what two processes (or one process after a restart)
    // pointed at the same sessions directory looks like to this crate.
    let first = DirectoryCatalog::new(&dir);
    let second = DirectoryCatalog::new(&dir);

    // Created concurrently, because the interesting failure is the one a sequential test cannot
    // reach: both handles deciding the directory is empty at the same moment.
    let (created_by_first, created_by_second) =
        tokio::join!(first.create("alpha"), second.create("beta"));
    assert_eq!(
        created_by_first
            .expect("the first catalog created its session")
            .id,
        "alpha"
    );
    assert_eq!(
        created_by_second
            .expect("the second catalog created its session")
            .id,
        "beta"
    );

    // Each write goes through the handle its own catalog opened, so neither catalog can be relying on
    // a registry the other populated.
    let alpha_events = vec![SessionEvent::user_text(
        "m-alpha",
        "written through the first catalog",
    )];
    let beta_events = vec![SessionEvent::user_text(
        "m-beta",
        "written through the second catalog",
    )];
    let alpha = first
        .open("alpha")
        .expect("alpha is visible to its creator");
    for event in &alpha_events {
        alpha.append(event.clone()).await.expect("appended");
    }
    let beta = second.open("beta").expect("beta is visible to its creator");
    for event in &beta_events {
        beta.append(event.clone()).await.expect("appended");
    }
    drop(alpha);
    drop(beta);

    // Sorted before comparing: `list` orders newest-first by modification time, and two sessions
    // created in the same millisecond have no meaningful order between them.
    let mut listed_by_first: Vec<String> =
        first.list().await.into_iter().map(|row| row.id).collect();
    let mut listed_by_second: Vec<String> =
        second.list().await.into_iter().map(|row| row.id).collect();
    listed_by_first.sort_unstable();
    listed_by_second.sort_unstable();
    assert_eq!(
        listed_by_first,
        vec!["alpha".to_owned(), "beta".to_owned()],
        "the first catalog must see the session the second one created"
    );
    assert_eq!(
        listed_by_second, listed_by_first,
        "and the second must see both, so neither view is a subset of the other"
    );

    // Reading the *other* catalog's session proves the events came off the disk rather than out of
    // whichever instance wrote them.
    let cross_read = second
        .read("alpha")
        .await
        .expect("the second catalog reads the first one's session");
    assert_eq!(cross_read.id, "alpha");
    assert_eq!(
        cross_read.events, alpha_events,
        "the cross-read conversation must equal what the owning catalog wrote"
    );
    let back_read = first
        .read("beta")
        .await
        .expect("the first catalog reads the second one's session");
    assert_eq!(back_read.events, beta_events);

    let _ = std::fs::remove_dir_all(&dir);
}
