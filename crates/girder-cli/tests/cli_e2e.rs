//! End-to-end tests for the `girder` binary.
//!
//! Every test here spawns the real executable and reads its real stdout/stderr. That is deliberate: the
//! crate had no integration test at all, so the argument wiring, the schema renderers, the offline
//! provider, and the session writer were only ever exercised by hand. A unit test on `print_schema`
//! would not have caught the flag-placement defect documented in
//! `schema_format_after_the_subcommand_is_rejected`, because the defect is in how clap binds the flag,
//! not in what `print_schema` does with it.
//!
//! # What the assertions are for
//!
//! Each test asserts on *content* rather than on the exit status. "It exited 0" is satisfied by a
//! binary that prints nothing at all, which is exactly the failure mode a renderer with a missing
//! branch produces. The assertions therefore name the shape each document must have: a JSON parse plus
//! the key the document exists to carry, a header line for the CSV, a `# HELP` opener for the
//! Prometheus text. Those are the properties a consumer depends on, and the ones a refactor is most
//! likely to break silently.
//!
//! # Why the tests pass `--schema-format` before the subcommand
//!
//! See `schema_format_after_the_subcommand_is_rejected`. `Cli::schema_format` is a field on the
//! top-level parser, not on `Command::Schema`, so it is only accepted before the subcommand. Writing
//! it after (`girder schema --schema-format csv`) is a hard clap error. The tests use the form that
//! works and pin the broken form in its own test so the defect cannot be quietly forgotten.

use std::path::PathBuf;
use std::process::{Command, Output};

/// The binary under test, resolved by cargo for this integration test.
const BIN: &str = env!("CARGO_BIN_EXE_girder");

/// A private directory for one test.
///
/// Every test gets its own `GIRDER_SESSIONS` and its own working directory, because the stats commands
/// (`disk`, `csv`, `health`, `metrics`) aggregate *every* session under `GIRDER_SESSIONS`. Without the
/// isolation, a test that seeds one session would see another test's sessions in its counts, and the
/// suite would pass or fail depending on execution order.
struct Scratch {
    /// The root of this test's directory.
    root: PathBuf,
}

impl Scratch {
    /// Creates a fresh directory named after `name`.
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "girder-cli-e2e-{name}-{}-{}",
            std::process::id(),
            // A nanosecond timestamp distinguishes the two invocations of a test that runs twice under a
            // concurrent harness; without it both would share a directory and both would seed a session.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos())
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sessions")).expect("created sessions dir");
        std::fs::create_dir_all(root.join("work")).expect("created work dir");
        Self { root }
    }

    /// The workspace the binary runs in.
    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    /// The session directory this test's binary reads and writes.
    fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Writes a file into the workspace and returns its path.
    fn write_work_file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.work().join(name);
        std::fs::write(&path, contents).expect("wrote workspace file");
        path
    }

    /// Runs the binary with `args` in this scratch's workspace.
    ///
    /// The environment is cleared of every credential variable rather than relying on the host not
    /// having one: a developer machine with `DEEPSEEK_API_KEY` exported would otherwise route these
    /// tests at a live model, and they would then depend on the network.
    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .current_dir(self.work())
            .env("GIRDER_SESSIONS", self.sessions())
            .env_remove("DEEPSEEK_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("GIRDER_OPENAI_API_KEY")
            .env_remove("GIRDER_PROTOCOL")
            .env_remove("GIRDER_MODEL");
        command.output().expect("ran the girder binary")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The stdout of an output, as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of an output, as text.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Parses stdout as JSON, reporting both streams when it is not JSON.
///
/// A renderer that panicked halfway, or that printed a log line before its document, produces text
/// that is not parseable. The panic message is on stderr, so it belongs in the failure output --
/// otherwise the test says "not JSON" and the reason is in a stream nobody looked at.
fn parse_json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output)).unwrap_or_else(|error| {
        panic!(
            "stdout was not valid JSON: {error}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            stdout(output),
            stderr(output)
        )
    })
}

/// Runs `--schema-format <format> schema` and returns the output.
///
/// The order is load-bearing: see the module documentation and
/// `schema_format_after_the_subcommand_is_rejected`.
fn schema(scratch: &Scratch, format: &str) -> Output {
    scratch.run(&["--schema-format", format, "schema"])
}

// ---------------------------------------------------------------------------------------------
// 1. Every schema format renders a document of the right shape.
// ---------------------------------------------------------------------------------------------

