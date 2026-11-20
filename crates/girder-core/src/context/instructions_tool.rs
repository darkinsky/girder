//! The `instructions` tool: which instruction files are in force, and what they say.
//!
//! The harness injects `AGENTS.md`-compatible files into the conversation on its own, so a model can
//! usually read the rules it is following. What it cannot see is the *set*: which files were found,
//! where they came from, how large they are, and whether one was left out because a budget ran out.
//! A model that does not know a file exists cannot ask for it.
//!
//! # Why the empty answer names the paths that were searched
//!
//! "There are no instructions" and "the search looked in the wrong place" produce the same silence.
//! A model told nothing may conclude the workspace imposes no rules and act on that conclusion. The
//! list of attempted paths turns an empty result into evidence, and it says where a file would have
//! to be written for the next turn to find it.
//!
//! # Why a digest and not just the content
//!
//! The digest is what the renderer uses to decide whether a file changed, and it ignores
//! whitespace-only edits. Two listings compared by digest distinguish "the rules changed" from "the
//! file was touched" without re-reading either file.
//!
//! # Why `full` is bounded
//!
//! `full` exists so a model can read a file it was told about. It must not become a way to pull an
//! arbitrary amount of text into one result, because that result crowds out the work the model was
//! asked to do. The bound is on the whole answer rather than per file, so a single huge file cannot
//! push the listing of the others out of sight; what was cut, and how much of it, is stated in the
//! answer rather than left for the model to notice.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;

// `girder_core::` rather than `crate::`: this module is also compiled into an integration test
// through `#[path]`, where `crate` is the test binary and only the real crate name resolves. The
// crate aliases itself in `lib.rs`, so both spellings work inside the library.
use girder_core::{
    HARNESS_GLOBAL_DIRECTORY, INSTRUCTION_FILE_NAME, InstructionFile, InstructionLimits,
    ToolDefinition, ToolResult, ToolRunContext, USER_GLOBAL_DIRECTORY, digest,
    discover_instructions,
};

/// The name the model calls.
pub const TOOL_NAME: &str = "instructions";

/// The most text one call may return.
///
/// 100 KiB is a little under a tenth of a typical context window: enough to read a long `AGENTS.md`
/// in full, small enough that asking for it cannot crowd out the task. The listing is counted
/// against the same budget, so a workspace with a pathological number of files still answers within
/// it.
pub const MAX_OUTPUT_BYTES: usize = 100 * 1024;

/// The smallest content slice worth showing when the budget is nearly spent.
///
/// Below this a "content" block holds a fragment that states no usable rule, and presenting it would
/// tell the model it had read instructions it had not. The renderer applies the same judgement when
/// it omits a file instead of truncating it.
const MIN_CONTENT_SLICE: usize = 512;

/// Builds the `instructions` tool over a fixed workspace and home.
///
/// Both paths are captured rather than taken as arguments: a model choosing where to look for its
/// own instructions could be told about files that do not govern it, which is worse than being told
/// nothing, because the rules it then follows were written for a different tree.
#[must_use]
pub fn instructions_tool(workspace: PathBuf, home: Option<PathBuf>) -> ToolDefinition {
    ToolDefinition::new(
        TOOL_NAME,
        "Lists the instruction files (AGENTS.md) that apply to this workspace, in the order they \
         are rendered: global files first, then outermost directory to innermost. Each entry has \
         its path, size in bytes and a content digest. Set `full` to include the text of each file. \
         Use this to check which rules are in force, or to confirm that none were found.",
        json!({
            "type": "object",
            "properties": {
                "full": {
                    "type": "boolean",
                    "description": "Include the content of each file, not only its path, size and digest. The total answer is capped, and any cut is reported.",
                    "default": false
                }
            }
        }),
        Arc::new(move |args: serde_json::Value, ctx: ToolRunContext| {
            let workspace = workspace.clone();
            let home = home.clone();
            let cancel = ctx.cancel.clone();
            Box::pin(async move { Ok(run(&workspace, home.as_deref(), &args, &cancel).await) })
        }),
    )
}

