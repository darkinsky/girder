//! Starting MCP servers from a configuration document.
//!
//! # Why this exists
//!
//! `mcp_subprocess` can run a server and `mcp::Registry` can hold one, but nothing connected either
//! to configuration: the only code that knew how to spawn a server was a `Transport` implementation
//! waiting for a caller, so a deployment could store a server list and watch every entry in it stay
//! unstarted. This module is that caller. It reads the stored value, validates each entry into a
//! [`ServerConfig`], and starts the enabled ones.
//!
//! # Why one start failure does not stop the rest
//!
//! A deployment with six servers and a typo in the sixth command line wants the other five
//! answering, plus a sentence saying what the sixth did -- not five working servers held hostage by
//! one string. [`start_all`] therefore reports a per-server outcome in [`StartedServer`] rather than
//! one error for the batch, and [`StartedServer::failed`] carries the reason for the entry that did
//! not come up.
//!
//! # Why a parse error, unlike a start failure, refuses the whole list
//!
//! The two are read by different people in different circumstances. A start failure is a runtime
//! fact about one server while the others are still usable, so it is reported per entry. A parse
//! error is a configuration somebody has to go and edit, and starting the half of it that happened
//! to validate would run a deployment from a document its author has not finished fixing -- so
//! [`parse_config`] returns the first problem and the index it sits at, which is enough to find the
//! line, and starts nothing.
//!
//! # What starting leaves behind
//!
//! Nothing. [`StartedServer`] carries a name, a tool count and a failure -- no transport, so no
//! caller could reach a tool through it anyway. The child is dropped, and killed by `Subprocess`'s
//! `Drop`, when [`start_all`] returns. This answers which configured servers come up and how many
//! tools they publish, which is what a startup path checks before deciding to wire them into a
//! registry; keeping the processes alive would leak one per call with no handle to reach it.

use futures::executor::block_on;
use serde_json::Value;

// Named through the crate alias rather than `crate::` so this file compiles both as a module of the
// library and, by `tests/mcp_config.rs`, as a module of the test crate; `extern crate self as
// girder_core` in the crate root is what makes the path resolve from inside the library too.
use ::girder_core::{McpSubprocess, McpTransport as _, validate_server_name};

/// The key holding the server list, as an operator writes it.
pub const SERVERS_KEY: &str = "mcp.servers";

/// One MCP server as the configuration describes it.
///
/// A plain value rather than a handle: parsing a document and running a process are separate steps
/// in this module, and a configuration read for validation -- at boot, from a console, from a test --
/// must not spawn anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConfig {
    /// Its name, used to qualify the tool names it publishes.
    ///
    /// Validated with [`validate_server_name`] at parse time, because the name becomes the half of a
    /// tool name that routes a call: a name that cannot serve as a namespace would publish tools the
    /// call path cannot address.
    pub name: String,

    /// The executable to run.
    pub command: String,

    /// Its command-line arguments.
    pub args: Vec<String>,

    /// Whether [`start_all`] should run it.
    ///
    /// Absent from the document means `true`: a server someone took the trouble to list is one they
    /// intend to run, and requiring an explicit `true` would silently disable every entry whose
    /// author omitted a flag that reads as redundant.
    pub enabled: bool,
}

/// Why a configuration could not be read as a list of servers.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The value carries no array of servers.
    #[error("`{SERVERS_KEY}` must be an array of servers")]
    NotAnArray,

    /// An entry has no usable value for a required field.
    ///
    /// A field that is present but of the wrong type is reported as missing rather than under a
    /// separate variant: a caller responds the same way to both -- go and fix that field -- and two
    /// variants would make every caller handle the same action twice.
    #[error("`{SERVERS_KEY}[{index}]` has no usable `{field}`")]
    MissingField {
        /// Which entry, zero-based, so the operator can find the row.
        index: usize,
        /// Which field, by the name the document uses.
        field: &'static str,
    },

    /// An entry's name cannot be a tool namespace.
    ///
    /// The specific reason is the one [`validate_server_name`] gives; it is not carried here because
    /// the caller's action is the same for all of them, and `index` plus `name` already locate the
    /// one thing that has to change.
    #[error("`{SERVERS_KEY}[{index}]` has a name `{name}` that cannot be a tool namespace")]
    BadName {
        /// Which entry, zero-based.
        index: usize,
        /// The name that was rejected.
        name: String,
    },
}

/// Reads the server list out of a settings value.
///
/// Accepts the array itself (the value stored under `mcp.servers`), an object with a `servers`
/// member, or a whole settings document with the list at `mcp.servers`. All three are what a caller
/// naturally holds -- `SettingsStore::get` returns the leaf, a boot path holds the document -- and
/// making the caller dig out the right layer would be a second place to get the shape wrong.
///
/// Unknown fields are ignored rather than rejected: a document shared with other consumers may carry
/// keys this module does not read, and refusing them would make adding a sibling key a breaking
/// change.
///
/// # Errors
///
/// Returns [`ConfigError::NotAnArray`] when no server array is present, and the per-entry errors of
/// [`ConfigError`] for the first entry that cannot be used. The index in those errors is zero-based
/// in the array, not a line number of the document.
pub fn parse_config(value: &Value) -> Result<Vec<ServerConfig>, ConfigError> {
    let entries = server_entries(value).ok_or(ConfigError::NotAnArray)?;
    // The capacity hint is exact: every entry either becomes a config or stops the parse.
    let mut configs = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        configs.push(parse_entry(index, entry)?);
    }
    Ok(configs)
}

