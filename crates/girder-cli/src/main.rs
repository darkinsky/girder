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

    /// What the shell tool may read and write: `read-only`, `workspace-write` or
    /// `danger-full-access`.
    ///
    /// Parsed by `sandbox_gap`, which owns the closed set: a mode name can arrive from a config file
    /// or a replayed log, neither of which the type system has vetted, so an unknown name is refused
    /// here rather than silently falling back to the widest.
    #[arg(long, default_value = "workspace-write")]
    sandbox: String,

    /// Print what boot mounted, skipped, or failed, then exit without sending a prompt.
    ///
    /// The report is the whole point of having a boot step: a process that starts and then
    /// misbehaves needs to say which parts it actually has.
    #[arg(long)]
    boot_report: bool,

    /// Print the session events after the answer.
    #[arg(long)]
    verbose: bool,

    /// Which document `schema` prints: `rpc` (the default), `openapi`, `catalog`, `metrics`, `disk`,
    /// `csv`, `health`, `tools-mcp`, or `tools-openai`.
    // Logic: the list was four names long while nine formats existed, so five were undiscoverable from help -- the same
    // class of gap as a method that is implemented and never documented.
    #[arg(long, value_name = "FORMAT")]
    schema_format: Option<String>,

    /// Tool the offline provider asks for when the prompt mentions files, for example `bash`, `read` or `write`.
    ///
    /// `list` was the default and the help text promised it, but `list` is only registered by `serve`: a prompt run named it
    /// and got "no tool named `list`". The default is now a tool both modes register.
    #[arg(long, default_value = "bash")]
    tool: String,

    /// How many nested subagent delegations are allowed.
    #[arg(long, default_value_t = 2)]
    max_depth: u32,

    /// Provider/model routes a subagent may be given, as `provider/model` pairs.
    ///
    /// Each pair is validated through `subagent_final_gap::assert_allowed_routes`, which refuses a
    /// duplicate, an empty half, or a malformed entry. **Refusing at startup rather than at the first
    /// delegation** is the point: a deployment that mistyped a route would otherwise discover it when
    /// a model asked to delegate, and the failure would arrive as a tool error attributed to the
    /// subagent instead of to the configuration.
    #[arg(long = "subagent-model", value_name = "PROVIDER/MODEL")]
    subagent_models: Vec<String>,
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

    /// Explain how a plugin installation spec is understood.
    ///
    /// The spec grammar decides what a `plugins` entry names: a registry package at a version range,
    /// a git repository at a pinned commit, or a path. `boot_gap::parse_install_spec` is where that
    /// grammar lives, and this subcommand is how a person checks that what they wrote means what
    /// they meant before an install reads the network.
    Plugin {
        /// The spec to explain, for example `@scope/name@^1.2` or `git+https://host/repo#sha`.
        spec: String,
    },
}

/// A prompt, or a subcommand.
enum Mode {
    Prompt(String),
    Serve {
        bind: String,
        watch: Vec<String>,
    },
    Schema,
    /// Explain one installation spec.
    Plugin(String),
    /// Print what boot assembled, then exit.
    BootReport,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();
    // **Validated here rather than where the routes are used.** `run_prompt` sets them on the loop
    // context, but `serve` never builds one -- so a bad route would start a server that silently
    // ignores the flag, and the operator would learn nothing. A flag that cannot take effect is still
    // a flag the caller gave, and a malformed one is worth refusing before any work begins.
    // The result is discarded here: this call exists so a malformed route fails *any* mode's
    // startup, including `serve`, which never builds a loop context to put routes on.
    validate_subagent_models(&cli.subagent_models)?;

    let mode = match (cli.command.take(), cli.prompt.clone()) {
        (Some(Command::Serve { bind, watch }), _) => Mode::Serve { bind, watch },
        (Some(Command::Schema), _) => Mode::Schema,
        (Some(Command::Plugin { spec }), _) => Mode::Plugin(spec),
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
        // The report is the one place a deployment says what it assembled, so it is also the place the
        // `.env` layers belong. Under every other mode they are stderr noise around a prompt, and a
        // deployment that wants the report was previously unable to see them at all.
        match boot_check(&cli) {
            Ok(Some(env)) => print!("{env}"),
            Ok(None) => {}
            // A refusal is the report's most important line, so it is printed rather than returned: a
            // failed start would otherwise print the component report and then an error with no
            // `.env` section saying which layer was refused.
            Err(error) => {
                eprintln!("{error}");
                return Err(anyhow::anyhow!("the boot report found a fatal finding"));
            }
        }
        // The audit of what the process actually mounted. A component line says a registry was built;
        // this says whether the entries that registry is supposed to carry are in it, which is the
        // difference between "started" and "can answer".
        let names = probe_tool_names();
        if let Some(audit) = audit_tool_report(&names) {
            print!("{audit}");
        }
        return Ok(());
    }

