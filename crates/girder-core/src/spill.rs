//! Spill: sending oversized text to storage so a model gets a locator instead of a wall of output.
//!
//! A tool that produced a megabyte has to give the model something. It cannot be the megabyte — that fills a
//! context window with material the model cannot act on. It cannot be nothing — then the model knows the tool ran
//! and not what it said.
//!
//! A spill is the third option: the text is saved, and the model receives a locator plus guidance on how to read
//! it. The model can then ask for the part it needs.
//!
//! # What this owns and what it does not
//!
//! It owns saving text and returning a locator. It does **not** own the retention policy (how much a tool result
//! may carry before spilling — that belongs to the retention layer), nor the decision to replace a tool result
//! (that is the tool's), nor retrieval (a spill is read through the filesystem, and a search API would be a
//! different capability).
//!
//! Keeping those out is what makes this usable in two places with different policies: a shell tool and a web fetch
//! have different thresholds and the storage does not care.
//!
//! # Why the locator is model-facing text
//!
//! The model reads the locator, so it has to be something the model can use: a path it can pass to a read tool,
//! with a hint about how to read it usefully. A struct with fields would be more precise for a program and useless
//! to the model, which is the reader.

use std::path::{Path, PathBuf};

use crate::scope::BoxFut;

/// A saved spill.
///
/// The locator is what the model receives; the id is what a caller can use to find it again without parsing the
/// path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    /// An opaque id for this spill.
    pub id: String,
    /// The path a read tool can open.
    pub path: PathBuf,
    /// How many bytes were saved.
    pub bytes: u64,
    /// A hint about how to read it usefully.
    ///
    /// Written for the model, not for a program: it names the path's shape and suggests the read that answers the
    /// question the tool was asked, rather than the read that reproduces the file.
    pub hint: String,
}

impl Reference {
    /// The text a model receives in place of the spilled content.
    ///
    /// One block rather than several fields: the model reads prose, and a structured message would be rendered by
    /// whoever happened to hold it.
    #[must_use]
    pub fn to_text(&self) -> String {
        format!(
            "The output was too large to return in full. It was saved to `{}` ({} bytes).\n{}",
            self.path.display(),
            self.bytes,
            self.hint
        )
    }
}

/// What a caller asks to save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveRequest {
    /// The text.
    pub text: String,
    /// What produced it, for a file name a person can recognise.
    ///
    /// A label rather than a path: the store owns where files go, and a caller that supplied a path could write
    /// outside it.
    pub label: String,
}

impl SaveRequest {
    /// Builds a request.
    ///
    /// # Errors
    ///
    /// Returns `None` for blank text: saving nothing would produce a locator that names an empty file, and the
    /// model would read it looking for the answer.
    #[must_use]
    pub fn new(text: impl Into<String>, label: impl Into<String>) -> Option<Self> {
        let text = text.into();
        if text.trim().is_empty() {
            return None;
        }
        let label = label.into();
        // A label becomes a file name, so it is reduced to characters that cannot escape a directory.
        let label: String = label
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        let label = if label.trim_matches('-').is_empty() {
            "output".to_owned()
        } else {
            label
        };
        Some(Self { text, label })
    }
}

/// Why a spill failed.
#[derive(Debug, thiserror::Error)]
pub enum SpillError {
    /// The text could not be written.
    #[error("cannot save spill: {0}")]
    Write(String),

    /// No store is mounted.
    ///
    /// Distinct from a write failure: a deployment without a store cannot spill at all, and a tool that needed to
    /// should say so rather than truncate silently.
    #[error("this deployment has no spill store")]
    Unavailable,
}

/// The seam a deployment implements to store spills.
///
/// Async so a remote store is possible; the local one does not need it, but a trait that assumed a local
/// filesystem would force a deployment with a remote one to change every caller.
pub trait Store: Send + Sync {
    /// Saves text and returns a locator.
    fn save(&self, request: SaveRequest) -> BoxFut<Result<Reference, SpillError>>;
}

/// A store on the local filesystem.
pub struct LocalStore {
    root: PathBuf,
    counter: std::sync::atomic::AtomicU64,
}

impl LocalStore {
    /// Creates a store rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The root it writes under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Store for LocalStore {
    fn save(&self, request: SaveRequest) -> BoxFut<Result<Reference, SpillError>> {
        let root = self.root.clone();
        let sequence = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = crate::schedule::now_millis();
        Box::pin(async move {
            if let Err(error) = tokio::fs::create_dir_all(&root).await {
                return Err(SpillError::Write(error.to_string()));
            }
            let name = format!("{now}-{sequence}-{}.txt", request.label);
            let path = root.join(name);
            // Written atomically: a spill file that a reader opened mid-write would show a prefix of the output,
            // which is exactly the confusion the spill exists to avoid.
            if let Err(error) = crate::atomic_write::write_text_atomic(
                &path,
                &request.text,
                crate::atomic_write::Mode::PRIVATE,
            ) {
                return Err(SpillError::Write(error.to_string()));
            }
            let bytes = u64::try_from(request.text.len()).unwrap_or(u64::MAX);
            Ok(Reference {
                id: format!("{now}-{sequence}"),
                path: path.clone(),
                bytes,
                hint: format!(
                    "Read it with a range or a search rather than in full; it is {bytes} bytes."
                ),
            })
        })
    }
}

/// A store that refuses, for a deployment with no spill storage.
#[derive(Debug, Default)]
pub struct Unavailable;

impl Store for Unavailable {
    fn save(&self, _request: SaveRequest) -> BoxFut<Result<Reference, SpillError>> {
        Box::pin(async { Err(SpillError::Unavailable) })
    }
}

/// How many bytes a tool result may carry before it is spilled.
///
/// 32 KiB: comfortably more than a useful diagnostic and far less than a context window, so a chatty command is
/// replaced by a locator rather than filling the conversation.
pub const DEFAULT_SPILL_LIMIT: usize = 32 * 1024;

/// Whether a text is large enough that spilling is better than returning it.
///
/// A threshold rather than a policy: the caller decides the number, and the comparison lives here so two tools
/// with one threshold cannot disagree about whether a text of exactly that size spills.
#[must_use]
pub fn should_spill(text: &str, limit: usize) -> bool {
    text.len() > limit
}