/// Starts every enabled server, reporting one outcome per entry.
///
/// The results are in the same order as `configs` and never omit an entry: a caller that zipped the
/// output back onto its configuration would otherwise attribute a failure to the wrong server. A
/// disabled entry is present with no tools and no failure, which is what makes "listed but not run"
/// visible in the report instead of looking like an entry that was never configured.
///
/// Blocking, and deliberately so: the transport it drives is a child process spoken to over stdio,
/// which needs no reactor, and the one caller -- a boot path -- is already deciding what to run
/// before any work starts.
pub fn start_all(configs: &[ServerConfig]) -> Vec<StartedServer> {
    configs.iter().map(start_one).collect()
}

/// What starting one configured server produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartedServer {
    /// The configured name, so a report line can name the server it is about.
    pub name: String,

    /// How many tools the server published.
    ///
    /// Zero for a disabled server and zero for one that failed: both published nothing. The two are
    /// told apart by [`StartedServer::failed`]. A server that comes up and genuinely offers no tools
    /// also reports zero, and that is not an error -- the protocol allows a tool-less server.
    pub tools: usize,

    /// Why the server did not come up, when it did not.
    ///
    /// The transport's message, kept as text rather than as its error type so a report can be logged
    /// and compared without every consumer linking the protocol vocabulary.
    pub failed: Option<String>,
}

/// Starts one entry, turning every failure into a report line instead of an `Err`.
///
/// Infallible by construction: a batch that stopped at the first error would be the behaviour this
/// module exists to avoid.
fn start_one(config: &ServerConfig) -> StartedServer {
    if !config.enabled {
        // No process is created at all. A disabled server that reported the same `failed` as a
        // broken one would make an intentional off-switch look like a fault.
        return StartedServer {
            name: config.name.clone(),
            tools: 0,
            failed: None,
        };
    }

    // The transport names itself after the program, so the messages it produces already say which
    // command line failed; the configured name is only used for the report.
    let transport = McpSubprocess::new(config.command.clone(), config.args.clone());
    match block_on(transport.start()) {
        Ok(catalogue) => StartedServer {
            name: config.name.clone(),
            tools: catalogue.tools.len(),
            failed: None,
        },
        Err(error) => StartedServer {
            name: config.name.clone(),
            tools: 0,
            failed: Some(error.to_string()),
        },
    }
}

/// Finds the server array in whatever layer of the document the caller holds.
///
/// `None` when there is no array to be found, which the caller turns into [`ConfigError::NotAnArray`]:
/// a document whose `mcp.servers` is a string or an object is a mistake about the shape, not an empty
/// deployment, and reporting it as zero servers would hide it.
fn server_entries(value: &Value) -> Option<&[Value]> {
    if let Some(entries) = value.as_array() {
        return Some(entries);
    }
    let nested = value
        .get("servers")
        .or_else(|| value.get("mcp").and_then(|mcp| mcp.get("servers")))?;
    nested.as_array().map(Vec::as_slice)
}

/// Validates one entry, reporting the index so the error points at a row rather than at the document.
fn parse_entry(index: usize, entry: &Value) -> Result<ServerConfig, ConfigError> {
    // The name is checked first because it is the entry's identity: a row whose name is wrong will be
    // rejected whatever its command says, and reporting the command first would send an operator
    // back twice.
    let name = string_field(entry, index, "name")?;
    validate_server_name(&name).map_err(|_| ConfigError::BadName {
        index,
        name: name.clone(),
    })?;

    let command = string_field(entry, index, "command")?;
    if command.trim().is_empty() {
        // An empty command names no executable; spawning it would fail at start time with a message
        // about a program the operator never wrote. Catching it at parse time says which row.
        return Err(ConfigError::MissingField {
            index,
            field: "command",
        });
    }

    Ok(ServerConfig {
        name,
        command,
        args: args_field(index, entry)?,
        enabled: enabled_field(index, entry)?,
    })
}

/// Reads a required string field.
///
/// A non-string value is reported as [`ConfigError::MissingField`]: this module needs a string there,
/// and a caller that wrote a number where a command goes has the same fix to make as one that wrote
/// nothing.
fn string_field(entry: &Value, index: usize, field: &'static str) -> Result<String, ConfigError> {
    entry
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(ConfigError::MissingField { index, field })
}

/// Reads the optional argument list.
///
/// A missing `args` is an empty list, because a server that takes no arguments is the common case.
/// A present-but-unusable one is an error rather than a silently emptied list: dropping a stray
/// element would start the server with arguments its author did not write, and a server that starts
/// with the wrong flags does not look broken from the outside.
fn args_field(index: usize, entry: &Value) -> Result<Vec<String>, ConfigError> {
    let Some(raw) = entry.get("args") else {
        return Ok(Vec::new());
    };
    let config_error = || ConfigError::MissingField {
        index,
        field: "args",
    };
    let items = raw.as_array().ok_or_else(config_error)?;
    items
        .iter()
        .map(|item| item.as_str().map(str::to_owned).ok_or_else(config_error))
        .collect()
}

/// Reads the optional `enabled` flag.
///
/// Absent or `null` means enabled; see [`ServerConfig::enabled`] for why the default is not `false`.
/// A non-boolean is rejected rather than coerced, because `"false"` coerced to truthiness is exactly
/// the kind of silent inversion that leaves a server running when its operator turned it off.
fn enabled_field(index: usize, entry: &Value) -> Result<bool, ConfigError> {
    match entry.get("enabled") {
        None | Some(Value::Null) => Ok(true),
        Some(Value::Bool(enabled)) => Ok(*enabled),
        Some(_) => Err(ConfigError::MissingField {
            index,
            field: "enabled",
        }),
    }
}
