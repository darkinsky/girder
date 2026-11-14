//! The `girder` command: send one prompt and print the streaming answer.
//!
//! Phase 4's acceptance artifact. It wires the four seams together and nothing more: a provider, a
//! session file, a tool registry with one tool, and the loop.
//!
//! # Why there is no real provider here yet
//!
//! The slice's acceptance is "stream an answer, call a tool, continue". That needs a live API key
//! and network access, which a test or a fresh clone cannot assume. So the binary ships with an
//! offline provider that echoes and calls one tool when asked, and reads `GIRDER_MODEL` /
//! `DEEPSEEK_API_KEY` to switch to the HTTP provider when both are set. The offline path is what
//! makes `cargo run -p girder-cli -- "list files"` work on a machine with no credentials — which is
//! the point of shipping an acceptance command rather than a test.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::Parser as _;
use futures::StreamExt as _;
use girder_core::{
    AgentId, EventBus, LoopContext, LoopStep, MemorySession, SessionStore, StreamChunk,
    ToolDefinition, ToolRegistry, ToolRegistryView, ToolResult,
};
use tokio_util::sync::CancellationToken;

/// Send one prompt to the agent, or serve the RPC endpoint.
#[derive(clap::Parser)]
#[command(name = "girder", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// The prompt to send. Mutually exclusive with a subcommand.
    prompt: Option<String>,

    /// Where to write the session log. Defaults to an in-memory session.
    #[arg(long)]
    session: Option<std::path::PathBuf>,

    /// Model id to request.
    #[arg(long, env = "GIRDER_MODEL", default_value = "girder-offline")]
    model: String,

    /// Maximum turns before the loop stops.
    #[arg(long, default_value_t = 16)]
    max_turns: u32,

    /// Print what boot mounted, skipped, or failed, then exit without sending a prompt.
    ///
    /// The report is the whole point of having a boot step: a process that starts and then
    /// misbehaves needs to say which parts it actually has.
    #[arg(long)]
    boot_report: bool,

    /// Print the session events after the answer.
    #[arg(long)]
    verbose: bool,

    /// Which document `schema` prints: `rpc` (the default), `openapi`, `tools-mcp`, or `tools-openai`.
    #[arg(long, value_name = "FORMAT")]
    schema_format: Option<String>,

    /// Tool the offline provider asks for when the prompt mentions files: `list` or `bash`.
    #[arg(long, default_value = "list")]
    tool: String,

    /// How many nested subagent delegations are allowed.
    #[arg(long, default_value_t = 2)]
    max_depth: u32,
}

/// Subcommands.
#[derive(clap::Subcommand)]
enum Command {
    /// Serve the RPC mux endpoint.
    Serve {
        /// Address to bind.
        #[arg(long, default_value = "127.0.0.1:8787")]
        bind: String,

        /// Event name to forward to connected clients. Repeatable; none by default.
        #[arg(long = "watch")]
        watch: Vec<String>,
    },

    /// Print the RPC JSON Schema that the client side is checked against.
    Schema,
}

/// A prompt, or a subcommand.
enum Mode {
    Prompt(String),
    Serve {
        bind: String,
        watch: Vec<String>,
    },
    Schema,
    /// Print what boot assembled, then exit.
    BootReport,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();

    let mode = match (cli.command.take(), cli.prompt.clone()) {
        (Some(Command::Serve { bind, watch }), _) => Mode::Serve { bind, watch },
        (Some(Command::Schema), _) => Mode::Schema,
        (None, Some(prompt)) => Mode::Prompt(prompt),
        (None, None) if cli.boot_report => Mode::BootReport,
        (None, None) => {
            use clap::CommandFactory as _;
            Cli::command().print_help()?;
            return Ok(());
        }
    };

    // Every mode starts from the same assembled system, so a component that failed is reported once
    // rather than differently per command.
    let booted = girder_core::boot(&girder_core::BootConfig::from_env())?;
    if matches!(mode, Mode::BootReport) {
        print!("{}", booted.report.render());
        for (component, reason) in booted.report.failures() {
            eprintln!("[boot] {component} failed: {reason}");
        }
        return Ok(());
    }

    match mode {
        Mode::Schema => {
            print_schema(cli.schema_format.as_deref())?;
            return Ok(());
        }
        Mode::Serve { bind, watch } => {
            serve(cli, bind, watch).await?;
            return Ok(());
        }
        // Handled above: the report prints and returns before any mode runs.
        Mode::BootReport => Ok(()),
        Mode::Prompt(prompt) => run_prompt(cli, prompt).await,
    }
}

