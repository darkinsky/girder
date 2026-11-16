//! The presentation vocabulary a tool uses to say how its call and result should be drawn.
//!
//! Upstream `packages/core/tools/src/presentation.ts` is not a renderer. It is the *vocabulary*
//! between a tool and whatever UI is showing it: a tool declares one tagged intent, and every client
//! maps that intent to its own card without knowing the tool's name. A tool that could only return
//! text forces every client to re-parse the text to find the matches, the exit code, or the URL,
//! which is how a table ends up rendered as a paragraph.
//!
//! This module ports that vocabulary as data, plus the two things the data alone does not give a
//! caller: a checked constructor per variant ([`CallView`], [`ResultView`]) and the bridge-side
//! fallback each card documents — what a client shows when it has no card for the intent
//! ([`CallView::title_fallback`], [`ResultView::fallback_text`]).
//!
//! # Why the fallback is part of the contract
//!
//! Upstream states the fallback for every card in prose: a UI without a terminal card gets a fenced
//! `console` block, one without a search card gets the raw result content, and the `read` view keeps
//! a stripped copy of the model-facing text precisely so a generic client can still show something.
//! Prose is not checkable, and a UI that "falls back" by inventing its own text reintroduces the
//! parsing this vocabulary exists to remove. [`ResultView::fallback_text`] returns the same decision
//! as data, so a client with no card does not have to guess and the port has something to test.
//!
//! Two cards deliberately have no fallback text: [`ResultView::Diff`] and [`ResultView::Search`].
//! Both describe a view over content the UI already holds — the diff's hunks, the matched lines — and
//! a text rendering of them would be a *second* rendering of the same facts, which is how a client
//! ends up showing the matches twice. Their absence here is the same absence upstream documents for
//! the search card ("the view carries no result text"), made explicit.
//!
//! # What is deliberately re-used from the crate
//!
//! UI-facing content blocks are [`girder_core::ContentBlock`], the same type the model sees, because a
//! view that invented its own block type would need a conversion at every boundary and the two would
//! drift. This is the one place where "UI-facing" and "model-facing" are allowed to share a type:
//! the *presenter* decides which blocks a human sees, and the type carries no opinion about who reads
//! it.
//!
//! # What is deliberately absent
//!
//! No renderer, no icons, no card layout, no session wiring. A card is drawn by the client that has
//! one; this crate can only say what was intended. The upstream `output.presentationMeta` channel —
//! structured data a tool persists with its result so a *replay* can rebuild the view — is also
//! absent: it is a session-log field, and belongs with the log rather than with the vocabulary.

use serde::{Deserialize, Serialize};

use girder_core::ContentBlock;

/// The category of a call, which a UI uses to pick an icon or a treatment.
///
/// [`CallKind::Other`] is the default: a tool that has not thought about its category must not
/// claim one, because a wrong icon is a small lie that makes every later glance at the log wrong.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallKind {
    /// Reads state and changes nothing.
    Read,
    /// Creates or modifies state.
    Edit,
    /// Removes state.
    Delete,
    /// Renames or relocates state.
    Move,
    /// Looks for something without changing it.
    Search,
    /// Runs a command or a program.
    Execute,
    /// Retrieves something from outside the workspace.
    Fetch,
    /// Anything else, and the only safe default.
    #[default]
    Other,
}

impl CallKind {
    /// The stable name this category is serialized as.
    ///
    /// The names are the persisted contract, so they are written out rather than derived from the
    /// variant name: a rename of the Rust variant must not silently change what a client reads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Delete => "delete",
            Self::Move => "move",
            Self::Search => "search",
            Self::Execute => "execute",
            Self::Fetch => "fetch",
            Self::Other => "other",
        }
    }

    /// Reads a category back, returning [`None`] for a name this vocabulary does not define.
    ///
    /// A view arriving from a newer client may carry a category this build has never heard of; the
    /// caller is expected to fall back to [`CallKind::Other`] rather than refuse the whole view.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "read" => Some(Self::Read),
            "edit" => Some(Self::Edit),
            "delete" => Some(Self::Delete),
            "move" => Some(Self::Move),
            "search" => Some(Self::Search),
            "execute" => Some(Self::Execute),
            "fetch" => Some(Self::Fetch),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

