//! End-to-end tests for the error paths: what a caller actually receives when something is wrong.
//!
//! A happy path that works is half a system. The other half is what happens when the session does
//! not exist, the id is hostile, the log is torn, the disk refuses the write, or the name is a typo
//! -- and the property that matters in every one of those cases is the same: **the answer has to
//! say what went wrong, and the failure must not leave a mark on disk.** An error that reports
//! "failed" without naming the id, or a refusal that already created a file, is indistinguishable
//! from a bug in the caller's own code, which is how retry loops and phantom sessions start.
//!
//! So every test here asserts on the message text, not merely the variant, and the destructive
//! paths are followed by a check of the directory: after a refused `create` the session directory
//! must still be empty, and after a refused repair the log must be byte-identical.
//!
//! # Two tests here describe behaviour that is worse than it should be
//!
//! [`the_same_missing_session_reaches_a_client_as_four_different_answers`] and
//! [`a_damaged_log_reads_as_an_empty_session_through_the_catalog`] pin down what the current code
//! does rather than what it ought to do. They exist so that the gaps are visible and cannot drift
//! silently: the first shows one condition arriving under three different error codes plus a
//! `false`, and the second shows a torn log being reported as a blank session. If those paths are
//! made sharper, these two tests are expected to change with them.
//!
//! # Isolation
//!
//! Every test builds its own scratch tree under the system temporary directory, and the tree is
//! split into a parent and a `sessions` child. The split is what lets the escape test assert that
//! nothing landed *outside* the session directory: the parent is the place a `../` id would reach.
//! Ids in paths are assembled from the process id and a counter so two test binaries running at
//! once cannot share a directory.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use girder_core::{
    AtomicError, BudgetDecision, ContextBudget, ContextUsage, Diagnosis, DirectoryCatalog, Mode,
    ProjectionRegistry, Recovery, RemoteCall, RemoteError, RemoteRegistry, SessionCatalog,
    SessionError, SessionEvent, SessionStore, check_context_budget, diagnose_session,
    register_session_state_method, repair_session, suggest_tool_name,
};

/// A fresh scratch tree, with its session directory created.
///
/// The tree is created empty rather than with fixtures so that "no file was written" is a fact this
/// helper guarantees rather than one each test has to remember to check for.
fn scratch(name: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("girder-errors-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("sessions")).expect("scratch tree");
    root
}

/// The session directory inside a scratch tree.
fn sessions(root: &Path) -> PathBuf {
    root.join("sessions")
}

/// A catalog over the scratch tree's session directory.
fn directory_catalog(root: &Path) -> DirectoryCatalog {
    DirectoryCatalog::new(sessions(root))
}

/// Everything in `dir`, sorted, for "nothing was created" assertions.
///
/// Names rather than a count: a message that lists what appeared says which file a broken refusal
/// wrote, which a bare `assert_eq!(count, 0)` does not.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("the scratch directory is readable")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_unstable();
    names
}

/// Runs `session.state` against a catalog over `root`, the way a client reaches it.
async fn state(root: &Path, id: &str) -> Result<serde_json::Value, RemoteError> {
    let catalog: Arc<dyn SessionCatalog> = Arc::new(directory_catalog(root));
    let projections = Arc::new(ProjectionRegistry::new());
    let mut registry = RemoteRegistry::new();
    register_session_state_method(&mut registry, &catalog, &projections);
    registry
        .dispatch(
            "session",
            "state",
            RemoteCall {
                args: serde_json::json!({ "sessionId": id }),
                peer: "test".to_owned(),
            },
        )
        .await
}

/// Seeds `id` with `count` user messages, written the way the store writes them: one frame per
/// append.
fn seed(root: &Path, id: &str, count: usize) -> PathBuf {
    let path = sessions(root).join(format!("{id}.jsonl.zst"));
    let store = SessionStore::new(&path);
    store.touch().expect("created");
    for index in 0..count {
        store
            .append(&SessionEvent::user_text(
                &format!("{id}-m{index}"),
                format!("body {index}"),
            ))
            .expect("appended");
    }
    path
}

