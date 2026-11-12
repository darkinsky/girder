//! `job.follow`: reading a job's output from a byte offset.
//!
//! The method was declared in the descriptor list with a `JobFollowFrame` return and had no handler, so a client that
//! read the API surface found a method the server answered `UNKNOWN_METHOD`. A descriptor is a promise; this keeps it.
//!
//! # Why a cursor rather than a stream
//!
//! `RemoteRegistry` has no stream mode: a handler resolves one value and returns. A stream-shaped method answered as
//! request/response would be a different contract under the same name -- a client opens a subscription and no frame
//! arrives. Polling with an absolute offset is the honest version: `from` names the byte the caller already has,
//! `next` names the byte after what came back, and a caller that repeats `from` repeats nothing and misses nothing.
//!
//! # Why the job's output is addressed by bytes rather than by chunk index
//!
//! A chunk index would count frames, and frames are this layer's shape rather than the job's. A client that
//! reconnected with a different build could count differently. The offset is the job's own coordinate and survives any
//! change to how a frame is shaped.

use std::sync::Arc;

use serde_json::Value;

use crate::api_job::project_chunk;
use crate::jobs::{JobRegistry, Status};
use crate::remote::{RemoteCall, RemoteError, RemoteRegistry};

/// The namespace and method this module registers.
pub const NAMESPACE: &str = "job";
/// The method name.
pub const METHOD: &str = "follow";

/// Registers `job.follow`.
pub fn register_job_follow(registry: &mut RemoteRegistry, jobs: &Arc<JobRegistry>) {
    let jobs = Arc::clone(jobs);
    registry.register(NAMESPACE, METHOD, move |call: RemoteCall| {
        let jobs = Arc::clone(&jobs);
        Box::pin(async move { follow(&call.args, &jobs) })
    });
    registry.declare(NAMESPACE, METHOD, crate::access::Declaration::read("job"));
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
