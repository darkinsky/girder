//! Human commands: the slash-command surface a plugin registers.
//!
//! A command is not a tool. A tool is something a *model* calls; a command is something a *person* types.
//! The two differ in who acts, what the arguments mean, and what a result is for — a tool result goes back
//! into the conversation, a command result goes to the screen.
//!
//! # Why a command declares its input
//!
//! A slash command with free-form arguments would need the UI to guess what to show and the server to
//! guess what it received. Declaring an input shape lets a client render the right control before the
//! user types, so `/rename` asks for one line and `/attach` asks for a file.
//!
//! # Why execution carries an id
//!
//! A command can take time — an attachment upload, a network call. The id lets a client correlate a
//! result with the request it made, which is what a UI needs to put the answer beside the right prompt
//! when two commands are in flight.
//!
//! # Why a command is scoped
//!
//! Plugins register commands, and a plugin that unloads must take its commands with it. A command table
//! without ownership would leave a `/deploy` that resolves to a handler whose plugin is gone.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::scope::{BoxDisposer, BoxFut};

/// Identifies one command definition.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DefinitionId(String);

impl DefinitionId {
    /// Builds an id.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty() {
            return None;
        }
        Some(Self(value))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DefinitionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifies one execution.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId(String);

impl CommandId {
    /// Builds an id.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty() {
            return None;
        }
        Some(Self(value))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CommandId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a command asks the user for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    /// Nothing beyond the command name.
    None,
    /// One line of text.
    Text {
        /// What to ask for.
        label: String,
        /// Whether an empty answer is allowed.
        optional: bool,
    },
    /// One or more files.
    Files {
        /// What to ask for.
        label: String,
    },
    /// A choice from a fixed set.
    Choice {
        /// What to ask for.
        label: String,
        /// The options, in display order.
        options: Vec<String>,
    },
}

impl Input {
    /// What to call this input.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Text { label, .. } | Self::Files { label } | Self::Choice { label, .. } => {
                Some(label)
            }
        }
    }
}

/// One registered command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    /// Its id.
    pub id: DefinitionId,
    /// The name a user types, without the leading slash.
    pub name: String,
    /// What it does, for a help listing.
    pub description: String,
    /// What it asks for.
    pub input: Input,
}

impl Definition {
    /// Builds a definition.
    ///
    /// # Errors
    ///
    /// Returns `None` for a blank name or description: a command with no name cannot be typed, and one
    /// with no description cannot be chosen from a list.
    #[must_use]
    pub fn new(
        id: DefinitionId,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Option<Self> {
        let name = name.into();
        let description = description.into();
        if name.trim().is_empty() || description.trim().is_empty() {
            return None;
        }
        // A leading slash is dropped rather than accepted: the name is what follows it, and two spellings
        // of one command would be two entries.
        let name = name.trim().trim_start_matches('/').to_owned();
        if name.is_empty() || name.contains(char::is_whitespace) {
            return None;
        }
        Some(Self {
            id,
            name,
            description,
            input: Input::None,
        })
    }

    /// Declares what it asks for.
    #[must_use]
    pub fn asking_for(mut self, input: Input) -> Self {
        self.input = input;
        self
    }
}

/// What a command received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arguments {
    /// Nothing.
    None,
    /// One line of text.
    Text(String),
    /// File references.
    Files(Vec<String>),
    /// One chosen option.
    Choice(String),
}

/// The outcome of one execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandResult {
    /// Which execution this answers.
    pub id: CommandId,
    /// What to show.
    pub message: String,
    /// Whether it failed.
    pub is_error: bool,
}

impl CommandResult {
    /// A successful result.
    #[must_use]
    pub fn ok(id: CommandId, message: impl Into<String>) -> Self {
        Self {
            id,
            message: message.into(),
            is_error: false,
        }
    }

    /// A failed result.
    #[must_use]
    pub fn error(id: CommandId, message: impl Into<String>) -> Self {
        Self {
            id,
            message: message.into(),
            is_error: true,
        }
    }
}

