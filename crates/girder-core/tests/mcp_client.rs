//! Tests for the MCP JSON-RPC wire layer.
//!
//! Two properties carry most of the weight here, and both fail silently in production rather than loudly:
//!
//! - **Id uniqueness.** JSON-RPC correlates a reply to a request only by `id`. A duplicated id does not
//!   raise an error; it delivers one request's result to another's caller, or leaves a request waiting
//!   forever because its answer was consumed elsewhere. So the allocator is tested under concurrency and at
//!   its exhaustion boundary, not just called once.
//! - **Name qualification.** Two servers offering `read` must produce two distinct tools. A test that only
//!   checked one server's name would pass while the collision it exists to prevent was live.
//!
//! The rest of the file checks that the three requests have the wire shape a server actually validates, that a
//! protocol-level error is reported as a value rather than a parse failure, and that replies no server should
//! ever send are rejected instead of being read as one of the two valid shapes.
//!
//! The module is included by path rather than through the crate's re-exports, so these tests compile against it
//! as written even before the crate root wires it in.

#[path = "../src/mcp_client.rs"]
mod mcp_client;

use std::collections::HashSet;

use mcp_client::{
    IdAllocator, JSONRPC_VERSION, McpError, McpResponse, McpTool, NAMESPACE_SEPARATOR,
    PROTOCOL_VERSION, RpcError, ServerConfig, call_tool_request, initialize_request,
    list_tools_request, parse_response, parse_tools, qualified_name, split_qualified_name,
    validate_server_name,
};
use serde_json::{Value, json};

/// Collects every string in a value, so a test can assert that a payload appears exactly where expected
/// without depending on key order.
fn object_field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .get(key)
        .unwrap_or_else(|| panic!("request has no `{key}`: {value}"))
}

// ---- request shapes ------------------------------------------------------------------------------

#[test]
fn initialize_request_is_a_jsonrpc_call_with_a_client_info() {
    let request = initialize_request(7);

    assert_eq!(object_field(&request, "jsonrpc"), JSONRPC_VERSION);
    assert_eq!(object_field(&request, "id"), &json!(7));
    assert_eq!(object_field(&request, "method"), "initialize");

    let params = object_field(&request, "params");
    assert_eq!(
        object_field(params, "protocolVersion"),
        PROTOCOL_VERSION,
        "the negotiated revision must be stated, not left for the server to guess"
    );
    assert_eq!(
        object_field(params, "capabilities"),
        &json!({}),
        "this client claims no capabilities; claiming one would invite messages it cannot handle"
    );
    // A server logs `clientInfo`, so it has to be present and has to be a string, not a nested struct that a
    // human reading the log would have to expand.
    let client = object_field(params, "clientInfo");
    assert!(object_field(client, "name").is_string(), "{request}");
    assert!(object_field(client, "version").is_string(), "{request}");
}

#[test]
fn list_tools_request_sends_an_empty_params_object() {
    let request = list_tools_request(11);

    assert_eq!(object_field(&request, "jsonrpc"), JSONRPC_VERSION);
    assert_eq!(object_field(&request, "id"), &json!(11));
    assert_eq!(object_field(&request, "method"), "tools/list");
    // Explicit `{}` rather than an omitted `params`: servers that validate the field report an omitted one as
    // malformed instead of reading it as "no cursor".
    assert_eq!(object_field(&request, "params"), &json!({}));
    assert!(
        request.get("result").is_none() && request.get("error").is_none(),
        "a request must not carry a response member: {request}"
    );
}

#[test]
fn call_tool_request_nests_name_and_arguments_under_params() {
    let request = call_tool_request(12, "read", json!({"path": "/etc/hosts"}));

    assert_eq!(object_field(&request, "method"), "tools/call");
    let params = object_field(&request, "params");
    assert_eq!(object_field(params, "name"), "read");
    assert_eq!(
        object_field(params, "arguments"),
        &json!({"path": "/etc/hosts"}),
        "arguments are forwarded unmodified; the server validates them against its own schema"
    );
}

#[test]
fn a_non_object_arguments_value_is_forwarded_not_repaired() {
    // The server's `-32602` names the offending parameter. Rewriting the value into an object would hide the
    // caller that built arguments from the wrong schema.
    let request = call_tool_request(13, "read", json!("not an object"));

    assert_eq!(
        object_field(object_field(&request, "params"), "arguments"),
        &json!("not an object")
    );
}