impl std::fmt::Display for CallKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A file a call reads or modifies, so a capable UI can follow along.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileLocation {
    /// The path as the tool reported it, which is what the model saw.
    pub path: String,
    /// An optional one-based line to focus, such as a read's offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

impl FileLocation {
    /// A location with no line focus.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            line: None,
        }
    }

    /// A location focused on one one-based line.
    ///
    /// Line zero does not exist in any file, so it is normalised to "no focus" rather than carried
    /// through: a UI asked to jump to line 0 either errors or jumps to the top, and neither is what
    /// the tool meant.
    #[must_use]
    pub fn at_line(path: impl Into<String>, line: u32) -> Self {
        Self {
            path: path.into(),
            line: (line != 0).then_some(line),
        }
    }
}

/// A single-file change a call is about to make, for a UI that renders inline diffs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    /// The file being changed.
    pub path: String,
    /// Prior content, or [`None`] for a new file.
    ///
    /// [`None`] also covers an overwrite, deliberately: a call-time presenter has no access to the
    /// file's prior content, and claiming an empty prior content would render a create as an edit
    /// that inserts every line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    /// Content after the change.
    pub new_text: String,
}

impl FileDiff {
    /// A diff that adds a file the reader has no prior content for.
    #[must_use]
    pub fn create(path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            old_text: None,
            new_text: content.into(),
        }
    }

    /// A diff with both sides known.
    #[must_use]
    pub fn edit(
        path: impl Into<String>,
        old_text: impl Into<String>,
        new_text: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            old_text: Some(old_text.into()),
            new_text: new_text.into(),
        }
    }

    /// Whether this diff creates the file rather than changing it.
    #[must_use]
    pub fn is_create(&self) -> bool {
        self.old_text.is_none()
    }
}

/// How a pending call should be drawn.
///
/// Exactly one variant is chosen by the tool; there is no way to express two intents at once,
/// because a client asked to choose between a terminal card and a diff card would pick differently
/// on every run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "card", rename_all = "lowercase")]
pub enum CallView {
    /// The default card: a title, an optional category, and optional detail.
    Generic {
        /// A short, always-visible label for what this call does.
        title: String,
        /// Category for an icon or a treatment; [`None`] means [`CallKind::Other`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<CallKind>,
        /// The salient input for a detail view, not the whole argument object.
        #[serde(rename = "rawInput", default, skip_serializing_if = "Option::is_none")]
        raw_input: Option<serde_json::Value>,
        /// UI-facing blocks to show alongside the title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ContentBlock>>,
        /// Files the call touches, for follow-along.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locations: Option<Vec<FileLocation>>,
    },
    /// A shell command running in a working directory.
    Terminal {
        /// The command, as the card's header line.
        title: String,
        /// An optional one-line summary rendered above the card.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// The working directory, which the bridge resolves when relative.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// A call that creates or modifies files.
    Diff {
        /// The card header, such as `Write foo.txt`.
        title: String,
        /// One entry per file the call changes.
        diffs: Vec<FileDiff>,
        /// Files the call modifies, usually the diffs' paths.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locations: Option<Vec<FileLocation>>,
    },
}

impl CallView {
    /// A generic card with only a title.
    #[must_use]
    pub fn generic(title: impl Into<String>) -> Self {
        Self::Generic {
            title: title.into(),
            kind: None,
            raw_input: None,
            content: None,
            locations: None,
        }
    }

    /// The title every variant carries, for a client that can only draw a header line.
    #[must_use]
    pub fn title(&self) -> &str {
        match self {
            Self::Generic { title, .. }
            | Self::Terminal { title, .. }
            | Self::Diff { title, .. } => title,
        }
    }

