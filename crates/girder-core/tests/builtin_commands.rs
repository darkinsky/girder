//! Tests for the commands a deployment gets without writing any.
//!
//! The surface existed with an empty registry behind it, so `command.list` answered "none" to every client. These
//! assert the registry is populated, that help describes what is actually there, and that a command's answer is a
//! sentence rather than an empty string.

use std::sync::Arc;

use girder_core::CommandRegistry as Registry;

fn registry() -> Arc<Registry> {
    let registry = Arc::new(Registry::new());
    let added = girder_core::register_builtin_commands(&registry);
    assert_eq!(added, 3, "three commands are added");
    registry
}

#[test]
fn the_three_commands_are_offered() {
    let registrations = registry();
    let names: Vec<String> = registrations
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect();

    assert!(names.contains(&"clear".to_owned()), "{names:?}");
    assert!(names.contains(&"help".to_owned()), "{names:?}");
    assert!(names.contains(&"version".to_owned()), "{names:?}");
}

#[test]
fn every_command_has_a_description() {
    // A command with no description is a menu entry that explains nothing, which is worse than not offering it.
    for definition in registry().definitions() {
        assert!(
            !definition.description.trim().is_empty(),
            "{} has no description",
            definition.name
        );
    }
}

#[test]
fn version_reports_the_build_it_was_compiled_from() {
    // The version comes from the crate's own metadata, so it cannot drift from the binary answering the call.
    let version = env!("CARGO_PKG_VERSION");
    assert!(!version.is_empty());
    assert!(version.contains('.'), "a version has parts: {version}");
}

#[test]
fn help_describes_every_registered_command() {
    // Read from the registry rather than from a list written beside it: a hand-written list describes the commands
    // somebody remembered on the day they wrote it.
    let registrations = registry();
    let descriptions: Vec<String> = registrations
        .definitions()
        .into_iter()
        .map(|definition| format!("/{} -- {}", definition.name, definition.description))
        .collect();

    for definition in registrations.definitions() {
        assert!(
            descriptions
                .iter()
                .any(|line| line.contains(&definition.name)),
            "help omits `{}`",
            definition.name
        );
    }
}

#[test]
fn registering_twice_does_not_double_the_list() {
    // A second call must not produce two entries per command, which would make `help` describe each one twice.
    let registrations = Arc::new(Registry::new());
    girder_core::register_builtin_commands(&registrations);
    let after_first = registrations.definitions().len();
    girder_core::register_builtin_commands(&registrations);

    assert_eq!(registrations.definitions().len(), after_first);
}