/// Reads the arguments and produces the answer.
async fn run(
    workspace: &Path,
    home: Option<&Path>,
    args: &serde_json::Value,
    cancel: &CancellationToken,
) -> ToolResult {
    let limits = InstructionLimits {
        // Without a home there is nowhere to look, and claiming the global directories were searched
        // would be the exact confusion this tool exists to prevent.
        include_global: home.is_some(),
        ..InstructionLimits::default()
    };
    let found = discover_instructions(workspace, home, &limits);
    if found.is_empty() {
        return ToolResult::text(no_files(workspace, home));
    }

    let mut entries = Vec::with_capacity(found.len());
    let mut cancelled = false;
    for file in &found {
        if cancel.is_cancelled() {
            cancelled = true;
            break;
        }
        // `None` means the file vanished between discovery and reading. The workspace changing under
        // a read is normal, and a listing that fails because of it tells the model less than one that
        // reports what it did find.
        if let Some(entry) = read(file).await {
            entries.push(entry);
        }
    }

    if entries.is_empty() {
        // Everything discovery saw is gone; the honest answer is the same as having found nothing.
        return ToolResult::text(no_files(workspace, home));
    }

    let full = args
        .get("full")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    ToolResult::text(render(&entries, full, cancelled))
}

/// One discovered file, read or not.
struct Entry {
    /// The path a model sees, as discovery reported it.
    display_path: String,
    /// What reading it produced.
    body: Body,
}

/// The outcome of reading one instruction file.
enum Body {
    /// The file was read. `bytes` is the size on disk; `content` is the same bytes decoded as text.
    Read {
        /// Its text, lossily decoded when the file is not valid UTF-8.
        content: String,
        /// Its size in bytes, before any decoding.
        bytes: u64,
    },
    /// The file exists but could not be read; the string is the reason, for the model to act on.
    Failed(String),
}