    /// The category a UI should use, with the documented default already applied.
    ///
    /// A terminal card is [`CallKind::Execute`] and a diff card is [`CallKind::Edit`] even though
    /// neither carries a `kind` field upstream: the card *is* the category, and a client that reads
    /// one field should not have to special-case the two variants that imply theirs.
    #[must_use]
    pub fn kind(&self) -> CallKind {
        match self {
            Self::Generic { kind, .. } => kind.unwrap_or_default(),
            Self::Terminal { .. } => CallKind::Execute,
            Self::Diff { .. } => CallKind::Edit,
        }
    }

    /// The files this call touches, or an empty slice.
    ///
    /// A terminal card touches nothing the UI can follow, which is an empty slice rather than a
    /// [`None`] the caller must branch on: "no files" is one answer, not two.
    #[must_use]
    pub fn locations(&self) -> &[FileLocation] {
        match self {
            Self::Generic { locations, .. } | Self::Diff { locations, .. } => {
                locations.as_deref().unwrap_or_default()
            }
            Self::Terminal { .. } => &[],
        }
    }

    /// The text a client with no card for this intent can show instead.
    ///
    /// Upstream hands a terminal-capable client the command and lets an incapable one fall back to a
    /// generic card whose body is the fenced output. This returns that header line — the command,
    /// prefixed so a reader can tell a command from prose — and the plain title for the other cards.
    #[must_use]
    pub fn title_fallback(&self) -> String {
        match self {
            Self::Terminal { title, cwd, .. } => match cwd {
                Some(cwd) => format!("$ {title}  ({cwd})"),
                None => format!("$ {title}"),
            },
            Self::Generic { title, .. } | Self::Diff { title, .. } => title.clone(),
        }
    }
}

/// One numbered line of a file, as a read view carries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadFileLine {
    /// The one-based line number in the file, not a re-count of the window.
    pub number: u32,
    /// The line without its trailing newline.
    pub text: String,
}

impl ReadFileLine {
    /// A numbered line.
    #[must_use]
    pub fn new(number: u32, text: impl Into<String>) -> Self {
        Self {
            number,
            text: text.into(),
        }
    }
}

/// One matched line inside a [`SearchMatches`] group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchLineMatch {
    /// The one-based line number of the match within its file.
    pub line_number: u32,
    /// The matched line, with the per-line preview budget already applied.
    pub line: String,
}

/// One file's grouped matches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchFileMatches {
    /// The file the matches belong to.
    pub path: String,
    /// The file's matched lines, in output order.
    pub matches: Vec<SearchLineMatch>,
}

/// One citeable source in a web search result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSource {
    /// The source URL.
    pub url: String,
    /// The source title, when the provider returned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A short excerpt or summary, when the provider returned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// The grouped-by-file matches of a `grep`-style result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatches {
    /// Replacement title, or [`None`] to keep the pending one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Matched lines grouped by file, in first-seen file order.
    pub files: Vec<SearchFileMatches>,
    /// Whether the tool capped the result, so a UI never presents a partial list as complete.
    pub truncated: bool,
    /// Total matches found before capping.
    pub total: usize,
}

/// The flat path list of a `glob`-style result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPaths {
    /// Replacement title, or [`None`] to keep the pending one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The discovered paths, in the tool's order.
    pub paths: Vec<String>,
    /// Whether the tool capped the result.
    pub truncated: bool,
    /// Total paths found before capping.
    pub total: usize,
}