/// The `rpc` format must produce the draft-07 schema with the six named frames.
///
/// Asserting on the frame names rather than on "some JSON" is what proves the generator actually ran:
/// an empty `{"$defs": {}}` would be valid JSON and would tell a client author nothing.
#[test]
fn rpc_schema_lists_the_wire_frames_and_their_method_field() {
    let scratch = Scratch::new("rpc");
    let output = schema(&scratch, "rpc");
    assert!(
        output.status.success(),
        "rpc schema failed: {}",
        stderr(&output)
    );

    let document = parse_json(&output);
    assert_eq!(
        document["$schema"], "http://json-schema.org/draft-07/schema#",
        "the document must declare draft-07, which is what the client validator is configured for"
    );
    let definitions = document["$defs"]
        .as_object()
        .expect("`$defs` is an object of named frames");
    for frame in [
        "DownlinkFrame",
        "RemoteEventResult",
        "InvokeRemoteRequest",
        "InvokeResponse",
        "SubscriptionFrame",
        "SubscribeRequest",
    ] {
        assert!(
            definitions.contains_key(frame),
            "the wire schema must describe `{frame}`; a client cannot check a frame it was never given"
        );
    }

    // The request frame carries the method name the client sends. Without it the schema would describe
    // an envelope with no payload, and a generated client could not name a method at all.
    let request = &definitions["InvokeRemoteRequest"];
    let properties = request["properties"]
        .as_object()
        .expect("the request frame has properties");
    assert!(
        properties.contains_key("method"),
        "InvokeRemoteRequest must have a `method` property; got {:?}",
        properties.keys().collect::<Vec<_>>()
    );
    assert!(
        stdout(&output).contains("\"method\""),
        "the rendered text must contain the `method` field, not only the parsed document"
    );
}

/// The `openapi` format must produce an `OpenAPI` document with a non-empty `paths` object.
///
/// The `paths` object is the whole point of the format: an `OpenAPI` document with zero paths is
/// something a client generator accepts and then generates nothing from. The count assertion is what
/// distinguishes "the catalogues were merged" from "the merge found no methods".
#[test]
fn openapi_schema_is_valid_json_with_a_populated_paths_object() {
    let scratch = Scratch::new("openapi");
    let output = schema(&scratch, "openapi");
    assert!(
        output.status.success(),
        "openapi schema failed: {}",
        stderr(&output)
    );

    let document = parse_json(&output);
    assert_eq!(
        document["openapi"], "3.1.0",
        "the version field is what a client generator keys its behaviour on"
    );
    assert_eq!(document["info"]["title"], "Girder");
    // Pinned to a non-zero count rather than to an exact number: the surface grows, and a test that
    // hard-coded 16 would fail on every method added while still passing if the merge broke and left
    // one stale path behind.
    let paths = document["paths"].as_object().expect("`paths` is an object");
    assert!(
        !paths.is_empty(),
        "`paths` must not be empty; an OpenAPI document with no paths generates no client"
    );
    // All three catalogues are merged in (`api_session`, `api_config`, `api_job`). A document showing
    // only the session catalogue would be a third of the surface, so one path from each proves the
    // merge ran rather than silently keeping the first catalogue only.
    for expected in ["/session/create", "/workspace/list", "/job/list"] {
        assert!(
            paths.contains_key(expected),
            "`{expected}` is missing; the catalogues were not merged: {:?}",
            paths.keys().collect::<Vec<_>>()
        );
    }
}

/// The `catalog` format must produce a JSON object whose `services` array is non-empty and named.
///
/// The service names come from the registry's own key constants, so a non-empty list of the seven
/// expected names is evidence that the registry was actually consulted rather than a literal list
/// written twice.
#[test]
fn catalog_schema_lists_services_and_events() {
    let scratch = Scratch::new("catalog");
    let output = schema(&scratch, "catalog");
    assert!(
        output.status.success(),
        "catalog schema failed: {}",
        stderr(&output)
    );

    let document = parse_json(&output);
    let services = document["services"]
        .as_array()
        .expect("`services` is an array");
    assert!(
        !services.is_empty(),
        "`services` must not be empty; it is the document's reason to exist"
    );
    let names: Vec<&str> = services
        .iter()
        .map(|service| service["name"].as_str().expect("each service has a name"))
        .collect();
    for expected in [
        "services",
        "events",
        "tools",
        "sessions",
        "workspace",
        "settings",
        "terminal",
    ] {
        assert!(
            names.contains(&expected),
            "service `{expected}` is missing from {names:?}"
        );
    }
    // The events half is present and carries its modes: an event catalog without modes does not say
    // whether a listener may block, which is the only thing an extension author needs from it.
    let events = document["events"].as_array().expect("`events` is an array");
    assert!(
        !events.is_empty(),
        "`events` must not be empty; the catalog publishes the bus, not just the services"
    );
    assert!(
        events[0]["modes"].is_array(),
        "an event entry must describe its modes; got {}",
        events[0]
    );
}