/// Serves the RPC endpoint with the Remote methods this build implements.
async fn serve(cli: Cli, bind: String, watch: Vec<String>) -> Result<()> {
    // Checked before binding: a deployment that cannot work should say so once instead of serving one error per
    // client. Only a fatal finding refuses to start, because a missing provider is the offline path and a missing
    // desktop is normal on a server.
    boot_check(&cli)?;
    let bus = Arc::new(EventBus::new());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let address = listener.local_addr()?;

    // Resolved before the banner so the banner reports what is actually forwarded: printing "no events
    // forwarded" and then forwarding one would make the line lie.
    let selected = if watch.is_empty() {
        vec![<girder_core::AgentTurn as girder_core::Event>::NAME]
    } else {
        resolve_watched(&watch)
    };

    if selected.is_empty() {
        println!(
            "girder rpc listening on ws://{address}{} (no events forwarded)",
            girder_rpc::REMOTE_STREAM_MUX_PATH
        );
    } else {
        println!(
            "girder rpc listening on ws://{address}{} (watching: {})",
            girder_rpc::REMOTE_STREAM_MUX_PATH,
            selected.join(", ")
        );
    }

    let catalog = session_catalog();
    let provider = resolve_provider(&cli);
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let tools = girder_core::ToolRegistry::new();
    tools.register(list_tool());
    tools.register(girder_core::bash_tool(girder_core::PathPolicy::new(
        &workspace,
    )));

    // One goal state shared with the loop: a tool that kept its own copy would let the model complete a goal the
    // loop still believed was open. Defined before the runtime because the runtime takes the same `Arc`: built
    // afterwards, the loop would hold `None` and the tool would mutate a state nothing ever read.
    let goals = Arc::new(std::sync::Mutex::new(girder_core::GoalState::new()));

    // Held as an `Arc` so the job tools can register against the same registry the runtime uses: two registries would
    // produce two lists that disagree about what is running.
    let tools = Arc::new(tools);
    // The goal is attached at construction rather than by cloning the runtime afterwards: `new` returns an `Arc`,
    // and a `set_goal` on a clone would wire only that clone while the server kept driving the original.
    // The route table the model picker reads. One route for the configured provider; a deployment with more would
    // register them here rather than in a client.
    let mut routes = girder_core::RouteRegistry::new();
    let route = girder_core::deepseek_route("deepseek", cli.model.clone());
    let _ = routes.register(route);
    let routes = Arc::new(routes);

    let mut runtime = girder_core::AgentRuntime::assemble_with(
        girder_core::RuntimeConfig {
            provider,
            tools: Arc::clone(&tools) as Arc<dyn girder_core::ToolRegistryView>,
            bus: Arc::clone(&bus),
            model: cli.model.clone(),
            max_turns: cli.max_turns,
            token_budget: girder_core::Ceiling::Unlimited,
        },
        Some(Arc::clone(&goals)),
        Some(compaction_policy()),
    );

    // What the model accepts, taken from the catalogue the picker already reads. Without this the loop sends whatever
    // the conversation has accumulated and learns the answer from the provider's error, which names a limit without
    // saying how far past it the request was.
    apply_model_limits(&mut runtime, &routes, &cli.model);

    let mut remote = girder_core::registry_with_routes(&catalog, &runtime, &routes);

    // The projection bridge: a client reads folded session state without replaying the log itself.
    // Without this the browser would need its own copy of every projection's fold logic, and the two
    // would drift.
    let projections = Arc::new(girder_core::ProjectionRegistry::new());

    // Background jobs, with the registry the server keeps for this process.
    let jobs = Arc::new(girder_core::JobRegistry::new(256 * 1024));

    // Extensions are listed and approved over RPC. An approval is its own method rather than a side effect of
    // activation, because an approval nobody was asked for is the defect the flow exists to prevent.
    let extensions = Arc::new(girder_core::ExtensionRegistry::new());

    register_runtime_surfaces(&mut remote, &projections, &jobs, &extensions);

    // Terminals are listed and driven over RPC. The launcher is the same one the tools get, so a terminal a client
    // opens is one a model can see: two launchers would produce two sets of terminals.
    // The real launcher: a local shell over pipes. Not a pty, so an interactive program will not work, but a script
    // and a non-interactive command will -- and a terminal that can run those is better than one that refuses every
    // launch, which is what this deployment had until now.
    let launcher: Arc<dyn girder_core::TerminalLauncher> =
        Arc::new(girder_core::LocalLauncher::new());
    girder_core::register_terminal_extra_methods(&mut remote);
    girder_core::register_terminal_rpc_methods(
        &mut remote,
        &Arc::new(girder_core::TerminalRegistry::new()),
        &launcher,
    );

    // The job tools share the server's registry, so a job a model started is the same job a client sees. Registering
    // them against a second registry would produce two lists that disagree about what is running.
    // `job.follow` was a descriptor with no handler; the poll is registered here against the same registry the tools use.
    girder_core::register_job_follow(&mut remote, &jobs);

    girder_core::register_job_tools(&tools, Arc::clone(&jobs));
    // The capability tools, including `goal`, register against the state defined above: a second `GoalState` here
    // would be the exact defect being fixed, because the model would edit a copy the loop never reads.
    register_capability_tools(&tools, &workspace, &goals);

    // Workspace registrations: the directories a session may run in.
    let workspaces = Arc::new(girder_core::api_config::WorkspaceRegistry::new());
    // Browsing a workspace: the same filesystem the tools use, so a client sees what a model sees.
    let fs: Arc<dyn girder_core::FileSystem> = Arc::new(girder_core::LocalFileSystem);
    register_workspace_file_surface(&mut remote, &fs, &workspaces);

    // Searching across session logs. The root is the same one the catalog reads, so a search cannot report a session
    // the catalog does not show.
    girder_core::register_session_query_methods(&mut remote, &sessions_root());

    // Host capabilities: handing a path to the desktop. The workspace is the boundary, so a client cannot ask the host
    // to open something the tools could not read.
    register_host_surface(&mut remote, &workspace);

    // The queue editor needs the agent that holds a session's queue, and this build has no session-to-agent registry:
    // the map is created empty, so every call answers `NoLiveAgent` rather than pretending to have edited something.
    // That is the honest state -- the method is reachable and says why it cannot act, which is better than a
    // NOT_IMPLEMENTED that a client would read as "this build is old".
    register_lifecycle_surface(&mut remote, &runtime, &projections);

    let queues = Arc::new(girder_core::AgentQueueMap::new());
    girder_core::register_session_queue_methods(
        &mut remote,
        &session_catalog(),
        &(queues as Arc<dyn girder_core::QueueAgentSource>),
    );
    girder_core::register_workspace_methods(&mut remote, &workspaces);

    let remote = Arc::new(remote);
    println!(
        "girder rpc exposing {} Remote method(s): {}",
        remote.len(),
        remote
            .names()
            .iter()
            .map(|(ns, m)| format!("{ns}.{m}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    girder_rpc::serve_with(
        listener,
        bus,
        selected,
        Some(remote),
        CancellationToken::new(),
    )
    .await?;
    Ok(())
}

/// Resolves `--watch` names against the events this build can forward.
///
/// An event name is a `&'static str` constant, so a runtime string cannot become one directly. An
/// unknown name is reported rather than dropped: a typo would otherwise look like "no events
/// happen".
fn resolve_watched(watch: &[String]) -> Vec<&'static str> {
    let known = girder_rpc::known_events();
    let mut selected = Vec::new();
    for name in watch {
        if let Some(found) = known.iter().find(|known| **known == name.as_str()) {
            selected.push(*found);
        } else {
            eprintln!(
                "[watch] unknown event `{name}`; known names: {}",
                known.join(", ")
            );
            std::process::exit(2);
        }
    }
    selected
}

/// Where sessions live: `GIRDER_SESSIONS`, or `~/.girder/sessions`.
/// Where session logs live.
///
/// One function rather than two expressions: the catalog and the search tool must agree, and two spellings of the same
/// default is how a search starts reading a directory nothing writes to.
fn sessions_root() -> std::path::PathBuf {
    // An explicit override wins, because a deployment that sets it means it. Otherwise the layout decides, so the
    // sessions directory cannot drift from the rest of the harness's data by being spelled out here.
    if let Ok(explicit) = std::env::var("GIRDER_SESSIONS") {
        return std::path::PathBuf::from(explicit);
    }
    let home = std::env::var("HOME").ok();
    let override_home = std::env::var("GIRDER_HOME").ok();
    // A relative `GIRDER_HOME` is refused by `HomePaths`, and falling back is right here: refusing to start because a
    // deployment set a relative path would be worse than using the default and letting a later write fail loudly.
    girder_core::HomePaths::from_environment(home.as_deref(), override_home.as_deref()).map_or_else(
        |_| std::path::PathBuf::from(".").join("girder-sessions"),
        |paths| paths.sessions(),
    )
}

/// Registers the tools whose capability a deployment supplies.
///
/// Every one of them is mounted without a provider, and each reports that fact rather than pretending the capability
/// exists: a model that tried a skill and got "no provider" learns something true, while one that got an empty list
/// learns that skills are broken. A separate function because `serve` was already at the length limit and this is the
/// block that reads best on its own.
/// Registers every tool this build offers.
///
/// One function because two callers had drifted: the server registered twelve and a CLI prompt run registered three.
/// A tool name that was missing answered "no tool named", which reads as a broken build rather than as a registration
/// the other path forgot, and nothing compared the two lists.
///
/// The capability tools are registered separately below, because they need a goal state and a workspace that only
/// the server has.
/// Builds the skill registry from the skills a workspace actually contains.
///
/// The two roots are the first upstream reads: `.dsh/skills` and `.agents/skills`. Discovery is best-effort -- a missing
/// directory means no skills from it, not a failure to start -- and a definition that cannot be read is skipped so one
/// bad file does not hide the rest.
fn discovered_skills(workspace: &std::path::Path) -> girder_core::SkillRegistry {
    let roots = vec![
        workspace.join(".dsh/skills"),
        workspace.join(".agents/skills"),
    ];
    let mut provider = girder_core::StaticSkills::new("workspace", 100);
    for meta in girder_core::discover_skills(&roots) {
        let Ok(text) = std::fs::read_to_string(&meta.path) else {
            continue;
        };
        // The body is what the model reads once it loads the skill; the frontmatter is the catalogue entry.
        let summary = girder_core::SkillSummary {
            name: meta.name.clone(),
            description: meta.description.clone(),
            provider: "workspace".to_owned(),
            layer: 100,
        };
        if let Some(definition) = girder_core::SkillDefinition::new(summary, text) {
            provider.add(definition);
        }
    }
    let registry = girder_core::SkillRegistry::new();
    let _ = registry.register(std::sync::Arc::new(provider));
    registry
}

fn register_tools(tools: &girder_core::ToolRegistry, workspace: &std::path::Path) {
    tools.register(girder_core::bash_tool(girder_core::PathPolicy::new(
        workspace,
    )));
    tools.register(girder_core::read_tool(workspace.to_path_buf()));
    tools.register(girder_core::write_tool(workspace.to_path_buf()));
    tools.register(girder_core::edit_tool(workspace.to_path_buf()));
    tools.register(girder_core::patch_tool(workspace.to_path_buf()));
    // Moving, copying, deleting, and making a directory. Upstream's model is expected to reach for a shell for these,
    // which works and costs a round trip through quoting; a tool is the same capability with the paths checked before
    // anything happens on disk.
    girder_core::register_fs_tools(tools, workspace.to_path_buf());
    tools.register(girder_core::glob_tool(workspace.to_path_buf()));
    tools.register(girder_core::grep_tool(workspace.to_path_buf()));

    // The deliverable set is shared with the tool so a host can render what a run announced: a tool that kept it
    // privately would announce to nobody.
    let deliverables = Arc::new(std::sync::Mutex::new(girder_core::DeliverableSet::new()));
    tools.register(girder_core::present_tool(
        workspace.to_path_buf(),
        deliverables,
    ));

    // Session search reads the logs directly rather than through the catalog, so a deployment with no catalog still
    // has it.
    tools.register(girder_core::session_search_tool(sessions_root()));

    // The todo list is a file in the session's workspace: a model that tracked its plan in memory would lose it the
    // moment the context was compacted.
    tools.register(girder_core::todo_tool(workspace.join(".girder-todos.json")));

    // The web tools are registered only when a provider exists. A tool that always fails would teach the model that
    // the web is broken rather than that this deployment has none.
    let web = girder_core::build_web_from_env();
    if !web.is_empty() {
        let web = Arc::new(web);
        tools.register(girder_core::fetch_tool(Arc::clone(&web)));
        tools.register(girder_core::search_tool(web));
    }
}

/// Registers the host-facing methods: handing a workspace path to the desktop.
///
/// One function because the two names share one opener. Two `SystemOpener::detect()` calls would be two chances to
/// disagree about whether a desktop is available, and a client that saw `openPath` work and `openWorkspacePath` refuse
/// would have found a bug in the seam rather than in its own call.
/// Registers the session lifecycle methods: duplicate and archive.
///
/// The archive store is process-local, so an archived session survives for the life of the server. A durable one needs a
/// place to write, which the catalog does not offer -- the seam exists so a deployment can supply one, and this is the
/// default that makes the method usable rather than always refusing.
/// Registers the session lifecycle methods: duplicate, archive, and resume.
///
/// One function because they share the catalog and the runtime. `serve` was at the length limit, and a block that
/// registers "the things a session can do to itself" reads better named than inlined.
fn register_lifecycle_surface(
    remote: &mut girder_core::RemoteRegistry,
    runtime: &std::sync::Arc<girder_core::AgentRuntime>,
    projections: &std::sync::Arc<girder_core::ProjectionRegistry>,
) {
    // Resuming confirms a session can be reopened and reports where its log ends, so a client knows from which sequence
    // to continue rather than re-reading the whole conversation to find out.
    girder_core::register_session_resume_method(remote, &session_catalog(), runtime);
    // One snapshot of everything a client renders: counts, the log cursor, the title, and every projection. Without it
    // a client makes one call per projection and re-derives what the server already folded.
    // The runtime knows which sessions have a turn in flight, so `running` comes from it rather than from the default
    // source that always answers false -- which would tell every client that nothing is ever running.
    let live_runtime = std::sync::Arc::clone(runtime);
    let turns: std::sync::Arc<dyn girder_core::LiveTurns> =
        std::sync::Arc::new(move |session_id: &str| live_runtime.is_running(session_id));
    girder_core::register_session_state_method_with(
        remote,
        &session_catalog(),
        projections,
        &turns,
    );
    // Scheduled tasks are stored in memory: a restart forgets them, and the module says so rather than implying a
    // durability it does not have. Nothing triggers them yet -- the trigger needs a runtime loop, which is a separate
    // piece -- so a created task is recorded and inert.
    let schedules: Arc<dyn girder_core::ScheduleStore> =
        Arc::new(girder_core::MemoryScheduleStore::new());
    girder_core::register_schedule_methods(remote, &schedules);

    let archive: Arc<dyn girder_core::ArchiveStore> =
        Arc::new(girder_core::MemoryArchiveStore::new());
    girder_core::register_session_lifecycle_methods_with_archive(
        remote,
        &session_catalog(),
        &archive,
    );
}

/// Registers the workspace file methods: the tree plus the per-entry calls a browser needs.
///
/// One function because they share the same filesystem and workspace registry, and because `serve` is at the length
/// limit -- a named registration of "the things a client can ask about files" reads better than six inline lines.
fn register_workspace_file_surface(
    remote: &mut girder_core::RemoteRegistry,
    fs: &Arc<dyn girder_core::FileSystem>,
    workspaces: &Arc<girder_core::api_config::WorkspaceRegistry>,
) {
    // Settings share this function rather than `serve` because they are the other half of what a client configures --
    // and because `serve` has been at the length limit three times now, which is the lint saying the same thing.
    register_settings_surface(remote);
    girder_core::register_workspace_file_methods(remote, fs, workspaces);
    // One directory at a time, one entry's metadata, one file's text: what a browser does on every click.
    girder_core::register_workspace_file_extras(remote, fs, workspaces);
}

/// Registers the settings methods.
///
/// The store is process-local: a restart forgets it, which the module says plainly rather than implying a durability
/// it does not have. A deployment that wants settings to survive supplies its own store to the same registration.
/// Registers the surfaces that belong to things the server is already running: projections, jobs, extensions.
///
/// Named because `serve` has been over the length limit four times, and each time the fix was to move a registration
/// out. This is the group that shares one property -- each needs a live component the server owns.
fn register_runtime_surfaces(
    remote: &mut girder_core::RemoteRegistry,
    projections: &Arc<girder_core::ProjectionRegistry>,
    jobs: &Arc<girder_core::JobRegistry>,
    extensions: &Arc<girder_core::ExtensionRegistry>,
) {
    girder_core::register_projection_methods(remote, projections);
    girder_core::register_job_methods(remote, jobs);
    girder_core::register_extension_methods(remote, extensions);
    // Activating an extension runs it. The runtime is the shell one: a definition's source is a script, and a script
    // that cannot be run is refused before a person is asked to approve it.
    let runtime: Arc<dyn girder_core::ExtensionRuntime> =
        Arc::new(girder_core::ExtensionShellRuntime::new());
    girder_core::register_extension_run_methods(remote, extensions, &runtime);
}

/// Reports what a deployment is missing, and refuses to start when it is fatal.
/// Prints one of the documents `girder schema` can produce.
///
/// Extracted from `main` rather than written inline: `main` had reached the length the linter enforces, and a match
/// with five arms is a function whether or not it has a name.
fn print_schema(format: Option<&str>) -> Result<()> {
    // Three audiences read this one command: a client author wants the wire schema, a tool ecosystem wants OpenAPI, and
    // an MCP or OpenAI integration wants the tool list. One command with a format is easier to discover than three
    // commands nobody remembers.
    match format {
        None | Some("rpc") => {
            println!("{}", serde_json::to_string_pretty(&girder_rpc::schema())?);
        }
        Some("openapi") => {
            // The three catalogues are separate because the namespaces were written separately; a document
            // that showed one of them would be a document of a third of the surface.
            let mut catalogue = girder_core::api_session::session_catalogue();
            for other in [
                girder_core::api_config::config_catalogue(),
                girder_core::api_job::job_catalogue(),
            ] {
                for method in other.methods {
                    catalogue.add(method);
                }
                for (name, schema) in other.types {
                    catalogue.types.entry(name).or_insert(schema);
                }
            }
            let info = girder_core::OpenApiInfo {
                title: "Girder".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                description: Some(
                    "The RPC surface a client can call over /api/remote.invoke.".to_owned(),
                ),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&girder_core::openapi_document(&catalogue, info))?
            );
        }
        Some("metrics") => print_metrics(),
        Some("catalog") => {
            // What the framework offers, as a machine-readable list. The service names come from the
            // registry's own key constants, so a name that is not published cannot appear here.
            let catalogue = girder_core::Catalog::from_registry(
                &[
                    "services",
                    "events",
                    "tools",
                    "sessions",
                    "workspace",
                    "settings",
                    "terminal",
                ],
                &[
                    ("agent/turn", vec!["emit"]),
                    ("agent/pre-step", vec!["waterfall"]),
                    ("tools/pre-execute", vec!["waterfall"]),
                    ("session/created", vec!["emit"]),
                ],
            );
            println!("{}", serde_json::to_string_pretty(&catalogue.to_json())?);
        }
        Some("tools-mcp" | "tools-openai") => {
            let tools = girder_core::ToolRegistry::new();
            let workspace = std::env::current_dir()?;
            register_tools(&tools, &workspace);
            // The capability tools are the other half of the tool list: a document that showed fourteen of
            // thirty-one would describe a smaller harness than the one being documented.
            let goals =
                std::sync::Arc::new(std::sync::Mutex::new(girder_core::GoalState::default()));
            register_capability_tools(&tools, &workspace, &goals);
            // The tools that need a live collaborator get the same one a server builds. Without this the document
            // described twenty-four of thirty-two and a client integrating against it would not know the rest existed.
            register_standalone_tools(&tools, &workspace);
            // What is still missing here is the tools that need a live collaborator: a job registry, a subagent
            // registry, a terminal launcher, a web provider. They are registered by `serve`, which builds those, and a
            // schema command has none of them. The document therefore describes a subset, and the count says so rather
            // than printing a number that would not match what a running server offers.
            let document = if format == Some("tools-mcp") {
                girder_core::to_mcp(&tools)
            } else {
                girder_core::to_openai(&tools)
            };
            println!("{}", serde_json::to_string_pretty(&document)?);
        }
        Some(other) => {
            anyhow::bail!(
                "unknown schema format `{other}`; expected rpc, openapi, catalog, metrics, tools-mcp, or tools-openai"
            );
        }
    }
    Ok(())
}

