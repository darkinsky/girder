// ---- environment clearing ---------------------------------------------------------------------

#[tokio::test]
async fn a_cleared_environment_hides_the_parent_variable() {
    // A command a model asked to run must not inherit a credential the harness was launched with.
    let cancel = tokio_util::sync::CancellationToken::new();
    let options =
        girder_core::SpawnOptions::new("sh", vec!["-c".into(), "echo $GIRDER_SECRET".into()])
            .with_env("GIRDER_SEcret_MARKER", "should-not-appear");

    let outcome = girder_core::spawn_process(options, cancel)
        .await
        .expect("ran");
    assert_eq!(outcome.stdout.trim(), "", "the added variable is ignored");
}

#[tokio::test]
async fn an_ordinary_environment_is_inherited_by_default() {
    // Off by default: a command usually needs the environment it was started with.
    let cancel = tokio_util::sync::CancellationToken::new();
    let options = girder_core::SpawnOptions::new("sh", vec!["-c".into(), "echo $HOME".into()]);

    let outcome = girder_core::spawn_process(options, cancel)
        .await
        .expect("ran");
    assert!(!outcome.stdout.trim().is_empty(), "HOME is inherited");
}

#[tokio::test]
async fn an_added_variable_reaches_an_uncleared_child() {
    let cancel = tokio_util::sync::CancellationToken::new();
    let options = girder_core::SpawnOptions::new("sh", vec!["-c".into(), "echo $MARKER".into()])
        .with_env("MARKER", "visible");

    let outcome = girder_core::spawn_process(options, cancel)
        .await
        .expect("ran");
    assert_eq!(outcome.stdout.trim(), "visible");
}

#[tokio::test]
async fn a_cleared_child_still_sees_what_was_added_explicitly() {
    let cancel = tokio_util::sync::CancellationToken::new();
    let options = girder_core::SpawnOptions::new("sh", vec!["-c".into(), "echo $MARKER".into()])
        .with_env("MARKER", "visible")
        .with_cleared_environment();

    let outcome = girder_core::spawn_process(options, cancel)
        .await
        .expect("ran");
    assert_eq!(
        outcome.stdout.trim(),
        "visible",
        "env_clear must run before the additions"
    );
}
