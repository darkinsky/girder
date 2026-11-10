//! Language servers: the four semantic queries a model asks about code.
//!
//! The protocol has dozens of operations. A model needs four: where a name is defined, where it is used,
//! what implements it, and what it means. Exposing the rest would give the model an escape hatch into raw
//! JSON-RPC, which is unreadable to it and unreviewable by us.
//!
//! # Why positions are zero-based UTF-16
//!
//! That is the protocol's convention, and converting at the seam would mean every provider converts back.
//! The model-facing tool owns the one-based convention a person reads, and the conversion happens once, at
//! that boundary.
//!
//! # Why the language is not in the request
//!
//! A language server is registered for one language. A request that named it too would let a caller ask a
//! Rust server about a Python file, and the provider would either refuse or answer nonsense — both worse than
//! a request that cannot express the mistake.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::scope::BoxFut;

/// A zero-based UTF-16 cursor coordinate, matching the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    /// Zero-based line.
    pub line: u32,
    /// Zero-based UTF-16 code-unit offset within the line.
    ///
    /// UTF-16 rather than bytes: the protocol counts code units, and a multi-byte character occupies two.
    /// Counting bytes would put every position after a non-ASCII character in the wrong column.
    pub character: u32,
}

impl Position {
    /// Builds a position.
    #[must_use]
    pub fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// A zero-based half-open range `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    /// Where it begins, inclusive.
    pub start: Position,
    /// Where it ends, exclusive.
    pub end: Position,
}

impl Range {
    /// Builds a range.
    #[must_use]
    pub fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// Whether the range is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// The four semantic queries.
///
/// A closed set: adding one is a change across the seam, every provider, and the tool. Symbols and call
/// hierarchy are deliberately absent — they need different result shapes, and a union that tried to hold
/// both would make every consumer match cases it cannot use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Where a name is defined.
    Definition,
    /// Where a name is used.
    References,
    /// What implements a trait or interface member.
    Implementation,
    /// What a name means.
    Hover,
}

impl Operation {
    /// The protocol's method name.
    #[must_use]
    pub fn method(self) -> &'static str {
        match self {
            Self::Definition => "textDocument/definition",
            Self::References => "textDocument/references",
            Self::Implementation => "textDocument/implementation",
            Self::Hover => "textDocument/hover",
        }
    }

    /// The name a client shows.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "goToDefinition",
            Self::References => "findReferences",
            Self::Implementation => "goToImplementation",
            Self::Hover => "hover",
        }
    }

    /// Whether the operation returns locations rather than text.
    ///
    /// `Hover` is the odd one: it returns prose, and a consumer that expected locations would render an
    /// empty list for an answer it actually received.
    #[must_use]
    pub fn returns_locations(self) -> bool {
        !matches!(self, Self::Hover)
    }
}

/// One query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Which query.
    pub operation: Operation,
    /// The workspace it applies to.
    ///
    /// Caller-supplied rather than derived from the file: a file may be outside every registered workspace,
    /// and a server started at the wrong root answers about a different project.
    pub workspace_root: std::path::PathBuf,
    /// The file.
    pub path: std::path::PathBuf,
    /// Where in it.
    pub position: Position,
}

impl Request {
    /// Builds a request.
    #[must_use]
    pub fn new(
        operation: Operation,
        workspace_root: impl Into<std::path::PathBuf>,
        path: impl Into<std::path::PathBuf>,
        position: Position,
    ) -> Self {
        Self {
            operation,
            workspace_root: workspace_root.into(),
            path: path.into(),
            position,
        }
    }
}

/// One location an answer names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// The file.
    pub path: std::path::PathBuf,
    /// The range within it.
    pub range: Range,
}

/// What a query answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Locations, for the three navigation queries.
    Locations(Vec<Location>),
    /// Prose, for `hover`.
    Text(String),
    /// The server had nothing to say.
    ///
    /// Distinct from an empty location list: "no definition exists" and "I could not determine one" are
    /// different facts, and a model that treated them alike would stop looking too early.
    Nothing,
}

impl Answer {
    /// The locations, when this is a location answer.
    #[must_use]
    pub fn locations(&self) -> &[Location] {
        match self {
            Self::Locations(locations) => locations,
            Self::Text(_) | Self::Nothing => &[],
        }
    }
}

/// Why a query failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LspError {
    /// No server serves that file's language.
    #[error("no language server serves `{path}`")]
    NoProvider {
        /// The file.
        path: String,
    },

    /// The server is not running.
    #[error("language server `{name}` is not running")]
    NotRunning {
        /// The server.
        name: String,
    },

    /// The protocol exchange failed.
    #[error("language server `{name}` failed: {reason}")]
    Protocol {
        /// The server.
        name: String,
        /// Why.
        reason: String,
    },

    /// The server answered something this seam cannot represent.
    #[error("language server `{name}` answered an unexpected shape")]
    UnexpectedAnswer {
        /// The server.
        name: String,
    },
}

