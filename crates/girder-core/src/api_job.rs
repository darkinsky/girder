//! The job Remote surface: listing, reading, and stopping background jobs.
//!
//! The `jobs` module owns the lifecycle; this is what a client sees of it. Two things are worth
//! stating because they are the parts a client depends on and a naive port would get wrong.
//!
//! # Why a list frame carries the whole set
//!
//! `job.list` sends every job a session can see, not a delta. A reconnect's first frame is then
//! already the truth: with a delta the client would have to have seen every prior frame, and a
//! reconnect that missed one renders a list that is wrong in a way nothing detects.
//!
//! # Why the offset is a byte count rather than a chunk index
//!
//! A client resumes a stream with `from`. A chunk index would drift the moment retention trims the
//! ring — the same chunk gets a different index — while a byte offset means the same thing before and
//! after a trim. That is the property [`crate::jobs::OutputRing`] is built around.

use crate::typert::{Catalogue, Method, ObjectSchema, Parameter, Schema};

/// Asks for the jobs one session can see.
#[derive(Debug, Clone, crate::Typert)]
pub struct JobListRequest {
    /// The session whose list to read.
    pub session_id: String,
}

/// One job, as a client renders it.
#[derive(Debug, Clone, crate::Typert)]
pub struct JobView {
    /// Its id.
    pub id: String,
    /// Its producer kind, for an icon or a label.
    pub kind: String,
    /// Its human-readable label.
    pub label: String,
    /// Where it is: `running`, `stopping`, `completed`, `killed`, or `failed`.
    pub status: String,
    /// Its exit code, once settled.
    pub code: Option<i64>,
}

/// The complete set of jobs after any lifecycle change.
#[derive(Debug, Clone, crate::Typert)]
pub struct JobListFrame {
    /// The jobs, in id order.
    ///
    /// The whole set rather than a delta: a reconnecting client's first frame is already correct.
    pub jobs: Vec<JobView>,
}

/// Asks to read one job's output from an absolute byte offset.
#[derive(Debug, Clone, crate::Typert)]
pub struct JobFollowRequest {
    /// The job.
    pub job_id: String,
    /// The byte offset to resume from; omitted starts at the oldest retained byte.
    ///
    /// A byte count rather than a chunk index: retention trims the ring, and an index would then
    /// mean something different than it did before the trim.
    pub from: Option<u64>,
}

/// One chunk of a job's output.
#[derive(Debug, Clone, crate::Typert)]
pub struct JobFollowFrame {
    /// The absolute offset of this chunk's first byte.
    pub at: u64,
    /// The text.
    pub text: String,
    /// Which stream: `stdout`, `stderr`, or `log`.
    pub channel: String,
    /// Whether bytes just before this chunk were lost.
    ///
    /// A client that ignored this would render a hole as continuity.
    pub gap: bool,
    /// The offset to resume from next.
    pub next: u64,
}

/// Asks to stop one job.
#[derive(Debug, Clone, crate::Typert)]
pub struct JobKillRequest {
    /// The session whose list carries the job.
    pub session_id: String,
    /// The job.
    pub job_id: String,
}

/// The receipt after a kill is accepted.
#[derive(Debug, Clone, crate::Typert)]
pub struct JobKillValue {
    /// Whether a stop was requested, or the job had already finished.
    ///
    /// A string rather than a boolean: "already finished" is not a failure and not a success, and a
    /// client that could not tell them apart would show an error for a harmless case.
    pub outcome: String,
}

/// The job subsystem's method descriptors.
#[must_use]
pub fn job_methods() -> Vec<Method> {
    vec![
        Method::new("job", "list")
            .with_parameter(Parameter::required("sessionId", Schema::String))
            .returning(Schema::Ref("JobListFrame".to_owned()))
            .with_docs("Lists every job a session can see."),
        Method::new("job", "follow")
            .with_parameter(Parameter::required("jobId", Schema::String))
            .with_parameter(Parameter::optional(
                "from",
                Schema::Optional(Box::new(Schema::Int64)),
            ))
            .returning(Schema::Ref("JobFollowFrame".to_owned()))
            .with_docs("Streams one job's output from an absolute byte offset."),
        Method::new("job", "kill")
            .with_parameter(Parameter::required("sessionId", Schema::String))
            .with_parameter(Parameter::required("jobId", Schema::String))
            .returning(Schema::Ref("JobKillValue".to_owned()))
            .with_docs("Stops one background job."),
    ]
}

/// The job subsystem's catalogue.
#[must_use]
pub fn job_catalogue() -> Catalogue {
    let mut catalogue = Catalogue::new(1);
    catalogue.define(name(JobListRequest::typert_schema(), "JobListRequest"));
    catalogue.define(name(JobView::typert_schema(), "JobView"));
    catalogue.define(name(JobListFrame::typert_schema(), "JobListFrame"));
    catalogue.define(name(JobFollowRequest::typert_schema(), "JobFollowRequest"));
    catalogue.define(name(JobFollowFrame::typert_schema(), "JobFollowFrame"));
    catalogue.define(name(JobKillRequest::typert_schema(), "JobKillRequest"));
    catalogue.define(name(JobKillValue::typert_schema(), "JobKillValue"));
    for method in job_methods() {
        catalogue.add(method);
    }
    catalogue
}

/// Asserts the derive's name matches the reference name, then files it.
///
/// A mismatch would make every `$ref` to it wrong, and a client would fail at runtime with nothing
/// pointing at the cause.
fn name(mut schema: ObjectSchema, expected: &str) -> ObjectSchema {
    assert_eq!(
        schema.name, expected,
        "derive and reference names must agree"
    );
    schema.name.clear();
    schema.name.push_str(expected);
    schema
}

/// Projects a registry job into the wire view.
///
/// A free function rather than a method on `Job`: the registry type has no business knowing the wire
/// shape, and a change to the wire format should touch this file only.
#[must_use]
pub fn project(job: &crate::jobs::Job) -> JobView {
    JobView {
        id: job.id.to_string(),
        kind: job.kind.clone(),
        label: job.label.clone(),
        status: job.status.as_str().to_owned(),
        code: job.code.map(i64::from),
    }
}

/// Projects one chunk into a wire frame, given the offset to resume from next.
#[must_use]
pub fn project_chunk(chunk: &crate::jobs::JobChunk) -> JobFollowFrame {
    let channel = match chunk.channel {
        crate::jobs::Channel::Stdout => "stdout",
        crate::jobs::Channel::Stderr => "stderr",
        crate::jobs::Channel::Log => "log",
    };
    JobFollowFrame {
        at: chunk.at,
        text: chunk.text.clone(),
        channel: channel.to_owned(),
        gap: chunk.gap,
        next: chunk
            .at
            .saturating_add(u64::try_from(chunk.text.len()).unwrap_or(0)),
    }
}

/// The outcome string for a kill request.
///
/// Pure so a caller can decide without touching the registry, and so the two values are testable.
#[must_use]
pub fn kill_outcome(status: crate::jobs::Status) -> &'static str {
    if status.is_terminal() {
        "already-finished"
    } else {
        "requested"
    }
}
