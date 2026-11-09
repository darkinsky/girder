//! Workspace instruction files: discovery, rendering, and the omission rules.
//!
//! Upstream's `agent-instructions` loads `AGENTS.md`-compatible files from the workspace and from two
//! global directories, then renders them into one block the model reads. The interesting part is not
//! the reading — it is the rules about what happens when there are too many, or too large, files.
//!
//! # Discovery walks up
//!
//! Instructions apply to everything below them, so a file in the workspace root governs the whole
//! tree and one in `src/` governs `src/`. Discovery therefore walks from the working directory to
//! the filesystem root, collecting every `AGENTS.md` on the way. The nearest file is not the most
//! important one — all of them apply, in order.
//!
//! # Omission is not failure
//!
//! A workspace can have more instruction text than a context window should carry. Upstream records
//! three distinct outcomes:
//!
//! | outcome | meaning |
//! |---|---|
//! | rendered | included in the block |
//! | truncated | included, cut at the byte budget, with the cut recorded |
//! | omitted | not included at all, with the file named so the model knows it exists |
//!
//! The third is the one worth being careful about: silently dropping a file the user wrote leaves
//! the model acting without instructions it was supposed to follow. Naming it is the difference
//! between "the model knows there is a file it cannot see" and "the model does not know".
//!
//! # Why a digest
//!
//! Upstream keeps a digest of the *trimmed* content, so a whitespace-only edit does not re-inject the
//! whole file into the conversation. That matters because instruction files are re-checked after every
//! filesystem tool call, and a re-injection per touch would fill the context with copies.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::scope::BoxFut;

/// The file name searched for in each directory.
pub const INSTRUCTION_FILE_NAME: &str = "AGENTS.md";

/// Where global instruction directories live, relative to a home.
pub const USER_GLOBAL_DIRECTORY: &str = ".agents";

/// The harness-global directory, relative to a home.
pub const HARNESS_GLOBAL_DIRECTORY: &str = ".girder";

/// The default per-file byte budget.
///
/// 32 KiB is far above any real `AGENTS.md` and far below a context window. A file over it is
/// truncated rather than omitted: the beginning of a long instruction file is usually the part that
/// states the rules.
pub const DEFAULT_MAX_BYTES_PER_FILE: u64 = 32 * 1024;

/// The default total byte budget for the rendered block.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 128 * 1024;

/// The smallest slice worth rendering instead of omitting.
///
/// Below this a "truncated" file holds a fragment that states no usable rule, and showing it as
/// content would tell the model it had been instructed when it had not. 256 bytes is roughly one
/// paragraph — the smallest thing that can carry a rule.
pub const MIN_USEFUL_SLICE: u64 = 256;

/// One instruction file, as identified before it is read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstructionFile {
    /// Absolute path, for reading.
    pub absolute_path: PathBuf,
    /// The path a model sees.
    pub display_path: String,
}

/// One instruction file that was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedInstruction {
    /// Its identity.
    pub file: InstructionFile,
    /// Its content.
    pub content: String,
}

/// A file that was cut at the byte budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruncatedInstruction {
    /// Which file.
    pub file: InstructionFile,
    /// Its full size.
    pub full_bytes: u64,
    /// How many bytes were kept.
    pub kept_bytes: u64,
}

/// What the renderer produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedInstructions {
    /// The text to inject, empty when there is nothing to say.
    pub text: String,
    /// Files left out entirely, named so the model knows they exist.
    pub omitted: Vec<InstructionFile>,
    /// Files included but cut.
    pub truncated: Vec<TruncatedInstruction>,
}

