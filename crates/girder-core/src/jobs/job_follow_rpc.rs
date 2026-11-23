//! `job.follow`: reading a job's output from a byte offset.
//!
//! The standard Remote mux receives an `opened` anchor, output batches, and a terminal `status` frame.
//! The existing cursor-based projection remains available to code that reads the job model directly:
//! `from` names the byte already held and `next` names the next byte to request.
//!
//! # Why the job's output is addressed by bytes rather than by chunk index
//!
//! A chunk index would count frames, and frames are this layer's shape rather than the job's. A client that
//! reconnected with a different build could count differently. The offset is the job's own coordinate and survives any
//! change to how a frame is shaped.

use std::sync::Arc;

use serde_json::Value;

use crate::api::job::{project, project_chunk};
use crate::api::remote::{RemoteCall, RemoteError, RemoteRegistry};
use crate::jobs::{JobRegistry, Status};

/// The namespace and method this module registers.
pub const NAMESPACE: &str = "job";
/// The method name.
pub const METHOD: &str = "follow";

/// Registers `job.follow`.
pub fn register_job_follow(registry: &mut RemoteRegistry, jobs: &Arc<JobRegistry>) {
    let unary_jobs = Arc::clone(jobs);
    registry.register(NAMESPACE, METHOD, move |call: RemoteCall| {
        let jobs = Arc::clone(&unary_jobs);
        Box::pin(async move { follow(&call.args, &jobs) })
    });
    let jobs = Arc::clone(jobs);
    registry.register_stream(NAMESPACE, METHOD, move |open| {
        let jobs = Arc::clone(&jobs);
        Box::pin(async move { follow_stream(open, jobs).await })
    });
    registry.declare(
        NAMESPACE,
        METHOD,
        crate::guard::access::Declaration::read("job"),
    );
}

async fn follow_stream(open: girder_core::StreamOpen, jobs: Arc<JobRegistry>) {
    let Some(job_id) = open.call.args.get("jobId").and_then(Value::as_str) else {
        let _ = open.fail(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: METHOD.to_owned(),
            message: "`jobId` is required".to_owned(),
        });
        return;
    };
    let job_id = job_id.to_owned();
    let job = match jobs.get(&job_id) {
        Ok(job) => job,
        Err(error) => {
            let _ = open.fail(RemoteError::InvalidArguments {
                namespace: NAMESPACE.to_owned(),
                method: METHOD.to_owned(),
                message: error.to_string(),
            });
            return;
        }
    };
    let mut cursor = open
        .call
        .args
        .get("from")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| job.output.head_offset());
    let opened = serde_json::json!({
        "type": "opened",
        "job": job_view(&job),
        "from": cursor,
    });
    if !open.push(opened) {
        return;
    }

    loop {
        let job = match jobs.get(&job_id) {
            Ok(job) => job,
            Err(_) => return,
        };
        let (chunks, lost) = job.output.read_from(cursor);
        if !chunks.is_empty() {
            let next = chunks.last().map_or(cursor, |chunk| {
                chunk
                    .at
                    .saturating_add(u64::try_from(chunk.text.len()).unwrap_or(0))
            });
            let frame = serde_json::json!({
                "type": "output",
                "chunks": chunks.iter().map(project_chunk_value).collect::<Vec<_>>(),
                "next": next,
                "lossy": lost.then_some(true),
            });
            cursor = next;
            if !open.push(frame) {
                return;
            }
            continue;
        }
        if job.status.is_terminal() {
            if open.push(serde_json::json!({
                "type": "status",
                "job": job_view(&job),
            })) {
                return;
            }
            return;
        }

        tokio::select! {
            () = open.cancel.cancelled() => return,
            incoming = open.uplink.recv() => {
                if !matches!(incoming, Ok(Some(_))) {
                    return;
                }
            }
            () = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }
}

fn job_view(job: &crate::jobs::Job) -> serde_json::Value {
    let view = project(job);
    serde_json::json!({
        "id": view.id,
        "kind": view.kind,
        "label": view.label,
        "status": view.status,
        "code": view.code,
    })
}

fn project_chunk_value(chunk: &crate::jobs::JobChunk) -> serde_json::Value {
    let frame = project_chunk(chunk);
    serde_json::json!({
        "at": frame.at,
        "text": frame.text,
        "channel": frame.channel,
        "gap": frame.gap,
    })
}

/// Runs one poll.
fn follow(args: &Value, jobs: &JobRegistry) -> Result<Value, RemoteError> {
    let job_id =
        args.get("jobId")
            .and_then(Value::as_str)
            .ok_or_else(|| RemoteError::InvalidArguments {
                namespace: NAMESPACE.to_owned(),
                method: METHOD.to_owned(),
                message: "`jobId` is required".to_owned(),
            })?;
    // Absent means "from the beginning", which is what a client that just learned the id wants. Zero and absent agree.
    let from = args.get("from").and_then(Value::as_u64).unwrap_or(0);

    let job = jobs
        .get(job_id)
        .map_err(|error| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: METHOD.to_owned(),
            message: error.to_string(),
        })?;

    let (chunks, lost) = job.output.read_from(from);
    let frames: Vec<Value> = chunks
        .iter()
        .map(|chunk| {
            let frame = project_chunk(chunk);
            serde_json::json!({
                "at": frame.at,
                "channel": frame.channel,
                "text": frame.text,
            })
        })
        .collect();

    // The offset *after* the last byte returned, which is `at` plus the text's length -- not `at` itself. Using the
    // starting offset re-reads the final line on the next poll, and a test caught exactly that: a cursor that resumes
    // one line early repeats a line, which a client would render twice.
    //
    // A poll that returned nothing leaves the cursor where it was, so a caller cannot be sent backwards.
    let next = chunks.last().map_or(from, |chunk| {
        chunk.at + u64::try_from(chunk.text.len()).unwrap_or(0)
    });

    Ok(serde_json::json!({
        "jobId": job_id,
        "frames": frames,
        "next": next,
        // A job that has stopped and has nothing further to send ends the subscription. Without this a client polls a
        // finished job forever, which is the shape of a bug that looks like a hang.
        // `lost` means the ring dropped data before `from`, so the caller's position is no longer continuous. That is
        // reported rather than hidden: a client resuming a log wants to know it missed output.
        "lost": lost,
        "done": !matches!(job.status, Status::Running),
        "status": job.status.as_str(),
    }))
}
