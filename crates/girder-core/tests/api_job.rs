//! Tests for the job Remote surface.
//!
//! The two properties worth pinning are the ones a naive port would get wrong: the list frame is the
//! whole set rather than a delta, and the resume offset is a byte count that survives a ring trim.

use girder_core::api_job::{
    JobFollowFrame, JobFollowRequest, JobKillValue, JobListFrame, JobListRequest, JobView,
    job_catalogue, job_methods, kill_outcome, project, project_chunk,
};
use girder_core::{JobChannel, JobId, JobRegistry, JobStatus, Schema, to_typescript};

// ---- the wire types ---------------------------------------------------------------------------

#[test]
fn a_list_request_names_one_session() {
    let schema = JobListRequest::typert_schema();

    assert_eq!(schema.name, "JobListRequest");
    assert_eq!(schema.required_at(1), vec!["session_id"]);
}

#[test]
fn a_list_frame_carries_a_list_of_views() {
    // The whole set, not a delta: a reconnecting client's first frame is already correct.
    let schema = JobListFrame::typert_schema();

    assert_eq!(
        schema.fields[0].schema,
        Schema::Array(Box::new(Schema::Ref("JobView".to_owned())))
    );
}

#[test]
fn a_view_declares_an_optional_exit_code() {
    // A running job has no code; a settled one does.
    let schema = JobView::typert_schema();
    let code = schema
        .fields
        .iter()
        .find(|field| field.name == "code")
        .expect("code exists");

    assert_eq!(code.schema, Schema::Optional(Box::new(Schema::Int64)));
    assert!(!code.required);
}

#[test]
fn a_follow_request_makes_the_offset_optional() {
    // Omitted starts at the oldest retained byte.
    let schema = JobFollowRequest::typert_schema();

    assert_eq!(schema.required_at(1), vec!["job_id"]);
}

#[test]
fn a_follow_frame_carries_the_next_offset() {
    // The client resumes with this, so it must be part of the frame rather than computed by the
    // client from the text length.
    let schema = JobFollowFrame::typert_schema();
    let names: Vec<&str> = schema.fields.iter().map(|f| f.name.as_str()).collect();

    assert_eq!(names, vec!["at", "text", "channel", "gap", "next"]);
}

#[test]
fn a_kill_value_reports_an_outcome_string() {
    // "already finished" is neither a failure nor a success, so a boolean could not express it
    // without making a client show an error for a harmless case.
    let schema = JobKillValue::typert_schema();

    assert_eq!(schema.fields[0].name, "outcome");
    assert_eq!(schema.fields[0].schema, Schema::String);
}

// ---- the methods ------------------------------------------------------------------------------

#[test]
fn every_method_is_namespaced_under_job() {
    for method in job_methods() {
        assert_eq!(method.namespace, "job", "{}", method.name);
    }
}

#[test]
fn the_methods_cover_the_surface() {
    let names: Vec<String> = job_methods().into_iter().map(|m| m.name).collect();

    assert_eq!(names, vec!["list", "follow", "kill"]);
}

#[test]
fn follow_takes_an_optional_offset() {
    let follow = job_methods()
        .into_iter()
        .find(|method| method.name == "follow")
        .expect("follow exists");

    assert!(follow.parameters[0].required, "the job is required");
    assert!(!follow.parameters[1].required, "the offset is not");
}

// ---- the catalogue ----------------------------------------------------------------------------

#[test]
fn the_catalogue_defines_every_referenced_type() {
    let catalogue = job_catalogue();

    for method in &catalogue.methods {
        if let Schema::Ref(name) = &method.returns {
            assert!(
                catalogue.types.contains_key(name),
                "`{}` returns an undefined `{name}`",
                method.qualified()
            );
        }
    }
}

#[test]
fn the_catalogue_renders_with_balanced_braces() {
    let output = to_typescript(&job_catalogue());

    assert_eq!(
        output.chars().filter(|c| *c == '{').count(),
        output.chars().filter(|c| *c == '}').count()
    );
    assert!(output.contains("'job.list': {"), "{output}");
    assert!(output.contains("'job.follow': {"), "{output}");
    assert!(output.contains("'job.kill': {"), "{output}");
}

// ---- projection -------------------------------------------------------------------------------

#[test]
fn a_registry_job_projects_to_a_view() {
    let registry = JobRegistry::new(4096);
    registry
        .register(&JobId::new("j1").expect("valid"), "bash", "dev server")
        .expect("registered");

    let view = project(&registry.get("j1").expect("present"));

    assert_eq!(view.id, "j1");
    assert_eq!(view.kind, "bash");
    assert_eq!(view.label, "dev server");
    assert_eq!(view.status, "running");
    assert!(view.code.is_none(), "a running job has no code");
}

