//! Tests for the command RPC surface.
//!
//! A slash command is typed by a person, so its failure is something to render rather than a transport error: the
//! person typed something and deserves to see why it did not work.

use std::sync::Arc;

use girder_core::{
    CommandArguments, CommandDefinition, CommandDefinitionId, CommandHandler, CommandInput,
    CommandRegistry, RemoteCall, RemoteRegistry,
};
use serde_json::json;

fn call(args: serde_json::Value) -> RemoteCall {
    RemoteCall {
        args,
        peer: "test".to_owned(),
    }
}

fn registry(commands: &Arc<CommandRegistry>) -> RemoteRegistry {
    let mut registry = RemoteRegistry::new();
    girder_core::register_command_methods(&mut registry, commands);
    registry
}

fn echo() -> CommandHandler {
    Arc::new(|arguments: CommandArguments| {
        Box::pin(async move {
            Ok(match arguments {
                CommandArguments::None => "none".to_owned(),
                CommandArguments::Text(text) => format!("text:{text}"),
                CommandArguments::Files(files) => format!("files:{}", files.join(",")),
                CommandArguments::Choice(choice) => format!("choice:{choice}"),
            })
        })
    })
}

fn command(name: &str, input: CommandInput) -> CommandDefinition {
    CommandDefinition::new(
        CommandDefinitionId::new(name).expect("valid"),
        name,
        "does something",
    )
    .expect("valid")
    .asking_for(input)
}

#[tokio::test]
async fn list_reports_every_command() {
    let commands = Arc::new(CommandRegistry::new());
    let _a = commands
        .register(
            command(
                "rename",
                CommandInput::Text {
                    label: "new title".to_owned(),
                    optional: false,
                },
            ),
            echo(),
        )
        .expect("registered");

    let value = registry(&commands)
        .dispatch("command", "list", call(json!({})))
        .await
        .expect("listed");

    let listed = value["commands"].as_array().expect("an array");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["name"], "rename");
    assert_eq!(listed[0]["input"]["kind"], "text");
    assert_eq!(listed[0]["input"]["label"], "new title");
}

#[tokio::test]
async fn a_command_with_no_input_reports_none() {
    // A client needs to know whether to show a field at all.
    let commands = Arc::new(CommandRegistry::new());
    let _a = commands
        .register(command("clear", CommandInput::None), echo())
        .expect("registered");

    let value = registry(&commands)
        .dispatch("command", "list", call(json!({})))
        .await
        .expect("listed");

    assert_eq!(value["commands"][0]["input"]["kind"], "none");
    assert!(value["commands"][0]["input"]["label"].is_null());
}

#[tokio::test]
async fn running_a_command_returns_its_message() {
    let commands = Arc::new(CommandRegistry::new());
    let _a = commands
        .register(
            command(
                "echo",
                CommandInput::Text {
                    label: "text".to_owned(),
                    optional: false,
                },
            ),
            echo(),
        )
        .expect("registered");

    let value = registry(&commands)
        .dispatch(
            "command",
            "run",
            call(json!({"name": "echo", "arguments": "hi"})),
        )
        .await
        .expect("ran");

    assert_eq!(value["message"], "text:hi");
    assert_eq!(value["isError"], false);
}

#[tokio::test]
async fn a_failing_command_is_a_result_not_a_transport_error() {
    // The person typed something and deserves to see why it did not work.
    let commands = Arc::new(CommandRegistry::new());
    let _a = commands
        .register(command("deploy", CommandInput::None), echo())
        .expect("registered");

    let value = registry(&commands)
        .dispatch(
            "command",
            "run",
            call(json!({"name": "nope", "arguments": null})),
        )
        .await
        .expect("a result");

    assert_eq!(value["isError"], true);
    let message = value["message"].as_str().expect("a message");
    assert!(message.contains("nope"), "{message}");
}

#[tokio::test]
async fn a_wrong_argument_kind_is_reported_to_the_person() {
    let commands = Arc::new(CommandRegistry::new());
    let _a = commands
        .register(
            command(
                "rename",
                CommandInput::Text {
                    label: "title".to_owned(),
                    optional: false,
                },
            ),
            echo(),
        )
        .expect("registered");

    let value = registry(&commands)
        .dispatch(
            "command",
            "run",
            call(json!({"name": "rename", "arguments": ["a.txt"]})),
        )
        .await
        .expect("a result");

    assert_eq!(value["isError"], true);
    let message = value["message"].as_str().expect("a message");
    assert!(message.contains("expects"), "{message}");
}

#[tokio::test]
async fn an_array_argument_becomes_files() {
    let commands = Arc::new(CommandRegistry::new());
    let _a = commands
        .register(
            command(
                "attach",
                CommandInput::Files {
                    label: "files".to_owned(),
                },
            ),
            echo(),
        )
        .expect("registered");

    let value = registry(&commands)
        .dispatch(
            "command",
            "run",
            call(json!({"name": "attach", "arguments": ["a.png", "b.png"]})),
        )
        .await
        .expect("ran");

    assert_eq!(value["message"], "files:a.png,b.png");
}

#[tokio::test]
async fn a_missing_name_is_a_remote_error() {
    // A client that sent no name made a protocol mistake, which is different from a command failing.
    let commands = Arc::new(CommandRegistry::new());

    let error = registry(&commands)
        .dispatch("command", "run", call(json!({})))
        .await
        .expect_err("missing name");

    assert!(matches!(
        error,
        girder_core::RemoteError::InvalidArguments { .. }
    ));
}

#[tokio::test]
async fn a_bad_argument_shape_is_a_remote_error() {
    let commands = Arc::new(CommandRegistry::new());

    let error = registry(&commands)
        .dispatch(
            "command",
            "run",
            call(json!({"name": "x", "arguments": {"a": 1}})),
        )
        .await
        .expect_err("bad shape");

    let girder_core::RemoteError::InvalidArguments { message, .. } = error else {
        panic!("expected InvalidArguments");
    };
    assert!(message.contains("string or an array"), "{message}");
}

#[tokio::test]
async fn the_plain_registry_offers_the_command_surface() {
    // A deployment that could list sessions but not run a command would be a strange half.
    let dir = std::env::temp_dir().join(format!("girder-cmd-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let catalog: Arc<dyn girder_core::SessionCatalog> =
        Arc::new(girder_core::DirectoryCatalog::new(&dir));

    let names = girder_core::default_registry(catalog).names();

    assert!(
        names
            .iter()
            .any(|(namespace, method)| namespace == "command" && method == "list")
    );
    assert!(
        names
            .iter()
            .any(|(namespace, method)| namespace == "command" && method == "run")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
