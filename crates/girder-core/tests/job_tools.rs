//! Tests for the job tools.
//!
//! Each of the four has one property that a plausible implementation would get wrong:
//!
//! - `job_start` must hand back an id that is already registered, or the caller's next call fails on
//!   a race it cannot see
//! - `job_list` must carry the whole set, including settled jobs, or a caller cannot tell "finished"
//!   from "never started"
//! - `job_output` must report what it had to leave out; a silently shortened log reads as a short one
//! - `job_kill` must not claim `killed` while the process group is still alive
//!
//! Everything runs real processes through a real shell, because the property that separates a job
//! from a tool call — the caller reads while the command still runs — cannot be faked.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use girder_core::{
    JobRegistry, JobStatus, PermissionCatalogue, PermissionState, ToolDefinition, ToolResult,
    ToolRunContext, job_kill_tool, job_list_tool, job_output_tool, job_start_tool,
};
use tokio_util::sync::CancellationToken;

/// Names a scratch directory uniquely enough for tests that may run in one process at once.
fn scratch(name: &str) -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("girder-jt-{name}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    dir
}

/// The four tools over one registry, which is how a session mounts them.
struct Tools {
    registry: Arc<JobRegistry>,
    start: ToolDefinition,
    list: ToolDefinition,
    output: ToolDefinition,
    kill: ToolDefinition,
}

impl Tools {
    fn new() -> Self {
        Self::with_ring(64 * 1024)
    }

    /// The four tools over a ring of a chosen size.
    ///
    /// The size is a parameter because the two bounds that can shorten a read are independent: the
    /// ring drops the oldest bytes, and the tool drops what does not fit in one result. A fixed size
    /// would only ever exercise one of them.
    fn with_ring(ring_bytes: u64) -> Self {
        let registry = Arc::new(JobRegistry::new(ring_bytes));
        Self {
            start: job_start_tool(Arc::clone(&registry)),
            list: job_list_tool(Arc::clone(&registry)),
            output: job_output_tool(Arc::clone(&registry)),
            kill: job_kill_tool(Arc::clone(&registry)),
            registry,
        }
    }
}

/// The text a result renders to, whichever variant carried it.
fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            girder_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Pulls the id out of `job_start`'s receipt.
///
/// The tool returns prose rather than JSON because a model reads it; the test therefore goes through
/// the registry instead of parsing, which also proves the id it printed is a real one.
fn started_id(tools: &Tools, text: &str) -> String {
    let ids = tools.registry.ids();
    assert_eq!(
        ids.len(),
        1,
        "`job_start` must register exactly one job: {text}"
    );
    ids.into_iter().next().expect("one id")
}

