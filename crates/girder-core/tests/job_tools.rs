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

// ---- job_output: incremental reads ------------------------------------------------------------

/// The `[next offset: N]` value a result ends with.
///
/// Parsed out of the rendered text rather than read from the job, because the point of the line is
/// that it is what a *caller* can use: an offset that is correct in the registry but printed wrongly
/// would still break every caller that followed it.
fn next_offset_of(text: &str) -> u64 {
    let marker = "[next offset: ";
    let start = text
        .rfind(marker)
        .unwrap_or_else(|| panic!("every `job_output` result must name the next offset: {text}"))
        + marker.len();
    let rest = &text[start..];
    let end = rest
        .find(']')
        .unwrap_or_else(|| panic!("the next-offset line must be closed: {text}"));
    rest[..end]
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("the next offset must be a number: {text}"))
}

/// Runs `command` to settlement and returns the job id.
async fn settled_job(tools: &Tools, command: &str) -> String {
    let result = (tools.start.execute)(
        serde_json::json!({ "command": command }),
        ToolRunContext::default(),
    )
    .await
    .expect("started");
    assert!(!result.is_error, "{}", text_of(&result));
    let id = started_id(tools, &text_of(&result));
    let _ = wait_settled(&tools.registry, &id).await;
    id
}

#[tokio::test]
async fn omitting_the_offset_reads_the_whole_retained_window() {
    // The old behaviour must survive: a caller that has just started a job wants the output so far,
    // not an empty delta from an offset it never chose.
    let tools = Tools::with_ring(1024);
    let id = settled_job(&tools, "seq 1 200000").await;

    let output = (tools.output.execute)(
        serde_json::json!({ "id": id, "wait_ms": 5_000 }),
        ToolRunContext::default(),
    )
    .await
    .expect("read");
    let text = text_of(&output);
    assert!(!output.is_error, "{text}");

    // The ring bound is still reported in its own words, and the newest output is still the part kept.
    assert!(text.contains("dropped by the output ring"), "{text}");
    assert!(text.contains("20000"), "{text}");

    // The next offset is the newest byte, so a caller can go incremental after one full read without
    // re-reading anything.
    let job = tools.registry.get(&id).expect("registered");
    assert_eq!(next_offset_of(&text), job.output.end_offset(), "{text}");
}

/// The job's own output as a result rendered it, with the header and the next-offset line removed.
///
/// A delta is what the caller appends to what it already has, so a test that wants to prove the
/// deltas are gapless has to read the body rather than the whole receipt.
fn body_of(text: &str) -> String {
    let Some(start) = text.find("---\n") else {
        return String::new();
    };
    let rest = &text[start + 4..];
    // The renderer always terminates the body with a newline and then writes the next-offset line, so
    // the body is everything before that line and keeps its own trailing newline.
    let end = rest.rfind("[next offset: ").unwrap_or(rest.len());
    rest[..end].to_owned()
}