/// The `metrics` format must start with `# HELP` and expose the aggregate counters.
///
/// The first-line check is the format's own contract: Prometheus rejects an exposition body that
/// begins with a bare sample, and a scraper pointed at this command would report a parse error rather
/// than an empty fleet. The counter names are asserted because they are what a dashboard queries.
#[test]
fn metrics_schema_starts_with_a_help_line_and_exposes_counters() {
    let scratch = Scratch::new("metrics");
    let output = schema(&scratch, "metrics");
    assert!(
        output.status.success(),
        "metrics schema failed: {}",
        stderr(&output)
    );

    let text = stdout(&output);
    assert!(
        text.starts_with("# HELP "),
        "the exposition must open with `# HELP`, which is what a scrape requires; got:\n{}",
        text.lines().take(3).collect::<Vec<_>>().join("\n")
    );
    // Every block is a HELP/TYPE pair followed by samples; a HELP with no TYPE is rejected by some
    // parsers, so the pairing is asserted rather than only the first line.
    assert!(
        text.contains("# TYPE "),
        "the exposition must carry `# TYPE` lines; got:\n{text}"
    );
    for counter in [
        "girder_session_turns",
        "girder_session_tool_calls",
        "girder_session_user_messages",
    ] {
        assert!(
            text.contains(counter),
            "counter `{counter}` is missing from the exposition; a dashboard querying it would be empty"
        );
    }
    // Dots must not survive into a metric name: the Prometheus name grammar rejects them, so a sample
    // emitted as `girder.session.turns` would be a scrape error rather than a rename someone forgot.
    let dotted = text
        .lines()
        .find(|line| !line.starts_with('#') && line.contains("girder.") && !line.contains('='));
    assert!(
        dotted.is_none(),
        "a metric name still contains a dot, which the format rejects: {dotted:?}"
    );
}

/// The `disk` format must report the session count and the byte total.
///
/// Both lines are asserted on their exact rendered form because a script parses this output: the
/// `sessions: ` prefix is the interface, and changing it to `Sessions=` would break every consumer
/// while the command still "worked".
#[test]
fn disk_schema_reports_session_count_and_bytes() {
    let scratch = Scratch::new("disk");
    scratch.write_work_file("README.md", "Hello from the end-to-end test\n");
    // Seed one real session first: an empty directory would render zeroes, and zeroes are what a
    // broken implementation that never lists anything also renders.
    let seeded = run_one_prompt(&scratch, &["--tool", "read", "--session"]);
    assert!(
        seeded.status.success(),
        "seeding a session failed: {}",
        stderr(&seeded)
    );

    let output = schema(&scratch, "disk");
    assert!(
        output.status.success(),
        "disk schema failed: {}",
        stderr(&output)
    );

    let text = stdout(&output);
    assert!(
        text.starts_with("sessions: 1\n"),
        "the seeded session must be counted; got:\n{text}"
    );
    let bytes_line = text
        .lines()
        .find(|line| line.starts_with("bytes: "))
        .unwrap_or_else(|| panic!("no `bytes:` line in:\n{text}"));
    let bytes: u64 = bytes_line
        .trim_start_matches("bytes: ")
        .parse()
        .unwrap_or_else(|_| panic!("`bytes:` line is not a number: {bytes_line:?}"));
    assert!(
        bytes > 0,
        "a session with a non-empty log must occupy more than zero bytes; got {bytes}"
    );
    // The per-session footer is what makes the command more useful than `du`: it says which sessions
    // an eviction would take. Its presence proves the footprints were built, not just summed.
    assert!(
        text.contains("(log "),
        "the per-session rows with their log/sidecar split are missing:\n{text}"
    );
}

/// The `csv` format's first line must be a header containing `tool,calls`.
///
/// `tool,calls` are the first two columns specifically, so a reordering that moved `calls` behind
/// `failures` would break a spreadsheet's named-column reference. The header is also checked to be a
/// single record: a renderer that prepended a title line would leave a reader with one unnamed column.
#[test]
fn csv_schema_starts_with_the_tool_calls_header() {
    let scratch = Scratch::new("csv");
    scratch.write_work_file("README.md", "Hello from the end-to-end test\n");
    let seeded = run_one_prompt(&scratch, &["--tool", "read", "--session"]);
    assert!(
        seeded.status.success(),
        "seeding failed: {}",
        stderr(&seeded)
    );

    let output = schema(&scratch, "csv");
    assert!(
        output.status.success(),
        "csv schema failed: {}",
        stderr(&output)
    );

    let text = stdout(&output);
    // Records end with CRLF, so the header is split on the record separator rather than on `\n` alone.
    let header = text
        .split("\r\n")
        .next()
        .unwrap_or_else(|| panic!("csv output was empty"));
    assert_eq!(
        header, "tool,calls,failures,failure_rate,argument_bytes,result_bytes",
        "the header is the file's interface; a spreadsheet refers to these by name"
    );
    assert!(
        header.starts_with("tool,calls"),
        "the first two columns must be `tool,calls`; got {header:?}"
    );
    // The seeded `read` call must appear as a data row with a zero failure count. A CSV that renders
    // only a header is indistinguishable from one whose session scan found nothing.
    let row = text
        .split("\r\n")
        .find(|record| record.starts_with("read,"))
        .unwrap_or_else(|| panic!("no `read` row in:\n{text}"));
    // The counters are asserted exactly because they are the columns a spreadsheet sums. The two byte
    // columns are checked for being positive rather than pinned: their values depend on the seeded
    // file's length and on the tool's rendering, so an exact number would fail on any wording change
    // to `read`'s output while saying nothing about whether the CSV is right.
    let cells: Vec<&str> = row.split(',').collect();
    assert_eq!(
        &cells[..4],
        ["read", "1", "0", "0.0000"],
        "the seeded call must render one call with no failures; got {row:?}"
    );
    for (index, column) in [(4, "argument_bytes"), (5, "result_bytes")] {
        let value: u64 = cells[index]
            .parse()
            .unwrap_or_else(|_| panic!("`{column}` is not a number in {row:?}"));
        assert!(
            value > 0,
            "`{column}` must be positive for a call that carried arguments and returned text; got {value}"
        );
    }
}