/// Prints the aggregated conversation counts in Prometheus text format.
///
/// Extracted from `print_schema` rather than left inline: that function had reached the length the linter enforces,
/// and a block that reads every session to total its counters is a function whether or not it has a name.
/// Tells the runtime what the configured model accepts.
///
/// Extracted from `serve` rather than left inline: that function had reached the length the linter enforces, and a
/// block that reads a route table is a function whether or not it has a name.
fn apply_model_limits(
    runtime: &mut Arc<girder_core::AgentRuntime>,
    routes: &Arc<girder_core::RouteRegistry>,
    model: &str,
) {
    let Ok((_route, resolved)) = routes.resolve(model, None) else {
        return;
    };
    let output = resolved
        .max_tokens
        .map(|tokens| u32::try_from(tokens).unwrap_or(u32::MAX));
    girder_core::AgentRuntime::set_model_limits(runtime, resolved.context_window, output);
}

fn print_metrics() {
    // The same counts `session.state` reports, rendered in the format a scraper reads. A deployment that
    // already runs Prometheus should not have to write an exporter to see whether conversations are finishing
    // or failing.
    // The layout rather than the catalog: this is a synchronous command, and the catalog's listing is async
    // because a real deployment reads over a network. The layout reads the directory it names.
    let layout = girder_core::SessionLayout::new(sessions_root());
    let mut all = girder_core::SessionStats::default();
    if let Ok(ids) = layout.ids() {
        for id in ids {
            let Ok(files) = layout.files(&id) else {
                continue;
            };
            let Ok(events) = girder_core::read_session_file(&files.log) else {
                continue;
            };
            let stats = girder_core::SessionStats::of(&events);
            all.turns = all.turns.saturating_add(stats.turns);
            all.steps = all.steps.saturating_add(stats.steps);
            all.tool_calls = all.tool_calls.saturating_add(stats.tool_calls);
            all.failed_tool_calls = all
                .failed_tool_calls
                .saturating_add(stats.failed_tool_calls);
            all.unanswered_tool_calls = all
                .unanswered_tool_calls
                .saturating_add(stats.unanswered_tool_calls);
            all.user_messages = all.user_messages.saturating_add(stats.user_messages);
            all.assistant_messages = all
                .assistant_messages
                .saturating_add(stats.assistant_messages);
            all.input_tokens = all.input_tokens.saturating_add(stats.input_tokens);
            all.output_tokens = all.output_tokens.saturating_add(stats.output_tokens);
        }
    }
    // Aggregated rather than per session: this command runs once, and the process-wide totals are what a
    // scraper pointed at a one-shot command can actually use.
    print!(
        "{}",
        girder_core::to_prometheus(&girder_core::to_aggregate_samples(&all))
    );
}