#[tokio::test]
async fn a_saved_next_offset_reads_only_what_was_added() {
    // The property that makes polling usable: a caller that passes back the offset it was given gets
    // every byte once. Re-reading from zero would grow quadratically, and a delta that overlapped
    // would make the caller unable to tell new output from old.
    //
    // The job is held open by a marker file rather than a sleep, so the first read is guaranteed to
    // happen while `second` does not exist yet: a timing-based version of this test would pass or
    // fail depending on how the machine was loaded.
    let dir = scratch("delta");
    let tools = Tools::new();
    let result = (tools.start.execute)(
        serde_json::json!({
            "command": "echo first; while [ ! -f release ]; do sleep 0.05; done; echo second"
        }),
        ToolRunContext::default().in_workspace(dir.clone()),
    )
    .await
    .expect("started");
    let id = started_id(&tools, &text_of(&result));

    let mut text = String::new();
    for _ in 0..80 {
        let output =
            (tools.output.execute)(serde_json::json!({ "id": id }), ToolRunContext::default())
                .await
                .expect("read");
        text = text_of(&output);
        if text.contains("first") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(text.contains("first"), "{text}");
    assert!(
        !text.contains("second"),
        "the job must still be waiting when the first read happens: {text}"
    );
    let saved = next_offset_of(&text);

    std::fs::write(dir.join("release"), "go\n").expect("released");

    let mut delta_text = String::new();
    for _ in 0..80 {
        let delta = (tools.output.execute)(
            serde_json::json!({ "id": id, "offset": saved }),
            ToolRunContext::default(),
        )
        .await
        .expect("read");
        delta_text = text_of(&delta);
        assert!(!delta.is_error, "{delta_text}");
        if delta_text.contains("second") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(delta_text.contains("second"), "{delta_text}");
    assert!(
        !delta_text.contains("first"),
        "a delta must not repeat bytes the caller already read: {delta_text}"
    );
    // Nothing is left, so the position the caller is told to poll is the one it was already at.
    let job = wait_settled(&tools.registry, &id).await;
    assert_eq!(
        next_offset_of(&delta_text),
        job.output.end_offset(),
        "{delta_text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_empty_delta_says_there_is_nothing_new() {
    // Silence and "nothing new" are the same text without a note, and a caller watching a quiet job
    // would not know whether its read had worked.
    let tools = Tools::new();
    let id = settled_job(&tools, "echo quiet").await;
    let end = tools
        .registry
        .get(&id)
        .expect("registered")
        .output
        .end_offset();

    let delta = (tools.output.execute)(
        serde_json::json!({ "id": id, "offset": end, "wait_ms": 5_000 }),
        ToolRunContext::default(),
    )
    .await
    .expect("read");
    let delta_text = text_of(&delta);

    assert!(!delta.is_error, "{delta_text}");
    assert!(
        delta_text.contains("no new output"),
        "an empty delta must say so rather than looking like silence: {delta_text}"
    );
    assert_eq!(next_offset_of(&delta_text), end, "{delta_text}");
}

#[tokio::test]
async fn an_offset_before_the_head_is_reported_as_lost() {
    // The one case that must never be silent. The ring has discarded the bytes the caller asked for,
    // so returning the retained head without saying anything would splice two logs together across a
    // hole and read as a continuous log.
    let tools = Tools::with_ring(1024);
    let id = settled_job(&tools, "seq 1 200000").await;
    let job = tools.registry.get(&id).expect("registered");
    let head = job.output.head_offset();
    assert!(
        head > 0,
        "the ring must have dropped the head for this test"
    );

    let output = (tools.output.execute)(
        serde_json::json!({ "id": id, "offset": 0, "wait_ms": 5_000 }),
        ToolRunContext::default(),
    )
    .await
    .expect("read");
    let text = text_of(&output);

    assert!(!output.is_error, "{text}");
    assert!(
        text.contains("is lost") && text.contains("can never be read"),
        "the loss must be stated, not implied: {text}"
    );
    assert!(
        text.contains(&format!("before offset {head}")),
        "the result must name where the retained text really starts: {text}"
    );
    // Its own wording, not the whole-window note: the two mean different things to a caller.
    assert!(
        !text.contains("bytes at the head were dropped by the output ring"),
        "a resumed read reports a lost position, not a truncated window: {text}"
    );
    // The bytes it could not honour are not silently included: what follows starts at the head.
    assert_eq!(next_offset_of(&text), job.output.end_offset(), "{text}");
}

#[tokio::test]
async fn an_offset_past_the_end_returns_nothing_without_failing() {
    // A caller that polled past the end has asked a well-formed question whose answer is "nothing
    // yet". Answering it with an error would make a poll loop treat normal progress as a failure.
    let tools = Tools::new();
    let id = settled_job(&tools, "echo ready").await;
    let end = tools
        .registry
        .get(&id)
        .expect("registered")
        .output
        .end_offset();
    let beyond = end + 1_000;

    let output = (tools.output.execute)(
        serde_json::json!({ "id": id, "offset": beyond, "wait_ms": 5_000 }),
        ToolRunContext::default(),
    )
    .await
    .expect("read");
    let text = text_of(&output);

    assert!(!output.is_error, "{text}");
    assert!(
        text.contains("past the end") && text.contains(&end.to_string()),
        "the result must say where the output actually ends: {text}"
    );
    assert!(
        !text.contains("ready"),
        "there is nothing between the end and the asked-for offset: {text}"
    );
    assert!(
        !text.contains("---"),
        "an empty read must carry no body: {text}"
    );
    // Staying put is what lets a caller keep polling from where it is instead of walking backwards.
    assert_eq!(next_offset_of(&text), beyond, "{text}");
}

#[tokio::test]
async fn a_resumed_read_is_cut_at_the_end_and_says_so() {
    // A delta larger than one result must be returned from its start, not from its newest bytes: a
    // caller that asked for offset 0 and was handed the tail would lose exactly what it asked for.
    // Following the reported offsets must then reconstruct the log exactly — no gap, no repeat.
    let tools = Tools::with_ring(1024 * 1024);
    let id = settled_job(&tools, "seq 1 30000").await;
    let job = tools.registry.get(&id).expect("registered");
    let end = job.output.end_offset();
    let whole = job.output.text();

    let first = (tools.output.execute)(
        serde_json::json!({ "id": id, "offset": 0, "wait_ms": 5_000 }),
        ToolRunContext::default(),
    )
    .await
    .expect("read");
    let first_text = text_of(&first);
    assert!(!first.is_error, "{first_text}");
    assert!(
        first_text.contains("more bytes are retained"),
        "a cut delta must say the rest is still there: {first_text}"
    );
    // A prefix is returned, so the beginning of the log is present and its end is not.
    assert!(body_of(&first_text).starts_with("1\n2\n"), "{first_text}");
    assert!(
        !body_of(&first_text).contains("30000"),
        "a delta must not skip ahead to the newest bytes: {first_text}"
    );

    let mut cursor = 0;
    let mut rebuilt = String::new();
    for _ in 0..20 {
        let read = (tools.output.execute)(
            serde_json::json!({ "id": id, "offset": cursor, "wait_ms": 5_000 }),
            ToolRunContext::default(),
        )
        .await
        .expect("read");
        let text = text_of(&read);
        assert!(!read.is_error, "{text}");
        let next = next_offset_of(&text);
        let body = body_of(&text);

        // The ring still holds the whole log, so it is the ground truth for what these bytes should
        // be. The one difference allowed is the newline the renderer adds so the next-offset line
        // does not run into the output; it is terminated text, not an extra byte of the log.
        let expected = whole
            .get(usize::try_from(cursor).expect("fits")..usize::try_from(next).expect("fits"))
            .expect("the offsets must address the retained log");
        assert!(
            body == expected || body == format!("{expected}\n"),
            "the bytes at {cursor} must be the log's bytes there: expected {expected:?}, got {body:?}"
        );

        assert!(
            next > cursor || cursor >= end,
            "a read must make progress: {text}"
        );
        rebuilt.push_str(expected);
        cursor = next;
        if cursor >= end {
            break;
        }
    }

    assert_eq!(cursor, end, "following the offsets must reach the end");
    // Every byte of the log was delivered exactly once, in order.
    assert!(
        rebuilt == whole,
        "the deltas must reconstruct the log exactly (head={} end={} whole_len={} rebuilt_len={})",
        job.output.head_offset(),
        end,
        whole.len(),
        rebuilt.len()
    );
}

#[tokio::test]
async fn a_malformed_offset_is_an_error_and_null_is_not() {
    // A silently coerced offset is the failure this argument exists to prevent: a caller that meant
    // to resume from 1000 and was read from 0 would treat old bytes as new. Null, on the other hand,
    // is how an interpolated optional field arrives, so it means "omitted".
    let tools = Tools::new();
    let id = settled_job(&tools, "echo checked").await;

    for bad in [serde_json::json!(-1), serde_json::json!("1000")] {
        let result = (tools.output.execute)(
            serde_json::json!({ "id": id, "offset": bad }),
            ToolRunContext::default(),
        )
        .await
        .expect("answered");
        assert!(result.is_error, "{}", text_of(&result));
        assert!(
            text_of(&result).contains("`offset`"),
            "{}",
            text_of(&result)
        );
    }

    let null = (tools.output.execute)(
        serde_json::json!({ "id": id, "offset": serde_json::Value::Null }),
        ToolRunContext::default(),
    )
    .await
    .expect("read");
    assert!(!null.is_error, "{}", text_of(&null));
    assert!(text_of(&null).contains("checked"), "{}", text_of(&null));
}

#[test]
fn the_output_schema_advertises_the_offset_argument() {
    // The model can only pass an argument the schema shows it. `offset` is optional, so it must not
    // appear in `required`.
    let tools = Tools::new();

    assert_eq!(
        tools.output.parameters["properties"]["offset"]["type"],
        "number"
    );
    assert_eq!(
        tools.output.parameters["required"],
        serde_json::json!(["id"])
    );
    assert!(
        tools.output.parameters["properties"]["offset"]["description"]
            .as_str()
            .is_some_and(|description| description.contains("next offset")),
        "the description must tell the model where the value comes from"
    );
}