impl RenderedInstructions {
    /// Whether there is anything to inject.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// How discovery and rendering are bounded.
#[derive(Clone, Debug)]
pub struct InstructionLimits {
    /// Per-file byte budget.
    pub max_bytes_per_file: u64,
    /// Total budget for the rendered block.
    pub max_total_bytes: u64,
    /// Whether to search the global directories.
    pub include_global: bool,
}

impl Default for InstructionLimits {
    fn default() -> Self {
        Self {
            max_bytes_per_file: DEFAULT_MAX_BYTES_PER_FILE,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            include_global: true,
        }
    }
}

/// A digest of a file's trimmed content.
///
/// Trimmed so a whitespace-only edit does not re-inject the file: instruction files are re-checked
/// after every filesystem tool call, and a re-injection per touch would fill the context with copies.
#[must_use]
pub fn digest(content: &str) -> u64 {
    // FNV-1a over the trimmed bytes. A cryptographic hash would be more uniform and cost more; this
    // only ever compares two strings for equality, where collision resistance beyond 64 bits buys
    // nothing.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in content.trim().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Finds instruction files governing `cwd`, nearest last.
///
/// Walks from `cwd` to the filesystem root, so a file in the workspace root and one in `src/` both
/// apply. Global directories come first, as the least specific.
#[must_use]
pub fn discover(
    cwd: &Path,
    home: Option<&Path>,
    limits: &InstructionLimits,
) -> Vec<InstructionFile> {
    let mut found = Vec::new();

    if limits.include_global {
        if let Some(home) = home {
            for directory in [USER_GLOBAL_DIRECTORY, HARNESS_GLOBAL_DIRECTORY] {
                let candidate = home.join(directory).join(INSTRUCTION_FILE_NAME);
                if candidate.is_file() {
                    found.push(InstructionFile {
                        display_path: candidate.display().to_string(),
                        absolute_path: candidate,
                    });
                }
            }
        }
    }

    // From the root down to the working directory: the least specific first, so the rendered text
    // reads as "these are the general rules, and here are the local ones".
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut cursor = Some(cwd);
    while let Some(directory) = cursor {
        chain.push(directory.to_path_buf());
        cursor = directory.parent();
    }
    chain.reverse();

    for directory in chain {
        let candidate = directory.join(INSTRUCTION_FILE_NAME);
        if !candidate.is_file() {
            continue;
        }
        // A global directory already contributed its file; adding it again would render twice.
        if found.iter().any(|known| known.absolute_path == candidate) {
            continue;
        }
        found.push(InstructionFile {
            display_path: candidate.display().to_string(),
            absolute_path: candidate,
        });
    }

    found
}

/// Renders instruction files into one block within the budgets.
///
/// The order is discovery order: global first, then outermost to innermost, so a local rule appears
/// after the general one it refines.
///
/// # Errors
///
/// Returns the I/O error for a file that cannot be read. A file that disappears between discovery and
/// reading is skipped rather than reported — the workspace changed under us, which is normal.
pub async fn render(
    files: &[InstructionFile],
    limits: &InstructionLimits,
    cancel: tokio_util::sync::CancellationToken,
) -> std::io::Result<RenderedInstructions> {
    let mut rendered = RenderedInstructions::default();
    let mut budget = limits.max_total_bytes;

    for file in files {
        if cancel.is_cancelled() {
            break;
        }
        let bytes = match tokio::fs::read(&file.absolute_path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let full_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);

        // A file that would blow the remaining budget is omitted, not silently dropped: the model is
        // told it exists. Silently dropping instructions the user wrote leaves the model acting
        // without rules it was supposed to follow.
        //
        // Omission rather than truncation when the *remaining budget* is too small to carry a rule.
        // A file cut to a handful of bytes states nothing usable, and presenting it as content would
        // tell the model it had been instructed when it had not.
        //
        // The threshold applies to the budget, not to the per-file limit: an operator who sets a
        // small per-file cap is asking for short excerpts, which is a deliberate choice.
        if budget < MIN_USEFUL_SLICE {
            rendered.omitted.push(file.clone());
            continue;
        }
        let per_file_cap = limits.max_bytes_per_file.min(budget);

        let (kept, truncated) = if full_bytes > per_file_cap {
            let kept = usize::try_from(per_file_cap).unwrap_or(usize::MAX);
            // Cut on a character boundary: slicing UTF-8 mid-sequence panics, and an instruction
            // file is prose, so losing one character at the cut is acceptable where a panic is not.
            let boundary = floor_char_boundary(&bytes, kept);
            (&bytes[..boundary], true)
        } else {
            (&bytes[..], false)
        };

        let text = String::from_utf8_lossy(kept).into_owned();
        budget = budget.saturating_sub(u64::try_from(kept.len()).unwrap_or(u64::MAX));

        if !rendered.text.is_empty() {
            rendered.text.push_str("\n\n");
        }
        let _ = write!(rendered.text, "## {}\n\n", file.display_path);
        rendered.text.push_str(&text);

        if truncated {
            rendered.truncated.push(TruncatedInstruction {
                file: file.clone(),
                full_bytes,
                kept_bytes: u64::try_from(kept.len()).unwrap_or(u64::MAX),
            });
        }
    }

    // The omissions are appended to the text so the model sees them in the same block rather than
    // having to notice a separate field.
    if !rendered.omitted.is_empty() {
        if !rendered.text.is_empty() {
            rendered.text.push_str("\n\n");
        }
        rendered.text.push_str("## Not shown\n\n");
        for file in &rendered.omitted {
            let _ = writeln!(
                rendered.text,
                "- {} (omitted: the instruction budget was exhausted)",
                file.display_path
            );
        }
    }

    Ok(rendered)
}

/// Finds the largest index at or below `index` that is a UTF-8 character boundary.
///
/// A byte slice cut at an arbitrary offset can split a multi-byte sequence, and a `str` built from
/// it would be lossily replaced rather than refused. Backing up to a boundary keeps the cut clean.
fn floor_char_boundary(bytes: &[u8], index: usize) -> usize {
    let mut index = index.min(bytes.len());
    // `bytes[index]` would read past the end when `index == len`; the loop condition already stops
    // at 0, so only the non-boundary case needs the index check.
    while index > 0 && index < bytes.len() && (bytes[index] & 0xc0) == 0x80 {
        index -= 1;
    }
    index
}

/// The assembler an owner holds: discovered files plus their digests.
///
/// Kept so a re-check after a filesystem tool call can skip files whose trimmed content has not
/// changed, rather than re-injecting the whole block.
#[derive(Default)]
pub struct InstructionState {
    digests: std::collections::HashMap<PathBuf, u64>,
}

impl InstructionState {
    /// Creates an empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a file's digest, returning whether it changed.
    ///
    /// The first time a file is seen counts as a change, so its content is injected once.
    pub fn record(&mut self, path: &Path, content: &str) -> bool {
        let digest = digest(content);
        match self.digests.insert(path.to_path_buf(), digest) {
            Some(previous) => previous != digest,
            None => true,
        }
    }

    /// Files whose digest is known, for diagnosis.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.digests.len()
    }
}

/// Reads and renders in one step, the way a caller uses it.
///
/// # Errors
///
/// Same as [`render`].
pub fn load(
    cwd: PathBuf,
    home: Option<PathBuf>,
    limits: InstructionLimits,
    cancel: tokio_util::sync::CancellationToken,
) -> BoxFut<std::io::Result<RenderedInstructions>> {
    Box::pin(async move {
        let files = discover(&cwd, home.as_deref(), &limits);
        render(&files, &limits, cancel).await
    })
}