// ---- id uniqueness -------------------------------------------------------------------------------

#[test]
fn allocator_hands_out_distinct_increasing_ids() {
    let allocator = IdAllocator::new();

    let mut previous = None;
    for _ in 0..1_000 {
        let id = match allocator.next_id() {
            Ok(id) => id,
            Err(error) => panic!("ids exhausted after a thousand allocations: {error}"),
        };
        if let Some(previous) = previous {
            assert!(id > previous, "ids went backwards: {previous} then {id}");
        }
        previous = Some(id);
    }
}

#[test]
fn ids_stay_unique_when_requests_are_built_concurrently() {
    // The property is about concurrent callers: a per-call counter would hand the same id to two requests that
    // are in flight together, and the second reply would be delivered to the first caller.
    let allocator = std::sync::Arc::new(IdAllocator::new());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let allocator = std::sync::Arc::clone(&allocator);
            std::thread::spawn(move || {
                let mut ids = Vec::with_capacity(250);
                for _ in 0..250 {
                    match allocator.next_id() {
                        Ok(id) => ids.push(id),
                        Err(error) => panic!("ids exhausted under load: {error}"),
                    }
                }
                ids
            })
        })
        .collect();

    let mut seen = HashSet::new();
    for thread in threads {
        // A panicking thread is a test failure, not an id the collector can skip: the count assertion below
        // would otherwise pass on a thread that never allocated.
        let Ok(ids) = thread.join() else {
            panic!("a thread panicked while allocating");
        };
        for id in ids {
            assert!(seen.insert(id), "id {id} was handed out twice");
        }
    }
    assert_eq!(seen.len(), 8 * 250, "every allocation must be distinct");
}

#[test]
fn ids_built_into_requests_are_unique() {
    // The allocator being unique is only useful if the builders carry the id through unmodified; a builder that
    // hard-coded or rounded the id would defeat it while the allocator's own test still passed.
    let allocator = IdAllocator::new();
    let mut seen = HashSet::new();
    for _ in 0..500 {
        let id = match allocator.next_id() {
            Ok(id) => id,
            Err(error) => panic!("ids exhausted: {error}"),
        };
        for request in [
            initialize_request(id),
            list_tools_request(id),
            call_tool_request(id, "read", json!({})),
        ] {
            assert_eq!(
                object_field(&request, "id"),
                &json!(id),
                "the builder dropped the allocated id"
            );
        }
        assert!(seen.insert(id), "id {id} was used for two requests");
    }
}

#[test]
fn allocator_starts_at_one_and_can_start_elsewhere() {
    let fresh = IdAllocator::new();
    assert_eq!(
        fresh.next_id(),
        Ok(1),
        "starting above zero keeps a placeholder id distinguishable from an issued one"
    );

    let resumed = IdAllocator::starting_at(4_000);
    assert_eq!(resumed.next_id(), Ok(4_000));
    assert_eq!(resumed.next_id(), Ok(4_001));
}

#[test]
fn allocator_refuses_to_wrap_instead_of_reissuing_ids() {
    // A wrapped id is worse than an error: it collides with a reply that may still be in flight.
    let allocator = IdAllocator::starting_at(u64::MAX - 2);

    assert_eq!(allocator.next_id(), Ok(u64::MAX - 2));
    assert_eq!(allocator.next_id(), Ok(u64::MAX - 1));
    assert_eq!(
        allocator.next_id(),
        Err(McpError::IdExhausted),
        "the last id is refused rather than wrapping to zero"
    );
    assert_eq!(
        allocator.next_id(),
        Err(McpError::IdExhausted),
        "the refusal must be sticky, not a one-off"
    );
}

// ---- response parsing ----------------------------------------------------------------------------

#[test]
fn a_success_response_is_parsed_with_its_id_and_result() {
    let response = parse_response(&json!({
        "jsonrpc": "2.0",
        "id": 42,
        "result": {"tools": []},
    }));

    match response {
        Ok(McpResponse::Success { id, result }) => {
            assert_eq!(id, 42);
            assert_eq!(result, json!({"tools": []}));
        }
        other => panic!("expected a success response, got {other:?}"),
    }
}

