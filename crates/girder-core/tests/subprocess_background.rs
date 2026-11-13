//! A command that starts background work must not leave it behind, and must not block on it.
//!
//! Measured before the fix: a leader running `sleep 3 &` exited in about 3 milliseconds while the drain blocked for the
//! full 3 seconds, because the background child inherited the stdout pipe. With `sleep 999 &` the drain never returned.

use std::time::{Duration, Instant};

use girder_core::SpawnOptions;

fn options(script: &str, timeout: Duration) -> SpawnOptions {
    let mut options = SpawnOptions::new("sh", vec!["-c".to_owned(), script.to_owned()]);
    options.timeout = timeout;
    options.new_process_group = true;
    options
}

#[tokio::test]
async fn a_background_child_does_not_block_the_drain() {
    // The property is the elapsed time, not just the exit: a drain that waited for the background process would return
    // the right output eventually, and much too late.
    let started = Instant::now();

    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        girder_core::run_process(
            options("sleep 30 & echo started", Duration::from_secs(8)),
            tokio_util::sync::CancellationToken::new(),
        ),
    )
    .await
    .expect("the command returns within the outer bound")
    .expect("the command runs");

    assert!(outcome.stdout.contains("started"), "{:?}", outcome.stdout);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the drain did not wait for the background child: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_background_child_is_gone_afterwards() {
    // Writing the pid to a file lets the test ask the kernel whether that pid still exists, which is the only way to
    // tell "killed" from "abandoned but lucky".
    let dir = std::env::temp_dir().join(format!("girder-bg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    let marker = dir.join("pid");
    let script = format!("sleep 30 & echo $! > {}; echo done", marker.display());

    girder_core::run_process(
        options(&script, Duration::from_secs(8)),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("the command runs");

    let pid: i32 = std::fs::read_to_string(&marker)
        .expect("the pid was written")
        .trim()
        .parse()
        .expect("a number");
    // Signal 0 asks whether it exists without affecting it.
    let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
    assert!(!alive, "the background child {pid} is still running");
    let _ = std::fs::remove_dir_all(&dir);
}