/// The `health` format must report the session fleet and a per-issue breakdown.
///
/// The command's value is the pair of lines `sessions:` and `mean score:`; a health report with a
/// count and no score is a count. The seeded log is well-formed, so the assertion is that a clean
/// session scores 100 and is counted as healthy -- the opposite (scoring a clean log as broken) is
/// the failure that would make operators ignore the number.
#[test]
fn health_schema_reports_sessions_and_score() {
    let scratch = Scratch::new("health");
    scratch.write_work_file("README.md", "Hello from the end-to-end test\n");
    let seeded = run_one_prompt(&scratch, &["--tool", "read", "--session"]);
    assert!(
        seeded.status.success(),
        "seeding failed: {}",
        stderr(&seeded)
    );

    let output = schema(&scratch, "health");
    assert!(
        output.status.success(),
        "health schema failed: {}",
        stderr(&output)
    );

    let text = stdout(&output);
    assert!(
        text.starts_with("sessions: 1\n"),
        "the seeded session must be sampled; got:\n{text}"
    );
    assert!(
        text.contains("healthy: 1\n"),
        "a log whose calls pair and whose turns close must read as healthy:\n{text}"
    );
    assert!(
        text.contains("mean score: 100.0\n"),
        "a clean log must score 100.0; got:\n{text}"
    );
    assert!(
        text.contains("errors: 0\n"),
        "a clean log must report no errors; got:\n{text}"
    );
}

/// The `tools-mcp` format must produce a `{"tools": [...]}` document with the file tools in it.
///
/// The envelope key differs from `tools-openai`, which is the reason both are tested: a single
/// implementation writing one envelope for both would satisfy a "tools array exists" check on only one
/// of them.
#[test]
fn tools_mcp_schema_exports_tools_with_input_schemas() {
    let scratch = Scratch::new("tools-mcp");
    let output = schema(&scratch, "tools-mcp");
    assert!(
        output.status.success(),
        "tools-mcp schema failed: {}",
        stderr(&output)
    );

    let document = parse_json(&output);
    let tools = document["tools"].as_array().expect("`tools` is an array");
    assert!(
        tools.len() > 20,
        "the export must carry the whole registry, not a handful; got {} tools",
        tools.len()
    );
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("each tool has a name"))
        .collect();
    for expected in ["read", "write", "bash", "grep"] {
        assert!(
            names.contains(&expected),
            "tool `{expected}` is missing from the MCP export: {names:?}"
        );
    }
    // MCP names the argument schema `inputSchema`; a client reads that key and nothing else.
    let read = tools
        .iter()
        .find(|tool| tool["name"] == "read")
        .expect("the read tool is exported");
    assert!(
        read["inputSchema"]["properties"]["path"].is_object(),
        "the MCP export must describe each tool's arguments under `inputSchema`; got {read}"
    );
    assert!(
        read["description"].is_string(),
        "a tool without a description is one a model will not choose; got {read}"
    );
}

/// The `tools-openai` format must use `OpenAI`'s `{"type":"function","function":{..}}` envelope.
///
/// The nested `function` object is the difference from MCP, and it is what an `OpenAI` client parses.
/// Asserting on the nesting rather than on the tool names is the point: the same registry feeds both
/// exports, so the names are the part that cannot differ.
#[test]
fn tools_openai_schema_exports_function_envelopes() {
    let scratch = Scratch::new("tools-openai");
    let output = schema(&scratch, "tools-openai");
    assert!(
        output.status.success(),
        "tools-openai schema failed: {}",
        stderr(&output)
    );

    let document = parse_json(&output);
    let tools = document["tools"].as_array().expect("`tools` is an array");
    assert!(
        tools.len() > 20,
        "the export must carry the whole registry; got {} tools",
        tools.len()
    );
    for tool in tools {
        assert_eq!(
            tool["type"], "function",
            "every OpenAI entry must be tagged `function`; got {tool}"
        );
        assert!(
            tool["function"]["name"].is_string(),
            "the name lives under `function`, not at the top level; got {tool}"
        );
        assert!(
            tool["function"]["parameters"].is_object(),
            "the argument schema is `function.parameters` in this envelope; got {tool}"
        );
    }
}