#[test]
fn a_protocol_error_is_a_value_not_a_parse_failure() {
    // A server that answers "method not found" is speaking the protocol; the deployment reports a missing
    // capability, not a broken connection.
    let response = parse_response(&json!({
        "jsonrpc": "2.0",
        "id": 9,
        "error": {"code": -32601, "message": "method not found", "data": {"method": "tools/call"}},
    }));

    match response {
        Ok(McpResponse::Failure { id, error }) => {
            assert_eq!(id, Some(9));
            assert_eq!(
                error,
                RpcError {
                    code: -32601,
                    message: "method not found".to_owned(),
                    data: Some(json!({"method": "tools/call"})),
                }
            );
            assert!(error.is_method_not_found());
        }
        other => panic!("expected a failure response, got {other:?}"),
    }
}

#[test]
fn an_application_error_code_is_not_mistaken_for_a_missing_method() {
    let response = parse_response(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "error": {"code": -32602, "message": "invalid params"},
    }));

    match response {
        Ok(McpResponse::Failure { error, .. }) => {
            assert!(!error.is_method_not_found());
            assert_eq!(error.data, None);
        }
        other => panic!("expected a failure response, got {other:?}"),
    }
}

#[test]
fn a_failure_may_answer_an_unknown_message_and_carry_a_null_id() {
    // JSON-RPC allows a null id when the server could not read the request's id. Refusing it would turn
    // "your framing was rejected" into a client-side parse failure and lose the server's explanation.
    let response = parse_response(&json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": {"code": -32700, "message": "parse error"},
    }));

    match response {
        Ok(McpResponse::Failure { id, error }) => {
            assert_eq!(id, None);
            assert_eq!(error.code, -32700);
        }
        other => panic!("expected a failure response, got {other:?}"),
    }
}

#[test]
fn response_accessors_separate_the_two_shapes() {
    let success = parse_response(&json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": true}}));
    let failure = parse_response(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "error": {"code": -32603, "message": "internal"},
    }));

    match (success, failure) {
        (Ok(success), Ok(failure)) => {
            assert_eq!(success.id(), Some(1));
            assert_eq!(success.result(), Some(&json!({"ok": true})));
            assert_eq!(success.error(), None);

            assert_eq!(failure.id(), Some(2));
            assert_eq!(failure.result(), None);
            assert!(failure.error().is_some());
        }
        (success, failure) => panic!("both replies should parse: {success:?} / {failure:?}"),
    }
}

#[test]
fn malformed_replies_are_rejected() {
    let cases: Vec<(&str, Value, McpError)> = vec![
        (
            "a bare array is not a reply",
            json!([{"jsonrpc": "2.0", "id": 1, "result": {}}]),
            McpError::ResponseNotAnObject,
        ),
        (
            "a string is not a reply",
            json!("id: 1 result: {}"),
            McpError::ResponseNotAnObject,
        ),
        (
            "no version member",
            json!({"id": 1, "result": {}}),
            McpError::MissingVersion,
        ),
        (
            "a foreign protocol version",
            json!({"jsonrpc": "1.0", "id": 1, "result": {}}),
            McpError::UnsupportedVersion {
                found: "1.0".to_owned(),
            },
        ),
        (
            "no id at all",
            json!({"jsonrpc": "2.0", "result": {}}),
            McpError::MissingId,
        ),
        (
            "a string id cannot be matched to a u64 request",
            json!({"jsonrpc": "2.0", "id": "1", "result": {}}),
            McpError::InvalidId {
                found: "\"1\"".to_owned(),
            },
        ),
        (
            "a negative id is outside the issued range",
            json!({"jsonrpc": "2.0", "id": -1, "result": {}}),
            McpError::InvalidId {
                found: "-1".to_owned(),
            },
        ),
        (
            "a success with a null id cannot be delivered",
            json!({"jsonrpc": "2.0", "id": null, "result": {}}),
            McpError::MissingId,
        ),
        (
            "both result and error is a protocol violation",
            json!({"jsonrpc": "2.0", "id": 1, "result": {}, "error": {"code": 1, "message": "x"}}),
            McpError::ResultAndError,
        ),
        (
            "neither result nor error says nothing",
            json!({"jsonrpc": "2.0", "id": 1}),
            McpError::NeitherResultNorError,
        ),
        (
            "an error that is not an object",
            json!({"jsonrpc": "2.0", "id": 1, "error": "boom"}),
            McpError::ErrorNotAnObject,
        ),
        (
            "an error with no code",
            json!({"jsonrpc": "2.0", "id": 1, "error": {"message": "boom"}}),
            McpError::ErrorMissingCode,
        ),
        (
            "an error with a non-integer code",
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": "1", "message": "boom"}}),
            McpError::ErrorMissingCode,
        ),
        (
            "an error with no message",
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32603}}),
            McpError::ErrorMissingMessage,
        ),
    ];

    for (label, value, expected) in cases {
        match parse_response(&value) {
            Err(error) => assert_eq!(error, expected, "{label}: wrong rejection"),
            Ok(parsed) => panic!("{label}: should have been rejected, got {parsed:?}"),
        }
    }
}