/// How a completed call should be drawn.
///
/// The `card` and `shape` discriminants are flattened into each variant's serialization by the
/// [`serde`] tags below, so a client switches on exactly the two fields upstream defines and never
/// has to know which Rust variant it received.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "card", rename_all = "lowercase")]
pub enum ResultView {
    /// The default completed card.
    Generic {
        /// Replacement title, or [`None`] to keep the pending one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Reformatted UI-facing content, or [`None`] to render the raw result.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ContentBlock>>,
    },
    /// The completed state of a terminal call.
    Terminal {
        /// Replacement title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Captured output.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// The exit code, when the run ended by exiting.
        #[serde(rename = "exitCode", default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// The signal that killed the process, mutually exclusive with `exit_code`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
    /// A completed file mutation.
    Diff {
        /// Replacement title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// The change to show, in file order.
        diffs: Vec<FileDiff>,
    },
    /// A completed search, with the two shapes discriminated by `shape`.
    Search {
        /// Which search shape this is: `matches` or `paths`.
        shape: SearchShape,
        /// Replacement title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// The shape's payload.
        #[serde(flatten)]
        payload: SearchPayload,
    },
    /// A completed file read.
    Read {
        /// Replacement title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// The file's path.
        path: String,
        /// The one-based first line of the window, kept even when the window is empty.
        offset: u32,
        /// The returned window's lines, in file order.
        lines: Vec<ReadFileLine>,
        /// The exact total line count of the file.
        #[serde(rename = "totalLines")]
        total_lines: u32,
        /// A syntax-highlighting hint, or [`None`] when the extension is unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lang: Option<String>,
        /// The model-facing text with its envelope stripped, for a client without a read card.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ContentBlock>>,
    },
    /// A completed web retrieval.
    Web {
        /// Which web shape this is: `search` or `fetch`.
        kind: WebKind,
        /// Replacement title.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// The shape's payload.
        #[serde(flatten)]
        payload: WebPayload,
    },
}

/// The discriminant that separates the two search shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchShape {
    /// Grouped-by-file content matches.
    Matches,
    /// A flat path list.
    Paths,
}

impl SearchShape {
    /// The stable name this shape is serialized as.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Matches => "matches",
            Self::Paths => "paths",
        }
    }
}

/// The payload of a search view, matching its [`SearchShape`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SearchPayload {
    /// Grouped-by-file content matches.
    Matches(SearchMatches),
    /// A flat path list.
    Paths(SearchPaths),
}

/// The discriminant that separates a web search from a web fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebKind {
    /// A completed `web_search`.
    Search,
    /// A completed `web_fetch`.
    Fetch,
}

impl WebKind {
    /// The stable name this kind is serialized as.
    ///
    /// These values deliberately match the [`CallKind`] names `search` and `fetch`, so a call and
    /// its result read as one category — but they are a different vocabulary, and a new arm here is
    /// a union edit rather than any [`CallKind`] value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Fetch => "fetch",
        }
    }
}

/// The payload of a web view, matching its [`WebKind`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WebPayload {
    /// The sources a `web_search` cited.
    Search {
        /// The structured sources, which the render text cannot losslessly carry.
        sources: Vec<WebSource>,
        /// The provider-generated answer, when any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
        /// Whether the service cut the source list to honour the result cap.
        truncated: bool,
    },
    /// The retrieval summary of a `web_fetch`.
    Fetch {
        /// The final URL after allowed redirects.
        url: String,
        /// The HTTP status code of the fetched response.
        #[serde(rename = "statusCode")]
        status_code: u16,
        /// Whether the body or the rendered text was cut.
        truncated: bool,
    },
}

/// A truncated total that contradicts its own flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruncationMismatch {
    /// The flag the view claimed.
    pub truncated: bool,
    /// The total it claimed.
    pub total: usize,
    /// The retained count it actually carried.
    pub retained: usize,
}

impl std::fmt::Display for TruncationMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "truncated={} but retained {} of a claimed total of {}",
            self.truncated, self.retained, self.total
        )
    }
}