/// Reads one discovered file.
async fn read(file: &InstructionFile) -> Option<Entry> {
    match tokio::fs::read(&file.absolute_path).await {
        Ok(bytes) => {
            let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            Some(Entry {
                display_path: file.display_path.clone(),
                // Lossy rather than refusing: an instruction file with one stray byte in it is still
                // the file that states the rules, and a refusal would hide all of them.
                body: Body::Read {
                    content: String::from_utf8_lossy(&bytes).into_owned(),
                    bytes: size,
                },
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(Entry {
            display_path: file.display_path.clone(),
            body: Body::Failed(error.to_string()),
        }),
    }
}

/// The answer when no file was found, naming what was searched.
fn no_files(workspace: &Path, home: Option<&Path>) -> String {
    let mut text = String::from("no instruction files found\n\nSearched, in this order:\n");
    for location in searched_locations(workspace, home) {
        let _ = writeln!(text, "- {location}");
    }
    if home.is_none() {
        text.push_str(
            "\nNo home directory was given, so the global directories were not searched.\n",
        );
    }
    let _ = write!(
        text,
        "\nA file must be named {INSTRUCTION_FILE_NAME} to be found: one in the workspace or any \
         directory above it, or one in a global directory under a home directory.\n"
    );
    text
}

/// The paths discovery would have accepted a file at, in discovery order.
#[must_use]
fn searched_locations(workspace: &Path, home: Option<&Path>) -> Vec<String> {
    let mut locations = Vec::new();

    if let Some(home) = home {
        for directory in [USER_GLOBAL_DIRECTORY, HARNESS_GLOBAL_DIRECTORY] {
            locations.push(
                home.join(directory)
                    .join(INSTRUCTION_FILE_NAME)
                    .display()
                    .to_string(),
            );
        }
    }

    // Outermost first, matching the order discovery reports files in: the least specific rule is the
    // one that applies most widely.
    let mut chain: Vec<PathBuf> = Vec::new();
    let mut cursor = Some(workspace);
    while let Some(directory) = cursor {
        chain.push(directory.to_path_buf());
        cursor = directory.parent();
    }
    chain.reverse();
    for directory in chain {
        locations.push(directory.join(INSTRUCTION_FILE_NAME).display().to_string());
    }

    locations
}

/// Renders the listing, and the content when it was asked for, within [`MAX_OUTPUT_BYTES`].
#[must_use]
fn render(entries: &[Entry], full: bool, cancelled: bool) -> String {
    let mut listing = format!(
        "found {} instruction file(s), global first and then outermost to innermost:\n\n",
        entries.len()
    );
    let mut listing_truncated = false;
    for entry in entries {
        // The listing is one short line per file, but a workspace can have thousands of them, and the
        // cap applies to the whole answer rather than only to the content.
        if listing.len() >= MAX_OUTPUT_BYTES {
            listing_truncated = true;
            break;
        }
        match &entry.body {
            Body::Read { content, bytes } => {
                let _ = writeln!(
                    listing,
                    "- {} ({bytes} bytes, digest {})",
                    entry.display_path,
                    short_digest(content)
                );
            }
            Body::Failed(reason) => {
                let _ = writeln!(
                    listing,
                    "- {} (could not be read: {reason})",
                    entry.display_path
                );
            }
        }
    }
    if listing_truncated {
        let _ = writeln!(
            listing,
            "- further files are not listed: the {MAX_OUTPUT_BYTES}-byte output limit was reached"
        );
    }

    // The listing is written before the budget is opened for content: a listing the model cannot see
    // is worse than no content at all, because without it the model does not know the files exist.
    let mut report = Report::new(MAX_OUTPUT_BYTES.saturating_sub(listing.len()));
    report.text.push_str(&listing);

    if full {
        report_contents(&mut report, entries);
    }

    if cancelled {
        report
            .text
            .push_str("\n\n[cancelled: the remaining files were not read]\n");
    }
    if report.truncated {
        let _ = write!(
            report.text,
            "\n\n[truncated: the answer is capped at {MAX_OUTPUT_BYTES} bytes; content past the cap \
             is not included]\n"
        );
    }

    report.text
}

/// Appends each file's content, stopping when the budget is spent.
fn report_contents(report: &mut Report, entries: &[Entry]) {
    for entry in entries {
        let Body::Read { content, .. } = &entry.body else {
            continue;
        };
        // A fragment this short states no usable rule, so it is omitted rather than shown as if it
        // were the file. The path is already in the listing, so nothing is hidden.
        if report.remaining < MIN_CONTENT_SLICE {
            report.omitted.push(entry.display_path.clone());
            continue;
        }

        let heading = format!("\n\n### {}\n\n", entry.display_path);
        let _ = report.push(&heading);
        let kept = report.push(content);
        if kept < content.len() {
            let _ = write!(
                report.text,
                "\n\n[truncated: showed {kept} of {} bytes of this file]\n",
                content.len()
            );
        }
    }

    if !report.omitted.is_empty() {
        let _ = write!(
            report.text,
            "\n\n[content not shown for {} file(s): the output limit was reached]\n",
            report.omitted.len()
        );
        for path in &report.omitted {
            let _ = writeln!(report.text, "- {path}");
        }
    }
}

/// The eight low hex digits of a content digest.
///
/// Eight digits are enough to compare two listings by eye without spending a line on sixteen, and the
/// full digest is only ever compared for equality, never used as a key.
#[must_use]
fn short_digest(content: &str) -> String {
    format!("{:08x}", digest(content) & 0xffff_ffff)
}

/// An answer being built against a byte budget.
struct Report {
    /// The text so far.
    text: String,
    /// How many bytes may still be appended as content.
    remaining: usize,
    /// Whether the budget cut something.
    truncated: bool,
    /// Files whose content was left out, named so the model knows they are not empty.
    omitted: Vec<String>,
}

impl Report {
    /// Starts a report with `limit` bytes available.
    fn new(limit: usize) -> Self {
        Self {
            text: String::new(),
            remaining: limit,
            truncated: false,
            omitted: Vec::new(),
        }
    }

    /// Appends as much of `value` as the budget allows, returning how many bytes were kept.
    ///
    /// The cut lands on a character boundary: slicing UTF-8 mid-sequence panics, and losing one
    /// character at the cut costs less than a tool that fails instead of truncating.
    fn push(&mut self, value: &str) -> usize {
        if value.len() <= self.remaining {
            self.text.push_str(value);
            self.remaining -= value.len();
            return value.len();
        }

        let mut kept = self.remaining.min(value.len());
        while kept > 0 && !value.is_char_boundary(kept) {
            kept -= 1;
        }
        self.text.push_str(&value[..kept]);
        self.remaining = 0;
        self.truncated = true;
        kept
    }
}