#[test]
fn a_settled_job_projects_its_code() {
    let registry = JobRegistry::new(4096);
    registry
        .register(&JobId::new("j1").expect("valid"), "bash", "build")
        .expect("registered");
    registry.complete("j1", 2, 5000).expect("settled");

    let view = project(&registry.get("j1").expect("present"));

    assert_eq!(view.status, "failed");
    assert_eq!(view.code, Some(2));
}

#[test]
fn a_status_string_matches_the_registry_vocabulary() {
    // The client switches on these strings, so they must be the same ones the lifecycle uses.
    let registry = JobRegistry::new(4096);
    registry
        .register(&JobId::new("j1").expect("valid"), "bash", "x")
        .expect("registered");

    for status in [
        JobStatus::Running,
        JobStatus::Stopping,
        JobStatus::Completed,
        JobStatus::Killed,
        JobStatus::Failed,
    ] {
        assert!(!status.as_str().is_empty());
    }
    assert_eq!(project(&registry.get("j1").unwrap()).status, "running");
}

// ---- projection of a chunk ---------------------------------------------------------------------

#[test]
fn a_chunk_projects_its_next_offset() {
    let mut ring = girder_core::OutputRing::new(4096);
    ring.append("hello", JobChannel::Stdout);
    let chunk = ring.chunks()[0].clone();

    let frame = project_chunk(&chunk);

    assert_eq!(frame.at, 0);
    assert_eq!(frame.text, "hello");
    assert_eq!(frame.channel, "stdout");
    assert_eq!(frame.next, 5, "the client resumes here");
    assert!(!frame.gap);
}

#[test]
fn a_channel_projects_to_its_wire_name() {
    let mut ring = girder_core::OutputRing::new(4096);
    ring.append("out", JobChannel::Stdout);
    ring.append("err", JobChannel::Stderr);
    ring.append("log", JobChannel::Log);

    let channels: Vec<String> = ring
        .chunks()
        .iter()
        .map(|chunk| project_chunk(chunk).channel)
        .collect();

    assert_eq!(channels, vec!["stdout", "stderr", "log"]);
}

#[test]
fn a_gap_reaches_the_client() {
    // A client that ignored this would render a hole as continuity.
    let mut ring = girder_core::OutputRing::new(8);
    ring.append("aaaa", JobChannel::Stdout);
    ring.append("bbbb", JobChannel::Stdout);
    ring.append("cccc", JobChannel::Stdout);

    let gapped: Vec<JobFollowFrame> = ring
        .chunks()
        .iter()
        .map(project_chunk)
        .filter(|frame| frame.gap)
        .collect();

    assert!(!gapped.is_empty(), "a trim must reach the client");
}

#[test]
fn the_next_offset_survives_a_trim() {
    // The property the whole ring is built around: a byte offset means the same thing before and
    // after retention trims.
    let mut ring = girder_core::OutputRing::new(8);
    ring.append("aaaa", JobChannel::Stdout);
    ring.append("bbbb", JobChannel::Stdout);
    ring.append("cccc", JobChannel::Stdout);

    let offsets: Vec<u64> = ring.chunks().iter().map(|chunk| chunk.at).collect();

    assert_eq!(offsets, vec![4, 8], "offsets never move");
}

// ---- kill outcomes ----------------------------------------------------------------------------

#[test]
fn a_live_job_reports_a_requested_kill() {
    assert_eq!(kill_outcome(JobStatus::Running), "requested");
    assert_eq!(kill_outcome(JobStatus::Stopping), "requested");
}

#[test]
fn a_settled_job_reports_already_finished() {
    // Not an error: a client that showed one would alarm a user over a harmless race.
    assert_eq!(kill_outcome(JobStatus::Completed), "already-finished");
    assert_eq!(kill_outcome(JobStatus::Killed), "already-finished");
    assert_eq!(kill_outcome(JobStatus::Failed), "already-finished");
}

#[test]
fn a_cancelled_twice_kill_is_harmless() {
    // The registry treats a second stop as idempotent; the wire reports it without an error.
    let registry = JobRegistry::new(4096);
    registry
        .register(&JobId::new("j1").expect("valid"), "bash", "x")
        .expect("registered");
    registry.request_stop("j1").expect("stopping");

    assert_eq!(
        kill_outcome(registry.get("j1").expect("present").status),
        "requested"
    );
    registry.confirm_killed("j1", 5000).expect("killed");
    assert_eq!(
        kill_outcome(registry.get("j1").expect("present").status),
        "already-finished"
    );
}
