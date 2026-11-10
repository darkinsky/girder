//! Deliverables: files a run produced that a person should open.
//!
//! A tool can write a file, but writing one is not the same as telling a person it exists. A deliverable is the
//! announcement: this file matters, here is what it is, and here is how to open it.
//!
//! # Why an announcement rather than a directory
//!
//! A directory of outputs is not a deliverable set. A build writes hundreds of files and one of them is the
//! report; the announcement is what distinguishes it, and it is the agent's judgement rather than a scan's.
//!
//! # Why the kind is declared
//!
//! A person opening a spreadsheet and a person opening a slide deck want different applications. Declaring the kind
//! lets a host offer the right action, and a consumer that guessed from the extension would be wrong for every
//! `.json` that is meant to be read as text.
//!
//! # Why a deliverable may be absent
//!
//! The file can be deleted between the announcement and the click. A deliverable that reported only its path would
//! make the click produce a filesystem error; recording whether it still exists lets a host show it as gone.

use std::path::PathBuf;

/// What a deliverable is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A document someone reads.
    Document,
    /// A spreadsheet.
    Spreadsheet,
    /// Slides.
    Deck,
    /// An image.
    Image,
    /// A file meant to be run or opened by another tool.
    Artefact,
    /// Something else.
    Other,
}

impl Kind {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Spreadsheet => "spreadsheet",
            Self::Deck => "deck",
            Self::Image => "image",
            Self::Artefact => "artefact",
            Self::Other => "other",
        }
    }

    /// The kind a path most likely is, when nothing was declared.
    ///
    /// A fallback, not a substitute for declaring: every `.json` is not a document, and a consumer that relied on
    /// this would be wrong for the ones meant to be read as data.
    #[must_use]
    pub fn from_path(path: &std::path::Path) -> Self {
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            return Self::Other;
        };
        match extension.to_ascii_lowercase().as_str() {
            "md" | "txt" | "pdf" | "doc" | "docx" | "rtf" | "html" => Self::Document,
            "csv" | "tsv" | "xls" | "xlsx" | "ods" => Self::Spreadsheet,
            "ppt" | "pptx" | "odp" | "key" => Self::Deck,
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" => Self::Image,
            "wasm" | "so" | "dylib" | "exe" | "zip" | "tar" | "gz" => Self::Artefact,
            _ => Self::Other,
        }
    }
}

/// One announced file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deliverable {
    /// Where it is.
    pub path: PathBuf,
    /// What to call it in a list.
    pub title: String,
    /// What it is.
    pub kind: Kind,
    /// One line about what it contains.
    pub description: Option<String>,
    /// How many bytes it was when announced.
    ///
    /// Recorded so a host can show a size without a stat, and so a file that changed since is detectable rather
    /// than silently offered as the thing that was announced.
    pub bytes: Option<u64>,
}

impl Deliverable {
    /// Builds a deliverable.
    ///
    /// # Errors
    ///
    /// Returns `None` for a blank title: an entry with no title cannot be chosen from a list.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, title: impl Into<String>, kind: Kind) -> Option<Self> {
        let title = title.into();
        if title.trim().is_empty() {
            return None;
        }
        Some(Self {
            path: path.into(),
            title,
            kind,
            description: None,
            bytes: None,
        })
    }

    /// Adds a description.
    #[must_use]
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Records the size.
    #[must_use]
    pub fn with_size(mut self, bytes: u64) -> Self {
        self.bytes = Some(bytes);
        self
    }

    /// Whether the file is still there.
    ///
    /// Checked when a host renders the list rather than when it was announced: the file can be deleted in between,
    /// and a click on a missing file should be a state rather than a filesystem error.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.path.is_file()
    }
}

/// The deliverables one run announced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Set {
    entries: Vec<Deliverable>,
}

impl Set {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Announces one.
    ///
    /// A path already announced is left as it was: a tool that wrote the same file twice announced it once, and a
    /// list that showed it twice would look like two outputs.
    pub fn add(&mut self, deliverable: Deliverable) {
        if self
            .entries
            .iter()
            .any(|existing| existing.path == deliverable.path)
        {
            return;
        }
        self.entries.push(deliverable);
    }

    /// The entries, in announcement order.
    ///
    /// Order is preserved rather than sorted: the agent announced them in the order it judged useful, and a sort
    /// would put the report after the raw data.
    #[must_use]
    pub fn entries(&self) -> &[Deliverable] {
        &self.entries
    }

    /// How many were announced.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing was announced.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entries whose files still exist.
    #[must_use]
    pub fn present(&self) -> Vec<&Deliverable> {
        self.entries.iter().filter(|entry| entry.exists()).collect()
    }

    /// The entries whose files are gone.
    #[must_use]
    pub fn missing(&self) -> Vec<&Deliverable> {
        self.entries
            .iter()
            .filter(|entry| !entry.exists())
            .collect()
    }
}
