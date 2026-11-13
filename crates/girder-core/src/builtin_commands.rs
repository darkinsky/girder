//! The commands a session can be given without a deployment writing any.
//!
//! `command.list` and `command.run` were exposed and the registry they read was empty -- and worse, created inline at
//! the registration site so nothing else could ever add to it. A client was offered a command surface with nothing
//! behind it.
//!
//! # Why these three
//!
//! A command that does nothing useful costs a place in a menu and a line of help text. `clear`, `help`, and `version`
//! are the three a client cannot answer for itself: the first describes what discarding means, the second describes the
//! server's own catalogue, and the third reports which build is answering. Anything else belongs to a deployment.

use std::sync::Arc;

use crate::commands::{Arguments, Definition, DefinitionId, Handler, Input, Registry};

/// Registers the built-in commands and returns how many were added.
///
/// A count rather than `()` so a caller can report what it offers and a test can assert the list is not silently empty.
pub fn register_builtin_commands(registry: &Arc<Registry>) -> usize {
    let mut added = 0;

    if add(
        registry,
        "builtin-clear",
        "clear",
        "Explain what discarding a session's visible context does",
        clear_handler(),
    ) {
        added += 1;
    }
    if add(
        registry,
        "builtin-help",
        "help",
        "List every command this build offers",
        help_handler(Arc::clone(registry)),
    ) {
        added += 1;
    }
    if add(
        registry,
        "builtin-version",
        "version",
        "Report which build is answering",
        version_handler(),
    ) {
        added += 1;
    }

    added
}

/// Declares and registers one command.
///
/// Both constructors return `Option` and both can only fail on a blank name, which is a programming error here rather
/// than a runtime one. Chaining them once keeps the three registrations readable and lets the caller count successes.
fn add(
    registry: &Arc<Registry>,
    id: &str,
    name: &str,
    description: &str,
    handler: Handler,
) -> bool {
    let Some(definition_id) = DefinitionId::new(id) else {
        return false;
    };
    let Some(definition) = Definition::new(definition_id, name, description) else {
        return false;
    };
    registry
        .register(definition.asking_for(Input::None), handler)
        .is_ok()
}

/// What `clear` answers.
///
/// It reports rather than clears: this module has no session, and a command that claimed to have discarded something it
/// could not reach would be worse than one that says what discarding means. The session log is append-only by design,
/// so there is nothing here to delete.
fn clear_handler() -> Handler {
    Arc::new(|_arguments: Arguments| {
        Box::pin(async {
            Ok("the log is append-only; clear hides prior messages in the client's view and writes nothing"
                .to_owned())
        })
    })
}

/// What `help` answers: the catalogue, read from the registry rather than from a list written here.
///
/// Reading the registry is what keeps the answer true. A hand-written list describes the commands somebody remembered
/// on the day they wrote it.
fn help_handler(registry: Arc<Registry>) -> Handler {
    Arc::new(move |_arguments: Arguments| {
        let registry = Arc::clone(&registry);
        Box::pin(async move {
            let mut names: Vec<String> = registry
                .definitions()
                .into_iter()
                .map(|definition| format!("/{} -- {}", definition.name, definition.description))
                .collect();
            names.sort();
            if names.is_empty() {
                return Ok("no commands are registered".to_owned());
            }
            Ok(names.join("\n"))
        })
    })
}

/// What `version` answers.
fn version_handler() -> Handler {
    Arc::new(|_arguments: Arguments| {
        Box::pin(async { Ok(format!("girder {}", env!("CARGO_PKG_VERSION"))) })
    })
}