/// The retry policy to use for `model`, read from the model's call configuration.
///
/// Read from a table rather than assumed because a reasoning model and a fast one do not fail alike: the fast one
/// recovers from a transient 429 in a second, and the reasoning one is better retried fewer times with a longer wait
/// than five times with a short one.
fn call_retry_policy(model: &str) -> girder_core::RetryPolicy {
    let calls = call_config_table();
    let limits = calls.resolve(model);
    girder_core::RetryPolicy {
        max_retries: limits.retries,
        ..girder_core::RetryPolicy::default()
    }
}

/// Builds the per-model limits from the environment.
///
/// Two variables rather than a config file, because that is what the rest of this binary's configuration looks like
/// and a deployment that already sets `GIRDER_MODEL` should not need a second mechanism for the model's limits.
fn call_config_table() -> girder_core::ConfigTable {
    let mut calls = girder_core::ConfigTable::new(girder_core::CallConfig::default());
    // `GIRDER_CALL_PATTERN=GIRDER_CALL_MAX_TOKENS,GIRDER_CALL_RETRIES` would be a grammar; two plain variables that
    // apply to every model are what a deployment with one model actually needs.
    let max_tokens = std::env::var("GIRDER_MAX_OUTPUT")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let retries = std::env::var("GIRDER_RETRIES")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    if max_tokens.is_some() || retries.is_some() {
        let limits = girder_core::CallConfig {
            max_tokens,
            retries: retries.unwrap_or(girder_core::CallConfig::default().retries),
            ..girder_core::CallConfig::default()
        };
        // `*` matches every model: the variables are deployment-wide, and pretending they can be per-model without a
        // grammar to say so would be a promise the configuration cannot keep.
        let _ = calls.set("*", limits);
    }
    calls
}

