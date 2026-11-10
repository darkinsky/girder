//! Tests for the human-command registry.
//!
//! A command is typed by a person and a tool is called by a model, so the surface differs: the input shape
//! is declared up front and validated by the registry, and a disposer must take the command with it.

use std::sync::Arc;

use girder_core::{
    CommandArguments, CommandDefinition, CommandDefinitionId, CommandError, CommandHandler,
    CommandInput, CommandRegistry,
};

fn id(value: &str) -> CommandDefinitionId {
    CommandDefinitionId::new(value).expect("valid")
}

/// A handler that echoes its arguments so a test can see what arrived.
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

fn command(name: &str) -> CommandDefinition {
    CommandDefinition::new(id(name), name, "does something").expect("valid")
}

// ---- definitions ------------------------------------------------------------------------------

#[test]
fn a_definition_drops_a_leading_slash() {
    // The name is what follows the slash; two spellings of one command would be two entries.
    let definition = CommandDefinition::new(id("x"), "/help", "shows help").expect("valid");

    assert_eq!(definition.name, "help");
}

#[test]
fn a_blank_name_or_description_is_refused() {
    // A command with no name cannot be typed, and one with no description cannot be chosen from a list.
    assert!(CommandDefinition::new(id("a"), "", "x").is_none());
    assert!(CommandDefinition::new(id("b"), "x", "  ").is_none());
    assert!(CommandDefinition::new(id("c"), "/", "x").is_none());
}

#[test]
fn a_name_with_whitespace_is_refused() {
    assert!(CommandDefinition::new(id("a"), "two words", "x").is_none());
}

#[test]
fn an_input_declares_what_it_asks_for() {
    let definition = command("rename").asking_for(CommandInput::Text {
        label: "new title".to_owned(),
        optional: false,
    });

    assert_eq!(definition.input.label(), Some("new title"));
}

#[test]
fn no_input_has_no_label() {
    assert_eq!(CommandInput::None.label(), None);
}

// ---- registration -----------------------------------------------------------------------------

#[test]
fn a_registered_command_is_listed() {
    let registry = CommandRegistry::new();
    let _ = registry
        .register(command("help"), echo())
        .expect("registered");

    assert_eq!(registry.names(), vec!["help"]);
    assert_eq!(registry.definitions().len(), 1);
}

#[test]
fn names_are_sorted() {
    // A help listing is rendered from them, and an order that changed between runs would make it jump.
    let registry = CommandRegistry::new();
    for name in ["zeta", "alpha"] {
        let _ = registry
            .register(command(name), echo())
            .expect("registered");
    }

    assert_eq!(registry.names(), vec!["alpha", "zeta"]);
}

#[test]
fn a_duplicate_name_is_refused() {
    let registry = CommandRegistry::new();
    let _ = registry
        .register(command("help"), echo())
        .expect("registered");

    // `expect_err` needs the success type to be `Debug`, and a disposer is a boxed closure.
    let Err(error) = registry.register(command("help"), echo()) else {
        panic!("a duplicate name must be refused");
    };

    assert!(matches!(error, CommandError::Duplicate { .. }));
}

#[test]
fn the_disposer_removes_the_command() {
    // A plugin that unloads must take its commands with it.
    let registry = CommandRegistry::new();
    let disposer = registry
        .register(command("help"), echo())
        .expect("registered");
    assert_eq!(registry.names().len(), 1);

    futures::executor::block_on(disposer());

    assert!(registry.names().is_empty());
}

#[test]
fn disposing_one_command_leaves_the_others() {
    let registry = CommandRegistry::new();
    let first = registry.register(command("a"), echo()).expect("registered");
    let _disposer = registry.register(command("b"), echo()).expect("registered");

    futures::executor::block_on(first());

    assert_eq!(registry.names(), vec!["b"]);
}

#[test]
fn a_definition_is_retrievable_by_name_without_its_slash() {
    let registry = CommandRegistry::new();
    let _ = registry
        .register(command("help"), echo())
        .expect("registered");

    assert!(registry.definition("help").is_some());
    assert!(registry.definition("/help").is_some());
}

// ---- execution --------------------------------------------------------------------------------

#[tokio::test]
async fn a_command_runs_and_returns_its_message() {
    let registry = CommandRegistry::new();
    let _disposer = registry
        .register(
            command("echo").asking_for(CommandInput::Text {
                label: "text".to_owned(),
                optional: false,
            }),
            echo(),
        )
        .expect("registered");

    let message = registry
        .execute("echo", CommandArguments::Text("hi".to_owned()))
        .await
        .expect("ran");

    assert_eq!(message, "text:hi");
}

#[tokio::test]
async fn an_unknown_command_is_named_in_the_error() {
    let registry = CommandRegistry::new();
    let error = registry
        .execute("nope", CommandArguments::None)
        .await
        .expect_err("unknown");

    let CommandError::Unknown { name } = error else {
        panic!("expected Unknown");
    };
    assert_eq!(name, "nope");
}

#[tokio::test]
async fn the_registry_validates_the_argument_kind() {
    // Checked here rather than inside the handler, so every command validates the same way.
    let registry = CommandRegistry::new();
    let _disposer = registry
        .register(
            command("rename").asking_for(CommandInput::Text {
                label: "title".to_owned(),
                optional: false,
            }),
            echo(),
        )
        .expect("registered");

    let error = registry
        .execute("rename", CommandArguments::None)
        .await
        .expect_err("wrong kind");

    let CommandError::WrongArguments { expected, got, .. } = error else {
        panic!("expected WrongArguments");
    };
    assert_eq!(expected, "text");
    assert_eq!(got, "no arguments");
}

#[tokio::test]
async fn a_command_asking_for_nothing_accepts_no_arguments() {
    let registry = CommandRegistry::new();
    let _ = registry
        .register(command("clear"), echo())
        .expect("registered");

    assert_eq!(
        registry
            .execute("clear", CommandArguments::None)
            .await
            .expect("ran"),
        "none"
    );
}

#[tokio::test]
async fn a_failing_command_is_distinct_from_a_wrong_argument() {
    // The request was well-formed and the handler rejected it, which a caller reports differently.
    let registry = CommandRegistry::new();
    let handler: CommandHandler =
        Arc::new(|_arguments| Box::pin(async { Err("the plugin is not loaded".to_owned()) }));
    let _ = registry
        .register(command("deploy"), handler)
        .expect("registered");

    let error = registry
        .execute("deploy", CommandArguments::None)
        .await
        .expect_err("failed");

    let CommandError::Failed { message, .. } = error else {
        panic!("expected Failed, got {error:?}");
    };
    assert_eq!(message, "the plugin is not loaded");
}

#[tokio::test]
async fn a_command_can_be_run_by_its_slashed_name() {
    // A client sends what the user typed, which includes the slash.
    let registry = CommandRegistry::new();
    let _ = registry
        .register(command("clear"), echo())
        .expect("registered");

    assert_eq!(
        registry
            .execute("/clear", CommandArguments::None)
            .await
            .expect("ran"),
        "none"
    );
}

#[test]
fn the_debug_form_lists_names_not_handlers() {
    let registry = CommandRegistry::new();
    let _ = registry
        .register(command("alpha"), echo())
        .expect("registered");

    let text = format!("{registry:?}");
    assert!(text.contains("alpha"));
    assert!(text.contains("Registry"));
}
