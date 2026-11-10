//! Tests for spill storage.
//!
//! A spill exists so a model receives a locator instead of a wall of output, so the properties worth pinning are
//! that the locator is usable and that the file it names is complete.

use girder_core::{
    LocalSpillStore, SpillError, SpillSaveRequest, SpillStore, UnavailableSpillStore, should_spill,
};

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("girder-spill-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

// ---- requests ---------------------------------------------------------------------------------

#[test]
fn a_blank_text_is_refused() {
    // Saving nothing would produce a locator naming an empty file, and the model would read it looking for the
    // answer.
    assert!(SpillSaveRequest::new("", "bash").is_none());
    assert!(SpillSaveRequest::new("   ", "bash").is_none());
}

#[test]
fn a_label_is_reduced_to_a_safe_file_name() {
    // A label becomes a file name, so it is reduced to characters that cannot escape a directory.
    let request = SpillSaveRequest::new("text", "../../etc/passwd").expect("valid");

    assert!(!request.label.contains('/'), "{}", request.label);
    assert!(!request.label.contains('.'), "{}", request.label);
    assert_eq!(request.label, "------etc-passwd");
}

#[test]
fn a_label_of_only_punctuation_falls_back() {
    let request = SpillSaveRequest::new("text", "///").expect("valid");

    assert_eq!(request.label, "output");
}

#[test]
fn an_alphanumeric_label_survives() {
    let request = SpillSaveRequest::new("text", "bash-output_1").expect("valid");

    assert_eq!(request.label, "bash-output_1");
}

// ---- the local store --------------------------------------------------------------------------

#[tokio::test]
async fn a_saved_text_is_readable_at_the_locator() {
    // A locator the model cannot read is not a spill.
    let dir = scratch("save");
    let store = LocalSpillStore::new(&dir);
    let text = "x".repeat(1000);

    let reference = store
        .save(SpillSaveRequest::new(text.clone(), "bash").expect("valid"))
        .await
        .expect("saved");

    let read = std::fs::read_to_string(&reference.path).expect("readable");
    assert_eq!(read, text);
    assert_eq!(reference.bytes, 1000);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_locator_names_the_path_and_the_size() {
    let dir = scratch("text");
    let store = LocalSpillStore::new(&dir);

    let reference = store
        .save(SpillSaveRequest::new("abc", "bash").expect("valid"))
        .await
        .expect("saved");

    let text = reference.to_text();
    assert!(
        text.contains(&reference.path.display().to_string()),
        "{text}"
    );
    assert!(text.contains("3 bytes"), "{text}");
    assert!(text.contains("too large"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn two_spills_of_one_label_do_not_collide() {
    let dir = scratch("twice");
    let store = LocalSpillStore::new(&dir);

    let first = store
        .save(SpillSaveRequest::new("one", "bash").expect("valid"))
        .await
        .expect("saved");
    let second = store
        .save(SpillSaveRequest::new("two", "bash").expect("valid"))
        .await
        .expect("saved");

    assert_ne!(first.path, second.path);
    assert_ne!(first.id, second.id);
    assert_eq!(
        std::fs::read_to_string(&second.path).expect("readable"),
        "two"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_store_creates_its_root() {
    let dir = scratch("create").join("nested");
    let store = LocalSpillStore::new(&dir);

    store
        .save(SpillSaveRequest::new("text", "bash").expect("valid"))
        .await
        .expect("saved");

    assert!(dir.is_dir());
    let _ = std::fs::remove_dir_all(dir.parent().expect("parent"));
}

#[tokio::test]
async fn a_saved_file_is_not_world_readable() {
    // The text may contain whatever the tool printed, including a secret it echoed.
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch("mode");
    let store = LocalSpillStore::new(&dir);

    let reference = store
        .save(SpillSaveRequest::new("secret", "bash").expect("valid"))
        .await
        .expect("saved");

    let mode = std::fs::metadata(&reference.path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the spill must not be readable by others");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_root_is_reported() {
    let store = LocalSpillStore::new("/tmp/spills");
    assert_eq!(store.root(), std::path::Path::new("/tmp/spills"));
}

// ---- the unavailable store --------------------------------------------------------------------

#[tokio::test]
async fn an_unavailable_store_refuses_distinctly() {
    // A deployment without a store cannot spill at all, and a tool that needed to should say so rather than
    // truncate silently.
    let error = UnavailableSpillStore
        .save(SpillSaveRequest::new("text", "bash").expect("valid"))
        .await
        .expect_err("unavailable");

    assert!(matches!(error, SpillError::Unavailable));
}

// ---- the threshold ----------------------------------------------------------------------------

#[test]
fn a_text_over_the_limit_spills() {
    assert!(should_spill(&"x".repeat(101), 100));
}

#[test]
fn a_text_at_the_limit_does_not_spill() {
    // Two tools sharing one threshold must not disagree about a text of exactly that size.
    assert!(!should_spill(&"x".repeat(100), 100));
}

#[test]
fn a_small_text_does_not_spill() {
    assert!(!should_spill("short", 100));
}