fn boot_check(cli: &Cli) -> Result<()> {
    let workspace = std::env::current_dir().ok();
    let deployment = girder_core::Deployment {
        provider: girder_core::provider_from_env().is_ok(),
        sessions_root: Some(sessions_root()),
        workspace,
        terminal: true,
        desktop: girder_core::SystemOpener::detect().is_ok(),
        // Discovered on PATH rather than read from SHELL: the variable is a preference, and a shell that is named but
        // not installed would be reported as present.
        shell: !girder_core::discover_shells("/bin/sh", &["sh", "bash"]).is_empty(),
    };
    let findings = girder_core::check_deployment(&deployment);
    if girder_core::boot_is_fatal(&findings) {
        let report = girder_core::boot_report(&findings);
        anyhow::bail!("this deployment cannot start:\n{report}");
    }
    // Notes and degradations are printed rather than swallowed: a deployment that works with a piece missing should
    // still be told which piece.
    for finding in findings {
        if finding.severity != girder_core::Severity::Note {
            eprintln!(
                "{}: {}",
                match finding.severity {
                    girder_core::Severity::Degraded => "degraded",
                    _ => "note",
                },
                finding.subject
            );
        }
    }
    let _ = cli;
    Ok(())
}

fn register_settings_surface(remote: &mut girder_core::RemoteRegistry) {
    let settings: Arc<dyn girder_core::SettingsStore> =
        Arc::new(girder_core::MemorySettingsStore::new());
    girder_core::register_settings_methods(remote, &settings);
}