impl ResultView {
    /// The replacement title, when the view carries one.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        match self {
            Self::Generic { title, .. }
            | Self::Terminal { title, .. }
            | Self::Diff { title, .. }
            | Self::Search { title, .. }
            | Self::Read { title, .. }
            | Self::Web { title, .. } => title.as_deref(),
        }
    }

    /// Checks the invariants a client would otherwise have to defend against.
    ///
    /// Two are hard errors and one is reported as [`TruncationMismatch`]:
    ///
    /// - A terminal view must not carry both an exit code and a signal. A process that exited was
    ///   not killed by a signal, and a UI showing both pills would report two contradictory endings.
    /// - A read view's `offset` is one-based, so zero is not a window into any file.
    /// - A search view must report a `total` at least as large as the list it carries, and a list
    ///   shorter than `total` must be flagged `truncated`. This is the invariant that keeps a
    ///   partial group from being presented as complete, and it is the one a tool is most likely to
    ///   get wrong by forgetting to update `total` when it caps.
    ///
    /// # Errors
    ///
    /// Returns the message describing the first violated invariant.
    pub fn check(&self) -> Result<(), String> {
        if let Self::Terminal {
            exit_code, signal, ..
        } = self
        {
            if exit_code.is_some() && signal.is_some() {
                return Err("a terminal view carries both an exit code and a signal".to_owned());
            }
        }
        if let Self::Read { offset, .. } = self {
            if *offset == 0 {
                return Err("a read view's offset is one-based, so it cannot be 0".to_owned());
            }
        }
        if let Self::Search { payload, .. } = self {
            let (truncated, total, retained) = match payload {
                SearchPayload::Matches(matches) => {
                    (matches.truncated, matches.total, matches.files.len())
                }
                SearchPayload::Paths(paths) => (paths.truncated, paths.total, paths.paths.len()),
            };
            let mismatch = TruncationMismatch {
                truncated,
                total,
                retained,
            };
            if retained > total {
                return Err(format!(
                    "a search view retains more entries than its total: {mismatch}"
                ));
            }
            if retained < total && !truncated {
                return Err(format!(
                    "a search view dropped entries without flagging truncation: {mismatch}"
                ));
            }
        }
        Ok(())
    }

    /// The text a client with no card for this view can show instead, when there is one.
    ///
    /// [`None`] means the view carries no text of its own on purpose: a diff and a search are views
    /// over content the client already holds, and rendering them to text would duplicate it. The
    /// caller's answer is the raw tool result, not a synthesized one.
    #[must_use]
    pub fn fallback_text(&self) -> Option<String> {
        match self {
            Self::Generic { content, .. } => content.as_ref().map(|blocks| blocks_text(blocks)),
            Self::Terminal { output, .. } => Some(match output {
                Some(output) => format!("```console\n{output}\n```"),
                None => "```console\n```".to_owned(),
            }),
            Self::Diff { .. } | Self::Search { .. } => None,
            Self::Read {
                path,
                offset,
                lines,
                total_lines,
                content,
                ..
            } => Some(content.as_ref().map_or_else(
                || {
                    let last = offset.saturating_add(
                        u32::try_from(lines.len())
                            .unwrap_or(u32::MAX)
                            .saturating_sub(1),
                    );
                    format!("{path} (lines {offset}-{last} of {total_lines})")
                },
                |blocks| blocks_text(blocks),
            )),
            Self::Web { kind, payload, .. } => Some(match (kind, payload) {
                (
                    WebKind::Search,
                    WebPayload::Search {
                        sources, answer, ..
                    },
                ) => {
                    use std::fmt::Write as _;
                    let mut text = String::new();
                    if let Some(answer) = answer {
                        text.push_str(answer);
                        text.push('\n');
                    }
                    for source in sources {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        match &source.title {
                            Some(title) => {
                                let _ = writeln!(text, "- [{title}]({})", source.url);
                            }
                            None => {
                                let _ = writeln!(text, "- {}", source.url);
                            }
                        }
                    }
                    text
                }
                (
                    WebKind::Fetch,
                    WebPayload::Fetch {
                        url, status_code, ..
                    },
                ) => {
                    format!("{url} (HTTP {status_code})")
                }
                // A mismatched pair cannot be built by the constructors below; the fallback keeps
                // the build total rather than panicking on a deserialized view.
                _ => String::new(),
            }),
        }
    }
}