/// Why a command operation failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CommandError {
    /// No command has that name.
    #[error("no command `/{name}`")]
    Unknown {
        /// The name.
        name: String,
    },

    /// The name is taken.
    #[error("a command `/{name}` is already registered")]
    Duplicate {
        /// The name.
        name: String,
    },

    /// The arguments did not match what the command asks for.
    #[error("`/{name}` expects {expected}, got {got}")]
    WrongArguments {
        /// The name.
        name: String,
        /// What it asks for.
        expected: &'static str,
        /// What it received.
        got: &'static str,
    },

    /// The command ran and failed.
    ///
    /// Distinct from a bad argument: the request was well-formed and the handler rejected it, which a
    /// caller reports differently.
    #[error("`/{name}` failed: {message}")]
    Failed {
        /// The name.
        name: String,
        /// What the handler said.
        message: String,
    },
}

/// A command's handler.
pub type Handler = Arc<dyn Fn(Arguments) -> BoxFut<Result<String, String>> + Send + Sync>;

/// One registered command and its handler.
struct Entry {
    definition: Definition,
    handler: Handler,
}

/// The command registry.
///
/// The table lives behind an `Arc` so a disposer can hold a weak reference to it: a disposer that held the
/// registry strongly would keep it alive for the process's life, which is the opposite of what unlinking a
/// command should do.
#[derive(Default, Clone)]
pub struct Registry {
    commands: Arc<std::sync::Mutex<BTreeMap<String, Entry>>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("commands", &self.names())
            .finish()
    }
}

impl Registry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a command.
    ///
    /// The returned disposer removes it, so a plugin that unloads takes its commands with it. A table
    /// without ownership would leave a name resolving to a handler whose plugin is gone.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError::Duplicate`] when the name is taken.
    pub fn register(
        &self,
        definition: Definition,
        handler: Handler,
    ) -> Result<BoxDisposer, CommandError> {
        let name = definition.name.clone();
        let mut commands = self.lock();
        if commands.contains_key(&name) {
            return Err(CommandError::Duplicate { name });
        }
        commands.insert(
            name.clone(),
            Entry {
                definition,
                handler,
            },
        );

        // The disposer holds a weak reference: holding the table strongly would keep it alive for the
        // process's life, which is the opposite of what unlinking a command should do.
        let weak = Arc::downgrade(&self.commands);
        Ok(Box::new(move || {
            if let Some(commands) = weak.upgrade() {
                commands
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&name);
            }
            // The removal is synchronous and its outcome is not a failure: a command already gone is the
            // state the caller asked for.
            Box::pin(async {})
        }))
    }

    /// Every registered command, sorted by name.
    ///
    /// Sorted because a help listing is rendered from it, and an order that changed between runs would
    /// make the list jump.
    #[must_use]
    pub fn definitions(&self) -> Vec<Definition> {
        self.lock()
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    /// The registered names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// One definition.
    #[must_use]
    pub fn definition(&self, name: &str) -> Option<Definition> {
        self.lock()
            .get(name.trim_start_matches('/'))
            .map(|entry| entry.definition.clone())
    }

    /// Runs a command.
    ///
    /// # Errors
    ///
    /// Returns [`CommandError::Unknown`] for a name nothing registered, and
    /// [`CommandError::WrongArguments`] when the arguments do not match the declared input — checked here
    /// rather than inside the handler, so every command validates the same way.
    pub async fn execute(&self, name: &str, arguments: Arguments) -> Result<String, CommandError> {
        let name = name.trim_start_matches('/').to_owned();
        let (handler, input) = {
            let commands = self.lock();
            let entry = commands
                .get(&name)
                .ok_or_else(|| CommandError::Unknown { name: name.clone() })?;
            (Arc::clone(&entry.handler), entry.definition.input.clone())
        };

        // The argument kind must match the declared input, or a handler would receive a shape it cannot
        // read and fail in its own way rather than in one place.
        let (expected, got) = (kind_of(&input), kind_of_arguments(&arguments));
        if expected != got {
            return Err(CommandError::WrongArguments {
                name,
                expected,
                got,
            });
        }

        handler(arguments)
            .await
            .map_err(|message| CommandError::Failed { name, message })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Entry>> {
        self.commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The name of an input kind, for an error message.
fn kind_of(input: &Input) -> &'static str {
    match input {
        Input::None => "no arguments",
        Input::Text { .. } => "text",
        Input::Files { .. } => "files",
        Input::Choice { .. } => "a choice",
    }
}

/// The name of an argument kind, for an error message.
fn kind_of_arguments(arguments: &Arguments) -> &'static str {
    match arguments {
        Arguments::None => "no arguments",
        Arguments::Text(_) => "text",
        Arguments::Files(_) => "files",
        Arguments::Choice(_) => "a choice",
    }
}