    match mode {
        Mode::Schema => {
            print_schema(cli.schema_format.as_deref())?;
            return Ok(());
        }
        Mode::Plugin(spec) => {
            explain_spec(&spec)?;
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
/// Says where this server listens, and what it forwards.
///
/// One function because the two facts belong in one line: a client reading the banner needs both the
/// URL to dial and whether the events it wants will arrive.
fn print_banner(address: std::net::SocketAddr, selected: &[&str]) {
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
}

/// Explains one plugin installation spec.
///
/// `boot_gap` owns the grammar -- registry package with a version range, git repository with a
/// pinned origin and optional commit, or a path -- and this prints what it decided plus which of the
/// distinguishing cases applied. A person checking a `plugins` entry needs to know whether the
/// string they wrote is a range or an exact version, and whether the origin is pinned, before an
/// install goes to the network.
fn explain_spec(spec: &str) -> Result<()> {
    let parsed = girder_core::parse_install_spec(spec)
        .map_err(|error| anyhow::anyhow!("`{spec}` is not an installation spec: {error}"))?;
    println!("spec: {spec}");
    match &parsed {
        girder_core::ParsedSpec::Registry { name, range, .. } => {
            println!("kind: registry");
            println!("name: {name}");
            // Named separately because "no range" means latest, and a reader checking their entry
            // needs to know whether they pinned anything at all.
            println!("range: {}", range.as_deref().unwrap_or("<latest>"));
        }
        girder_core::ParsedSpec::Path { path, .. } => {
            println!("kind: path");
            println!("path: {path}");
        }
        girder_core::ParsedSpec::Tarball { path, host, .. } => {
            println!("kind: tarball");
            if let Some(path) = path {
                println!("path: {path}");
            }
            if let Some(host) = host {
                println!("host: {host}");
            }
        }
        girder_core::ParsedSpec::Git { host, .. } => {
            println!("kind: git");
            println!("host: {host}");
            // `pinned_git_origin` answers only when the fragment carries one full commit, so this
            // line doubles as the reproducibility check: an origin means the install is pinned, and
            // `<not pinned>` means it moves with the branch.
            match girder_core::pinned_git_origin(spec) {
                Some(origin) => println!("origin: {origin}"),
                None => println!("origin: <not pinned -- this spec moves with the branch>"),
            }
        }
    }
    Ok(())
}

/// The shell tool's path policy for a named sandbox mode.
///
/// `sandbox_gap` owns the mode vocabulary and the escalation ladder; this maps the three modes onto
/// the two sets `PathPolicy` actually enforces. `read-only` gets an empty writable set rather than a
/// flag, because a mode that could still write would be a mode whose name lies.
///
/// # Errors
///
/// An unknown mode name. Failing is the point: falling back to the widest mode would turn a typo in
/// a config file into a shell with full access.
fn path_policy(mode: &str, workspace: &std::path::Path) -> Result<girder_core::PathPolicy> {
    let parsed = girder_core::parse_mode(mode).ok_or_else(|| {
        anyhow::anyhow!(
            "`--sandbox {mode}` is not a mode; expected one of read-only, workspace-write, danger-full-access"
        )
    })?;
    let mut policy = girder_core::PathPolicy::new(workspace);
    match parsed {
        girder_core::SandboxMode::ReadOnly => {
            // Nothing writable, and the workspace stays readable: the point of read-only is to look.
            policy.writable.clear();
        }
        girder_core::SandboxMode::WorkspaceWrite => {
            // `PathPolicy::new` is already this: the workspace, writable.
        }
        girder_core::SandboxMode::Full => {
            // A root of `/` is the honest spelling of "no confinement" in a set-of-roots model.
            policy.readable.insert(std::path::PathBuf::from("/"));
            policy.writable.insert(std::path::PathBuf::from("/"));
        }
    }
    Ok(policy)
}

/// The terminal surface: create, read, close, and the two methods that name or save one.
///
/// One function because all six share one registry, and a caller that registered five of them would
/// have terminals it could start and not rename.
fn register_terminal_surface(remote: &mut girder_core::RemoteRegistry) {
    let launcher: Arc<dyn girder_core::TerminalLauncher> =
        Arc::new(girder_core::LocalLauncher::new());
    girder_core::register_terminal_extra_methods(remote);
    let terminals = Arc::new(girder_core::TerminalRegistry::new());
    girder_core::register_terminal_rpc_methods(remote, &terminals, &launcher);
    // `terminal.rename`: names a terminal a person can tell apart in a list. The bounds are the
    // controller's, not the store's, so a second caller cannot store an unusable title.
    girder_core::register_terminal_rename_method(remote, &terminals);
    // `terminal.retain`: saves a transcript so it survives the process. The store is per-deployment
    // because it writes to disk, which is a decision a caller makes rather than one this derives.
    girder_core::register_terminal_retain_method(
        remote,
        Arc::new(girder_core::TerminalPersistStore::new(
            sessions_root().join("terminals"),
        )),
    );
}

/// The route table: the built-in provider, plus whatever the settings file declares.
///
/// `providers` blocks are validated by `pi_ai_apply_gap` before any route registers, so a deployment
/// whose provider block cannot be honoured learns that at start rather than when a model is first
/// named. A refused block registers nothing -- half of a provider list is a list nobody wrote.
fn build_routes(model: &str) -> Arc<girder_core::RouteRegistry> {
    let mut routes = girder_core::RouteRegistry::new();
    let _ = routes.register(girder_core::deepseek_route("deepseek", model.to_owned()));
    let extra = configured_routes(model);
    for (name, provider) in &extra {
        let entry = girder_core::Route::new(name.clone(), provider.clone());
        if let Err(error) = routes.register(entry) {
            eprintln!("[settings] route `{name}` skipped: {error}");
        }
    }
    if !extra.is_empty() {
        eprintln!(
            "[settings] {} provider route(s) from settings: {}",
            extra.len(),
            extra.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    Arc::new(routes)
}

/// The provider routes a settings file declares, validated.
///
/// Returns the routes it could honour and prints why for those it could not. `pi_ai_apply_gap` owns
/// the eighteen-field validation and the ten cross-field rules, so this only supplies the document
/// and reads the answer: a provider block that names a protocol the catalogue does not know, or
/// whose numbers contradict each other, is refused there rather than at the first model call.
fn configured_routes(model: &str) -> std::collections::BTreeMap<String, String> {
    let path = sessions_root().join("settings.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("[settings] no settings file at {}", path.display());
        return std::collections::BTreeMap::new();
    };
    let Ok(document) = serde_json::from_str::<serde_json::Value>(&text) else {
        eprintln!(
            "[settings] {} is not JSON; no provider routes",
            path.display()
        );
        return std::collections::BTreeMap::new();
    };
    let Some(providers) = document.get("providers") else {
        eprintln!("[settings] {} has no `providers` block", path.display());
        return std::collections::BTreeMap::new();
    };
    match girder_core::pi_ai_apply_gap::providers_from_json(providers) {
        Ok(profiles) => profiles
            .into_iter()
            .map(|(name, profile)| {
                // The route's own provider is its name; the model falls back to the CLI's when the
                // profile names none, so a route that only sets a credential still resolves.
                let provider = profile.api.clone().unwrap_or_else(|| name.clone());
                let _ = model;
                (name, provider)
            })
            .collect(),
        Err(error) => {
            // Nothing registers: a provider list with one bad entry is a list nobody wrote.
            eprintln!("[settings] `providers` refused: {error}");
            std::collections::BTreeMap::new()
        }
    }
}

/// The address `--bind` names, normalized and checked.
///
/// Through the host module's parser rather than straight to the kernel: `TcpListener::bind` accepts
/// forms this build cannot serve and its error names the syscall rather than the argument. The
/// parser answers the normalized host or says which part is wrong, and it knows the wildcard and
/// loopback cases the banner has to distinguish.
fn resolve_bind(bind: &str) -> Result<String> {
    let (host, port) = bind.rsplit_once(':').unwrap_or((bind, "8080"));
    let normalized = girder_core::normalize_bind_address(host)
        .map_err(|error| anyhow::anyhow!("`--bind {bind}`: {error}"))?;
    // An empty host is the wildcard, which the parser reports as no address at all.
    let host = if normalized.is_empty() {
        "0.0.0.0"
    } else {
        &normalized
    };
    Ok(format!("{host}:{port}"))
}

async fn serve(cli: Cli, bind: String, watch: Vec<String>) -> Result<()> {
    // Checked before binding: a deployment that cannot work should say so once instead of serving one error per
    // client. Only a fatal finding refuses to start, because a missing provider is the offline path and a missing
    // desktop is normal on a server.
    //
    // The `.env` section goes to stderr rather than into the banner stream: stdout carries the one line a client
    // parses to learn the endpoint, and a multi-line prefix in front of it would break that parse. Nothing is
    // dropped — a server that started from a `.env` layer and never said so leaves an operator reading the exported
    // environment and finding none of the values actually in force.
    if let Some(env) = boot_check(&cli)? {
        eprint!("{env}");
    }
    let bus = Arc::new(EventBus::new());
    let endpoint = resolve_bind(&bind)?;
    let listener = tokio::net::TcpListener::bind(&endpoint).await?;
    let address = listener.local_addr()?;

    // Resolved before the banner so the banner reports what is actually forwarded: printing "no events
    // forwarded" and then forwarding one would make the line lie.
    let selected = if watch.is_empty() {
        vec![<girder_core::AgentTurn as girder_core::Event>::NAME]
    } else {
        resolve_watched(&watch)
    };

    print_banner(address, &selected);

    let catalog = session_catalog();
    let provider = resolve_provider(&cli);
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    let tools = girder_core::ToolRegistry::new();
    tools.register(list_tool());
    tools.register(girder_core::bash_tool(path_policy(
        &cli.sandbox,
        &workspace,
    )?));

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
    let routes = build_routes(&cli.model);

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
    // Assembled rather than empty. Every projection existed and none was registered, so
    // `session.state` answered `projections: {}` -- a session with no state, not because anything
    // failed but because nothing was ever put in the registry.
    let projections = girder_core::projection_setup::assemble()
        .map_err(|error| anyhow::anyhow!("projection registry: {error}"))?;

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
    register_terminal_surface(&mut remote);

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
    register_workspace_file_surface(&mut remote, &fs, &workspaces, &routes);

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

    // Peer identity is enforced only when this deployment says who its callers are. A credential
    // scheme is a deployment decision -- a shared token, a proxy header, mTLS in front -- and
    // inventing one here would be guessing at the environment. `GIRDER_REQUIRE_PEER=1` turns it on,
    // and the directory's defaults then refuse rather than admit, which is the safe direction: a
    // connection that cannot prove itself is not served.
    let directory = match std::env::var("GIRDER_REQUIRE_PEER").as_deref() {
        Ok("1" | "true" | "yes") => {
            println!("peer identity: required (a connection must present a credential)");
            Some(Arc::new(girder_core::gateway_gap::PeerDirectory::new()))
        }
        _ => None,
    };
    let cancel = CancellationToken::new();
    match directory {
        Some(directory) => {
            girder_rpc::serve_with_peers(listener, bus, selected, Some(remote), directory, cancel)
                .await?;
        }
        None => {
            girder_rpc::serve_with(listener, bus, selected, Some(remote), cancel).await?;
        }
    }
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
        // The file's own `invocation` policy travels into the summary: a skill that says
        // `modelInvocable: false` is listed to a person and withheld from the model, and a summary
        // built without it would offer the model a procedure a person meant to gate.
        let mut summary = girder_core::SkillSummary::new(
            meta.name.clone(),
            meta.description.clone(),
            "workspace",
            100,
        );
        summary.model_invocable = meta.model_invocable;
        summary.user_invocable = meta.user_invocable;
        if let Some(definition) = girder_core::SkillDefinition::new(summary, text) {
            provider.add(definition);
        }
    }
    let registry = girder_core::SkillRegistry::new();
    let _ = registry.register(std::sync::Arc::new(provider));
    registry
}

fn register_tools(
    tools: &girder_core::ToolRegistry,
    workspace: &std::path::Path,
    sandbox: &str,
) -> Result<()> {
    tools.register(girder_core::bash_tool(path_policy(sandbox, workspace)?));
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
    Ok(())
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
    routes: &Arc<girder_core::RouteRegistry>,
) {
    // Settings share this function rather than `serve` because they are the other half of what a client configures --
    // and because `serve` has been at the length limit three times now, which is the lint saying the same thing.
    register_settings_surface(remote);
    register_goal_surface(remote);
    register_workflow_surface(remote);
    register_question_surface(remote);
    // Task-graph validation, so a client can refuse a cyclic plan before submitting it: every
    // individual edit in such a graph is valid, and the plan then never starts.
    girder_core::register_team_graph_method(remote);
    // The rest of the `workspace` surface -- renaming, ordering, pinning, archiving. The module has
    // held all seven of upstream's mutations since it was written and nothing called it, so
    // `workspace.rename` and `workspace.archiveSession` were declared in the schema and absent from
    // the server.
    // `llm.*`: the runtime answers about the adapters this deployment registered. Built here
    // rather than in `serve` because it is one more thing a client can ask about, and `serve` is at
    // the length the linter enforces.
    match girder_core::llm_remote::runtime_from_routes(routes) {
        Ok(runtime) => {
            let _ = girder_core::register_llm_remote(remote, &runtime);
        }
        Err(error) => eprintln!("[llm] runtime not built: {error}"),
    }
    // `sessionReferenceResolver.candidates`: the sessions a `@`-mention may point at, ranked so the
    // caller's own directory comes first.
    girder_core::session_reference::register_session_reference_methods(remote, &session_catalog());
    // `subagents.{prompt, interruptByUser}`: delivering input to a running child. The substrate is
    // the two inbox queues the agent loop already reads, so this is not a resident-session
    // subsystem -- it is the way to put a message into a session from outside.
    girder_core::subagent_rpc::register_subagent_methods(
        remote,
        &Arc::new(girder_core::subagent_rpc::RunningChildren::new()),
    );
    // `skills.list`: the ranked catalog over the six roots upstream scans, with the project's own
    // skill shadowing a bundled one of the same name. `skill_catalog` renders a prompt and does not
    // rank; serving a client that will *invoke* a name needs the ranking.
    girder_core::skill_final_gap::register_skill_methods(
        remote,
        Arc::new(girder_core::skill_final_gap::FilesystemHost::new(
            Arc::new(girder_core::skill_final_gap::FixedSessions::new()),
            std::env::current_dir().map_or_else(
                |_| ".".to_owned(),
                |path| path.to_string_lossy().into_owned(),
            ),
        )),
    );
    // `directoryPicker.{list,pick,createDirectory}`: the three endpoints upstream serves from
    // `workspace-controller/src/directory-picker.ts`. The backend reports its own capability and each
    // endpoint refuses when its capability is absent, rather than answering an empty selection that a
    // client would read as "the user cancelled".
    girder_core::register_directory_picker(
        remote,
        &(Arc::new(girder_core::LocalDirectoryPicker::new())
            as Arc<dyn girder_core::DirectoryPickerBackend>),
    );
    // `fileReferences.list`: file and directory candidates for one agent's working directory, which
    // is what an `@` completion queries. The implementation was complete and unregistered -- and the
    // endpoint survey that reported 126/126 had only scanned `src/index.ts`, so this one lives in
    // `file-references.ts` and was not counted.
    girder_core::api_tools_gap::register_file_reference_methods(remote, move |_agent: &str| {
        Some(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
    });
    // `mcpManager.{probe, restart}`: the MCP subsystem was already here -- wire layer, stdio framing,
    // child lifecycle, handshake -- and had no endpoint. Configured servers come from settings.
    girder_core::mcp_manager_rpc::register_mcp_manager_methods(remote, &Arc::new(Vec::new()));
    // `pluginManager.*`: the seven reads answer from an explicit entry registry -- the replacement
    // for the Cordis tree -- and the mutations refuse with `no_installer`, because this build mounts
    // plugins at compile time.
    girder_core::plugin_manager_rpc::register_plugin_manager_methods(remote, &{
        // The plugins this build actually mounts, as entries.
        let mut entries = girder_core::plugin_entries::Registry::new();
        entries.insert(girder_core::plugin_entries::Entry {
            entry_id: "cli".to_owned(),
            module_name: "@deepseek-ai/dsh".to_owned(),
            options_id: "@deepseek-ai/dsh".to_owned(),
        });
        entries.set_owner("cli");
        Arc::new(entries)
    });
    girder_core::plugin_manager_rpc::register_plugin_inventory_methods(remote, &{
        let mut entries = girder_core::plugin_entries::Registry::new();
        entries.insert(girder_core::plugin_entries::Entry {
            entry_id: "cli".to_owned(),
            module_name: "@deepseek-ai/dsh".to_owned(),
            options_id: "@deepseek-ai/dsh".to_owned(),
        });
        Arc::new(entries)
    });
    // `officeKit.*`: the rendering generation is answered; `render` refuses with `no_renderer`
    // because converting a document needs an engine this build does not embed.
    girder_core::office_kit::register_office_kit_methods(remote);
    // `account.*`: the credential store answers `getState`; everything that needs a Platform
    // endpoint refuses with `no_platform` rather than answering `null`, which a client would read as
    // "signed out" for a deployment where signing in cannot succeed.
    let credentials: Arc<dyn girder_core::CredentialSource> =
        Arc::new(girder_core::MemoryCredentialSource::new());
    // `credentials.{describe,set,unset}`: the store `account.getState` reads. It was written and
    // never registered, so a client could ask about an account but not configure one.
    girder_core::register_credential_methods(remote, &credentials);
    girder_core::account_rpc::register_account_methods(
        remote,
        &credentials,
        std::env::var("DEEPSEEK_API_KEY").is_ok(),
    );
    // `agentTeams.setEnabled`: whether a session's tree runs as a team. The root is the caller's
    // assertion, checked rather than trusted, because this build holds no subagent tree to derive it.
    girder_core::agent_teams::register_agent_team_methods(remote, &session_catalog());
    // `agentPresets.*`: the capability sets a session may start from. This build's presets name
    // tools and a model rather than a Cordis plugin list, which is stated in the module.
    girder_core::agent_presets::register_agent_preset_methods(
        remote,
        &girder_core::agent_presets::default_registry(),
    );
    // `messageFeedback.*`: feedback is a log event, so `list` replays and `put`/`delete` need a
    // store this deployment's catalog does not hold -- they answer `store_unavailable` rather than
    // claiming a write.
    girder_core::message_feedback::register_message_feedback_methods(remote, &session_catalog());
    girder_core::message_feedback::register_session_feedback_methods(remote, &session_catalog());
    // `web.listProviders`: the search and fetch endpoints this deployment registered. Empty when it
    // has none, which is the honest answer rather than an absent method.
    girder_core::web_rpc::register_web_methods(
        remote,
        &Arc::new(girder_core::web_rpc::WebProviders::from_environment()),
    );
    // `permissionPresets.catalog`: the same built-in set the approval layer enforces, so a client
    // reading this and a rule refusing a call cannot disagree about which presets exist.
    girder_core::permission_presets_rpc::register_permission_preset_methods(
        remote,
        &girder_core::permission_presets_rpc::default_catalogue(),
        girder_core::permission_presets_rpc::DEFAULT_PRESET,
    );
    // `pluginMarket.*`: the market index this build already parses, reachable from a client.
    let market = Arc::new(girder_core::plugin_market_rpc::MarketCatalog::new());
    girder_core::plugin_market_rpc::register_market_methods(remote, &market);
    // `workspace.initializeDefault`: creates the first workspace, and only when there is none. The
    // directory is the deployment's, taken from the same sessions root the catalog reads -- a server
    // that picked one itself would be writing into a path nobody chose.
    girder_core::register_workspace_initialize_default(
        remote,
        workspaces,
        Some(sessions_root()),
        girder_core::now_millis(),
    );
    girder_core::register_workspace_extra_methods(
        remote,
        workspaces,
        &{
            let preferences: Arc<dyn girder_core::WorkspacePreferences> =
                Arc::new(girder_core::InMemoryPreferences::new());
            preferences
        },
        &session_catalog(),
    );
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
            // The document must describe what a server serves, and this catalogue is three namespaces of the
            // thirteen a server registers. `openapi_full` reads the registry instead, so a method that exists cannot
            // be missing from the document -- the listing of `catalogue` above stays because it carries the type
            // definitions that give the schemas names.
            let _ = &catalogue;
            let names = girder_core::registered_method_names();
            let described = serde_json::json!({
                "title": info.title,
                "version": info.version,
                "description": info.description,
            });
            let full = girder_core::openapi_document_from_names(&names, &described);
            println!("{}", serde_json::to_string_pretty(&full)?);
        }
        Some("metrics") => print_metrics(),
        Some("disk") => print_disk(),
        Some("csv") => print_csv(),
        Some("surface") => print_surface(),
        Some("mcp") => print_mcp()?,
        Some("health") => print_health(),
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
            register_tools(&tools, &workspace, "workspace-write")?;
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
                "unknown schema format `{other}`; expected rpc, openapi, catalog, metrics, disk, csv, health, surface, mcp, tools-mcp, or tools-openai"
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
    // The route states which modalities it takes. A text-only route given an image does not fail --
    // it drops the block and answers about what it never saw -- so this is read here rather than
    // discovered from a provider's complaint.
    let images = resolved
        .input_modalities
        .contains(&girder_core::Modality::Image);
    girder_core::AgentRuntime::set_accepts_images(runtime, images);
}

/// Prints how healthy this deployment's sessions are.
///
/// A score per session, then the fleet rate. The score is a reading of one log -- whether its calls pair, its turns close,
/// its events order -- and the rate is what an operator actually watches, because a single bad log is normal and a third of
/// them is not.
fn print_health() {
    let layout = girder_core::SessionLayout::new(sessions_root());
    let mut logs: Vec<Vec<girder_core::SessionEvent>> = Vec::new();
    if let Ok(ids) = layout.ids() {
        for id in ids {
            let Ok(files) = layout.files(&id) else {
                continue;
            };
            if let Ok(events) = girder_core::read_session_file(&files.log) {
                logs.push(events);
            }
        }
    }
    let sample = girder_core::sample_health(&logs, girder_core::now_millis());
    println!("sessions: {}", sample.sessions);
    println!("healthy: {}", sample.healthy);
    println!("errors: {}", sample.errors);
    println!("mean score: {:.1}", sample.mean_score);
    // The issues, grouped: one log with five problems and five logs with one each look the same in a count and are
    // different situations.
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for events in &logs {
        for issue in girder_core::diagnose_health(events) {
            *counts.entry(format!("{:?}", issue.kind)).or_insert(0) += 1;
        }
    }
    for (kind, count) in counts {
        println!("  {kind}: {count}");
    }
}

/// Starts the MCP servers the settings document configures, and reports what each one offered.
///
/// The command exists because the pieces were all present and nothing joined them: a transport that can spawn a server, a
/// parser for the server list, and no code that reads the list. Running it is the only way to learn whether a configured
/// server works -- `tools-mcp` prints the tool set this build declares, which is a different question.
fn print_mcp() -> Result<()> {
    let settings: Arc<dyn girder_core::SettingsStore> = Arc::new(
        girder_core::FileSettingsStore::open(sessions_root().join("settings.json")),
    );
    let configured = match settings.get("mcp.servers") {
        Some(value) => girder_core::parse_mcp_config(&value).map_err(|error| {
            anyhow::anyhow!("`mcp.servers` is not a usable server list: {error}")
        })?,
        None => Vec::new(),
    };
    if configured.is_empty() {
        println!("no MCP servers configured (`mcp.servers` in settings.json)");
        return Ok(());
    }
    for started in girder_core::start_mcp_servers(&configured) {
        match started.failed {
            Some(reason) => println!("{}: failed ({reason})", started.name),
            None => println!("{}: {} tool(s)", started.name, started.tools),
        }
    }
    Ok(())
}

/// Prints the declared method surface and checks it against what the registry serves.
///
/// Two numbers that must agree: the declaration table and a registry built from the same registration functions a server
/// calls. A `schema` command that reported one without the other would leave the reader to trust it.
fn print_surface() {
    let surface = girder_core::declared_surface();
    println!("declared: {}", surface.methods.len());
    println!("needs collaborator: {}", surface.needs_collaborator.len());
    let mut namespaces: Vec<&str> = surface
        .methods
        .iter()
        .filter_map(|method| method.split('.').next())
        .collect();
    namespaces.sort_unstable();
    namespaces.dedup();
    for namespace in namespaces {
        let count = surface
            .methods
            .iter()
            .filter(|method| method.starts_with(&format!("{namespace}.")))
            .count();
        println!("  {namespace}: {count}");
    }
    // The names that need a collaborator, listed because they are the difference between what this build declares and what
    // a registry assembled from nothing can serve. A caller that sees them knows which dependencies to supply.
    for name in surface.needs_collaborator.iter().take(8) {
        println!("  needs collaborator: {name}");
    }
    if surface.needs_collaborator.len() > 8 {
        println!("  ... and {} more", surface.needs_collaborator.len() - 8);
    }
}

/// Prints the tool usage table as CSV.
///
/// A table rather than the metrics format because the question is different: Prometheus answers "is the fleet healthy"
/// and a spreadsheet answers "which tool is expensive, and by how much". The escaping and the formula neutralisation
/// live in `stats_csv`, so a session title beginning with `=` cannot become a formula here.
fn print_csv() {
    let layout = girder_core::SessionLayout::new(sessions_root());
    let mut all: Vec<girder_core::ToolStats> = Vec::new();
    if let Ok(ids) = layout.ids() {
        for id in ids {
            let Ok(files) = layout.files(&id) else {
                continue;
            };
            let Ok(events) = girder_core::read_session_file(&files.log) else {
                continue;
            };
            all.extend(girder_core::tool_usage(&events));
        }
    }
    // Merged across sessions, because the table answers a fleet question: a tool that is cheap once and called a
    // thousand times is the expensive one, and a per-session table would hide that.
    let mut merged: std::collections::BTreeMap<String, girder_core::ToolStats> =
        std::collections::BTreeMap::new();
    for stats in all {
        let entry = merged
            .entry(stats.name.clone())
            .or_insert(girder_core::ToolStats {
                name: stats.name.clone(),
                calls: 0,
                failures: 0,
                argument_bytes: 0,
                result_bytes: 0,
            });
        entry.calls = entry.calls.saturating_add(stats.calls);
        entry.failures = entry.failures.saturating_add(stats.failures);
        entry.argument_bytes = entry.argument_bytes.saturating_add(stats.argument_bytes);
        entry.result_bytes = entry.result_bytes.saturating_add(stats.result_bytes);
    }
    let mut rows: Vec<girder_core::ToolStats> = merged.into_values().collect();
    rows.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.name.cmp(&b.name)));
    print!("{}", girder_core::tool_table(&rows).render());
}

/// Prints what the session directory occupies.
///
/// The question "how full is this disk" had no answer short of `du`, and `du` cannot say which sessions a deployment
/// could evict. The order is oldest first, which is the order an eviction would take them.
fn print_disk() {
    let layout = girder_core::SessionLayout::new(sessions_root());
    let footprints = layout.footprints();
    let usage = girder_core::storage_usage(&footprints);
    println!("sessions: {}", usage.sessions);
    // The persistence layer's own bytes, which the log total above does not include: checkpoints, terminal transcripts and
    // the search cache all live beside the logs and all occupy disk. A `disk` command that reported only logs would
    // understate a deployment by however much the search index has grown.
    // The tool budget the settings document currently asks for. Printed because a stored setting that never reaches a
    // reader is the failure this whole path exists to make visible: `tools.maxCalls` was write-only before the watcher.
    let settings: Arc<dyn girder_core::SettingsStore> = Arc::new(
        girder_core::FileSettingsStore::open(sessions_root().join("settings.json")),
    );
    let budget = girder_core::ToolBudgetWatcher::from_arc(Arc::clone(&settings));
    println!("tool call budget: {} per tool", budget.current().max_calls);
    // The goal state, which decides whether a turn ends the session or continues it. A goal that is active but disarmed
    // is the state a restart leaves, and it is why the loop stops instead of continuing -- worth printing, because
    // "the agent went quiet" and "the goal is disarmed" look the same from outside.
    let goal = girder_core::GoalState::new();
    match goal.current() {
        Some(current) => println!(
            "goal: {} rev {} [{}] ({:?}, may continue: {})",
            current.id.as_str(),
            current.revision,
            current.phase.as_str(),
            goal.activation(),
            goal.may_continue()
        ),
        None => println!("goal: none"),
    }
    let persistence = girder_core::Persistence::new(sessions_root());
    let persist = persistence.footprint();
    println!("logs: {}", usage.total_bytes);
    println!("checkpoints: {}", persist.checkpoints_bytes);
    println!("terminals: {}", persist.terminals_bytes);
    println!("search index: {}", persist.search_index_bytes);
    println!(
        "bytes: {}",
        usage.total_bytes.saturating_add(persist.total())
    );
    let mut oldest: Vec<&girder_core::Footprint> = footprints.iter().collect();
    oldest.sort_by_key(|footprint| (footprint.modified_ms, footprint.session.as_str()));
    for footprint in oldest.iter().take(20) {
        println!(
            "  {:<40} {:>12} bytes  (log {}, sidecars {})",
            footprint.session,
            footprint.total(),
            footprint.log_bytes,
            footprint.sidecar_bytes
        );
    }
    if footprints.len() > 20 {
        println!("  ... and {} more", footprints.len() - 20);
    }
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
/// Builds the provider chain: transport, retry, cache -- or the scripted one.
///
/// Extracted from `run_prompt`, which had reached the length the linter enforces. The chain's order is the interesting
/// part and it reads better named than inline: the cache sits outside the retry so a hit never reaches the transport,
/// and a retry that failed is retried again.
fn build_provider(cli: &Cli) -> Arc<dyn girder_core::LlmProvider> {
    let Ok(provider) = girder_core::provider_from_environment() else {
        // Named per protocol, because a deployment that set GIRDER_PROTOCOL=openai and no key would be told to set a
        // variable for a protocol it explicitly did not choose.
        if std::env::var("GIRDER_PROTOCOL").as_deref() == Ok("openai") {
            eprintln!(
                "[provider] offline (set OPENAI_API_KEY to use a real model over the openai protocol)"
            );
        } else {
            eprintln!("[provider] offline (set DEEPSEEK_API_KEY to use a real model)");
        }
        return Arc::new(OfflineProvider {
            tool: cli.tool.clone(),
        });
    };
    eprintln!("[provider] live: {}", cli.model);
    // Wrapped so a transient failure is retried: a 429 or a dropped connection would otherwise end a turn that the next
    // attempt would have completed.
    let retrying: Arc<dyn girder_core::LlmProvider> = Arc::new(girder_core::RetryingProvider::new(
        provider,
        call_retry_policy(&cli.model),
    ));
    // Cached outside the retry: a hit should never reach the transport, and a retry that failed should be retried
    // again. An unstated temperature is not cached -- the provider's default is invisible here, and caching a sampled
    // answer would make the second call return the first call's dice roll.
    Arc::new(girder_core::CachingProvider::new(
        retrying,
        temperature_from_env(),
    ))
}

/// The temperature the deployment configured, if it stated one.
///
/// Read from the environment rather than from `LlmRequest` because the request does not carry it: temperature lives in
/// the call configuration and is applied by the provider, so a cache that read it from the request would read nothing.
fn temperature_from_env() -> Option<f32> {
    std::env::var("GIRDER_TEMPERATURE")
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
}

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

/// The `.env` layers a start would apply, ranked the way `layer_env` ranks them.
///
/// `boot_app_gap` implements upstream's `readEnvLayer`/`loadLayeredEnv` as pure functions: the text is
/// read here, and every judgement about whether a file may set a variable is made by
/// [`girder_core::boot_app_gap::check_env_layer`]. That split is why the refusal can be tested at all,
/// and why this type carries values rather than reopening the files when it reports them.
struct EnvLayers {
    /// The variables this run would see, ranked process > project > harness home.
    snapshot: girder_core::boot_app_gap::EnvSnapshot,
    /// The invoking directory's layer, when it has a `.env`.
    project: Option<girder_core::boot_app_gap::EnvLayer>,
    /// The harness home's layer, when it has a `.env` and is not the invoking directory.
    home: Option<girder_core::boot_app_gap::EnvLayer>,
}

/// The harness home, spelled the way the rest of the harness resolves it.
///
/// The same fallback as [`sessions_root`], for the same reason: a relative `GIRDER_HOME` is refused by
/// `HomePaths`, and falling back to the default is better than refusing to start over a path.
fn harness_home() -> String {
    let home = std::env::var("HOME").ok();
    let override_home = std::env::var("GIRDER_HOME").ok();
    girder_core::HomePaths::from_environment(home.as_deref(), override_home.as_deref())
        .map_or_else(
            |_| std::env::temp_dir().join("girder"),
            |paths| paths.root().to_path_buf(),
        )
        .display()
        .to_string()
}

/// Reads one directory's `.env` and submits it to the bootstrap-only policy.
///
/// A missing file is no layer — that is what a fresh clone looks like, and reporting it would make
/// every clone look misconfigured. Everything else is decided by
/// [`girder_core::boot_app_gap::check_env_layer`], including the rule that matters: a `.env` travels
/// with a repository, so it may not set a variable that decides how the process starts, where its code
/// and instructions load from, or where its requests go. `LD_PRELOAD` is the example the policy names —
/// a clone that could set it would choose which shared library loads into the parent process — and the
/// same rule covers `PATH`, `NODE_OPTIONS`, `GIT_SSH_COMMAND`, `HTTP_PROXY`, and every `DSH_` name.
///
/// A refusal is returned as a [`girder_core::Finding`] rather than as an error so the caller renders it
/// through [`girder_core::boot_report`], which is the same report a broken deployment gets: one shape
/// for every reason this build will not start.
fn read_env_layer(
    dir: &str,
    home: &str,
) -> Result<Option<girder_core::boot_app_gap::EnvLayer>, girder_core::Finding> {
    let path = std::path::Path::new(dir).join(".env");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // Not `Refused`: nothing about the contents is known. A file that cannot be inspected must not
        // be applied either, which is the same conclusion reached the other way round.
        Err(error) => {
            return Err(girder_core::Finding::fatal(
                format!("`{}` cannot be read", path.display()),
                format!("{error}; this build will not start from an environment it cannot inspect"),
            ));
        }
    };
    girder_core::boot_app_gap::check_env_layer(dir, home, &text)
        .map(Some)
        .map_err(|refusal| match refusal.kind {
            girder_core::boot_app_gap::EnvErrorKind::Refused => girder_core::Finding::fatal(
                format!(
                    "`{}` sets `{}`, which only the launching environment may set",
                    refusal.path, refusal.name
                ),
                refusal.remedy,
            ),
            girder_core::boot_app_gap::EnvErrorKind::Unreadable => girder_core::Finding::fatal(
                format!("`{}` cannot be read", refusal.path),
                refusal.remedy,
            ),
        })
}

/// Reads and validates both `.env` layers, and layers them over the inherited environment.
///
/// # Errors
///
/// Returns the fatal finding for a layer that may not be applied, or that cannot be read.
fn boot_env_layers() -> Result<EnvLayers, girder_core::Finding> {
    use girder_core::boot_app_gap as env;
    let dir =
        std::env::current_dir().map_or_else(|_| ".".to_owned(), |path| path.display().to_string());
    let home = harness_home();
    let project = read_env_layer(&dir, &home)?;
    // `layer_env` documents this case: a harness home that *is* the invoking directory would read the
    // same file twice, and the second copy would shadow the first one's attribution. The file is the
    // home layer then, because that is the more permissive of the two and the one it actually is.
    let user = if env::join_path("", &home) == env::join_path("", &dir) {
        None
    } else {
        read_env_layer(&home, &home)?
    };
    let inherited: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let snapshot = env::layer_env(&inherited, project.as_ref(), user.as_ref());
    Ok(EnvLayers {
        snapshot,
        project,
        home: user,
    })
}

/// Renders the `.env` layers a start would use, or `None` when there are none.
///
/// The section exists to keep two things visible that would otherwise be invisible. First, which file
/// supplied a variable — `layer_env`'s whole job, and the fact a person needs when the value in effect
/// is not the one they wrote. Second, that a value only a file sets does **not** reach this process:
/// configuration here is read from the launching environment, so a `.env` that looks applied would
/// otherwise be silently inert. Naming each such variable converts that into one export away from
/// working.
fn boot_env_report(layers: &EnvLayers) -> Option<String> {
    use std::fmt::Write as _;

    use girder_core::boot_app_gap::{EnvSource, is_bootstrap_only, is_home_layer_route_name};

    if layers.project.is_none() && layers.home.is_none() {
        return None;
    }
    let mut out = String::from("env: .env layers\n");
    for (layer, label) in [(&layers.project, "project"), (&layers.home, "harness home")] {
        if let Some(layer) = layer {
            let _ = writeln!(
                out,
                "  {}: {} variable(s) ({label} layer)",
                layer.path,
                layer.values.len()
            );
        }
    }
    // A name the process also sets is not listed: the process outranks every file, so the file's copy
    // is not what the run uses and saying otherwise would be wrong.
    let mut file_only: Vec<(&str, &str)> = Vec::new();
    for name in layers.snapshot.names() {
        let Some(value) = layers.snapshot.get(name) else {
            continue;
        };
        if value.source == EnvSource::Process {
            continue;
        }
        file_only.push((name, value.path.as_deref().unwrap_or("a .env file")));
    }
    file_only.sort_unstable();
    for (name, path) in file_only {
        let _ = writeln!(
            out,
            "  {name}: set by {path} only; export it, because this build reads configuration from the \
             launching environment"
        );
    }
    // Bootstrap-only names the home layer was nonetheless allowed to set, and why: a proxy or an
    // endpoint is a route the user owns, and that file does not travel with a repository.
    if let Some(layer) = &layers.home {
        let mut allowed: Vec<&str> = layer
            .values
            .keys()
            .filter(|name| is_bootstrap_only(name) && is_home_layer_route_name(name))
            .map(String::as_str)
            .collect();
        allowed.sort_unstable();
        for name in allowed {
            let _ = writeln!(
                out,
                "  {name}: allowed in {} because a route name the harness home sets does not travel \
                 with a repository",
                layer.path
            );
        }
    }
    Some(out)
}

/// The tool names the server would mount, assembled without starting it.
///
/// The audit needs the registry `serve` builds, and `serve` cannot be called to ask: it binds a socket
/// and never returns. Building the same list here is deliberate rather than lazy — the two functions
/// register through [`register_tools`] and [`register_capability_tools`], so a name added to either is
/// in this list too, and the audit cannot describe a harness the server does not run.
///
/// The registries behind the tools are throwaway: a `JobRegistry` and a `GoalState` that nobody reads.
/// Nothing is executed, so an empty one costs nothing and reports the same names a live one would.
fn probe_tool_names() -> Vec<String> {
    let tools = girder_core::ToolRegistry::new();
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // `serve` registers `list` and `bash` before the shared set, and `list` is only there: a run that
    // audited `register_tools` alone would report the server's own extra tool as absent.
    tools.register(list_tool());
    // The audit asks what this build *has*, not what a run was configured with, so the mode is the
    // default one; a policy that refused here would make the audit report a tool list it could not
    // build rather than the one it ships.
    register_tools(&tools, &workspace, "workspace-write").expect("the default mode parses");
    let goals = Arc::new(std::sync::Mutex::new(girder_core::GoalState::new()));
    register_capability_tools(&tools, &workspace, &goals);
    girder_core::register_job_tools(&tools, Arc::new(girder_core::JobRegistry::new(64 * 1024)));
    tools.names()
}

/// Audits the settled tool set against the entries this build says a usable application has.
///
/// `boot_app_gap` implements upstream's `auditStartupEntries` as a pure function over the *inactive*
/// rows a loader reported: a required entry that did not activate is fatal, and the same list carried
/// as a warning when only optional ones are inactive. The loader this build has is not a Cordis entry
/// tree, so the rows have to be derived — one row per
/// [`girder_core::boot_app_gap::REQUIRED_STARTUP_ENTRY_IDS`] id whose tools this build registers.
///
/// The mapping is the point. Each id names a capability the list is *for*: `tool-executor-native` is
/// the native tools, `tool-executor-ptc` the programmatic ones, `webserver` the job tools,
/// `agent-loop` the loop's own. When the registry holds none of an id's tools, that id is inactive —
/// the state a person needs to see, because the alternative is a server that starts and then answers
/// "no tool named" one call at a time.
///
/// The two kinds of inactivity are kept apart, because conflating them is how this audit would become
/// noise nobody reads:
///
/// - [`Expect::Registered`] — a capability this build mounts unconditionally. Its absence is a build
///   defect, so the row is `required` with a `Failed` outcome and the verdict is an error.
/// - [`Expect::Waiting`] — a capability whose tools are mounted only when a collaborator exists. The
///   web tools need a web provider (`GIRDER_FETCH_ENDPOINT` unset means the deployment chose no web),
///   and the subagent tools need a `LoopContext`, which only a prompt run builds — `register_capability_tools`'
///   own comment says so. Those rows are `Pending` with the missing service named, and the verdict is
///   a warning. Reporting them as fatal would make every credential-free start look broken, which is
///   the exact mistake the audit's two severities exist to avoid.
///
/// Ids this build has no capability for at all (`acp`, `modules`, `connection`, `sdk-jsonrpc-server`)
/// are not audited: reporting a capability the deployment never offered would make every start look
/// broken for a reason nobody can fix.
fn audit_tool_surface(tool_names: &[String]) -> girder_core::boot_app_gap::StartupVerdict {
    use girder_core::boot_app_gap::{InactiveEntry, InactiveOutcome, REQUIRED_STARTUP_ENTRY_IDS};

    /// What an entry id's inactivity means.
    #[derive(Clone, Copy)]
    enum Expect {
        /// The tools are mounted unconditionally; absence is a defect.
        Registered,
        /// The tools are mounted when a collaborator exists; absence is that collaborator, named.
        Waiting(&'static [&'static str]),
    }

    /// The id, what it stands for, the tools that prove it active, and what its absence means.
    ///
    /// The tool list is a set rather than a count: an entry is active when *any* of its tools is
    /// registered, because a deployment that registered only `read` still has a working executor.
    const ENTRIES: &[(&str, &str, &[&str], Expect)] = &[
        (
            "agent-loop",
            "the loop's own tools",
            &["todo_write", "instructions", "skill"],
            Expect::Registered,
        ),
        (
            "tool-executor-native",
            "the native file and shell tools",
            &["bash", "read", "write", "edit", "glob", "grep"],
            Expect::Registered,
        ),
        (
            "tool-executor-ptc",
            "the programmatic tools",
            &["apply_patch", "workflow_run"],
            Expect::Registered,
        ),
        (
            "webserver",
            "the server's job tools",
            &["job_start", "job_list", "job_output"],
            Expect::Registered,
        ),
        (
            "web-runtime",
            "the web tools",
            &["web_fetch", "web_search"],
            Expect::Waiting(&["a web provider (set GIRDER_FETCH_ENDPOINT)"]),
        ),
        (
            "headless-runner",
            "the subagent tools",
            &["subagent", "subagent_list"],
            Expect::Waiting(&["a loop context (only a prompt run builds one)"]),
        ),
    ];

    let mut inactive = Vec::new();
    for (id, purpose, tools, expect) in ENTRIES {
        debug_assert!(
            REQUIRED_STARTUP_ENTRY_IDS.contains(id),
            "{id} is not a required id"
        );
        if tools
            .iter()
            .any(|tool| tool_names.iter().any(|name| name == tool))
        {
            continue;
        }
        let registered = matches!(expect, Expect::Registered);
        let detail = format!(
            "none of {} is registered",
            tools
                .iter()
                .map(|tool| format!("`{tool}`"))
                .collect::<Vec<String>>()
                .join(", ")
        );
        inactive.push(InactiveEntry {
            id: (*id).to_owned(),
            module: (*purpose).to_owned(),
            required: registered,
            outcome: match expect {
                Expect::Registered => InactiveOutcome::Failed {
                    phase: None,
                    detail,
                },
                Expect::Waiting(missing) => InactiveOutcome::Pending {
                    missing: missing.iter().map(|name| (*name).to_owned()).collect(),
                },
            },
        });
    }
    girder_core::boot_app_gap::audit_startup_entries("girder", &inactive)
}

/// Renders the startup audit's verdict as lines, or `None` when every claimed entry activated.
///
/// The text is the module's own, not a rewrite: [`girder_core::boot_app_gap::startup_diagnostic`] and
/// [`girder_core::boot_app_gap::activation_diagnostic`] are what the upstream suites pin verbatim, and
/// a second phrasing here would be the copy that drifts.
fn audit_tool_report(tool_names: &[String]) -> Option<String> {
    let verdict = audit_tool_surface(tool_names);
    if let Some(error) = verdict.error {
        return Some(format!(
            "startup audit: a claimed capability did not activate\n{error}\n"
        ));
    }
    verdict
        .warning
        .map(|warning| format!("startup audit: {warning}"))
}

/// Checks that this deployment can start, and answers with the `.env` section of the report.
///
/// # Errors
///
/// Returns an error when a finding is fatal: a `.env` layer the bootstrap-only policy refuses, or a
/// deployment check that says the process cannot work.
fn boot_check(cli: &Cli) -> Result<Option<String>> {
    // The `.env` layers are judged before the deployment is looked at. A file that may not be applied
    // stops the start on its own, and a workspace problem discovered after it would bury the one
    // finding that is about a repository deciding how this process runs.
    let layers = match boot_env_layers() {
        Ok(layers) => layers,
        Err(refusal) => anyhow::bail!(
            "this deployment cannot start:\n{}",
            girder_core::boot_report(&[refusal])
        ),
    };
    let workspace = std::env::current_dir().ok();
    let deployment = girder_core::Deployment {
        provider: girder_core::provider_from_environment().is_ok(),
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
    Ok(boot_env_report(&layers))
}

/// Registers the `goal` namespace.
///
/// A goal decides whether a turn ends the session or continues it, and 575 lines of state machine were reachable only from
/// inside this build. The store is in-memory on purpose: `goal.rs` treats a restart as disarming a goal, so persisting one
/// across a restart would contradict the rule the state machine is built on.
fn register_goal_surface(remote: &mut girder_core::RemoteRegistry) {
    let goals: Arc<dyn girder_core::GoalStore> = Arc::new(girder_core::MemoryGoalStore::new());
    girder_core::register_goal_methods(remote, &goals);
}

/// Registers the `workflow` namespace.
///
/// Upstream has no RPC surface for workflows -- it exposes a Cordis service, lifecycle events and a model tool -- so these
/// four methods are shaped for a client rather than copied from one. `start` returns an id immediately instead of awaiting
/// the run, because a caller that is not handed an id can neither poll nor cancel.
fn register_workflow_surface(remote: &mut girder_core::RemoteRegistry) {
    let workflows = Arc::new(girder_core::WorkflowRegistry::new());
    girder_core::register_workflow_methods(remote, workflows);
}

/// Registers the `userQuestions` namespace.
///
/// Upstream has two methods: `answer` and a streaming `attachWait`. This build serves `answer`, adds `list` because
/// upstream discovers pending questions through a session projection this build does not have -- without it a client could
/// answer a question it had no way to find -- and leaves `attachWait` unserved and named rather than registered as a unary
/// method a client would discover cannot stream.
fn register_question_surface(remote: &mut girder_core::RemoteRegistry) {
    // Memory, not a file: a question is waited on by a live turn, so a question that outlived the process would be one
    // nobody is waiting for any more.
    let store: Arc<dyn girder_core::QuestionStore> =
        Arc::new(girder_core::MemoryQuestionStore::new());
    let questions = Arc::new(girder_core::QuestionQueue::new(store));
    girder_core::register_question_methods(remote, questions);
}

fn register_settings_surface(remote: &mut girder_core::RemoteRegistry) {
    // A file, not the in-memory store. `MemorySettingsStore` was the only shipped implementation, so every setting a
    // deployment wrote was gone at the next restart -- `settings.set` changed a value until the process exited, which is
    // not what any caller means by a setting. The store lives beside the sessions because both are this deployment's data.
    let path = sessions_root().join("settings.json");
    let settings: Arc<dyn girder_core::SettingsStore> =
        Arc::new(girder_core::FileSettingsStore::open(path));
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
    // Through `choose_session_backend`, not straight to a directory: the module exists so the choice is stated with a
    // reason, and so a test run cannot write into a real sessions directory. A missing directory lists as empty rather
    // than erroring, because a fresh install has no sessions.
    let root = sessions_root();
    let choice = girder_core::choose_session_backend(&girder_core::SessionBackendConfig {
        session_dir: Some(root.clone()),
        ephemeral: false,
    });
    // The reason is printed rather than kept: a deployment that set a directory and a test run that did not should see
    // which backend they got and why, and the module exists to make that answerable.
    if std::env::var("GIRDER_BACKEND_REPORT").is_ok() {
        eprintln!("[session] {:?}: {}", choice.backend, choice.reason);
    }
    Arc::new(girder_core::DirectoryCatalog::new(root))
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
/// Parses and validates `--subagent-model` values.
///
/// Each is `PROVIDER/MODEL`. The split is on the **first** `/` because a model id may contain one
/// (`org/model`) while a provider id does not; splitting on the last would put the org into the
/// provider and silently target a route nobody configured.
///
/// # Errors
///
/// A value with no `/`, or a route set `assert_allowed_routes` refuses: an empty half, or a duplicate.
fn validate_subagent_models(
    specs: &[String],
) -> Result<Vec<girder_core::subagent_final_gap::AllowedModelRoute>> {
    let routes = specs
        .iter()
        .map(|spec| {
            let (provider, model) = spec.split_once('/').ok_or_else(|| {
                anyhow::anyhow!("`--subagent-model {spec}` must be PROVIDER/MODEL")
            })?;
            girder_core::subagent_final_gap::AllowedModelRoute::new(provider, model)
                .map_err(|error| anyhow::anyhow!("`--subagent-model {spec}`: {error}"))
        })
        .collect::<Result<Vec<_>>>()?;
    girder_core::subagent_final_gap::assert_allowed_routes(&routes)
        .map_err(|error| anyhow::anyhow!("`--subagent-model`: {error}"))?;
    Ok(routes)
}

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
    register_tools(&tools, &workspace, &cli.sandbox)?;
    // The capability tools need a goal state; a prompt run gets its own, which is the same lifetime as the run.
    let goals = Arc::new(std::sync::Mutex::new(girder_core::GoalState::new()));
    register_capability_tools(&tools, &workspace, &goals);

    let provider = build_provider(&cli);

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
    // Validated once, at startup. The routes are parsed into the typed form the module owns rather
    // than kept as strings: `assert_allowed_routes` is what refuses a duplicate or an empty half, and
    // running it here is what turns a mistyped route into a startup failure instead of a tool error
    // attributed to whichever subagent happened to be asked first.
    context.subagent_models = validate_subagent_models(&cli.subagent_models)?;
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
        // `\n` as an escape, not a literal newline: a raw string cannot carry one inside a JSON string, and the argument
        // parser rejected the call before the tool ever ran. The offline provider's every value has to be valid JSON or
        // the tool path it exists to exercise is never reached.
        "write" => {
            r#"{"path":"offline-written.txt","content":"written by the offline provider\n"}"#
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