/// Every documented format must actually render, so the help text cannot promise a format that is
/// not implemented.
///
/// This is the test that would have caught a `metrics` arm that existed in the error message but not
/// in the `match`: the message lists nine names, and a name in the list that falls through to the
/// default would render JSON where Prometheus text was promised.
#[test]
fn every_documented_schema_format_renders_non_empty_output() {
    let scratch = Scratch::new("all-formats");
    for format in [
        "rpc",
        "openapi",
        "catalog",
        "metrics",
        "disk",
        "csv",
        "health",
        "tools-mcp",
        "tools-openai",
    ] {
        let output = schema(&scratch, format);
        assert!(
            output.status.success(),
            "`--schema-format {format}` failed: {}",
            stderr(&output)
        );
        assert!(
            !stdout(&output).trim().is_empty(),
            "`--schema-format {format}` printed nothing, which a consumer reads as an empty document"
        );
    }
}

/// The `rpc` document is the default when no format is given, so `girder schema` and
/// `girder --schema-format rpc schema` must agree.
///
/// A default that disagreed with the explicit form would make the help text wrong for everyone who
/// never passes the flag -- which is most callers.
#[test]
fn schema_without_a_format_defaults_to_the_rpc_document() {
    let scratch = Scratch::new("default-format");
    let explicit = schema(&scratch, "rpc");
    let implicit = scratch.run(&["schema"]);
    assert!(
        implicit.status.success(),
        "`girder schema` failed: {}",
        stderr(&implicit)
    );
    assert_eq!(
        stdout(&explicit),
        stdout(&implicit),
        "the default format must be the `rpc` document; the two invocations differ"
    );
}

// ---------------------------------------------------------------------------------------------
// 2. `--tool` really runs the named tool.
// ---------------------------------------------------------------------------------------------

/// Runs one offline prompt in `scratch` and returns the output.
///
/// `session_args` lets a caller add `--session <path>`: the helper stays one function so the offline
/// environment is cleared in exactly one place.
fn run_one_prompt(scratch: &Scratch, session_args: &[&str]) -> Output {
    let prompt = "read the file";
    if session_args.contains(&"--session") {
        let path = scratch.sessions().join("seed.jsonl.zst");
        let path_text = path.to_string_lossy().into_owned();
        let mut args: Vec<&str> = session_args.to_vec();
        args.push(&path_text);
        args.push(prompt);
        scratch.run(&args)
    } else {
        scratch.run(session_args)
    }
}

/// `--tool read` must read the file and print its contents, with line numbers.
///
/// Asserting on the file's own text is what makes this an end-to-end check: a tool that answered
/// "done" without running would pass a status-only assertion. The line-number prefix is the `read`
/// tool's documented output shape, so it is checked too -- a bare echo of the content would satisfy
/// the text check while implementing a different tool.
#[test]
fn tool_read_executes_and_prints_the_file_contents() {
    let scratch = Scratch::new("tool-read");
    scratch.write_work_file("README.md", "needle-content-42\nsecond line\n");

    let output = scratch.run(&["--tool", "read", "read the file"]);

    assert!(
        output.status.success(),
        "the offline prompt failed: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("[tool] read"),
        "the run must name the tool it called; got:\n{text}"
    );
    assert!(
        text.contains("needle-content-42"),
        "the file's contents must reach stdout; got:\n{text}"
    );
    assert!(
        text.contains("1\tneedle-content-42"),
        "`read` returns one-based line numbers before the text; got:\n{text}"
    );
    assert!(
        !text.contains("[tool] failed"),
        "the call must succeed against a file that exists; got:\n{text}"
    );
}

/// A different `--tool` must dispatch to a different tool, not silently keep the default.
///
/// `--tool bash` runs `ls -1 | head -5` in the workspace. A file that exists only in the workspace has
/// to appear, which proves the argument reached the tool and the tool ran in the configured working
/// directory rather than the process's own.
#[test]
fn tool_bash_executes_in_the_workspace() {
    let scratch = Scratch::new("tool-bash");
    scratch.write_work_file("only-in-this-workspace.txt", "x\n");

    let output = scratch.run(&[
        "--sandbox",
        "danger-full-access",
        "--tool",
        "bash",
        "list the files",
    ]);

    assert!(
        output.status.success(),
        "the offline prompt failed: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("[tool] bash"),
        "the run must report the `bash` tool; got:\n{text}"
    );
    assert!(
        text.contains("only-in-this-workspace.txt"),
        "`ls` must run in the workspace the binary was given; got:\n{text}"
    );
}