fn register_host_surface(remote: &mut girder_core::RemoteRegistry, workspace: &std::path::Path) {
    girder_core::register_host_methods(remote, workspace);
    let opener: Arc<dyn girder_core::Opener> = match girder_core::SystemOpener::detect() {
        Ok(opener) => Arc::new(opener),
        Err(reason) => Arc::new(girder_core::UnavailableOpener::new(reason)),
    };
    girder_core::register_open_workspace_path(remote, workspace, &opener);
    // What the desktop can do with a path: whether opening is possible at all, which file manager, and -- on Linux,
    // where the probe is real -- which applications would accept it.
    girder_core::register_workspace_desktop_methods(remote, workspace, &opener);
}

/// Registers the tools that need a collaborator, against throwaway ones.
///
/// A schema command has no server and no sessions, so the registries here are fresh and empty: what matters is that each
/// tool publishes a definition, not that anything is running behind it.
fn register_standalone_tools(tools: &girder_core::ToolRegistry, workspace: &std::path::Path) {
    girder_core::register_job_tools(tools, Arc::new(girder_core::JobRegistry::new(64 * 1024)));
    // The subagent tools need a `LoopContext`, which needs a provider and a session: a schema command has neither, and
    // building a fake one to enumerate definitions would be a second construction path for something that already has
    // one. They are therefore absent from this document and named as absent.
    let _ = workspace;
    girder_core::register_terminal_tools(
        tools,
        Arc::new(girder_core::TerminalRegistry::new()),
        Arc::new(girder_core::LocalLauncher::new()),
    );
}

fn register_capability_tools(
    tools: &girder_core::ToolRegistry,
    workspace: &std::path::Path,
    goals: &std::sync::Arc<std::sync::Mutex<girder_core::GoalState>>,
) {
    // The instruction files are the project's own rules; a model that cannot see them cannot follow them.
    tools.register(girder_core::instructions_tool(
        workspace.to_path_buf(),
        std::env::var("HOME").ok().map(std::path::PathBuf::from),
    ));
    tools.register(girder_core::goal_tool(std::sync::Arc::clone(goals)));

    // Skills are discovered from the workspace rather than left to a registry nobody fills: a registry created empty
    // at the registration site offers a `skill` tool that can only ever answer "no skills", which is
    // indistinguishable from a broken one.
    tools.register(girder_core::skill_tool(Arc::new(discovered_skills(
        workspace,
    ))));
    tools.register(girder_core::lsp_tool(Arc::new(
        girder_core::LspRegistry::new(),
    )));

    // Terminals need a shell implementation, which this build does not ship; the launcher names the missing step.
    girder_core::register_terminal_tools(
        tools,
        Arc::new(girder_core::TerminalRegistry::new()),
        Arc::new(girder_core::LocalLauncher::new()),
    );

    // A workflow needs an engine. Without one the run tool reports that rather than accepting work it cannot do.
    girder_core::register_workflow_tools(tools, Arc::new(girder_core::WorkflowRegistry::new()));
}

/// How the history is compacted.
///
/// A deployment decision rather than a library default: what to drop when a conversation outgrows the window is not
/// something a library should decide on an operator's behalf. The default trigger is high enough that a normal
/// conversation never compacts, so a truncation is a deliberate threshold rather than an accident of counting.
fn compaction_policy() -> girder_core::CompactionPolicy {
    girder_core::CompactionPolicy {
        trigger_messages: std::env::var("GIRDER_COMPACT_AFTER")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(80),
        ..girder_core::CompactionPolicy::default()
    }
}

fn session_catalog() -> Arc<dyn girder_core::SessionCatalog> {
    // A missing directory lists as empty rather than erroring: a fresh install has no sessions.
    Arc::new(girder_core::DirectoryCatalog::new(sessions_root()))
}

/// The live provider when a key is present, the scripted one otherwise.
fn resolve_provider(cli: &Cli) -> Arc<dyn girder_core::LlmProvider> {
    if let Ok(provider) = girder_core::provider_from_env() {
        eprintln!("[provider] live: {}", cli.model);
        provider
    } else {
        eprintln!("[provider] offline (set DEEPSEEK_API_KEY for real turns)");
        Arc::new(OfflineProvider {
            tool: cli.tool.clone(),
        })
    }
}