/// Waits for a job to reach a terminal state, or panics after a bound.
async fn wait_settled(registry: &JobRegistry, id: &str) -> girder_core::Job {
    for _ in 0..200 {
        let job = registry.get(id).expect("registered");
        if job.is_settled() {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("job `{id}` did not settle");
}

// ---- job_start --------------------------------------------------------------------------------

#[tokio::test]
async fn starting_a_command_returns_a_registered_id() {
    let tools = Tools::new();

    let result = (tools.start.execute)(
        serde_json::json!({ "command": "echo hello" }),
        ToolRunContext::default(),
    )
    .await
    .expect("started");

    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    let id = started_id(&tools, &text);
    // The id in the receipt and the id in the registry must be the same one: a receipt naming an id
    // the registry does not hold is a caller's next call failing for no visible reason.
    assert!(text.contains(&id), "the receipt must name the job: {text}");

    let job = wait_settled(&tools.registry, &id).await;
    assert_eq!(job.status, JobStatus::Completed);
    assert!(
        job.output.text().contains("hello"),
        "{:?}",
        job.output.text()
    );
}

#[tokio::test]
async fn a_started_job_runs_in_the_session_workspace() {
    // `with_cwd` is easy to forget, and a command that silently ran in the harness's directory would
    // look like it worked — until it read or wrote the wrong file.
    let dir = scratch("cwd");
    std::fs::write(dir.join("marker.txt"), "in-the-workspace\n").expect("written");
    let tools = Tools::new();

    let result = (tools.start.execute)(
        serde_json::json!({ "command": "cat marker.txt" }),
        ToolRunContext::default().in_workspace(dir.clone()),
    )
    .await
    .expect("started");
    let id = started_id(&tools, &text_of(&result));
    let job = wait_settled(&tools.registry, &id).await;

    assert!(
        job.output.text().contains("in-the-workspace"),
        "{:?}",
        job.output.text()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn starting_without_a_command_is_an_error() {
    // An empty command would otherwise register a job that exits immediately, and the model would
    // think it had started something.
    let tools = Tools::new();

    for args in [
        serde_json::json!({}),
        serde_json::json!({ "command": "   " }),
    ] {
        let result = (tools.start.execute)(args, ToolRunContext::default())
            .await
            .expect("answered");
        assert!(result.is_error);
        assert!(text_of(&result).contains("command"));
    }
    assert!(tools.registry.ids().is_empty());
}

#[tokio::test]
async fn a_read_only_session_may_not_start_a_job() {
    // A background shell command is still a shell command. Refusing after the spawn would leave a
    // process running that the caller was told had been refused.
    let catalogue = PermissionCatalogue::builtin();
    let mut state = PermissionState::default();
    state.apply(&catalogue, "read-only").expect("applied");
    let ctx = ToolRunContext::default().with_permissions(Arc::new(state));

    let tools = Tools::new();
    let result = (tools.start.execute)(serde_json::json!({ "command": "sleep 5" }), ctx)
        .await
        .expect("answered");

    assert!(result.is_error);
    assert!(text_of(&result).contains("read-only"));
    assert!(tools.registry.ids().is_empty());
}

#[tokio::test]
async fn two_started_jobs_get_distinct_ids() {
    // The ids are the only handle a model has. Two jobs sharing one would make the second
    // unreadable and the first ambiguous.
    let tools = Tools::new();

    for command in ["sleep 0.2", "sleep 0.2"] {
        let result = (tools.start.execute)(
            serde_json::json!({ "command": command }),
            ToolRunContext::default(),
        )
        .await
        .expect("started");
        assert!(!result.is_error, "{}", text_of(&result));
    }

    let ids = tools.registry.ids();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_ne!(ids[0], ids[1]);
    for id in ids {
        let _ = wait_settled(&tools.registry, &id).await;
    }
}

// ---- job_list ---------------------------------------------------------------------------------

#[tokio::test]
async fn listing_reports_every_job_with_its_status() {
    let tools = Tools::new();
    let empty = (tools.list.execute)(serde_json::json!({}), ToolRunContext::default())
        .await
        .expect("listed");
    assert!(text_of(&empty).contains("no jobs"), "{}", text_of(&empty));

    let result = (tools.start.execute)(
        serde_json::json!({ "command": "echo listed" }),
        ToolRunContext::default(),
    )
    .await
    .expect("started");
    let id = started_id(&tools, &text_of(&result));
    wait_settled(&tools.registry, &id).await;

    let listed = (tools.list.execute)(serde_json::json!({}), ToolRunContext::default())
        .await
        .expect("listed");
    let text = text_of(&listed);
    assert!(!listed.is_error);

    // A settled job is still listed: a caller that saw only running jobs could not tell "finished"
    // from "never started" and would start it again.
    assert!(text.contains(&id), "{text}");
    assert!(text.contains("completed"), "{text}");
    assert!(
        text.contains("echo listed"),
        "the label must identify the job: {text}"
    );
}

// ---- job_output -------------------------------------------------------------------------------

#[tokio::test]
async fn output_returns_what_the_job_wrote() {
    let tools = Tools::new();
    let result = (tools.start.execute)(
        serde_json::json!({ "command": "echo first; echo second" }),
        ToolRunContext::default(),
    )
    .await
    .expect("started");
    let id = started_id(&tools, &text_of(&result));

    // `wait_ms` is what makes one call enough for a short command: without it the caller would have
    // to poll, and would not know how long to keep polling.
    let output = (tools.output.execute)(
        serde_json::json!({ "id": id, "wait_ms": 5_000 }),
        ToolRunContext::default(),
    )
    .await
    .expect("read");

    assert!(!output.is_error, "{}", text_of(&output));
    let text = text_of(&output);
    assert!(text.contains("completed"), "{text}");
    assert!(text.contains("first"), "{text}");
    assert!(text.contains("second"), "{text}");
}

#[tokio::test]
async fn output_says_when_the_job_is_still_running() {
    // A caller that mistook a partial log for a complete one would conclude the command had
    // finished silently.
    let tools = Tools::new();
    let result = (tools.start.execute)(
        serde_json::json!({ "command": "echo early; sleep 30" }),
        ToolRunContext::default(),
    )
    .await
    .expect("started");
    let id = started_id(&tools, &text_of(&result));

    let mut text = String::new();
    for _ in 0..40 {
        let output =
            (tools.output.execute)(serde_json::json!({ "id": id }), ToolRunContext::default())
                .await
                .expect("read");
        text = text_of(&output);
        if text.contains("early") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(text.contains("early"), "{text}");
    assert!(text.contains("still running"), "{text}");
    assert!(text.contains("running"), "{text}");

    tools.registry.request_stop(&id).expect("stopping");
    let _ = wait_settled(&tools.registry, &id).await;
}

#[tokio::test]
async fn output_reports_bytes_the_ring_dropped() {
    // The ring is bounded, so a chatty job's beginning is gone. Saying so is the difference between
    // a caller reading a tail and a caller believing it read the whole log. A small ring makes the
    // drop large enough to see without relying on the result-size bound.
    let tools = Tools::with_ring(1024);
    let result = (tools.start.execute)(
        // 200,000 numbers is about a megabyte, which overflows a 1 KiB ring many times over. Asking for more than
        // the bound by a wide margin keeps the test from depending on how fast the child happens to be scheduled:
        // an earlier version used 20,000 and failed intermittently on a loaded machine.
        serde_json::json!({ "command": "seq 1 200000" }),
        ToolRunContext::default(),
    )
    .await
    .expect("started");
    let id = started_id(&tools, &text_of(&result));
    let _ = wait_settled(&tools.registry, &id).await;

    let output = (tools.output.execute)(
        serde_json::json!({ "id": id, "wait_ms": 5_000 }),
        ToolRunContext::default(),
    )
    .await
    .expect("read");
    let text = text_of(&output);

    assert!(text.contains("dropped by the output ring"), "{text}");
    // The newest output is what a caller can act on, so it is the part kept.
    assert!(text.contains("20000"), "{text}");
}

#[tokio::test]
async fn output_reports_what_did_not_fit_in_one_result() {
    // Independent of the ring: a job that retained more than one result can hold still cannot return
    // all of it in one call. A caller told "this is the output" while holding a prefix draws
    // conclusions the evidence does not support, so the omitted count is stated. The ring is made
    // large deliberately: with the default 64 KiB ring the ring's cap is reached before this bound,
    // and the two notices would be indistinguishable.
    let tools = Tools::with_ring(1024 * 1024);
    let result = (tools.start.execute)(
        serde_json::json!({ "command": "seq 1 30000" }),
        ToolRunContext::default(),
    )
    .await
    .expect("started");
    let id = started_id(&tools, &text_of(&result));
    let _ = wait_settled(&tools.registry, &id).await;

    let output = (tools.output.execute)(
        serde_json::json!({ "id": id, "wait_ms": 5_000 }),
        ToolRunContext::default(),
    )
    .await
    .expect("read");
    let text = text_of(&output);

    assert!(text.contains("bytes omitted"), "{text}");
    assert!(
        text.contains("30000"),
        "the tail must be what is kept: {text}"
    );
    // The ring held the whole thing, so nothing was lost at the head.
    assert!(!text.contains("dropped by the output ring"), "{text}");
}

#[tokio::test]
async fn output_of_an_unknown_job_is_an_error() {
    let tools = Tools::new();

    let result = (tools.output.execute)(
        serde_json::json!({ "id": "no-such-job" }),
        ToolRunContext::default(),
    )
    .await
    .expect("answered");
    assert!(result.is_error);
    assert!(text_of(&result).contains("no-such-job"));

    let missing = (tools.output.execute)(serde_json::json!({}), ToolRunContext::default())
        .await
        .expect("answered");
    assert!(missing.is_error);
    assert!(text_of(&missing).contains("`id` is required"));
}

#[tokio::test]
async fn a_cancelled_wait_returns_instead_of_holding_the_turn() {
    // The wait must honour cancellation: a cancelled turn that still waited its full bound would
    // hold a slot for output nobody will read.
    let tools = Tools::new();
    let result = (tools.start.execute)(
        serde_json::json!({ "command": "sleep 30" }),
        ToolRunContext::default(),
    )
    .await
    .expect("started");
    let id = started_id(&tools, &text_of(&result));

    let cancel = CancellationToken::new();
    let ctx = ToolRunContext {
        cancel: cancel.clone(),
        ..Default::default()
    };
    let call = (tools.output.execute)(serde_json::json!({ "id": id, "wait_ms": 20_000 }), ctx);
    tokio::pin!(call);

    // The cancel arrives after the wait has begun rather than before it, which is the case a naive
    // "check the token once" implementation would miss.
    tokio::select! {
        result = &mut call => {
            let text = text_of(&result.expect("read"));
            panic!("the wait returned before being cancelled: {text}");
        }
        () = tokio::time::sleep(Duration::from_millis(100)) => {}
    }
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("the cancelled wait must return")
        .expect("read");
    let text = text_of(&result);
    assert!(text.contains("cancelled"), "{text}");
    // Said explicitly: the wait ended, the job did not.
    assert!(
        tools
            .registry
            .get(&id)
            .expect("registered")
            .status
            .is_live()
    );

    tools.registry.request_stop(&id).expect("stopping");
    let _ = wait_settled(&tools.registry, &id).await;
}

// ---- job_kill ---------------------------------------------------------------------------------

#[tokio::test]
async fn killing_a_running_job_stops_it() {
    let tools = Tools::new();
    let result = (tools.start.execute)(
        serde_json::json!({ "command": "sleep 30" }),
        ToolRunContext::default(),
    )
    .await
    .expect("started");
    let id = started_id(&tools, &text_of(&result));

    let killed = (tools.kill.execute)(
        serde_json::json!({ "id": id.clone() }),
        ToolRunContext::default(),
    )
    .await
    .expect("killed");
    assert!(!killed.is_error, "{}", text_of(&killed));
    // Not `killed` yet: the group needs time to die, and a caller told "killed" would start a second
    // job against a port the first still holds.
    assert!(
        text_of(&killed).contains("stopping"),
        "{}",
        text_of(&killed)
    );

    let job = wait_settled(&tools.registry, &id).await;
    assert_eq!(job.status, JobStatus::Killed);
}

#[tokio::test]
async fn killing_a_settled_job_is_not_a_failure() {
    // The caller wanted it gone and it is gone. Reporting an error would make a model retry a kill
    // that has nothing left to do.
    let tools = Tools::new();
    let result = (tools.start.execute)(
        serde_json::json!({ "command": "true" }),
        ToolRunContext::default(),
    )
    .await
    .expect("started");
    let id = started_id(&tools, &text_of(&result));
    let _ = wait_settled(&tools.registry, &id).await;

    let killed = (tools.kill.execute)(serde_json::json!({ "id": id }), ToolRunContext::default())
        .await
        .expect("answered");
    assert!(!killed.is_error, "{}", text_of(&killed));
    assert!(text_of(&killed).contains("already"), "{}", text_of(&killed));
}

#[tokio::test]
async fn killing_an_unknown_job_is_an_error() {
    let tools = Tools::new();

    let result = (tools.kill.execute)(
        serde_json::json!({ "id": "no-such-job" }),
        ToolRunContext::default(),
    )
    .await
    .expect("answered");
    assert!(result.is_error);
    assert!(text_of(&result).contains("no-such-job"));

    let missing = (tools.kill.execute)(serde_json::json!({}), ToolRunContext::default())
        .await
        .expect("answered");
    assert!(missing.is_error);
    assert!(text_of(&missing).contains("`id` is required"));
}

// ---- schemas ----------------------------------------------------------------------------------

#[test]
fn every_tool_advertises_the_name_and_arguments_a_model_needs() {
    // The schema is the only contract the model sees. A required argument that is not marked required
    // turns into a call the tool must reject, and the model has no way to know beforehand.
    let tools = Tools::new();

    assert_eq!(tools.start.name, "job_start");
    assert_eq!(
        tools.start.parameters["required"],
        serde_json::json!(["command"])
    );
    assert_eq!(
        tools.start.parameters["properties"]["command"]["type"],
        "string"
    );

    assert_eq!(tools.list.name, "job_list");
    assert_eq!(tools.list.parameters["properties"], serde_json::json!({}));

    assert_eq!(tools.output.name, "job_output");
    assert_eq!(
        tools.output.parameters["required"],
        serde_json::json!(["id"])
    );
    assert_eq!(
        tools.output.parameters["properties"]["wait_ms"]["type"],
        "number"
    );

    assert_eq!(tools.kill.name, "job_kill");
    assert_eq!(tools.kill.parameters["required"], serde_json::json!(["id"]));

    for tool in [&tools.start, &tools.list, &tools.output, &tools.kill] {
        assert_eq!(tool.parameters["type"], "object");
        assert!(!tool.description.is_empty());
    }
}