/// A language server.
///
/// One per language: a request that named a language would let a caller ask a Rust server about a Python
/// file, and the provider would either refuse or answer nonsense.
pub trait Provider: Send + Sync {
    /// Its name, for an error message.
    fn name(&self) -> &str;

    /// The file extensions it serves, without the dot.
    fn extensions(&self) -> &[String];

    /// Whether it can serve a path.
    fn serves(&self, path: &std::path::Path) -> bool {
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            return false;
        };
        self.extensions()
            .iter()
            .any(|served| served.eq_ignore_ascii_case(extension))
    }

    /// Runs one query.
    fn query(&self, request: Request) -> BoxFut<Result<Answer, LspError>>;
}

/// The language servers this deployment knows about.
#[derive(Default)]
pub struct Registry {
    providers: std::sync::Mutex<Vec<Arc<dyn Provider>>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("providers", &self.names())
            .finish()
    }
}

impl Registry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a server.
    ///
    /// # Errors
    ///
    /// Returns the name when it is taken. Two servers under one name would make an error message name a
    /// server that is not the one that answered.
    pub fn register(&self, provider: Arc<dyn Provider>) -> Result<(), String> {
        let name = provider.name().to_owned();
        let mut providers = self.lock();
        if providers.iter().any(|existing| existing.name() == name) {
            return Err(name);
        }
        providers.push(provider);
        // Sorted by name so two runs of one query resolve to the same server.
        providers.sort_by_key(|provider| provider.name().to_owned());
        Ok(())
    }

    /// The registered server names, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.lock()
            .iter()
            .map(|provider| provider.name().to_owned())
            .collect()
    }

    /// Runs one query through the first server that serves the file.
    ///
    /// # Errors
    ///
    /// Returns [`LspError::NoProvider`] when none serves the extension.
    pub async fn query(&self, request: Request) -> Result<Answer, LspError> {
        let provider = {
            let providers = self.lock();
            providers
                .iter()
                .find(|provider| provider.serves(&request.path))
                .cloned()
                .ok_or_else(|| LspError::NoProvider {
                    path: request.path.display().to_string(),
                })?
        };
        provider.query(request).await
    }

    /// The extensions this deployment can serve.
    #[must_use]
    pub fn extensions(&self) -> Vec<String> {
        let providers = self.lock();
        let mut extensions: Vec<String> = providers
            .iter()
            .flat_map(|provider| provider.extensions().iter().cloned())
            .collect();
        extensions.sort();
        extensions.dedup();
        extensions
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<dyn Provider>>> {
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A server that answers from a fixed map, for tests and for a build with no language servers.
pub struct Static {
    name: String,
    extensions: Vec<String>,
    answers: BTreeMap<(String, u32, u32), Answer>,
}

impl Static {
    /// Creates a server for the given extensions.
    #[must_use]
    pub fn new(name: impl Into<String>, extensions: Vec<String>) -> Self {
        Self {
            name: name.into(),
            extensions,
            answers: BTreeMap::new(),
        }
    }

    /// Records an answer for one position.
    pub fn answer(&mut self, line: u32, character: u32, answer: Answer) {
        self.answers
            .insert((String::new(), line, character), answer);
    }
}

impl Provider for Static {
    fn name(&self) -> &str {
        &self.name
    }

    fn extensions(&self) -> &[String] {
        &self.extensions
    }

    fn query(&self, request: Request) -> BoxFut<Result<Answer, LspError>> {
        let key = (
            String::new(),
            request.position.line,
            request.position.character,
        );
        let answer = self.answers.get(&key).cloned().unwrap_or(Answer::Nothing);
        Box::pin(async move { Ok(answer) })
    }
}

/// Converts a zero-based UTF-16 column to a byte offset in `line`.
///
/// The protocol counts UTF-16 code units; Rust strings are UTF-8. A character outside the BMP occupies two
/// UTF-16 units and four UTF-8 bytes, so a naive conversion puts every position after one in the wrong place.
#[must_use]
pub fn utf16_column_to_byte(line: &str, column: u32) -> Option<usize> {
    let mut units = 0u32;
    for (byte, character) in line.char_indices() {
        if units == column {
            return Some(byte);
        }
        // A character is at most two UTF-16 units, so the narrowing cannot truncate.
        units += u32::try_from(character.len_utf16()).unwrap_or(2);
        if units > column {
            // The column lands inside a surrogate pair, which no valid position does.
            return None;
        }
    }
    (units == column).then_some(line.len())
}