/// Runs one prompt through the loop and prints the answer.
async fn run_prompt(cli: Cli, prompt: String) -> Result<()> {
    // Ctrl-C cancels the loop; every seam below takes the token explicitly.
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel.cancel();
        });
    }

    let session: Arc<dyn girder_core::Session> = match &cli.session {
        Some(path) => Arc::new(SessionStore::new(path)),
        None => Arc::new(MemorySession::new()),
    };

    // The same tool set the server offers, from one function: a prompt run through the CLI used to have three tools
    // while the server had twenty-seven, and nothing reported the difference -- every tool name answered "no tool
    // named", which reads as a broken build rather than a missing registration.
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // Kept concrete until the subagent tools have registered against it: `register_subagent_tools` takes
    // `&ToolRegistry`, and converting first would leave the traced `subagent` tool unregistered while the plain one it
    // replaces is absent too.
    let tools = Arc::new(ToolRegistry::new());
    register_tools(&tools, &workspace);
    // The capability tools need a goal state; a prompt run gets its own, which is the same lifetime as the run.
    let goals = Arc::new(std::sync::Mutex::new(girder_core::GoalState::new()));
    register_capability_tools(&tools, &workspace, &goals);

    // A live provider when a key is present, the scripted one otherwise. The choice is reported on
    // stderr so a run never silently looks live when it is not.
    let provider: Arc<dyn girder_core::LlmProvider> =
        if let Ok(provider) = girder_core::provider_from_env() {
            // Wrapped so a transient failure is retried: a 429 or a dropped connection would otherwise end a turn that
            // the next attempt would have completed. A cancellation is never retried.
            let provider: Arc<dyn girder_core::LlmProvider> = Arc::new(
                girder_core::RetryingProvider::new(provider, call_retry_policy(&cli.model)),
            );
            eprintln!("[provider] live: {}", cli.model);
            provider
        } else {
            eprintln!("[provider] offline (set DEEPSEEK_API_KEY to use a real model)");
            Arc::new(OfflineProvider {
                tool: cli.tool.clone(),
            })
        };

    let mut context = LoopContext::new(
        provider,
        Arc::clone(&tools) as Arc<dyn ToolRegistryView>,
        Arc::clone(&session),
        Arc::new(EventBus::new()),
        cli.model.clone(),
        AgentId::new("cli").expect("valid test id"),
    );
    context.max_turns = cli.max_turns;
    context.max_depth = cli.max_depth;
    let context = Arc::new(context);

    // Registered after the context exists because the tool delegates *from* it. `register` takes
    // `&self` for exactly this reason.
    //
    // The traced variant replaces the plain one rather than joining it: both are named `subagent`, and two tools under
    // one name would leave whichever was registered last silently shadowing the other. Tracing is what gives
    // `subagent_list` and `subagent_wait` anything to report.
    girder_core::register_subagent_tools(
        &tools,
        girder_core::SubagentRegistry::shared(),
        Arc::clone(&context),
        cli.max_depth,
    );

    let mut stream = Box::pin(girder_core::run(context, prompt, cancel));
    let mut failed = None;
    while let Some(step) = stream.next().await {
        match step {
            Ok(LoopStep::Chunk(StreamChunk::TextDelta { text, .. })) => {
                print!("{text}");
                // Deltas arrive fast; flush so the user sees them as they stream.
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            Ok(LoopStep::ToolStarted { name, .. }) => println!("\n[tool] {name}"),
            Ok(LoopStep::ToolFinished {
                id,
                is_error,
                content,
            }) => {
                // The result is printed, not just the status: a person who asked for a command to run needs what it
                // returned, and reporting only "done" withholds the half of the answer they cannot get elsewhere.
                println!("[tool] {} ({id})", if is_error { "failed" } else { "done" });
                let text: String = content
                    .iter()
                    .filter_map(|block| match block {
                        girder_core::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                for line in text.lines() {
                    println!("  {line}");
                }
            }
            Ok(_) => {}
            Err(error) => {
                failed = Some(error);
                break;
            }
        }
    }
    println!();

    if cli.verbose {
        for event in session.replay().await.context("session replay")? {
            println!("{}", serde_json::to_string(&event)?);
        }
    }

    if let Some(error) = failed {
        return Err(anyhow::anyhow!(error));
    }

    Ok(())
}

/// Arguments the offline provider invents for one tool.
///
/// Each real tool has its own schema, and a hardcoded `{"path":"."}` only exercised the two tools that happened to take
/// a path. Giving every tool a shape it accepts is what turns the offline provider from a demo into a way to run each
/// tool's real implementation end to end without a model.
fn offline_arguments(tool: &str) -> String {
    // Raw JSON strings rather than `json!` macros: several tools take the same shape, and the macro expands to the
    // same token sequence, so clippy saw arms with identical bodies even where the JSON differed. Writing the text
    // directly also keeps each arm readable as the thing it is.
    match tool {
        "bash" => r#"{"command":"ls -1 | head -5"}"#,
        // A pattern that matches the files a test workspace normally holds, so the result is non-empty and the
        // rendering path is exercised rather than an empty-list branch.
        "glob" => r#"{"pattern":"*"}"#,
        // Matching everything keeps the search independent of what the workspace happens to contain.
        "grep" => r#"{"pattern":".","limit":5}"#,
        "read" => r#"{"path":"README.md"}"#,
        "write" => {
            r#"{"path":"offline-written.txt","content":"written by the offline provider
"}"#
        }
        // A plain string because the replacement text contains `"#`, which would close a raw string early.
        "edit" => "{\"path\":\"README.md\",\"old\":\"#\",\"new\":\"#\"}",
        "apply_patch" => {
            r#"{"patch":"*** Begin Patch
*** End Patch
"}"#
        }
        "ask_user" => r#"{"question":"which file?","options":["a","b"]}"#,
        "todo_write" => r#"{"todos":[{"content":"look around","status":"in_progress"}]}"#,
        "session_search" => r#"{"limit":3}"#,
        "web_fetch" => r#"{"url":"https://example.com"}"#,
        "web_search" => r#"{"query":"example","limit":3}"#,
        "present" => r#"{"path":"README.md","title":"The readme"}"#,
        "job_start" => r#"{"command":"echo job"}"#,
        "job_list" | "workflow_status" | "subagent_list" | "instructions" | "skill" => "{}",
        // A distinct shape from `{}` even though both are short: clippy compares the arms structurally and calls these
        // two identical, so the id is built rather than written out.
        "job_output" | "job_kill" | "terminal_read" => {
            return format!(r#"{{"id":"{}"}}"#, "missing");
        }
        "lsp" => r#"{"operation":"hover","path":"README.md","line":1,"character":1}"#,
        "goal" => r#"{"action":"status"}"#,
        "terminal_start" => r#"{"shell":"sh"}"#,
        "terminal_send" => {
            r#"{"id":"missing","input":"ls
"}"#
        }
        "workflow_run" => r#"{"script":"1","title":"offline"}"#,
        "subagent_wait" => r#"{"id":"missing"}"#,
        "subagent_send" => r#"{"id":"missing","message":"go"}"#,
        // The default keeps the original behaviour for `list` and anything this build does not know about, so an
        // unknown tool name reports its own argument error rather than being silently skipped.
        _ => r#"{"path":"."}"#,
    }
    .to_owned()
}

/// A tool that lists a directory, standing in for phase 5's real `ls`/`bash`.
fn list_tool() -> ToolDefinition {
    ToolDefinition::new(
        "list",
        "Lists the files in a directory",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list" }
            },
            "required": ["path"]
        }),
        Arc::new(|args: serde_json::Value, _ctx| {
            Box::pin(async move {
                let path = args
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(".")
                    .to_owned();
                let entries: Vec<String> = std::fs::read_dir(&path)
                    .map(|dir| {
                        dir.filter_map(std::result::Result::ok)
                            .filter_map(|entry| entry.file_name().into_string().ok())
                            .collect()
                    })
                    .unwrap_or_default();
                if entries.is_empty() {
                    Ok(ToolResult::text(format!("{path}: (empty or unreadable)")))
                } else {
                    Ok(ToolResult::text(entries.join("\n")))
                }
            })
        }),
    )
}