/// Concatenates the text of UI-facing blocks, ignoring non-text blocks.
fn blocks_text(blocks: &[ContentBlock]) -> String {
    let mut text = String::new();
    for block in blocks {
        if let ContentBlock::Text { text: line } = block {
            text.push_str(line);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{
        CallKind, CallView, FileDiff, FileLocation, ReadFileLine, ResultView, SearchFileMatches,
        SearchLineMatch, SearchMatches, SearchPaths, SearchPayload, SearchShape, WebKind,
        WebPayload, WebSource,
    };

    #[test]
    fn call_kind_names_round_trip() {
        for kind in [
            CallKind::Read,
            CallKind::Edit,
            CallKind::Delete,
            CallKind::Move,
            CallKind::Search,
            CallKind::Execute,
            CallKind::Fetch,
            CallKind::Other,
        ] {
            assert_eq!(CallKind::from_name(kind.as_str()), Some(kind));
        }
        assert_eq!(CallKind::from_name("teleport"), None);
        assert_eq!(CallKind::default(), CallKind::Other);
    }

    #[test]
    fn a_terminal_call_is_an_execute_and_carries_its_cwd() {
        let view = CallView::Terminal {
            title: "cargo test".to_owned(),
            description: None,
            cwd: Some("/w".to_owned()),
        };
        assert_eq!(view.kind(), CallKind::Execute);
        assert_eq!(view.title_fallback(), "$ cargo test  (/w)");
        assert!(view.locations().is_empty());
    }

    #[test]
    fn a_diff_call_is_an_edit() {
        let view = CallView::Diff {
            title: "Write a.txt".to_owned(),
            diffs: vec![FileDiff::create("a.txt", "hi")],
            locations: Some(vec![FileLocation::new("a.txt")]),
        };
        assert_eq!(view.kind(), CallKind::Edit);
        assert_eq!(view.locations().len(), 1);
    }

    #[test]
    fn a_zero_line_location_has_no_focus() {
        assert_eq!(FileLocation::at_line("a.rs", 0).line, None);
        assert_eq!(FileLocation::at_line("a.rs", 7).line, Some(7));
    }

    #[test]
    fn a_create_diff_has_no_prior_side() {
        let diff = FileDiff::create("new.txt", "body");
        assert!(diff.is_create());
        assert_eq!(diff.old_text, None);
    }

    #[test]
    fn a_terminal_fallback_is_a_console_block() {
        let view = ResultView::Terminal {
            title: None,
            output: Some("ok\n".to_owned()),
            exit_code: Some(0),
            signal: None,
        };
        assert_eq!(
            view.fallback_text().as_deref(),
            Some("```console\nok\n\n```")
        );
    }

    #[test]
    fn a_diff_and_a_search_have_no_fallback_text() {
        let diff = ResultView::Diff {
            title: None,
            diffs: Vec::new(),
        };
        assert_eq!(diff.fallback_text(), None);
        let search = ResultView::Search {
            shape: SearchShape::Paths,
            title: None,
            payload: SearchPayload::Paths(SearchPaths {
                title: None,
                paths: vec!["a".to_owned()],
                truncated: false,
                total: 1,
            }),
        };
        assert_eq!(search.fallback_text(), None);
    }

    #[test]
    fn a_read_without_content_falls_back_to_a_window_summary() {
        let view = ResultView::Read {
            title: None,
            path: "a.rs".to_owned(),
            offset: 10,
            lines: vec![ReadFileLine::new(10, "x"), ReadFileLine::new(11, "y")],
            total_lines: 40,
            lang: Some("rs".to_owned()),
            content: None,
        };
        assert_eq!(
            view.fallback_text().as_deref(),
            Some("a.rs (lines 10-11 of 40)")
        );
    }

    #[test]
    fn a_web_search_fallback_lists_its_sources() {
        let view = ResultView::Web {
            kind: WebKind::Search,
            title: None,
            payload: WebPayload::Search {
                sources: vec![WebSource {
                    url: "https://example.test".to_owned(),
                    title: Some("Example".to_owned()),
                    snippet: None,
                }],
                answer: Some("the answer".to_owned()),
                truncated: false,
            },
        };
        assert_eq!(
            view.fallback_text().as_deref(),
            Some("the answer\n- [Example](https://example.test)\n")
        );
    }

    #[test]
    fn a_web_fetch_fallback_names_the_status() {
        let view = ResultView::Web {
            kind: WebKind::Fetch,
            title: None,
            payload: WebPayload::Fetch {
                url: "https://example.test".to_owned(),
                status_code: 404,
                truncated: false,
            },
        };
        assert_eq!(
            view.fallback_text().as_deref(),
            Some("https://example.test (HTTP 404)")
        );
    }

    #[test]
    fn check_rejects_a_terminal_view_with_two_endings() {
        let view = ResultView::Terminal {
            title: None,
            output: None,
            exit_code: Some(1),
            signal: Some("SIGTERM".to_owned()),
        };
        assert!(view.check().is_err());
    }

    #[test]
    fn check_rejects_a_zero_offset_read() {
        let view = ResultView::Read {
            title: None,
            path: "a".to_owned(),
            offset: 0,
            lines: Vec::new(),
            total_lines: 0,
            lang: None,
            content: None,
        };
        assert!(view.check().is_err());
    }

    #[test]
    fn check_rejects_an_unflagged_cap() {
        let view = ResultView::Search {
            shape: SearchShape::Paths,
            title: None,
            payload: SearchPayload::Paths(SearchPaths {
                title: None,
                paths: vec!["a".to_owned()],
                truncated: false,
                total: 5,
            }),
        };
        assert!(view.check().is_err());
    }

    #[test]
    fn check_accepts_a_flagged_cap_and_an_exact_match() {
        let capped = ResultView::Search {
            shape: SearchShape::Matches,
            title: None,
            payload: SearchPayload::Matches(SearchMatches {
                title: None,
                files: vec![SearchFileMatches {
                    path: "a".to_owned(),
                    matches: vec![SearchLineMatch {
                        line_number: 3,
                        line: "hit".to_owned(),
                    }],
                }],
                truncated: true,
                total: 9,
            }),
        };
        assert!(capped.check().is_ok());
        let exact = ResultView::Search {
            shape: SearchShape::Paths,
            title: None,
            payload: SearchPayload::Paths(SearchPaths {
                title: None,
                paths: vec!["a".to_owned()],
                truncated: false,
                total: 1,
            }),
        };
        assert!(exact.check().is_ok());
    }

    #[test]
    fn the_wire_discriminants_are_the_documented_names() {
        assert_eq!(SearchShape::Matches.as_str(), "matches");
        assert_eq!(SearchShape::Paths.as_str(), "paths");
        assert_eq!(WebKind::Search.as_str(), "search");
        assert_eq!(WebKind::Fetch.as_str(), "fetch");
    }

    #[test]
    fn a_search_view_serializes_its_shape_and_payload_together() {
        let view = ResultView::Search {
            shape: SearchShape::Paths,
            title: Some("paths".to_owned()),
            payload: SearchPayload::Paths(SearchPaths {
                title: None,
                paths: vec!["a".to_owned()],
                truncated: false,
                total: 1,
            }),
        };
        let json = serde_json::to_value(&view).expect("serialize");
        assert_eq!(json["card"], "search");
        assert_eq!(json["shape"], "paths");
        assert_eq!(json["paths"][0], "a");
        assert_eq!(json["title"], "paths");
    }

    #[test]
    fn a_call_view_serializes_its_card_tag() {
        let view = CallView::generic("List");
        let json = serde_json::to_value(&view).expect("serialize");
        assert_eq!(json["card"], "generic");
        assert_eq!(json["title"], "List");
    }
}
