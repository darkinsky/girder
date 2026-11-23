//! Tests for `job.follow`, the method whose descriptor existed before its handler.
//!
//! The property is the same one `session.follow` has: a cursor resumes exactly. `from` names the byte the caller
//! already has, so a poll that repeats it repeats nothing and a poll with `next` misses nothing.

use std::sync::Arc;
use std::time::Duration;

use girder_core::{JobId, JobRegistry, RemoteCall, RemoteError, RemoteRegistry};
use serde_json::json;

fn scratch() -> Arc<JobRegistry> {
    Arc::new(JobRegistry::new(64 * 1024))
}

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

fn registry(jobs: &Arc<JobRegistry>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    girder_core::register_job_follow(&mut registry, jobs);
    registry
}

async fn dispatch_follow(
    registry: &RemoteRegistry,
    args: serde_json::Value,
) -> Result<serde_json::Value, RemoteError> {
    let request = call(args);
    let handler = registry.stream_method("job", "follow", &request)?;
    let cancel = tokio_util::sync::CancellationToken::new();
    let (open, mut frames) = girder_core::StreamOpen::pair(
        request,
        Arc::new(girder_core::api::extra::UplinkInbox::new(
            1024,
            "job/follow",
        )),
        cancel.clone(),
    );
    let running = tokio::spawn(async move {
        handler(open).await;
    });
    let mut result = json!({
        "frames": [],
        "next": 0,
        "lost": false,
        "done": false,
    });
    loop {
        let Some(frame) = tokio::time::timeout(Duration::from_millis(100), frames.recv())
            .await
            .ok()
            .flatten()
        else {
            cancel.cancel();
            let _ = running.await;
            return Ok(result);
        };
        match frame {
            Ok(frame) => match frame["type"].as_str() {
                Some("opened") => result["next"] = frame["from"].clone(),
                Some("output") => {
                    result["frames"] = frame["chunks"].clone();
                    result["next"] = frame["next"].clone();
                    result["lost"] = frame["lossy"].as_bool().unwrap_or(false).into();
                }
                Some("status") => {
                    result["done"] = true.into();
                    result["status"] = frame["job"]["status"].clone();
                }
                _ => {}
            },
            Err(error) => {
                cancel.cancel();
                let _ = running.await;
                return Err(error);
            }
        }
    }
}

/// Registers a job and appends `lines` one line at a time.
fn seeded(lines: &[&str]) -> (Arc<JobRegistry>, String) {
    let jobs = scratch();
    let id = JobId::new("job-1").expect("valid");
    jobs.register(&id, "test", "a job").expect("registered");
    for line in lines {
        jobs.append("job-1", *line, girder_core::JobChannel::Stdout)
            .expect("appended");
    }
    (jobs, "job-1".to_owned())
}

#[tokio::test]
async fn a_first_poll_returns_the_whole_output() {
    let (jobs, id) = seeded(&["one\n", "two\n"]);

    let registry = registry(&jobs);
    let value = dispatch_follow(&registry, json!({"jobId": id}))
        .await
        .expect("follow");

    let frames = value["frames"].as_array().expect("frames");
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["text"], "one\n");
    let _ = value["next"].as_u64().expect("a cursor");
}

#[tokio::test]
async fn a_cursor_resumes_exactly() {
    // The whole point: `next` from one poll is `from` for the next, with no overlap and no gap.
    let (jobs, id) = seeded(&["one\n"]);
    let registry = registry(&jobs);

    let first = dispatch_follow(&registry, json!({"jobId": id}))
        .await
        .expect("follow");
    assert_eq!(first["frames"].as_array().map(Vec::len), Some(1));
    let next = first["next"].as_u64().expect("a cursor");

    jobs.append("job-1", "two\n", girder_core::JobChannel::Stdout)
        .expect("appended");
    let second = dispatch_follow(&registry, json!({"jobId": id, "from": next}))
        .await
        .expect("follow");

    let frames = second["frames"].as_array().expect("frames");
    assert_eq!(frames.len(), 1, "only the new line: {frames:?}");
    assert_eq!(frames[0]["text"], "two\n");
}

#[tokio::test]
async fn polling_past_the_end_returns_nothing_new() {
    let (jobs, id) = seeded(&["only\n"]);

    let registry = registry(&jobs);
    let value = dispatch_follow(&registry, json!({"jobId": id, "from": 99_999}))
        .await
        .expect("follow");

    assert_eq!(value["frames"].as_array().map(Vec::len), Some(0));
    assert_eq!(value["next"], 99_999, "the cursor does not move backwards");
}

#[tokio::test]
async fn a_finished_job_ends_the_subscription() {
    // Without this a client polls a finished job forever, which looks exactly like a hang.
    let (jobs, id) = seeded(&["done\n"]);
    jobs.complete("job-1", 0, 1).expect("settled");

    let registry = registry(&jobs);
    let value = dispatch_follow(&registry, json!({"jobId": id}))
        .await
        .expect("follow");

    assert_eq!(value["done"], true);
    assert_eq!(value["status"], "completed");
}