// ---- tool listing --------------------------------------------------------------------------------

#[test]
fn a_tool_listing_is_parsed_into_tools() {
    let tools = parse_tools(&json!({
        "tools": [
            {
                "name": "read",
                "description": "Read a file",
                "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}},
            },
            // No description: the field is optional in the protocol, and dropping the entry would hide a
            // working tool because its author was terse.
            {"name": "list", "inputSchema": {"type": "object", "properties": {}}},
        ],
    }));

    match tools {
        Ok(tools) => {
            assert_eq!(tools.len(), 2);
            assert_eq!(tools[0].name, "read");
            assert_eq!(tools[0].description, "Read a file");
            assert_eq!(tools[1].description, "");
            assert_eq!(
                tools[1].qualified("fs"),
                format!("fs{NAMESPACE_SEPARATOR}list")
            );
        }
        Err(error) => panic!("a well-formed listing should parse: {error}"),
    }
}

#[test]
fn an_unusable_tool_entry_fails_the_listing_instead_of_being_skipped() {
    // A skipped entry looks like a server that never offered it, which is the harder failure to diagnose.
    let cases: Vec<(&str, Value, McpError)> = vec![
        ("no tools member", json!({}), McpError::ToolsNotListed),
        (
            "tools is not an array",
            json!({"tools": {}}),
            McpError::ToolsNotListed,
        ),
        (
            "an entry is not an object",
            json!({"tools": ["read"]}),
            McpError::ToolNotAnObject,
        ),
        (
            "an entry has no name",
            json!({"tools": [{"inputSchema": {"type": "object"}}]}),
            McpError::InvalidTool {
                reason: "a tool has no string `name`",
            },
        ),
        (
            "an entry has an empty name",
            json!({"tools": [{"name": "", "inputSchema": {"type": "object"}}]}),
            McpError::InvalidTool {
                reason: "a tool name is empty",
            },
        ),
        (
            "a description that is not a string",
            json!({"tools": [{"name": "read", "description": 3, "inputSchema": {"type": "object"}}]}),
            McpError::InvalidTool {
                reason: "a tool's `description` is not a string",
            },
        ),
        (
            "a schema that is not an object",
            json!({"tools": [{"name": "read", "inputSchema": []}]}),
            McpError::InvalidTool {
                reason: "a tool's `inputSchema` is not an object",
            },
        ),
        (
            "an entry with no schema",
            json!({"tools": [{"name": "read"}]}),
            McpError::InvalidTool {
                reason: "a tool has no `inputSchema`",
            },
        ),
    ];

    for (label, value, expected) in cases {
        match parse_tools(&value) {
            Err(error) => assert_eq!(error, expected, "{label}: wrong rejection"),
            Ok(tools) => panic!("{label}: should have been rejected, got {tools:?}"),
        }
    }
}

#[test]
fn a_tool_without_a_description_is_still_usable() {
    let tool = McpTool::from_value(&json!({"name": "ping", "inputSchema": {"type": "object"}}));

    match tool {
        Ok(tool) => assert_eq!(tool.name, "ping"),
        Err(error) => panic!("a description is optional in the protocol: {error}"),
    }
}