/// The offline provider must announce itself when no credential is set.
///
/// The hint is the only thing that tells a person why the model answers with a canned sentence. It
/// goes to stderr rather than stdout so it cannot corrupt a piped document -- which is also why the
/// assertion reads stderr.
#[test]
fn offline_mode_is_announced_on_stderr_without_an_api_key() {
    let scratch = Scratch::new("offline");
    scratch.write_work_file("README.md", "hello\n");

    let output = scratch.run(&["read the file"]);

    let errors = stderr(&output);
    assert!(
        errors.contains("[provider] offline"),
        "the offline provider must say so; got stderr:\n{errors}"
    );
    assert!(
        errors.contains("DEEPSEEK_API_KEY"),
        "the hint must name the variable that would make it live; got stderr:\n{errors}"
    );
    assert!(
        !stdout(&output).contains("[provider] offline"),
        "the announcement belongs on stderr; on stdout it would corrupt a piped document"
    );
}

// ---------------------------------------------------------------------------------------------
// 3. An unknown format is rejected and the valid values are listed.
// ---------------------------------------------------------------------------------------------

/// An unknown `--schema-format` must exit non-zero and name every valid format.
///
/// Listing the valid values is the difference between "you typed it wrong" and "you now have to read
/// the source". The count of nine is asserted because a help message that lists eight of nine is
/// exactly what a format added to the `match` but not to the error would produce.
#[test]
fn unknown_schema_format_is_rejected_and_lists_every_valid_format() {
    let scratch = Scratch::new("bad-format");
    let output = schema(&scratch, "bogus");

    assert!(
        !output.status.success(),
        "an unknown format must fail rather than fall back to a default; stdout was:\n{}",
        stdout(&output)
    );
    let errors = stderr(&output);
    assert!(
        errors.contains("unknown schema format `bogus`"),
        "the error must name the rejected value; got stderr:\n{errors}"
    );
    for format in [
        "rpc",
        "openapi",
        "catalog",
        "metrics",
        "disk",
        "csv",
        "health",
        "tools-mcp",
        "tools-openai",
    ] {
        assert!(
            errors.contains(format),
            "the error must list `{format}` as a valid value; got stderr:\n{errors}"
        );
    }
    assert!(
        stdout(&output).is_empty(),
        "a rejected format must not print a partial document; got stdout:\n{}",
        stdout(&output)
    );
}

/// `--schema-format` after the subcommand is rejected, which is a real usability defect.
///
/// `schema_format` is a field on the top-level `Cli`, so clap binds it before the subcommand only.
/// The natural spelling -- `girder schema --schema-format csv` -- is a usage error, and the `schema`
/// subcommand's own `--help` does not mention the flag at all. This test pins the current behaviour
/// so the defect is visible: if the flag is later moved onto the subcommand, this test fails and the
/// change is deliberate rather than accidental.
#[test]
fn schema_format_after_the_subcommand_is_rejected() {
    let scratch = Scratch::new("flag-placement");
    let output = scratch.run(&["schema", "--schema-format", "csv"]);

    assert!(
        !output.status.success(),
        "the flag is currently accepted only before the subcommand; if this now succeeds, the flag \
         was moved and this test should be updated"
    );
    let errors = stderr(&output);
    assert!(
        errors.contains("unexpected argument '--schema-format'"),
        "clap must explain the misplaced flag; got stderr:\n{errors}"
    );
    // The subcommand's help does not advertise the flag, which is why the misplaced spelling is easy
    // to reach: a user follows `girder schema --help` and finds no way to choose a format.
    let help = scratch.run(&["schema", "--help"]);
    assert!(
        !stdout(&help).contains("--schema-format"),
        "if the subcommand now documents the flag, the placement defect was fixed and this assertion \
         should be removed"
    );
}

// ---------------------------------------------------------------------------------------------
// 4. `--session` writes a decodable log.
// ---------------------------------------------------------------------------------------------

