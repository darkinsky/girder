//! Tests for diagnosing and repairing a log left half-written by a crash.
//!
//! The properties that matter are asymmetric, and the tests are weighted accordingly. Cutting too
//! little leaves a file no reader accepts; cutting too much deletes events the user still has, and
//! nothing afterwards can tell that it happened. So the torn-tail tests assert the exact byte the
//! cut lands on, and the "must not repair" cases are tested as hard as the repair itself.
//!
//! The damaged files are built by hand from real `SessionStore` writes plus a deliberately truncated
//! zstd frame, because that is the only way to reproduce what a killed process leaves: every frame
//! before the last is complete and the last one stops mid-stream.

// The module is included by path rather than through the crate's re-exports, so these tests compile
// against the module as written even before the crate root wires it in.
#[path = "../src/session_recovery.rs"]
mod session_recovery;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use girder_core::{SessionEvent, SessionStore};
use session_recovery::{Diagnosis, Recovery, diagnose, repair};

fn scratch(name: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-recovery-{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// A log of `count` events, written the way the store writes one: one frame per append.
fn seeded(name: &str, count: usize) -> (PathBuf, PathBuf) {
    let dir = scratch(name);
    let path = dir.join("s.jsonl.zst");
    let store = SessionStore::new(&path);
    store.touch().expect("created");
    for index in 0..count {
        store
            .append(&SessionEvent::user_text(
                &format!("m{index}"),
                format!("body {index}"),
            ))
            .expect("appended");
    }
    (dir, path)
}

/// One complete frame for `event`, exactly as the store would append it.
fn frame_for(event: &SessionEvent) -> Vec<u8> {
    let mut line = serde_json::to_string(event).expect("serialised");
    line.push('\n');
    zstd::encode_all(line.as_bytes(), 3).expect("compressed")
}

/// Appends the first `keep` bytes of a frame — the state a kill leaves behind.
fn append_torn_frame(path: &Path, keep: usize) {
    let frame = frame_for(&SessionEvent::user_text("torn", "never finished"));
    assert!(
        keep < frame.len(),
        "the kept prefix must leave the frame short"
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("opened for append");
    std::io::Write::write_all(&mut file, &frame[..keep]).expect("appended the prefix");
}

fn torn_tail(diagnosis: &Diagnosis) -> u64 {
    match diagnosis.action {
        Recovery::TruncateTo { bytes } => bytes,
        ref other => panic!("expected a truncation, got {other:?}"),
    }
}

// ---- healthy logs ------------------------------------------------------------------------------

#[test]
fn a_healthy_log_is_not_repaired() {
    let (dir, path) = seeded("healthy", 3);
    let before = std::fs::read(&path).expect("read");

    let diagnosis = diagnose(&path).expect("diagnosed");

    assert_eq!(diagnosis.readable, 3);
    assert!(!diagnosis.torn_tail);
    assert_eq!(diagnosis.torn_at, None);
    assert_eq!(diagnosis.action, Recovery::Healthy);
    repair(&path, &diagnosis).expect("a no-op repair succeeds");
    assert_eq!(
        std::fs::read(&path).expect("read"),
        before,
        "the file is untouched"
    );
    // No evidence file is written when nothing was damaged.
    assert_eq!(std::fs::read_dir(&dir).expect("listed").count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_file_is_healthy_and_reads_as_no_events() {
    // A brand-new session is an empty file, not a damaged one: treating it as torn would make every
    // first launch try to repair a log that has nothing in it.
    let dir = scratch("empty");
    let path = dir.join("s.jsonl.zst");
    std::fs::write(&path, b"").expect("created");

    let diagnosis = diagnose(&path).expect("diagnosed");

    assert_eq!(diagnosis.readable, 0);
    assert!(!diagnosis.torn_tail);
    assert_eq!(diagnosis.torn_at, None);
    assert_eq!(diagnosis.action, Recovery::Healthy);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_file_is_an_error_rather_than_a_healthy_log() {
    // Answering "healthy" would send a caller on to read a file that is not there, and answering
    // "truncate to 0" would create one. The caller is looking in the wrong place and is told so.
    let dir = scratch("missing");
    let path = dir.join("nope.jsonl.zst");

    let error = diagnose(&path).expect_err("a missing log cannot be diagnosed");

    assert_eq!(error.kind(), ErrorKind::NotFound);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- a torn tail: the case this module exists for ----------------------------------------------

#[test]
fn a_half_written_frame_is_detected_and_cut_at_the_last_good_frame() {
    let (dir, path) = seeded("torn", 2);
    let whole = std::fs::metadata(&path).expect("metadata").len();
    append_torn_frame(&path, 12);
    assert!(std::fs::metadata(&path).expect("metadata").len() > whole);

    let diagnosis = diagnose(&path).expect("diagnosed");

    // Every event before the tear is still readable: the write that was interrupted had not landed.
    assert_eq!(diagnosis.readable, 2);
    assert!(diagnosis.torn_tail);
    // The cut lands exactly where the unfinished frame began — not at the last newline, not at the
    // end minus a constant. One byte either way would either leave the log undecodable or delete a
    // readable event.
    assert_eq!(diagnosis.torn_at, Some(whole));
    assert_eq!(torn_tail(&diagnosis), whole);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_torn_log_cannot_be_read_until_it_is_repaired() {
    // The premise of the module: the readers in this crate fail on a damaged tail rather than
    // skipping it, so one killed write makes a whole conversation unreadable.
    let (dir, path) = seeded("unreadable", 2);
    append_torn_frame(&path, 12);
    assert!(girder_core::read_session_file(&path).is_err());
    assert!(SessionStore::new(&path).read_all().is_err());

    let diagnosis = diagnose(&path).expect("diagnosed");
    repair(&path, &diagnosis).expect("repaired");

    let events = girder_core::read_session_file(&path).expect("the repaired log decodes");
    assert_eq!(events.len(), 2);
    assert_eq!(SessionStore::new(&path).read_all().expect("read").len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_kill_inside_the_frame_magic_is_a_torn_tail() {
    // The window is small but real: a write can stop inside the four magic bytes, leaving a prefix
    // that no frame-length call can even look at.
    let (dir, path) = seeded("magic", 1);
    let whole = std::fs::metadata(&path).expect("metadata").len();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("opened for append");
    std::io::Write::write_all(&mut file, b"\x28\xb5").expect("appended");
    drop(file);

    let diagnosis = diagnose(&path).expect("diagnosed");

    assert_eq!(diagnosis.readable, 1);
    assert!(diagnosis.torn_tail);
    assert_eq!(torn_tail(&diagnosis), whole);
    repair(&path, &diagnosis).expect("repaired");
    assert_eq!(
        girder_core::read_session_file(&path)
            .expect("decodes")
            .len(),
        1
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_log_that_is_a_torn_frame_and_nothing_else_cuts_to_zero() {
    // The file exists and holds one event that never landed. The answer is an empty log, not an
    // error: there is nothing to keep and nothing to guess.
    let dir = scratch("only-torn");
    let path = dir.join("s.jsonl.zst");
    let frame = frame_for(&SessionEvent::user_text("m", "body"));
    std::fs::write(&path, &frame[..10]).expect("written");

    let diagnosis = diagnose(&path).expect("diagnosed");

    assert_eq!(diagnosis.readable, 0);
    assert!(diagnosis.torn_tail);
    assert_eq!(torn_tail(&diagnosis), 0);
    repair(&path, &diagnosis).expect("repaired");
    assert_eq!(std::fs::metadata(&path).expect("metadata").len(), 0);
    assert!(
        girder_core::read_session_file(&path)
            .expect("decodes")
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the original is never destroyed -----------------------------------------------------------

#[test]
fn repairing_keeps_the_damaged_bytes_beside_the_log() {
    // The torn bytes are the only record of what went wrong, and the repair is the operation that
    // destroys them. A duplicate is harmless; losing the evidence is not.
    let (dir, path) = seeded("evidence", 2);
    append_torn_frame(&path, 12);
    let damaged = std::fs::read(&path).expect("read");

    let diagnosis = diagnose(&path).expect("diagnosed");
    repair(&path, &diagnosis).expect("repaired");

    let evidence = dir.join("s.jsonl.zst.torn-0");
    let kept = std::fs::read(&evidence).expect("the damaged log was preserved verbatim");
    assert_eq!(
        kept, damaged,
        "the evidence is byte-for-byte the damaged file"
    );
    // The live log really was cut, so the two are not the same file.
    assert!(std::fs::read(&path).expect("read").len() < kept.len());
    // The evidence name carries no `.jsonl.zst` suffix, so discovery does not pick it up as a log.
    assert!(!evidence.to_string_lossy().ends_with(".jsonl.zst"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_damage_does_not_overwrite_the_first_evidence_file() {
    let (dir, path) = seeded("evidence-twice", 1);
    append_torn_frame(&path, 12);
    let first = std::fs::read(&path).expect("read");
    let diagnosis = diagnose(&path).expect("diagnosed");
    repair(&path, &diagnosis).expect("repaired");

    // Damage the log again, the way a second crash would.
    append_torn_frame(&path, 12);
    let second = std::fs::read(&path).expect("read");
    let diagnosis = diagnose(&path).expect("diagnosed");
    repair(&path, &diagnosis).expect("repaired again");

    assert_eq!(
        std::fs::read(dir.join("s.jsonl.zst.torn-0")).expect("first evidence"),
        first
    );
    assert_eq!(
        std::fs::read(dir.join("s.jsonl.zst.torn-1")).expect("second evidence"),
        second
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- idempotence -------------------------------------------------------------------------------

#[test]
fn repairing_a_repaired_log_changes_nothing() {
    let (dir, path) = seeded("idempotent", 2);
    append_torn_frame(&path, 12);
    let diagnosis = diagnose(&path).expect("diagnosed");
    repair(&path, &diagnosis).expect("repaired");
    let once = std::fs::read(&path).expect("read");

    let again = diagnose(&path).expect("diagnosed");
    assert_eq!(again.action, Recovery::Healthy);
    repair(&path, &again).expect("a healthy repair is a no-op");

    assert_eq!(std::fs::read(&path).expect("read"), once);
    // The no-op did not manufacture a second evidence file.
    assert_eq!(std::fs::read_dir(&dir).expect("listed").count(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stale_diagnosis_is_refused_rather_than_applied_to_a_moved_log() {
    // A caller can hold a diagnosis while the log is appended to. Applying the old offset would then
    // cut readable events off the end, so the repair re-derives the diagnosis and refuses a mismatch.
    let (dir, path) = seeded("stale", 2);
    append_torn_frame(&path, 12);
    let stale = diagnose(&path).expect("diagnosed");
    // Repaired by someone else first; the file is now whole.
    repair(&path, &stale).expect("repaired");
    let repaired = std::fs::read(&path).expect("read");

    let error = repair(&path, &stale).expect_err("a stale truncation must be refused");

    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(
        std::fs::read(&path).expect("read"),
        repaired,
        "the refused repair left the log untouched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- damage that cutting a tail cannot fix -----------------------------------------------------

#[test]
fn a_torn_frame_followed_by_readable_events_is_unusable() {
    // This happens on the second run after a crash: the store appends onto the torn tail it never
    // repaired, so the file is frames, a tear, then frames. Truncating at the tear would delete the
    // events after it, so the only safe answer is to refuse and say what was found.
    let (dir, path) = seeded("mid-tear", 2);
    append_torn_frame(&path, 12);
    let store = SessionStore::new(&path);
    store
        .append(&SessionEvent::user_text("after", "written past the tear"))
        .expect("appended after the tear");

    let diagnosis = diagnose(&path).expect("diagnosed");

    assert_eq!(
        diagnosis.readable, 2,
        "only the frames before the tear are readable"
    );
    assert!(!diagnosis.torn_tail, "the file does not end inside a frame");
    assert!(diagnosis.torn_at.is_some());
    match diagnosis.action {
        Recovery::Unusable { reason } => {
            assert!(
                reason.contains("starts at byte"),
                "the reason names the break: {reason}"
            );
            assert!(reason.contains("readable frame starts at byte"), "{reason}");
        }
        ref other => panic!("expected an unusable verdict, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bytes_that_are_not_a_frame_are_unusable_and_nothing_is_written() {
    let (dir, path) = seeded("garbage", 1);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("opened for append");
    std::io::Write::write_all(&mut file, b"not a frame at all").expect("appended");
    drop(file);
    let before = std::fs::read(&path).expect("read");

    let diagnosis = diagnose(&path).expect("diagnosed");

    assert_eq!(diagnosis.readable, 1);
    assert!(!diagnosis.torn_tail);
    match diagnosis.action {
        Recovery::Unusable { .. } => {}
        ref other => panic!("expected an unusable verdict, got {other:?}"),
    }
    let error = repair(&path, &diagnosis).expect_err("an unusable log is not repaired");
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert_eq!(std::fs::read(&path).expect("read"), before);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the log keeps its identity after a repair -------------------------------------------------

#[test]
fn a_repaired_log_can_be_appended_to_again() {
    // The point of repairing is that the session continues, so the repaired file must be one the
    // store can still write to and read back — not merely a file that decodes once.
    let (dir, path) = seeded("continue", 2);
    append_torn_frame(&path, 12);
    let diagnosis = diagnose(&path).expect("diagnosed");
    repair(&path, &diagnosis).expect("repaired");

    let store = SessionStore::new(&path);
    store
        .append(&SessionEvent::user_text("next", "after the repair"))
        .expect("appended");
    let events = store.read_all().expect("read");

    assert_eq!(events.len(), 3);
    assert_eq!(
        diagnose(&path).expect("diagnosed").action,
        Recovery::Healthy
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn every_event_reported_readable_is_actually_readable() {
    // `readable` is a promise a caller acts on, so it is checked against the real reader rather than
    // against the diagnosis itself.
    let (dir, path) = seeded("count", 5);
    append_torn_frame(&path, 15);

    let diagnosis = diagnose(&path).expect("diagnosed");
    repair(&path, &diagnosis).expect("repaired");

    let events = girder_core::read_session_file(&path).expect("decodes");
    assert_eq!(events.len(), diagnosis.readable);
    let _ = std::fs::remove_dir_all(&dir);
}