#[tokio::test]
async fn a_running_job_does_not_end_the_subscription() {
    let (jobs, id) = seeded(&["working\n"]);

    let registry = registry(&jobs);
    let value = dispatch_follow(&registry, json!({"jobId": id}))
        .await
        .expect("follow");

    assert_eq!(value["done"], false);
}

#[tokio::test]
async fn an_unknown_job_is_an_argument_error() {
    let jobs = scratch();

    let registry = registry(&jobs);
    let error = dispatch_follow(&registry, json!({"jobId": "absent"}))
        .await
        .expect_err("unknown job");

    assert!(
        matches!(error, RemoteError::InvalidArguments { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_missing_job_id_is_refused() {
    let jobs = scratch();

    let registry = registry(&jobs);
    let error = dispatch_follow(&registry, json!({}))
        .await
        .expect_err("missing id");

    assert!(matches!(error, RemoteError::InvalidArguments { .. }));
}

#[tokio::test]
async fn an_absent_cursor_means_the_beginning() {
    // Zero and absent must agree, or a caller that typed `from: 0` would get a different answer from one that omitted it.
    let (jobs, id) = seeded(&["a\n", "b\n"]);
    let registry = registry(&jobs);

    let absent = dispatch_follow(&registry, json!({"jobId": id}))
        .await
        .expect("follow");
    let zero = dispatch_follow(&registry, json!({"jobId": id, "from": 0}))
        .await
        .expect("follow");

    assert_eq!(absent["frames"], zero["frames"]);
}

#[tokio::test]
async fn a_dropped_head_is_reported() {
    // `lost` means the ring discarded data before the caller's position, so a client resuming a log learns it missed
    // output rather than silently receiving a gap.
    let jobs = Arc::new(JobRegistry::new(8));
    let id = JobId::new("job-1").expect("valid");
    jobs.register(&id, "test", "a small ring")
        .expect("registered");
    for line in ["aaaa\n", "bbbb\n", "cccc\n"] {
        jobs.append("job-1", line, girder_core::JobChannel::Stdout)
            .expect("appended");
    }

    let registry = registry(&jobs);
    let value = dispatch_follow(&registry, json!({"jobId": "job-1", "from": 0}))
        .await
        .expect("follow");

    assert_eq!(value["lost"], true, "the ring dropped the head: {value}");
}

#[tokio::test]
async fn the_stream_follow_opens_outputs_and_then_status() {
    let (jobs, id) = seeded(&["one\n"]);
    let registry = registry(&jobs);
    let request = call(json!({"jobId": id}));
    let handler = registry
        .stream_method("job", "follow", &request)
        .expect("follow is a stream");
    let (open, mut frames) = girder_core::StreamOpen::pair(
        request,
        Arc::new(girder_core::api::extra::UplinkInbox::new(
            1024,
            "job/follow",
        )),
        tokio_util::sync::CancellationToken::new(),
    );
    let running = tokio::spawn(async move {
        handler(open).await;
    });

    let opened = tokio::time::timeout(Duration::from_secs(1), frames.recv())
        .await
        .expect("opened")
        .expect("stream")
        .expect("frame");
    assert_eq!(opened["type"], "opened");
    assert_eq!(opened["from"], 0);

    jobs.complete("job-1", 0, 1).expect("completed");
    let output = tokio::time::timeout(Duration::from_secs(1), frames.recv())
        .await
        .expect("output")
        .expect("stream")
        .expect("frame");
    assert_eq!(output["type"], "output");
    assert_eq!(output["chunks"][0]["text"], "one\n");
    let status = tokio::time::timeout(Duration::from_secs(1), frames.recv())
        .await
        .expect("status")
        .expect("stream")
        .expect("frame");
    assert_eq!(status["type"], "status");
    assert_eq!(status["job"]["status"], "completed");
    running.abort();
}

#[tokio::test]
async fn the_job_list_stream_replaces_rows_after_a_status_change() {
    let jobs = scratch();
    let id = JobId::new("job-1").expect("valid");
    jobs.register(&id, "test", "a job").expect("registered");
    let mut registry = RemoteRegistry::new();
    girder_core::register_job_methods(&mut registry, &jobs);
    let request = call(json!({"sessionId": "session-1"}));
    let handler = registry
        .stream_method("job", "list", &request)
        .expect("list is a stream");
    let cancel = tokio_util::sync::CancellationToken::new();
    let (open, mut frames) = girder_core::StreamOpen::pair(
        request,
        Arc::new(girder_core::api::extra::UplinkInbox::new(1024, "job/list")),
        cancel.clone(),
    );
    let running = tokio::spawn(async move {
        handler(open).await;
    });
    let first = tokio::time::timeout(Duration::from_secs(1), frames.recv())
        .await
        .expect("rows")
        .expect("stream")
        .expect("frame");
    assert_eq!(first["type"], "rows");
    assert_eq!(first["jobs"][0]["status"], "running");

    jobs.complete("job-1", 0, 1).expect("completed");
    let second = tokio::time::timeout(Duration::from_secs(1), frames.recv())
        .await
        .expect("changed rows")
        .expect("stream")
        .expect("frame");
    assert_eq!(second["type"], "rows");
    assert_eq!(second["jobs"][0]["status"], "completed");
    cancel.cancel();
    let _ = running.await;
}