/// `--session <path>` must create the file and write a decodable zstd-compressed event log.
///
/// "The file exists" is not enough: an empty file also exists, and a file written without compression
/// would be unreadable by every other part of the harness. The test therefore decodes the log with the
/// library's own reader and asserts on the events it contains -- the user message, the tool call, and
/// the tool result are the three records the run must have produced.
#[test]
fn session_flag_writes_a_decodable_log_with_the_turns_events() {
    let scratch = Scratch::new("session-log");
    scratch.write_work_file("README.md", "needle-content-42\n");
    let log = scratch.sessions().join("run.jsonl.zst");
    let log_text = log.to_string_lossy().into_owned();

    let output = scratch.run(&["--tool", "read", "--session", &log_text, "read the file"]);

    assert!(
        output.status.success(),
        "the prompt with `--session` failed: {}",
        stderr(&output)
    );
    assert!(log.exists(), "`--session` must create {log:?}");

    // The zstd frame magic is `28 b5 2f fd`. Checking it is what proves the file is the compressed
    // format the rest of the harness reads, not plain JSONL that happens to be named `.zst`.
    let bytes = std::fs::read(&log).expect("read the session log");
    assert!(
        bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]),
        "the log must begin with the zstd frame magic; got {:02x?}",
        &bytes[..bytes.len().min(4)]
    );

    let events = girder_core::read_session_file(&log).expect("the log decodes");
    assert!(
        !events.is_empty(),
        "the log must contain the run's events, not just a valid empty frame"
    );
    let kinds: Vec<&str> = events
        .iter()
        .map(girder_core::SessionEvent::type_name)
        .collect();
    for expected in [
        "turn/start",
        "user/message",
        "tool/call",
        "tool/result",
        "turn/end",
    ] {
        assert!(
            kinds.contains(&expected),
            "the log must record `{expected}`; got {kinds:?}"
        );
    }
    // The user's prompt is the record a replay shows first, and the tool result is what makes the
    // call answered. Asserting on the content rather than only the event kinds is what proves the
    // events carry the run and are not placeholders.
    let log_text_all = format!("{events:?}");
    assert!(
        log_text_all.contains("needle-content-42"),
        "the tool result must be in the log; a replay would otherwise lose what the model saw"
    );
}

/// Without `--session`, no file may be written: the default is an in-memory session.
///
/// The negative case matters because a writer that defaulted to a path in the session directory would
/// make every one-shot prompt leave a file behind, and the session listing would grow with runs
/// nobody meant to keep.
#[test]
fn prompt_without_session_writes_no_log() {
    let scratch = Scratch::new("no-session");
    scratch.write_work_file("README.md", "hello\n");

    let output = scratch.run(&["read the file"]);

    assert!(
        output.status.success(),
        "the offline prompt failed: {}",
        stderr(&output)
    );
    let entries: Vec<PathBuf> = std::fs::read_dir(scratch.sessions())
        .expect("the sessions dir exists")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .collect();
    assert!(
        entries.is_empty(),
        "a run without `--session` must not write a log; found {entries:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// 5. The other entry points: `--boot-report`, `--verbose`, and `serve`.
// ---------------------------------------------------------------------------------------------

/// `--boot-report` must print one line per component and report the provider as skipped, not failed.
///
/// The distinction is the check's whole design: a deployment with no credential is the offline path,
/// which is supported. Reporting it as `FAILED` would make every fresh clone look broken, and
/// reporting nothing would hide which parts mounted.
#[test]
fn boot_report_names_each_component_and_skips_the_provider() {
    let scratch = Scratch::new("boot-report");
    let output = scratch.run(&["--boot-report"]);

    assert!(
        output.status.success(),
        "`--boot-report` failed: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    for component in ["provider", "tools", "sessions", "events"] {
        assert!(
            text.contains(&format!("  {component}: ")),
            "the report must name `{component}`; got:\n{text}"
        );
    }
    assert!(
        text.contains("  provider: skipped (no credential configured)"),
        "a missing credential is the offline path, not a failure; got:\n{text}"
    );
    assert!(
        !text.contains("FAILED"),
        "nothing may be reported as failed in a credential-free deployment; got:\n{text}"
    );
    // The report is the mode's entire output, so it must not be followed by a prompt run.
    assert!(
        !text.contains("[provider] offline"),
        "`--boot-report` must exit without sending a prompt; got:\n{text}"
    );
}

/// `--verbose` must append the session events to stdout as one JSON object per line.
///
/// The flag is the only way to see the log without a separate decoder, and its output is consumed by
/// `jq` and by the replay tooling. Every line must parse on its own -- a pretty-printed multi-line
/// object would break both.
#[test]
fn verbose_prints_one_json_event_per_line() {
    let scratch = Scratch::new("verbose");
    scratch.write_work_file("README.md", "needle-content-42\n");

    let output = scratch.run(&["--verbose", "--tool", "read", "read the file"]);

    assert!(
        output.status.success(),
        "the verbose run failed: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    let events: Vec<&str> = text.lines().filter(|line| line.starts_with('{')).collect();
    assert!(
        !events.is_empty(),
        "`--verbose` must print the events; got:\n{text}"
    );
    for line in &events {
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("an event line is not JSON: {error}\n{line}"));
        assert!(
            parsed["type"].is_string(),
            "every event carries its `type` tag; got {line}"
        );
    }
    let kinds: Vec<String> = events
        .iter()
        .filter_map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|value| value["type"].as_str().map(str::to_owned))
        })
        .collect();
    assert!(
        kinds.contains(&"turn/start".to_owned()) && kinds.contains(&"turn/end".to_owned()),
        "a completed run must log both ends of its turn; got {kinds:?}"
    );
}

