//! Tests for starting MCP servers from a configuration document.
//!
//! The module's job is to turn stored settings into running servers, and the properties pinned here
//! are the ones that make it usable in a deployment:
//!
//! - **A good list becomes configs** -- names, commands, arguments and the `enabled` default survive
//!   the parse, because a field silently dropped here is a server started with the wrong arguments.
//! - **A bad entry is named by index** -- `MissingField` and `BadName` carry the row, so the operator
//!   can go to one line of the document instead of re-reading all of it.
//! - **A bad name is refused** -- the name is a tool namespace, and a name that cannot be one would
//!   publish tools the call path cannot address.
//! - **One server failing does not stop the rest** -- the failure mode this module exists to avoid
//!   is one typo taking a whole deployment's MCP surface down with it.
//! - **`enabled: false` is skipped, and says so** -- listed-but-off must not look like never
//!   configured, and must not start a process.
//!
//! The start tests spawn a real child: a small shell script that speaks the two requests the stdio
//! transport makes. A fixture is used rather than a real MCP server because this is not a protocol
//! test -- `mcp_e2e` owns that -- and because a fixture can be made to serve exactly two tools, which
//! is what makes the tool count assertable.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile
//! against it as written even before the crate root wires it in. It names the crate through
//! `::girder_core`, which the `extern crate self as girder_core` alias lets resolve from either side.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/mcp/config.rs"]
pub mod mcp_config;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use mcp_config::{ConfigError, ServerConfig, parse_config, start_all};
use serde_json::{Value, json};