// ---- qualified names -----------------------------------------------------------------------------

#[test]
fn two_servers_offering_the_same_tool_get_distinct_names() {
    // The collision this prevents: a registry keyed on the bare name lets the second server's `read` shadow the
    // first, and the model calls one while reading the other's description.
    let first = qualified_name("server_a", "read");
    let second = qualified_name("server_b", "read");

    assert_eq!(first, "server_a__read");
    assert_eq!(second, "server_b__read");
    assert_ne!(first, second);
    assert_ne!(first, "read", "the bare name must not survive as the key");
    assert_ne!(second, "read");
}

#[test]
fn a_qualified_name_splits_back_into_its_two_halves() {
    // A tool name may itself contain the separator: only the server half is forbidden from doing so, which is
    // what makes the first separator the split point rather than one candidate among several.
    let qualified = qualified_name("rainbow", "get_group__config");

    assert_eq!(
        split_qualified_name(&qualified),
        Ok(("rainbow", "get_group__config"))
    );
    assert_eq!(
        split_qualified_name("read"),
        Err(McpError::UnqualifiedToolName {
            name: "read".to_owned(),
        }),
        "a bare name would be routed to whichever server happened to be looked up first"
    );
}

#[test]
fn a_server_name_containing_the_separator_is_refused() {
    // `a__b` publishing `read` would produce `a__b__read`, which splits into server `a`. Allowing it would put
    // the collision back through the namespace itself.
    assert!(matches!(
        validate_server_name("a__b"),
        Err(McpError::InvalidServerName { .. })
    ));
    assert!(validate_server_name("rainbow").is_ok());
    assert!(validate_server_name("my-server_2").is_ok());
}

#[test]
fn unusable_server_names_are_refused_with_a_reason() {
    for (name, reason) in [
        ("", "a namespace cannot be empty"),
        ("   ", "a namespace cannot be empty"),
        (
            "has space",
            "a namespace may only use letters, digits, `-`, and `_`",
        ),
        (
            "slash/name",
            "a namespace may only use letters, digits, `-`, and `_`",
        ),
        (
            "dotted.name",
            "a namespace may only use letters, digits, `-`, and `_`",
        ),
        (
            "a__b",
            "a namespace cannot contain `__`, the tool-name separator",
        ),
    ] {
        match validate_server_name(name) {
            Err(McpError::InvalidServerName {
                name: rejected,
                reason: found,
            }) => {
                assert_eq!(rejected, name);
                assert_eq!(found, reason, "wrong reason for `{name}`");
            }
            Err(other) => panic!("`{name}` was refused for the wrong reason: {other}"),
            Ok(()) => panic!("`{name}` should have been refused"),
        }
    }
}

// ---- server configuration ------------------------------------------------------------------------

#[test]
fn a_server_configuration_round_trips_through_its_constructor() {
    let config = match ServerConfig::new("fs", "npx")
        .map(|config| config.with_args(["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]))
    {
        Ok(config) => config,
        Err(error) => panic!("a valid configuration was refused: {error}"),
    };

    assert_eq!(config.name, "fs");
    assert_eq!(config.command, "npx");
    assert_eq!(config.args.len(), 3);
    assert_eq!(config.validate(), Ok(()));
    assert_eq!(
        qualified_name(&config.name, "read"),
        "fs__read",
        "the configured name is exactly the namespace tools get"
    );
}

#[test]
fn a_configuration_without_a_program_or_namespace_is_refused_at_construction() {
    // Caught at construction because the alternative surfaces much later as "this server has no tools".
    assert!(matches!(
        ServerConfig::new("fs", ""),
        Err(McpError::EmptyCommand { .. })
    ));
    assert!(matches!(
        ServerConfig::new("fs", "   "),
        Err(McpError::EmptyCommand { .. })
    ));
    assert!(matches!(
        ServerConfig::new("", "npx"),
        Err(McpError::InvalidServerName { .. })
    ));

    // A struct built field by field bypasses the constructor, so it must be checkable too.
    let direct = ServerConfig {
        name: "a__b".to_owned(),
        command: "npx".to_owned(),
        args: Vec::new(),
    };
    assert!(matches!(
        direct.validate(),
        Err(McpError::InvalidServerName { .. })
    ));
}
