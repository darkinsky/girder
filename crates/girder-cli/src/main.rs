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
            println!("{}", serde_json::to_string_pretty(&girder_rpc::schema())?);
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
    // The file and discovery tools: without these an agent can run commands but cannot look at a project.
    tools.register(girder_core::read_tool(workspace.clone()));
    tools.register(girder_core::write_tool(workspace.clone()));
    tools.register(girder_core::edit_tool(workspace.clone()));
    tools.register(girder_core::patch_tool(workspace.clone()));
    tools.register(girder_core::glob_tool(workspace.clone()));
    tools.register(girder_core::grep_tool(workspace.clone()));

    // The deliverable set is shared with the tool so a host can render what a run announced: a tool that kept it
    // privately would announce to nobody.
    let deliverables = Arc::new(std::sync::Mutex::new(girder_core::DeliverableSet::new()));
    tools.register(girder_core::present_tool(
        workspace.clone(),
        Arc::clone(&deliverables),
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

    // Held as an `Arc` so the job tools can register against the same registry the runtime uses: two registries would
    // produce two lists that disagree about what is running.
    let tools = Arc::new(tools);
    let runtime = girder_core::AgentRuntime::new(girder_core::RuntimeConfig {
        provider,
        tools: Arc::clone(&tools) as Arc<dyn girder_core::ToolRegistryView>,
        bus: Arc::clone(&bus),
        model: cli.model.clone(),
        max_turns: cli.max_turns,
        token_budget: girder_core::Ceiling::Unlimited,
    });
    // The route table the model picker reads. One route for the configured provider; a deployment with
    // more would register them here rather than in a client.
    let mut routes = girder_core::RouteRegistry::new();
    let route = girder_core::deepseek_route("deepseek", cli.model.clone());
    let _ = routes.register(route);
    let routes = Arc::new(routes);

    let mut remote = girder_core::registry_with_routes(&catalog, &runtime, &routes);

    // The projection bridge: a client reads folded session state without replaying the log itself.
    // Without this the browser would need its own copy of every projection's fold logic, and the two
    // would drift.
    let projections = Arc::new(girder_core::ProjectionRegistry::new());
    girder_core::register_projection_methods(&mut remote, &projections);

    // Background jobs, with the registry the server keeps for this process.
    let jobs = Arc::new(girder_core::JobRegistry::new(256 * 1024));
    girder_core::register_job_methods(&mut remote, &jobs);

    // The job tools share the server's registry, so a job a model started is the same job a client sees. Registering
    // them against a second registry would produce two lists that disagree about what is running.
    girder_core::register_job_tools(&tools, Arc::clone(&jobs));
    register_capability_tools(&tools);

    // Workspace registrations: the directories a session may run in.
    let workspaces = Arc::new(girder_core::api_config::WorkspaceRegistry::new());
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
    std::env::var("GIRDER_SESSIONS").map_or_else(
        |_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".girder")
                .join("sessions")
        },
        std::path::PathBuf::from,
    )
}

/// Registers the tools whose capability a deployment supplies.
///
/// Every one of them is mounted without a provider, and each reports that fact rather than pretending the capability
/// exists: a model that tried a skill and got "no provider" learns something true, while one that got an empty list
/// learns that skills are broken. A separate function because `serve` was already at the length limit and this is the
/// block that reads best on its own.
fn register_capability_tools(tools: &girder_core::ToolRegistry) {
    tools.register(girder_core::skill_tool(Arc::new(
        girder_core::SkillRegistry::new(),
    )));
    tools.register(girder_core::lsp_tool(Arc::new(
        girder_core::LspRegistry::new(),
    )));

    // Terminals need a shell implementation, which this build does not ship; the launcher names the missing step.
    girder_core::register_terminal_tools(
        tools,
        Arc::new(girder_core::TerminalRegistry::new()),
        Arc::new(girder_core::UnavailableLauncher::no_shell()),
    );

    // A workflow needs an engine. Without one the run tool reports that rather than accepting work it cannot do.
    girder_core::register_workflow_tools(tools, Arc::new(girder_core::WorkflowRegistry::new()));
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

    let tools = ToolRegistry::new();
    tools.register(list_tool());
    // The real execution layer: a shell tool scoped to the working directory.
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    tools.register(girder_core::bash_tool(girder_core::PathPolicy::new(
        &workspace,
    )));
    let tools = Arc::new(tools);

    // A live provider when a key is present, the scripted one otherwise. The choice is reported on
    // stderr so a run never silently looks live when it is not.
    let provider: Arc<dyn girder_core::LlmProvider> =
        if let Ok(provider) = girder_core::provider_from_env() {
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
    tools.register(girder_core::subagent_tool(
        Arc::clone(&context),
        cli.max_depth,
    ));

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
            Ok(LoopStep::ToolFinished { id, is_error }) => {
                println!("[tool] {} ({id})", if is_error { "failed" } else { "done" });
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
                        lowered.contains("file")
                            || lowered.contains("目录")
                            || lowered.contains("list")
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
                    } else if self.tool == "bash" {
                        r#"{"command":"ls -1 | head -5"}"#.to_owned()
                    } else {
                        r#"{"path":"."}"#.to_owned()
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