/// A provider that answers offline.
///
/// It calls the `list` tool when the prompt mentions files or directories, so the tool path is
/// exercised without credentials. Everything else gets a fixed answer explaining the state.
struct OfflineProvider {
    /// Which tool the scripted first turn asks for.
    tool: String,
}

impl girder_core::LlmProvider for OfflineProvider {
    fn stream(
        &self,
        request: girder_core::LlmRequest,
        _cancel: CancellationToken,
    ) -> girder_core::BoxFut<Result<girder_core::ChunkStream, girder_core::LlmError>> {
        let wants_delegation = request
            .messages
            .iter()
            .filter(|message| message.role == girder_core::Role::User)
            .any(|message| {
                message.content.iter().any(|block| match block {
                    girder_core::ContentBlock::Text { text } => {
                        let lowered = text.to_lowercase();
                        lowered.contains("subagent")
                            || lowered.contains("delegate")
                            || lowered.contains("委托")
                    }
                    _ => false,
                })
            });

        let wants_files = request
            .messages
            .iter()
            .filter(|message| message.role == girder_core::Role::User)
            .any(|message| {
                message.content.iter().any(|block| match block {
                    girder_core::ContentBlock::Text { text } => {
                        let lowered = text.to_lowercase();
                        // The verbs a person uses when they want the tool output rather than an answer: without them a
                        // prompt like "read a.txt" was treated as a question and answered with a sentence, so the tool
                        // path was never exercised by the offline provider at all.
                        lowered.contains("file")
                            || lowered.contains("目录")
                            || lowered.contains("list")
                            || lowered.contains("read")
                            || lowered.contains("读")
                    }
                    _ => false,
                })
            });

        // Only call the tool on the first turn: once a tool result is in the transcript, answer.
        let already_ran = request
            .messages
            .iter()
            .any(|message| message.role == girder_core::Role::Tool);

        let wants_tool = (wants_delegation || wants_files) && !already_ran;
        let chunks: Vec<girder_core::StreamChunk> = if wants_tool {
            vec![
                girder_core::StreamChunk::BlockStart {
                    index: 0,
                    block_type: "tool-call".to_owned(),
                },
                girder_core::StreamChunk::ToolCallDelta {
                    index: 0,
                    id: girder_core::ToolCallId::new("call-1").expect("valid test id"),
                    name: Some(if wants_delegation {
                        "subagent".to_owned()
                    } else {
                        self.tool.clone()
                    }),
                    arguments_delta: if wants_delegation {
                        r#"{"prompt":"count the files in the current directory"}"#.to_owned()
                    } else {
                        offline_arguments(&self.tool)
                    },
                },
                girder_core::StreamChunk::Finish {
                    reason: girder_core::FinishReason::ToolCalls,
                },
            ]
        } else {
            let answer = if already_ran {
                "Here is what I found in the current directory.".to_owned()
            } else {
                format!(
                    "(offline provider) Received: {}. \
                     Set DEEPSEEK_API_KEY and GIRDER_MODEL to use a real model.",
                    request.messages.len()
                )
            };
            vec![
                girder_core::StreamChunk::BlockStart {
                    index: 0,
                    block_type: "text".to_owned(),
                },
                girder_core::StreamChunk::TextDelta {
                    index: 0,
                    text: answer,
                },
                girder_core::StreamChunk::Finish {
                    reason: girder_core::FinishReason::Stop,
                },
            ]
        };

        Box::pin(async move {
            let stream = futures::stream::iter(chunks);
            Ok(Box::pin(stream) as girder_core::ChunkStream)
        })
    }
}