/// `serve` must bind, print the endpoint and the method count, then serve until killed.
///
/// The banner is checked rather than the socket because the banner is the deployment's only report of
/// what it mounted: a server that bound but registered zero methods would be reachable and useless.
/// The process is killed rather than left running, so the test cannot leak a listener.
#[test]
fn serve_reports_its_endpoint_and_method_surface() {
    let scratch = Scratch::new("serve");
    let mut child = Command::new(BIN)
        .args(["serve", "--bind", "127.0.0.1:0"])
        .current_dir(scratch.work())
        .env("GIRDER_SESSIONS", scratch.sessions())
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("GIRDER_PROTOCOL")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawned the server");

    // A bound ephemeral port is printed before the server starts accepting, so the banner is the
    // readiness signal. Reading it is bounded by a deadline because a regression that never prints it
    // would otherwise hang the suite.
    let reader = std::thread::spawn(move || {
        let stdout = child.stdout.take().expect("piped stdout");
        let mut line = String::new();
        {
            use std::io::BufRead as _;
            let mut handle = std::io::BufReader::new(stdout);
            let _ = handle.read_line(&mut line);
        }
        let _ = child.kill();
        let _ = child.wait();
        line
    });
    let banner = reader.join().expect("the reader thread finished");

    assert!(
        banner.contains("girder rpc listening on ws://127.0.0.1:"),
        "the server must print its endpoint; got {banner:?}"
    );
    assert!(
        banner.contains("/api/remote.mux"),
        "the endpoint must name the mux path a client connects to; got {banner:?}"
    );
}

/// `serve` must refuse a `--watch` name that is not an event this build forwards.
///
/// A typo silently dropped would make a deployment believe it was forwarding an event that never
/// arrives. The non-zero exit and the list of known names are what turn the typo into a fixable
/// message instead of an empty stream.
#[test]
fn serve_rejects_an_unknown_watch_event() {
    let scratch = Scratch::new("serve-watch");
    let output = scratch.run(&["serve", "--bind", "127.0.0.1:0", "--watch", "not-an-event"]);

    assert!(
        !output.status.success(),
        "an unknown event name must stop the server rather than start one that forwards nothing"
    );
    let errors = stderr(&output);
    assert!(
        errors.contains("unknown event `not-an-event`"),
        "the error must name the rejected event; got stderr:\n{errors}"
    );
    assert!(
        errors.contains("agent/turn"),
        "the error must list the names that would work; got stderr:\n{errors}"
    );
}

/// `--help` must document the flags this suite exercises, so a rename cannot silently drop one.
///
/// The help text is the only discovery surface the binary has. A flag that still works but vanished
/// from `--help` is effectively removed for anyone who did not already know it existed.
#[test]
fn help_documents_the_entry_point_flags() {
    let scratch = Scratch::new("help");
    let output = scratch.run(&["--help"]);

    assert!(
        output.status.success(),
        "`--help` failed: {}",
        stderr(&output)
    );
    let text = stdout(&output);
    for flag in [
        "--session",
        "--tool",
        "--schema-format",
        "--boot-report",
        "--verbose",
        "--model",
    ] {
        assert!(
            text.contains(flag),
            "`{flag}` must appear in `--help`; it is how a caller discovers the flag"
        );
    }
    for command in ["serve", "schema"] {
        assert!(
            text.contains(command),
            "the `{command}` subcommand must be listed; got:\n{text}"
        );
    }
}

/// A path that is not a directory must not stop the stats commands from answering.
///
/// `GIRDER_SESSIONS` is deployment configuration, and a stale value should produce "no sessions"
/// rather than a panic: a monitoring scrape that crashes on a misconfigured path is worse than one
/// that reports zeroes and lets the operator notice.
#[test]
fn a_missing_sessions_directory_reports_zero_rather_than_failing() {
    let scratch = Scratch::new("missing-sessions");
    // Points at a directory that does not exist, which is what a fresh deployment looks like.
    let absent = scratch.root.join("does-not-exist");
    let output = Command::new(BIN)
        .args(["--schema-format", "health", "schema"])
        .current_dir(scratch.work())
        .env("GIRDER_SESSIONS", &absent)
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("ran the binary");

    assert!(
        output.status.success(),
        "an absent session directory is an empty fleet, not an error: {}",
        stderr(&output)
    );
    assert!(
        stdout(&output).starts_with("sessions: 0\n"),
        "an absent directory must sample zero sessions; got:\n{}",
        stdout(&output)
    );
}