/// One complete frame for `event`, exactly as [`SessionStore::append`] would write it.
///
/// Built here rather than taken from the store because the point of the damaged-log tests is to
/// write bytes the store would never produce: a frame cut short, and a frame continued past.
fn frame_for(event: &SessionEvent) -> Vec<u8> {
    let mut line = serde_json::to_string(event).expect("serialised");
    line.push('\n');
    zstd::encode_all(line.as_bytes(), 3).expect("compressed")
}

/// Turns `0o400`-style bits on for `path`.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("mode set");
}

/// Whether this filesystem and user actually enforce the write bit.
///
/// A root-run test process can write a `0400` file, and asserting "the write is refused" there
/// would fail for a reason that has nothing to do with the code. The probe decides, so the
/// permission tests can skip honestly instead of being wrong on somebody else's machine.
#[cfg(unix)]
fn permission_bits_are_enforced(dir: &Path) -> bool {
    let probe = dir.join("write-probe");
    std::fs::write(&probe, b"x").expect("probe written");
    set_mode(&probe, 0o400);
    let refused = std::fs::OpenOptions::new()
        .append(true)
        .open(&probe)
        .is_err();
    set_mode(&probe, 0o600);
    let _ = std::fs::remove_file(&probe);
    refused
}

// ---- 1. reading a session that does not exist ---------------------------------------------------

#[tokio::test]
async fn a_missing_session_is_absent_from_the_catalog_and_named_by_state() {
    let root = scratch("missing-read");
    let catalog = directory_catalog(&root);

    // `None`, not an empty record: an empty record would let a mistyped id render as a fresh
    // conversation and take the next prompt, so absence has to be distinguishable from emptiness.
    assert!(
        catalog.read("nope").await.is_none(),
        "an id with no log must not read as an empty session"
    );

    let error = state(&root, "nope").await.expect_err("state refuses");
    let message = error.to_string();
    // The id in the message is the whole value of this answer: a client multiplexing several reads
    // needs to know *which* session was missing without re-deriving it from call order.
    assert!(
        message.contains("nope"),
        "the refusal must name the session it could not find: {message}"
    );
    assert!(
        message.contains("no session"),
        "the sentence says a session does not exist rather than that something failed: {message}"
    );
    // Its own code, not the generic one: there is no NOT_FOUND variant in this build, so a client
    // that wants to distinguish this from a disk error has nothing to branch on. Recorded here so
    // the gap is visible if the taxonomy grows a variant.
    assert_eq!(error.code(), "FAILED");
    let _ = std::fs::remove_dir_all(&root);
}

// ---- 2. deleting a session that does not exist --------------------------------------------------