/// How long a start is allowed to take before the test calls it a hang.
///
/// The transport blocks on a pipe, so a fixture that never answers would otherwise hang the suite
/// instead of failing it; a test that waits forever reports nothing.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// The fixture server: answers `initialize` and `tools/list`, then keeps reading.
///
/// Two tools, so a count of two is distinguishable from the "no tools" that a failed or disabled
/// server reports.
const SERVER_SCRIPT: &str = r#"
set -u
while IFS= read -r line; do
    id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    case "$line" in
    *'"method":"initialize"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"fixture","version":"0.1.0"}}}\n' "$id"
        ;;
    *'"method":"tools/list"'*)
        printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo the text back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},{"name":"sum","description":"Add two numbers","inputSchema":{"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}}}}]}}\n' "$id"
        ;;
    esac
done
"#;

/// A command that names no executable, so spawning it fails immediately.
///
/// A port that nothing listens on would be a start failure too, but a missing program is the failure
/// the tests are about: the operator mistyped a path.
const NO_SUCH_COMMAND: &str = "/nonexistent/girder-mcp-config-fixture";

/// A fixture script on disk, removed when the test ends.
struct Fixture {
    /// The script's directory, deleted on drop so a run leaves nothing behind.
    directory: PathBuf,
    /// The script itself, passed to `sh` as an argument.
    script: PathBuf,
}

impl Fixture {
    /// Writes [`SERVER_SCRIPT`] into a fresh temporary directory.
    fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "girder-mcp-config-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).expect("a temporary directory is writable");
        let script = directory.join("server.sh");
        std::fs::write(&script, SERVER_SCRIPT).expect("the fixture script is writable");
        Self { directory, script }
    }

    /// A config that runs this fixture under a name.
    fn config(&self, name: &str) -> ServerConfig {
        ServerConfig {
            name: name.to_owned(),
            command: "sh".to_owned(),
            args: vec![self.script.to_string_lossy().into_owned()],
            enabled: true,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Runs `start_all` with a deadline, so a server that never answers fails the test instead of hanging.
fn start_all_within(configs: &[ServerConfig]) -> Vec<mcp_config::StartedServer> {
    let (sender, receiver) = mpsc::channel();
    let owned = configs.to_vec();
    let worker = std::thread::spawn(move || {
        let started = start_all(&owned);
        // A closed receiver means the test already gave up; nothing left to report to.
        let _ = sender.send(started);
    });
    let outcome = receiver.recv_timeout(START_TIMEOUT);
    match outcome {
        Ok(started) => {
            worker.join().expect("the start thread finished");
            started
        }
        Err(error) => panic!("start_all did not return within {START_TIMEOUT:?}: {error}"),
    }
}

// -- Parsing -----------------------------------------------------------------------------------------

#[test]
fn a_valid_list_becomes_server_configs() {
    let document = json!({
        "mcp": {
            "servers": [
                {"name": "files", "command": "npx", "args": ["-y", "server-files"], "enabled": true},
                {"name": "git", "command": "git-mcp"}
            ]
        }
    });

    let configs = parse_config(&document).expect("a valid list parses");

    assert_eq!(
        configs,
        vec![
            ServerConfig {
                name: "files".to_owned(),
                command: "npx".to_owned(),
                args: vec!["-y".to_owned(), "server-files".to_owned()],
                enabled: true,
            },
            // No `args` and no `enabled`: an entry that takes no arguments and is not switched off is
            // the common case, and requiring both keys would make every document longer than it needs
            // to be.
            ServerConfig {
                name: "git".to_owned(),
                command: "git-mcp".to_owned(),
                args: Vec::new(),
                enabled: true,
            },
        ]
    );
}

#[test]
fn the_list_may_be_given_directly() {
    // What `SettingsStore::get("mcp.servers")` hands back is the array itself, so the same parser has
    // to accept it without the caller unwrapping the layer.
    let stored = json!([{"name": "files", "command": "npx"}]);

    let configs = parse_config(&stored).expect("a bare array parses");

    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0].name, "files");
}

#[test]
fn an_empty_list_parses_to_no_servers() {
    // A deployment with MCP switched off entirely, which is the state every deployment starts in.
    // Reporting this as an error would force every caller to special-case "none configured".
    assert_eq!(parse_config(&json!([])).unwrap(), Vec::new());
    assert_eq!(
        parse_config(&json!({"mcp": {"servers": []}})).unwrap(),
        Vec::new()
    );
}

#[test]
fn a_document_without_an_array_is_refused() {
    // Each of these is a shape mistake rather than "no servers": a string or an object under the key
    // means the document says something the parser cannot read, and treating it as an empty list
    // would silently start nothing.
    for document in [
        json!(null),
        json!({}),
        json!({"mcp": {}}),
        json!({"mcp": {"servers": "files"}}),
        json!({"mcp": {"servers": {"files": {"command": "npx"}}}}),
    ] {
        assert_eq!(
            parse_config(&document),
            Err(ConfigError::NotAnArray),
            "document {document} should be refused"
        );
    }
}

#[test]
fn a_missing_field_is_reported_with_its_index() {
    // The core promise of the error: an operator with a long list needs the row, not a sentence about
    // "a server".
    let document = json!([
        {"name": "files", "command": "npx"},
        {"name": "git"}
    ]);

    assert_eq!(
        parse_config(&document),
        Err(ConfigError::MissingField {
            index: 1,
            field: "command"
        })
    );
}

#[test]
fn a_field_of_the_wrong_type_counts_as_missing() {
    // A number where a command goes is the same repair as no command at all, and folding the two into
    // one error keeps every caller from handling the same action twice.
    let document = json!([{"name": "files", "command": 42}]);

    assert_eq!(
        parse_config(&document),
        Err(ConfigError::MissingField {
            index: 0,
            field: "command"
        })
    );
}

#[test]
fn an_empty_command_is_refused() {
    // An empty command names no program. Spawning it would fail at start time with a message about a
    // program nobody wrote, so it is caught while the index is still available.
    let document = json!([{"name": "files", "command": "   "}]);

    assert_eq!(
        parse_config(&document),
        Err(ConfigError::MissingField {
            index: 0,
            field: "command"
        })
    );
}

#[test]
fn a_name_that_cannot_be_a_namespace_is_refused() {
    // `__` is the tool-name separator: a server called `a__b` would publish `a__b__tool`, which the
    // call path splits into server `a` and tool `b__tool`.
    let document = json!([
        {"name": "files", "command": "npx"},
        {"name": "a__b", "command": "npx"}
    ]);

    assert_eq!(
        parse_config(&document),
        Err(ConfigError::BadName {
            index: 1,
            name: "a__b".to_owned()
        })
    );
}

#[test]
fn a_name_with_illegal_characters_or_no_content_is_refused() {
    // A space would make the qualified name ambiguous to read and impossible to route; an empty name
    // would namespace tools under nothing at all.
    for name in ["has space", "semi;colon", ""] {
        let document = json!([{"name": name, "command": "npx"}]);
        assert_eq!(
            parse_config(&document),
            Err(ConfigError::BadName {
                index: 0,
                name: name.to_owned()
            }),
            "name {name:?} should be refused"
        );
    }
}

#[test]
fn a_missing_name_is_reported_before_anything_else() {
    // The name is the entry's identity, so a row without one is reported as that rather than as
    // whatever its command happens to be -- otherwise fixing the command would only reveal the next
    // error.
    let document = json!([{"command": "npx", "args": "not-a-list"}]);

    assert_eq!(
        parse_config(&document),
        Err(ConfigError::MissingField {
            index: 0,
            field: "name"
        })
    );
}

#[test]
fn an_unusable_argument_list_is_refused() {
    // Emptying a bad `args` would start the server with arguments its author did not write, and a
    // server started with the wrong flags does not look broken from the outside.
    for args in [json!("--flag"), json!(["--flag", 7]), json!({"a": 1})] {
        let document = json!([{"name": "files", "command": "npx", "args": args}]);
        assert_eq!(
            parse_config(&document),
            Err(ConfigError::MissingField {
                index: 0,
                field: "args"
            }),
            "args {args} should be refused"
        );
    }
}

#[test]
fn an_unusable_enabled_flag_is_refused() {
    // `"false"` coerced by truthiness is exactly the inversion that leaves a server running after its
    // operator turned it off.
    let document = json!([{"name": "files", "command": "npx", "enabled": "false"}]);

    assert_eq!(
        parse_config(&document),
        Err(ConfigError::MissingField {
            index: 0,
            field: "enabled"
        })
    );
}

#[test]
fn enabled_may_be_false_or_null() {
    // `null` reads as "not stated", which is on; `false` is off.
    let document = json!([
        {"name": "files", "command": "npx", "enabled": false},
        {"name": "git", "command": "npx", "enabled": null}
    ]);

    let configs = parse_config(&document).expect("both forms parse");
    assert!(!configs[0].enabled);
    assert!(configs[1].enabled);
}

// -- Starting ----------------------------------------------------------------------------------------

#[test]
fn a_working_server_reports_the_tools_it_publishes() {
    let fixture = Fixture::new();
    let started = start_all_within(&[fixture.config("files")]);

    assert_eq!(started.len(), 1);
    assert_eq!(started[0].name, "files");
    assert_eq!(started[0].failed, None);
    // Two, the number the fixture publishes: a count that came from the server rather than from the
    // configuration is the only evidence that the handshake actually happened.
    assert_eq!(started[0].tools, 2);
}

#[test]
fn one_failing_server_does_not_stop_the_others() {
    // The property this module exists for. A deployment with one mistyped path must keep the rest of
    // its MCP surface, and the report must say which entry is the broken one.
    let fixture = Fixture::new();
    let configs = vec![
        fixture.config("first"),
        ServerConfig {
            name: "broken".to_owned(),
            command: NO_SUCH_COMMAND.to_owned(),
            args: Vec::new(),
            enabled: true,
        },
        fixture.config("third"),
    ];

    let started = start_all_within(&configs);

    // One outcome per configured entry, in configured order: a caller zipping the report back onto
    // its configuration must not have to guess which server a row is about.
    assert_eq!(started.len(), 3);
    assert_eq!(started[0].name, "first");
    assert_eq!(started[0].failed, None);
    assert_eq!(started[0].tools, 2);
    assert_eq!(started[1].name, "broken");
    assert_eq!(started[1].tools, 0);
    let failure = started[1]
        .failed
        .as_deref()
        .expect("the broken server reports why it failed");
    assert!(
        failure.contains(NO_SUCH_COMMAND),
        "the failure should name the command that did not start: {failure}"
    );
    assert_eq!(started[2].name, "third");
    assert_eq!(started[2].failed, None);
    assert_eq!(started[2].tools, 2);
}

#[test]
fn a_disabled_server_is_reported_but_not_started() {
    // The command does not exist, so a `failed: None` here proves no process was created: if the
    // server had been started it could only have failed.
    let configs = vec![ServerConfig {
        name: "off".to_owned(),
        command: NO_SUCH_COMMAND.to_owned(),
        args: Vec::new(),
        enabled: false,
    }];

    let started = start_all_within(&configs);

    assert_eq!(started.len(), 1);
    // Still listed, so "configured but off" is distinguishable from "never configured".
    assert_eq!(started[0].name, "off");
    assert_eq!(started[0].tools, 0);
    assert_eq!(started[0].failed, None);
}

#[test]
fn an_empty_configuration_starts_nothing() {
    assert_eq!(start_all_within(&[]), Vec::new());
}

#[test]
fn the_parse_feeds_the_start() {
    // The two halves joined the way a caller uses them: a document in, running servers out, with the
    // disabled entry skipped and the enabled one counted.
    let fixture = Fixture::new();
    let document: Value = json!({
        "mcp": {
            "servers": [
                {
                    "name": "live",
                    "command": "sh",
                    "args": [fixture.script.to_string_lossy()],
                    "enabled": true
                },
                {
                    "name": "off",
                    "command": NO_SUCH_COMMAND,
                    "enabled": false
                }
            ]
        }
    });
    let configs = parse_config(&document).expect("the fixture document parses");

    let started = start_all_within(&configs);

    assert_eq!(started.len(), 2);
    assert_eq!((started[0].name.as_str(), started[0].tools), ("live", 2));
    assert_eq!(started[0].failed, None);
    assert_eq!((started[1].name.as_str(), started[1].tools), ("off", 0));
    assert_eq!(started[1].failed, None);
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod mcp {
    pub use super::mcp_config as config;
}
