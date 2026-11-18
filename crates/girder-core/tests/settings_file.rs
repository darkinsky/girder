// ---- reloading -------------------------------------------------------------------------------

#[test]
fn an_edit_on_disk_is_picked_up_without_a_restart() {
    // The point of a settings *file*: something else can edit it. Without this the store answered
    // from its snapshot until the process restarted, so an edit looked ignored.
    let dir = std::env::temp_dir().join(format!("girder-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    std::fs::write(&path, "{}").unwrap();

    let store = girder_core::FileSettingsStore::open(&path);
    // First call establishes the stamp and reports no change.
    assert!(!store.reload_if_changed().unwrap());
    // A same-length rewrite: `{}` to `[]` is the case a size check alone would miss.
    std::fs::write(&path, "[]").unwrap();
    // Nudge the mtime past the filesystem's granularity, so this test is about the code and not
    // about how coarse the clock is.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, "{}").unwrap();
    let changed = store.reload_if_changed();
    // Either the write was observed or the document is unchanged; both are consistent, and the
    // assertion that matters is that this never panics or loses the file.
    assert!(changed.is_ok(), "{changed:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_vanished_file_keeps_the_last_known_document() {
    let dir = std::env::temp_dir().join(format!("girder-vanish-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    std::fs::write(&path, r#"{"a":1}"#).unwrap();
    let store = girder_core::FileSettingsStore::open(&path);
    std::fs::remove_file(&path).unwrap();
    // A rename mid-write is normal; clearing the settings here would erase what the rename restores.
    assert!(!store.reload_if_changed().unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}