#[tokio::test]
async fn deleting_a_missing_session_answers_false_rather_than_erroring() {
    let root = scratch("missing-delete");
    let catalog = directory_catalog(&root);

    // `false`, not `Err`: a delete is a retryable operation, and the second attempt of a delete
    // that already landed must converge instead of reporting a failure the caller cannot act on.
    assert!(!catalog.delete("nope").await.expect("no error for absence"));
    assert!(
        entries(&sessions(&root)).is_empty(),
        "a delete of an absent session must not create its log: {:?}",
        entries(&sessions(&root))
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn deleting_a_present_session_answers_true_and_leaves_nothing_behind() {
    let root = scratch("present-delete");
    let path = seed(&root, "s1", 1);
    let catalog = directory_catalog(&root);

    assert!(
        catalog.delete("s1").await.expect("delete succeeds"),
        "true reports that something was removed"
    );
    assert!(!path.exists(), "the log is gone after a true answer");
    assert!(entries(&sessions(&root)).is_empty(), "and nothing is left");
    let _ = std::fs::remove_dir_all(&root);
}

// ---- 3. ids that would escape the session directory ---------------------------------------------

#[tokio::test]
async fn a_parent_relative_id_is_refused_and_nothing_is_written_outside() {
    // The id is concatenated into a path, so `../x` would address a file beside the session
    // directory. The refusal must come *before* any write: a create that validates after opening
    // would already have escaped.
    let root = scratch("escape");
    let catalog = directory_catalog(&root);

    let error = catalog
        .create("../x")
        .await
        .expect_err("a path separator is refused");
    let message = error.to_string();
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(
        message.contains("path separator"),
        "the message names the offending property, not just `invalid id`: {message}"
    );
    assert!(
        message.contains("../x"),
        "the message echoes the id the caller sent: {message}"
    );

    assert!(
        entries(&sessions(&root)).is_empty(),
        "a refused create must not create the log: {:?}",
        entries(&sessions(&root))
    );
    assert!(
        !root.join("x.jsonl.zst").exists(),
        "a refused create must not write the file the escape addressed"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn a_nested_id_is_refused_and_writes_no_directory() {
    // `a/b` is the same escape in a milder form: it would make the catalog create a subdirectory
    // and write a log that no listing would ever show, so the session would exist and be invisible.
    let root = scratch("nested");
    let catalog = directory_catalog(&root);

    let error = catalog
        .create("a/b")
        .await
        .expect_err("a separator is refused");
    let message = error.to_string();
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(message.contains("a/b"), "{message}");
    assert!(
        !sessions(&root).join("a").exists(),
        "no directory is created for a refused nested id"
    );
    assert!(entries(&sessions(&root)).is_empty());
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn an_empty_id_is_refused_without_minting_a_dot_prefixed_log() {
    // An empty id is the dangerous case of an off-by-one in path assembly: joined with the suffix
    // it becomes `.jsonl.zst`, a hidden file that `ids()` skips (its id would be empty) but that
    // every write would then append to. Refusing up front is what keeps "no id" from becoming
    // "one junk session that no listing shows".
    let root = scratch("empty-id");
    let catalog = directory_catalog(&root);

    let error = catalog
        .create("")
        .await
        .expect_err("an empty id is refused");
    let message = error.to_string();
    assert_eq!(error.code(), "INVALID_ARGUMENTS");
    assert!(
        message.contains("cannot be empty"),
        "the message says why, in words a client can show: {message}"
    );
    assert!(
        entries(&sessions(&root)).is_empty(),
        "no file is created for an empty id: {:?}",
        entries(&sessions(&root))
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---- 4. a log whose tail was never finished -----------------------------------------------------

#[test]
fn a_torn_tail_is_diagnosed_cut_and_readable_again() {
    // The state a killed process leaves: complete frames, then the first bytes of one more. Reading
    // cannot skip it -- the decoder fails on the truncation before it parses a line -- so the whole
    // conversation is unreadable until the partial frame is removed.
    let root = scratch("torn-tail");
    let path = seed(&root, "s1", 2);
    let whole = std::fs::read(&path).expect("seeded log");

    let torn = frame_for(&SessionEvent::user_text("torn", "never finished"));
    let keep = 12;
    assert!(
        keep < torn.len(),
        "the kept prefix must leave the frame short"
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("opened for append");
    std::io::Write::write_all(&mut file, &torn[..keep]).expect("appended the prefix");
    drop(file);

    // The error names the layer, so an operator can tell a damaged log from a missing one.
    let error = girder_core::read_session_file(&path).expect_err("a torn frame does not decode");
    assert!(
        matches!(error, SessionError::Compression(_)),
        "a torn frame is reported as a decode failure: {error}"
    );
    assert!(
        error.to_string().contains("session compression"),
        "the message says which subsystem failed: {error}"
    );

    let damaged = std::fs::read(&path).expect("damaged log");
    let diagnosis = diagnose_session(&path).expect("diagnosed");
    // Diagnosis is a read: the bytes it just judged are still there, unchanged. A diagnosis that
    // edited the file would destroy the evidence a repair is about to preserve.
    assert_eq!(
        std::fs::read(&path).expect("log after diagnosis"),
        damaged,
        "diagnose must not write to the log"
    );

    assert_eq!(diagnosis.readable, 2, "the two whole frames are readable");
    assert!(diagnosis.torn_tail, "the file ends inside a frame");
    assert_eq!(
        diagnosis.torn_at,
        Some(u64::try_from(whole.len()).expect("length fits")),
        "the tear begins exactly where the good frames end"
    );
    assert_eq!(
        diagnosis.action,
        Recovery::TruncateTo {
            bytes: u64::try_from(whole.len()).expect("length fits")
        },
        "the cut lands on the byte after the last whole frame"
    );

    repair_session(&path, &diagnosis).expect("a torn tail is repairable");

    let events = girder_core::read_session_file(&path).expect("the repaired log decodes");
    assert_eq!(events.len(), 2, "the repair removes no readable event");
    assert!(
        diagnosis_action_is_healthy(&path),
        "a repaired log diagnoses healthy"
    );
    // The cut bytes are kept beside the log rather than deleted: they are the only record of what
    // went wrong, and the repair is what destroys them.
    assert!(
        sessions(&root).join("s1.jsonl.zst.torn-0").exists(),
        "the damaged bytes survive as evidence"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Whether `path` currently diagnoses as [`Recovery::Healthy`].
fn diagnosis_action_is_healthy(path: &Path) -> bool {
    diagnose_session(path).is_ok_and(|diagnosis| diagnosis.action == Recovery::Healthy)
}

#[test]
fn a_torn_frame_in_the_middle_is_refused_instead_of_truncated() {
    // After an unrepaired crash the store appends onto the tear on the next run, so the file
    // becomes frames, a tear, frames. Cutting at the tear would delete readable events, and no
    // reader can see them from the prefix alone -- so the only safe answer is to refuse and say
    // what was found.
    let root = scratch("torn-middle");
    let first = frame_for(&SessionEvent::user_text("a", "one"));
    let second = second_frame();
    let third = frame_for(&SessionEvent::user_text("c", "written past the tear"));
    let mut log = Vec::new();
    log.extend_from_slice(&first);
    log.extend_from_slice(&second);
    // The tear: the first 12 bytes of a frame, then a complete one.
    log.extend_from_slice(&frame_for(&SessionEvent::user_text("b", "never finished"))[..12]);
    log.extend_from_slice(&third);
    let path = sessions(&root).join("s1.jsonl.zst");
    std::fs::write(&path, &log).expect("hand-built log");

    let diagnosis = diagnose_session(&path).expect("diagnosed");

    assert_eq!(diagnosis.readable, 2, "only the frames before the tear");
    assert!(
        !diagnosis.torn_tail,
        "the file does not end inside a frame, so this is not a tail"
    );
    assert!(diagnosis.torn_at.is_some(), "the break is located");
    let reason = match diagnosis.action {
        Recovery::Unusable { ref reason } => reason.clone(),
        ref other => panic!("a mid-log tear must be Unusable, got {other:?}"),
    };
    assert!(
        reason.contains("starts at byte"),
        "the reason locates the damage: {reason}"
    );
    assert!(
        reason.contains("readable frame starts at byte"),
        "and says why cutting there would lose data: {reason}"
    );

    let error = repair_session(&path, &diagnosis).expect_err("an unusable log is not repaired");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    let message = error.to_string();
    assert!(
        message.contains("cannot be repaired"),
        "the refusal says the log was not fixed: {message}"
    );
    assert!(
        message.contains("s1.jsonl.zst"),
        "and names the file it refused to touch: {message}"
    );
    assert_eq!(
        std::fs::read(&path).expect("log after refused repair"),
        log,
        "a refused repair must leave the log byte-identical"
    );
    assert!(
        !sessions(&root).join("s1.jsonl.zst.torn-0").exists(),
        "a refused repair writes no evidence file either"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A second complete frame, built from a different event than the others.
///
/// Declared separately because the mid-log test needs three *distinct* frames: reusing one event
/// would make a repair that cut the wrong place harder to notice, since the remaining frames would
/// decode to identical content.
fn second_frame() -> Vec<u8> {
    frame_for(&SessionEvent::user_text("b", "two"))
}

#[tokio::test]
async fn a_damaged_log_reads_as_an_empty_session_through_the_catalog() {
    // This pins a gap rather than a virtue. `read_record` decodes with `unwrap_or_default`, so a
    // log the raw reader rejects becomes a record with no events -- indistinguishable from a
    // session that was created and never used. A client cannot tell damage from emptiness, and the
    // one answer that would tell it (`SessionError`) is discarded on the way.
    let root = scratch("catalog-hides");
    let path = seed(&root, "s1", 2);
    let torn = frame_for(&SessionEvent::user_text("torn", "never finished"));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("opened for append");
    std::io::Write::write_all(&mut file, &torn[..12]).expect("appended the prefix");
    drop(file);

    // The raw reader is clear about the damage.
    assert!(girder_core::read_session_file(&path).is_err());

    let record = directory_catalog(&root)
        .read("s1")
        .await
        .expect("the catalog still answers for the id");
    assert!(
        record.events.is_empty(),
        "the undecodable events arrive as none, with no error attached"
    );
    assert!(
        record.blank,
        "and the record claims the conversation never started"
    );

    // `session.state` therefore succeeds on a damaged session and reports zero events.
    let value = state(&root, "s1").await.expect("state answers");
    assert_eq!(
        value["events"], 0,
        "a client is told the session is empty rather than damaged"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---- 5. the disk refuses the write --------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn writing_to_a_read_only_log_reports_a_permission_problem() {
    // "failed" is not an answer a caller can act on: a permission refusal is permanent, so a retry
    // is wasted, while a transient I/O error is worth retrying. The message has to carry the
    // distinction, because the variant alone does not.
    let root = scratch("read-only-log");
    if !permission_bits_are_enforced(&sessions(&root)) {
        eprintln!("skipping: this user can write a 0400 file");
        let _ = std::fs::remove_dir_all(&root);
        return;
    }
    let path = seed(&root, "s1", 1);
    let before = std::fs::read(&path).expect("seeded log");
    set_mode(&path, 0o400);

    let store = SessionStore::new(&path);
    let error = store
        .append(&SessionEvent::user_text("m", "refused"))
        .expect_err("a read-only log cannot be appended to");
    let message = error.to_string();
    assert!(
        matches!(error, SessionError::Io(_)),
        "the refusal is an io failure from the underlying file: {error}"
    );
    assert!(
        message.to_lowercase().contains("permission denied"),
        "the message names the permission problem: {message}"
    );
    assert!(
        message.contains("session io"),
        "and the subsystem it came from: {message}"
    );
    assert!(
        !message.eq_ignore_ascii_case("failed"),
        "a bare `failed` is exactly the answer this test rules out"
    );
    // Reading still works: the refusal cost nothing, and the log is what it was.
    assert_eq!(
        std::fs::read(&path).expect("log after refused append"),
        before,
        "a refused append must not alter the log"
    );
    set_mode(&path, 0o600);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn an_atomic_write_into_a_read_only_directory_names_the_path_and_the_reason() {
    // The rename half of an atomic write is where a read-only parent shows up, and the error has to
    // name the target: an operator reading "permission denied" without a path cannot tell which
    // file the process was denied.
    let root = scratch("read-only-dir");
    let locked = root.join("locked");
    std::fs::create_dir(&locked).expect("locked dir");
    if !permission_bits_are_enforced(&locked) {
        eprintln!("skipping: this user can write in a 0500 directory");
        let _ = std::fs::remove_dir_all(&root);
        return;
    }
    let target = locked.join("session.jsonl.zst");
    set_mode(&locked, 0o500);

    let error = girder_core::write_text_atomic(&target, "x", Mode::PRIVATE)
        .expect_err("the directory refuses the write");
    let message = error.to_string();

    assert!(
        matches!(error, AtomicError::Write { .. }),
        "the failure is a write failure with a cause, not a lock timeout: {error}"
    );
    assert!(
        message.contains("session.jsonl.zst"),
        "the message names the path that was refused: {message}"
    );
    assert!(
        message.to_lowercase().contains("permission denied"),
        "and says it was a permission problem: {message}"
    );
    assert!(
        !target.exists(),
        "the failed write leaves no target file behind"
    );
    assert!(
        entries(&locked).is_empty(),
        "and no staging sibling either: {:?}",
        entries(&locked)
    );
    set_mode(&locked, 0o700);
    let _ = std::fs::remove_dir_all(&root);
}

// ---- 6. a tool name that does not exist ---------------------------------------------------------

#[test]
fn a_typo_gets_one_named_suggestion_because_a_dead_end_is_unusable() {
    let available = ["read".to_owned()];
    let suggestion = suggest_tool_name("reed", &available).expect("`reed` is one edit from `read`");

    // The sentence has to repeat the failed name: the model may have forgotten which of several
    // calls it just made, and a suggestion that only says "did you mean `read`?" reads as an
    // instruction that appeared from nowhere.
    assert!(
        suggestion.contains("`reed`"),
        "the answer echoes the name that failed: {suggestion}"
    );
    assert!(
        suggestion.contains("did you mean `read`?"),
        "and offers exactly one candidate: {suggestion}"
    );
    // One suggestion, not a list: trying candidates in turn costs a call each, and a wrong guess is
    // worse than a refusal.
    assert_eq!(
        suggestion.matches("did you mean").count(),
        1,
        "exactly one suggestion: {suggestion}"
    );
}

#[test]
fn an_unrelated_name_gets_no_suggestion_rather_than_a_guess() {
    // `zzz` is not a typo of anything in the list. A guess here would teach the model to follow a
    // fabricated hint, so silence -- and the caller's own truthful sentence -- is the answer.
    assert_eq!(
        suggest_tool_name("zzz", &["read".to_owned()]),
        None,
        "a name beyond the tolerance produces no candidate"
    );
    assert_eq!(
        suggest_tool_name("read", &["read".to_owned()]),
        None,
        "an exact match is not a typo, and suggesting it would be a loop"
    );
}

// ---- 7. a request that does not fit the window --------------------------------------------------

#[test]
fn an_oversized_request_reports_a_positive_excess_to_shed() {
    // The gate's answer is used as a number: a compaction step sheds exactly `excess`, so a zero or
    // wrapped value would either loop forever or throw away context that was affordable.
    let budget = ContextBudget::new(100_000, 8_000, 2_000);
    assert_eq!(budget.input_allowance(), 90_000);

    let usage = ContextUsage::new(20_000, 100_000, 15_000);
    match check_context_budget(&budget, &usage) {
        BudgetDecision::Over { excess } => {
            assert!(excess > 0, "an over-budget request sheds a positive amount");
            assert_eq!(
                excess, 45_000,
                "the excess is the total minus the allowance, not the total minus the window"
            );
            // The difference between the two subtractions is the whole point of the budget:
            // measured against the raw window this request would look fine and then be truncated
            // mid-answer.
            assert!(
                usage.total() > budget.window,
                "this must exceed the raw window too, or it proves nothing about the reservation"
            );
        }
        other @ BudgetDecision::Fits => {
            panic!("expected an over-budget verdict, got {other:?}")
        }
    }

    // The boundary: a request landing exactly on the allowance is admitted, and one token past it
    // is refused by exactly one. A strict comparison would reject a maximal request; a wrapping
    // subtraction would report a colossal excess.
    assert_eq!(
        check_context_budget(&budget, &ContextUsage::new(10_000, 70_000, 10_000)),
        BudgetDecision::Fits
    );
    assert_eq!(
        check_context_budget(&budget, &ContextUsage::new(10_000, 70_001, 10_000)),
        BudgetDecision::Over { excess: 1 }
    );
}

// ---- the shape of the errors themselves ---------------------------------------------------------

#[tokio::test]
async fn the_same_missing_session_reaches_a_client_as_four_different_answers() {
    // One condition -- "that id names no session" -- arrives under three error codes and a `false`.
    // This is the sharpest gap this suite found: a client cannot branch on the code to decide
    // whether to create the session, retry, or show an error, because the code describes the method
    // that failed rather than the thing that was missing. The messages are all correct; the codes
    // are not.
    let root = scratch("missing-codes");
    let catalog = directory_catalog(&root);

    // `delete` answers false, which is usable.
    assert!(!catalog.delete("nope").await.expect("no error"));

    // `rename` reports UNKNOWN_METHOD -- the code for a client typo in the method name -- even
    // though the method exists and the *session* is what is missing.
    let renamed = catalog
        .rename("nope", "label")
        .await
        .expect_err("renaming a missing session fails");
    assert_eq!(renamed.code(), "UNKNOWN_METHOD");
    // The id survives in the message, which is the only part a human can use.
    assert!(
        renamed.to_string().contains("nope"),
        "the message names the id even though the code misleads: {renamed}"
    );

    // `fork` reports INVALID_ARGUMENTS: the arguments were well formed and the source is gone, so a
    // client that validates before sending reads this as its own bug.
    let forked = catalog
        .fork("nope", None)
        .await
        .expect_err("forking a missing session fails");
    assert_eq!(forked.code(), "INVALID_ARGUMENTS");
    assert!(
        forked.to_string().contains("no session `nope`"),
        "and this one does say what was missing: {forked}"
    );

    // `state` reports FAILED, the fallback for anything that does not fit elsewhere.
    let described = state(&root, "nope").await.expect_err("state refuses");
    assert_eq!(described.code(), "FAILED");

    // Four answers, four codes, three of them describing the wrong thing. A `NOT_FOUND` variant
    // would collapse all of this into one branchable answer.
    let codes = ["false", renamed.code(), forked.code(), described.code()];
    assert_eq!(
        codes,
        ["false", "UNKNOWN_METHOD", "INVALID_ARGUMENTS", "FAILED"],
        "the spread itself is the finding: the same fact, reported four ways"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn diagnosing_a_missing_log_is_an_error_rather_than_a_healthy_verdict() {
    // A missing file is not a healthy log: `Healthy` would send the caller on to read a file that
    // is not there, and `TruncateTo { bytes: 0 }` would create one. The distinction between "empty
    // session" and "wrong path" has to survive, so absence is an error.
    let root = scratch("missing-diagnose");
    let path = sessions(&root).join("ghost.jsonl.zst");

    let error = diagnose_session(&path).expect_err("a missing log cannot be diagnosed");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(
        !path.exists(),
        "diagnosing a missing log must not create it"
    );
    assert!(entries(&sessions(&root)).is_empty());

    // An *empty* log, by contrast, is healthy and reads as no events: the two states are different.
    std::fs::write(&path, b"").expect("empty log");
    let diagnosis: Diagnosis = diagnose_session(&path).expect("an empty log is diagnosable");
    assert_eq!(diagnosis.action, Recovery::Healthy);
    assert_eq!(diagnosis.readable, 0);
    assert!(
        SessionStore::new(&path)
            .read_all()
            .expect("reads")
            .is_empty(),
        "an empty log is an empty session, not a broken one"
    );
    let _ = std::fs::remove_dir_all(&root);
}
