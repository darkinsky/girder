//! Search result projection: bounding, grouping, and presenting `glob`/`grep` output.
//!
//! This is the gap left by `search_tools.rs`. That module answers the question "what matched",
//! and stops there: it joins raw lines and prints them. Upstream `packages/fs/tool-fs-search`
//! spends most of its 1,620 source lines on the question that follows — *how much of the answer
//! may a model see, and what does it get told about the rest*. Nothing here runs a search or
//! touches a disk; every function is a pure transform over an already-collected result, which is
//! what makes the presentation testable without a filesystem.
//!
//! # The three facts a bounded result must keep apart
//!
//! A capped result carries three different numbers and collapsing any two of them produces a lie:
//!
//! - **seen** — how many matches the search found. "Found 250 matches" is a claim about the tree.
//! - **kept** — how many were shown. The difference is the model's uncertainty.
//! - **truncated** — whether the cap actually bit.
//!
//! A model told "these are all the matches" when they are not draws a conclusion the evidence does
//! not support, and it will act on it. [`format_grep_output`] therefore says `Found 12 of 300
//! matches` rather than `Found 12 matches`, and points at the saved complete result.
//!
//! # Why a preview is not a truncation of the result
//!
//! One matched line can be a minified bundle or a base64 blob: a single match, kilobytes wide,
//! enough to spend a context window on one row. [`preview_line`] bounds the *line*, leaving the
//! match count exact. The complete line still lives in the file, so the correct recovery is
//! `read`, not "the search was incomplete" — which is why the preview marker is per line and the
//! footer is per result.
//!
//! # Why `glob` is sampled rather than headed
//!
//! A modification-time head over a capped `glob` answers "what changed recently", and for a broad
//! pattern that head can be one subtree's twelve files while the pattern matched six subtrees.
//! [`sample_across_top_level`] gives every top-level entry one slot before any entry gets a second,
//! so the inline page reflects the *shape* of what matched. A result that fits is never reordered:
//! over a complete result, modification-time order is the answer to an age question and must
//! survive.
//!
//! # What is deliberately absent
//!
//! The card views (`presentGlobCall`, `presentResult`) are UI projections over the tool view
//! vocabulary, and the Rust side owns its own presentation layer; only the metadata those cards
//! are built from — [`SearchMeta`] — is produced here. Running `rg` and parsing its `--json`
//! output (`search-core.ts`'s `runRipgrep`/`parseGrepMatches`) is transport, and belongs with the
//! tool that spawns it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Component, Path};

use girder_core::{
    ItemRetainer, ItemStrategy, RetainedItems, SpillReference, TextRetainer, TextStrategy,
};
use serde_json::{Value, json};

/// Upstream `GLOB_MAX_RESULTS`: how many paths a `glob` call shows inline.
///
/// 100 keeps a listing readable and is far below the point where a model can use the tail.
pub const GLOB_MAX_RESULTS: usize = 100;

/// Upstream `GREP_MAX_MATCHES`: how many matched lines a `grep` call shows inline.
pub const GREP_MAX_MATCHES: usize = 250;

/// Upstream `GREP_MAX_LINE_BYTES`: the per-matched-line preview budget.
///
/// 2,000 bytes is roughly a screen of code: enough to identify the match, short of the
/// one-line bundle that would otherwise consume the whole result budget.
pub const GREP_MAX_LINE_BYTES: usize = 2_000;

/// Upstream `SEARCH_META_MAX_BYTES`: the cap on a serialized [`SearchMeta`] payload.
///
/// Metadata is persisted with the session log and re-sent on every later request, so it is bounded
/// separately from the text: a deployment's output budget shrinks `content` and never `meta`.
pub const SEARCH_META_MAX_BYTES: usize = 65_536;

/// One parsed match: the file, the 1-based line number, and the (possibly previewed) line text.
///
/// Corresponds to upstream `@deepseek-ai/dsh-tool-fs-search/search-core` `GrepMatch`. The field
/// names are the Rust spelling of that interface; `line_number` is 1-based because that is what
/// `read` numbers and what a person quotes back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepMatch {
    /// The model-facing display path of the matched file, already relativized by the caller.
    pub path: String,
    /// The 1-based line number of the match within that file.
    pub line_number: u64,
    /// The matched line text, trailing newline already stripped.
    pub line: String,
}

/// One matched line as it appears in [`SearchMeta`].
///
/// Corresponds to upstream `presentation.ts`'s `MetaLineMatch`. The line text is already
/// previewed, so a card and the model-facing text agree about what survived the cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMatch {
    /// The 1-based line number.
    pub line_number: u64,
    /// The (possibly previewed) line text.
    pub line: String,
}

/// One file's grouped matches, in first-seen order.
///
/// Corresponds to upstream `presentation.ts`'s `MetaFileMatches`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMatches {
    /// The file's display path.
    pub path: String,
    /// That file's matches, in output order.
    pub matches: Vec<LineMatch>,
}

impl FileMatches {
    /// Projects one file group into its JSON form.
    ///
    /// Corresponds to the shape upstream `grepSearchMeta` serializes, including the camelCase
    /// `lineNumber` key, which is what a recorded session already holds.
    fn to_value(&self) -> Value {
        json!({
            "path": self.path,
            "matches": self
                .matches
                .iter()
                .map(|line| json!({ "lineNumber": line.line_number, "line": line.line }))
                .collect::<Vec<_>>(),
        })
    }

    /// Narrows one JSON file group, or `None` when any part of it is malformed.
    ///
    /// Corresponds to upstream `presentation.ts`'s `isSearchFileMatches`. A malformed group
    /// declines to the generic card instead of producing a card that misreports the result.
    fn from_value(value: &Value) -> Option<Self> {
        let record = value.as_object()?;
        let path = record.get("path")?.as_str()?.to_owned();
        let raw = record.get("matches")?.as_array()?;
        let mut matches = Vec::with_capacity(raw.len());
        for entry in raw {
            let line = entry.as_object()?;
            let line_number = line.get("lineNumber")?.as_u64()?;
            let text = line.get("line")?.as_str()?.to_owned();
            matches.push(LineMatch {
                line_number,
                line: text,
            });
        }
        Some(Self { path, matches })
    }
}

/// The structured search metadata a UI card is built from.
///
/// Corresponds to upstream `@deepseek-ai/dsh-tool-fs-search/presentation` `SearchMeta`: the union
/// of the `matches`-shaped `grep` projection and the `paths`-shaped `glob` projection. `total`
/// counts what the search found, not what the payload retains, so a byte-capped card still reports
/// the size of the complete result. A zero-result projection is valid — "no matches" is a real
/// answer, unlike an absent card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchMeta {
    /// A `grep` projection: matches grouped by file.
    Matches {
        /// One entry per file, in first-seen order.
        files: Vec<FileMatches>,
        /// Whether anything was dropped by an item cap or the byte cap.
        truncated: bool,
        /// How many matches the search found in total.
        total: u64,
    },
    /// A `glob` projection: the discovered paths.
    Paths {
        /// The retained paths, in output order.
        paths: Vec<String>,
        /// Whether anything was dropped by an item cap or the byte cap.
        truncated: bool,
        /// How many paths the search found in total.
        total: u64,
    },
}

impl SearchMeta {
    /// Serializes the projection to the JSON a session log stores.
    ///
    /// Corresponds to what upstream `grepSearchMeta`/`globSearchMeta` return, which is then
    /// required to be JSON-serializable so the read card survives replay.
    #[must_use]
    pub fn to_value(&self) -> Value {
        match self {
            Self::Matches {
                files,
                truncated,
                total,
            } => json!({
                "shape": "matches",
                "files": files.iter().map(FileMatches::to_value).collect::<Vec<_>>(),
                "truncated": truncated,
                "total": total,
            }),
            Self::Paths {
                paths,
                truncated,
                total,
            } => json!({
                "shape": "paths",
                "paths": paths,
                "truncated": truncated,
                "total": total,
            }),
        }
    }

    /// Narrows opaque JSON metadata back into a projection, or `None` when it is malformed.
    ///
    /// Corresponds to upstream `presentation.ts`'s `searchViewFromMeta`. Replayed or hand-edited
    /// logs must fall back to the generic card rather than throwing during replay, so every shape
    /// violation is a `None` and never a panic. An empty `files`/`paths` array narrows
    /// successfully: it is a legitimate zero-result card.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        let record = value.as_object()?;
        let truncated = record.get("truncated")?.as_bool()?;
        let total = record.get("total")?.as_u64()?;
        match record.get("shape")?.as_str()? {
            "matches" => {
                let raw = record.get("files")?.as_array()?;
                let mut files = Vec::with_capacity(raw.len());
                for entry in raw {
                    files.push(FileMatches::from_value(entry)?);
                }
                Some(Self::Matches {
                    files,
                    truncated,
                    total,
                })
            }
            "paths" => {
                let raw = record.get("paths")?.as_array()?;
                let mut paths = Vec::with_capacity(raw.len());
                for entry in raw {
                    paths.push(entry.as_str()?.to_owned());
                }
                Some(Self::Paths {
                    paths,
                    truncated,
                    total,
                })
            }
            _ => None,
        }
    }
}

/// Renders a display path relative to the directory the search ran in.
///
/// Corresponds to upstream `search-core.ts`'s `toWorkdirRelative`. The result is what the model
/// sees, so it is relative when the path is under the workdir and untouched otherwise: rewriting
/// an outside path into `../…` or into the workdir's own name would misreport where the file is.
///
/// A relative input is returned unchanged (it is already in the caller's frame), and so is a path
/// that cannot be expressed under `workdir` — a different root, or a `workdir` that is not
/// absolute and therefore has no meaning outside one process.
#[must_use]
pub fn to_workdir_relative(path: &str, workdir: &str) -> String {
    let original = Path::new(path);
    if !original.is_absolute() {
        return path.to_owned();
    }
    let Some(relative) = lexical_relative(Path::new(workdir), original) else {
        return path.to_owned();
    };
    if relative.is_empty() {
        return ".".to_owned();
    }
    // `..` as the leading segment means the target is outside the workdir. Reporting the real
    // absolute path is more useful than a `../..` chain whose depth depends on where the process
    // happened to run.
    if relative == ".." || relative.starts_with("../") {
        return path.to_owned();
    }
    relative
}

/// Bounds one matched line to `max_bytes`, preserving the UTF-8 boundary and marking the cut.
///
/// Corresponds to upstream `search-core.ts`'s `previewLine`. The marker is what stops a model from
/// reading a cut line as the whole line — silence there produces a quote that does not exist in
/// the file.
#[must_use]
pub fn preview_line(line: &str, max_bytes: usize) -> String {
    let mut retainer = TextRetainer::new(TextStrategy::Head { max_bytes });
    retainer.push(line);
    let kept = retainer.finish();
    if kept.truncated {
        format!("{} (line truncated)", kept.text)
    } else {
        kept.text
    }
}

/// Applies the per-line preview and the inline match cap in one pass.
///
/// Corresponds to upstream `search-core.ts`'s `retainGrepMatches`. Both the model-facing text and
/// the card consume *this* result, which is what keeps them from disagreeing about which matches
/// survived. `seen` stays exact: every offered match is counted even once the cap is full.
#[must_use]
pub fn retain_grep_matches(
    matches: Vec<GrepMatch>,
    max_matches: usize,
    max_line_bytes: usize,
) -> RetainedItems<GrepMatch> {
    let mut retainer = ItemRetainer::new(ItemStrategy::Head {
        max_items: max_matches,
    });
    for entry in matches {
        let line = preview_line(&entry.line, max_line_bytes);
        retainer.push(GrepMatch {
            path: entry.path,
            line_number: entry.line_number,
            line,
        });
    }
    retainer.finish()
}

/// Applies the inline path cap to a complete `glob` result.
///
/// Corresponds to upstream `search-core.ts`'s `retainGlobPaths`. Order is preserved: over a
/// complete result, modification-time order is part of the answer.
#[must_use]
pub fn retain_glob_paths(paths: Vec<String>, max_results: usize) -> RetainedItems<String> {
    let mut retainer = ItemRetainer::new(ItemStrategy::Head {
        max_items: max_results,
    });
    for entry in paths {
        retainer.push(entry);
    }
    retainer.finish()
}

/// Groups flat matches by file, keeping first-seen file order and each file's match order.
///
/// Corresponds to upstream `presentation.ts`'s `groupMatchesByFile`. The grouping matches what the
/// model-facing text does in [`format_grep_matches`], so a card and the text agree about file
/// order and membership.
#[must_use]
pub fn group_matches_by_file(matches: &[GrepMatch]) -> Vec<FileMatches> {
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut groups: Vec<FileMatches> = Vec::new();
    for entry in matches {
        if let Some(&at) = index.get(entry.path.as_str()) {
            if let Some(group) = groups.get_mut(at) {
                group.matches.push(LineMatch {
                    line_number: entry.line_number,
                    line: entry.line.clone(),
                });
            }
        } else {
            index.insert(entry.path.as_str(), groups.len());
            groups.push(FileMatches {
                path: entry.path.clone(),
                matches: vec![LineMatch {
                    line_number: entry.line_number,
                    line: entry.line.clone(),
                }],
            });
        }
    }
    groups
}

/// Renders retained matches as the model-facing body: one section per file, one row per match.
///
/// Corresponds to upstream `grep.ts`'s `formatGrepMatches`. Each section is the file's display path
/// followed by `Line N: <text>` rows, with a blank line between files — a shape a model can quote
/// back and a person can scan.
#[must_use]
pub fn format_grep_matches(matches: &[GrepMatch]) -> String {
    let groups = group_matches_by_file(matches);
    let mut sections: Vec<String> = Vec::with_capacity(groups.len());
    for group in groups {
        let mut section = String::with_capacity(group.path.len() + 1);
        section.push_str(&group.path);
        for line in &group.matches {
            section.push('\n');
            // Writing into a `String` cannot fail; the result is ignored rather than unwrapped so
            // the formatter has no panic path.
            let _ = write!(section, "Line {}: {}", line.line_number, line.line);
        }
        sections.push(section);
    }
    sections.join("\n\n")
}

/// Formats one `grep` result: a found-count header, the grouped matches, and a recovery footer.
///
/// Corresponds to upstream `grep.ts`'s `formatGrepOutput`. The header distinguishes the complete
/// answer from the capped one (`Found 12 of 300 matches`); the footer names where the complete
/// result went, or says plainly that it could not be saved. The omitted count is a budget fact, not
/// a failure: the search itself completed.
#[must_use]
pub fn format_grep_output(
    retained: &RetainedItems<GrepMatch>,
    saved: Option<&SpillReference>,
) -> String {
    let header = if retained.truncated {
        format!("Found {} of {} matches", retained.kept, retained.seen)
    } else {
        format!("Found {} {}", retained.seen, match_noun(retained.seen))
    };
    let body = format_grep_matches(&retained.items);
    let mut out = String::with_capacity(header.len() + body.len() + 128);
    out.push_str(&header);
    out.push_str("\n\n");
    out.push_str(&body);
    if !retained.truncated {
        return out;
    }
    out.push_str("\n\n(");
    out.push_str(&saved.map_or_else(missing_grep_recovery, |reference| {
        saved_recovery("Full grep result stored at", reference)
    }));
    out.push(')');
    out
}

/// Formats an already-retained match list, naming a zero-result search explicitly.
///
/// Corresponds to upstream `grep.ts`'s `formatRetainedGrep`. `Found 0 matches` reads like a failed
/// search; `No matches found` reads like an answer, and the distinction matters to a model deciding
/// whether to broaden the pattern.
#[must_use]
pub fn format_retained_grep(
    retained: &RetainedItems<GrepMatch>,
    saved: Option<&SpillReference>,
) -> String {
    if retained.seen == 0 {
        return "No matches found".to_owned();
    }
    format_grep_output(retained, saved)
}

/// The inline page of a capped `glob` result, plus how much of its top level it reaches.
///
/// Corresponds to upstream `glob.ts`'s `GlobSample`: `items` is the page itself, `shown` is how many
/// distinct top-level entries it reaches, and `total` is how many the complete result has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobSample {
    /// Paths to show inline, grouped by top-level entry and in input order within each group.
    pub items: Vec<String>,
    /// Distinct top-level entries the shown paths reach.
    pub shown: usize,
    /// Distinct top-level entries across the complete result.
    pub total: usize,
}

/// Chooses the inline page of an over-cap result by round-robin across top-level entries.
///
/// Corresponds to upstream `glob.ts`'s `sampleAcrossTopLevel`. Every top-level entry receives a
/// slot before any receives a second, and exhausted groups drop out, so a broad pattern's page
/// reflects the shape of the match instead of one subtree's head. Group order, and order within a
/// group, follow the input — a flat result therefore reproduces the modification-time head exactly.
///
/// `root` is the search root in the same display-path space as `paths`, so its prefix is stripped
/// before grouping; the default `.` leaves relative paths alone.
#[must_use]
pub fn sample_across_top_level(paths: &[String], max_items: usize, root: &str) -> GlobSample {
    // First pass: bucket by top-level entry, preserving first-seen group order.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for entry in paths {
        let key = top_level_segment(&relative_to_search_root(entry, root));
        if let Some(bucket) = groups.get_mut(&key) {
            bucket.push(entry.clone());
        } else {
            order.push(key.clone());
            groups.insert(key, vec![entry.clone()]);
        }
    }
    let total = order.len();

    // Second pass: one item per group per round until the page is full or every group is drained.
    let mut cursor: HashMap<String, usize> = HashMap::new();
    let mut taken: Vec<String> = Vec::new();
    let mut active = order;
    while !active.is_empty() && taken.len() < max_items {
        let mut next_active: Vec<String> = Vec::with_capacity(active.len());
        for key in active {
            if taken.len() >= max_items {
                break;
            }
            let at = cursor.get(&key).copied().unwrap_or(0);
            let Some(bucket) = groups.get(&key) else {
                continue;
            };
            let Some(entry) = bucket.get(at) else {
                continue;
            };
            taken.push(entry.clone());
            cursor.insert(key.clone(), at + 1);
            if at + 1 < bucket.len() {
                next_active.push(key);
            }
        }
        active = next_active;
    }

    GlobSample {
        shown: cursor.len(),
        items: taken,
        total,
    }
}

/// Formats a sampled page and its complete-result recovery path.
///
/// Corresponds to upstream `glob.ts`'s `formatGlobOutput`. When the sample covers every top-level
/// entry the footer stays plain — the page is then just the modification-time head and saying
/// otherwise would invent a caveat. Otherwise the footer states how much of the tree was sampled
/// and, when entries are still unreached, tells the caller to narrow the path.
#[must_use]
pub fn format_glob_output(
    sample: &GlobSample,
    seen: usize,
    saved: Option<&SpillReference>,
) -> String {
    let basis = if sample.total == seen {
        ".".to_owned()
    } else {
        let mut text = format!(
            ", sampled across {} of the {} top-level entries this pattern matched instead of taken in modification-time order.",
            sample.shown, sample.total
        );
        if sample.shown < sample.total {
            text.push_str(" Narrow path to inspect a specific subtree.");
        }
        text
    };
    format_glob_page(&sample.items, seen, saved, &basis)
}

/// Bounds and formats a complete path list for one `glob` result.
///
/// Corresponds to upstream `glob.ts`'s `renderGlobPaths`. Three outcomes, deliberately distinct:
/// no match is named as such; a result within the cap is returned whole and untouched; an over-cap
/// result is either the modification-time head or the top-level sample, according to
/// `sample_over_cap`. The head is still offered because a caller that asked for "what did I touch
/// recently" is better served by the head than by a balanced sample.
#[must_use]
pub fn render_glob_paths(
    paths: &[String],
    max_results: usize,
    root: &str,
    sample_over_cap: bool,
    saved: Option<&SpillReference>,
) -> String {
    if paths.is_empty() {
        return "No files found".to_owned();
    }
    // A result that fits is shown whole: over a complete result, modification-time order is the
    // answer to an age question and must survive.
    if paths.len() <= max_results {
        return paths.join("\n");
    }
    let seen = paths.len();
    if !sample_over_cap {
        let head: Vec<String> = paths.iter().take(max_results).cloned().collect();
        return format_glob_page(&head, seen, saved, ".");
    }
    format_glob_output(
        &sample_across_top_level(paths, max_results, root),
        seen,
        saved,
    )
}

/// Projects retained `grep` matches into card metadata, bounded to `max_meta_bytes`.
///
/// Corresponds to upstream `presentation.ts`'s `grepSearchMeta`. It consumes the same retained page
/// the model-facing text does — preview and item cap already applied — so the card cannot claim a
/// match the text omitted. `total` counts every match the search found, and the byte cap only drops
/// trailing groups.
#[must_use]
pub fn grep_search_meta(retained: &RetainedItems<GrepMatch>, max_meta_bytes: usize) -> SearchMeta {
    let meta = SearchMeta::Matches {
        files: group_matches_by_file(&retained.items),
        truncated: retained.truncated,
        total: retained.seen,
    };
    cap_meta_bytes(meta, max_meta_bytes)
}

/// Projects retained `glob` paths into card metadata, bounded to `max_meta_bytes`.
///
/// Corresponds to upstream `presentation.ts`'s `globSearchMeta`, the `paths`-shaped counterpart of
/// [`grep_search_meta`].
#[must_use]
pub fn glob_search_meta(retained: &RetainedItems<String>, max_meta_bytes: usize) -> SearchMeta {
    let meta = SearchMeta::Paths {
        paths: retained.items.clone(),
        truncated: retained.truncated,
        total: retained.seen,
    };
    cap_meta_bytes(meta, max_meta_bytes)
}

/// Bounds a serialized projection by dropping trailing items.
///
/// Corresponds to upstream `presentation.ts`'s `capMetaBytes`. At least one item is always kept: a
/// single group too large to fit is preferable to an empty card that hides a real result, so the
/// invariant is a bounded payload *wherever droppable*, never a silent zero. `total` is preserved
/// because it counts what was found, not what the payload retains.
fn cap_meta_bytes(meta: SearchMeta, max_meta_bytes: usize) -> SearchMeta {
    if meta_bytes(&meta) <= max_meta_bytes {
        return meta;
    }
    match meta {
        SearchMeta::Matches {
            mut files, total, ..
        } => {
            while files.len() > 1
                && meta_bytes(&SearchMeta::Matches {
                    files: files.clone(),
                    truncated: true,
                    total,
                }) > max_meta_bytes
            {
                files.pop();
            }
            SearchMeta::Matches {
                files,
                truncated: true,
                total,
            }
        }
        SearchMeta::Paths {
            mut paths, total, ..
        } => {
            while paths.len() > 1
                && meta_bytes(&SearchMeta::Paths {
                    paths: paths.clone(),
                    truncated: true,
                    total,
                }) > max_meta_bytes
            {
                paths.pop();
            }
            SearchMeta::Paths {
                paths,
                truncated: true,
                total,
            }
        }
    }
}

/// The serialized UTF-8 size of one projection — the quantity persisted and re-sent.
///
/// A serialization failure is treated as an over-budget payload, which makes the caller shrink it;
/// the alternative — claiming it fits — would forward an unbounded payload.
fn meta_bytes(meta: &SearchMeta) -> usize {
    serde_json::to_vec(&meta.to_value()).map_or(usize::MAX, |bytes| bytes.len())
}

/// Formats one bounded page, its size caveat, and the recovery path for the complete result.
///
/// Corresponds to upstream `glob.ts`'s `formatGlobPage`, shared by the sampled and the headed
/// rendering so both footers stay identical.
fn format_glob_page(
    items: &[String],
    seen: usize,
    saved: Option<&SpillReference>,
    basis: &str,
) -> String {
    let mut out = String::with_capacity(items.len() * 32 + 128);
    out.push_str(&items.join("\n"));
    out.push_str("\n\n(");
    let _ = write!(out, "Showing {} of {} paths{} ", items.len(), seen, basis);
    out.push_str(&saved.map_or_else(missing_glob_recovery, |reference| {
        saved_recovery("Full sorted result stored at", reference)
    }));
    out.push(')');
    out
}

/// The recovery sentence for a saved complete result.
///
/// Corresponds to the `spillRef.locator` / `spillRef.retrievalHint` interpolation upstream does in
/// `formatGlobPage` and `formatGrepOutput`; this crate's [`SpillReference`] carries the same two
/// facts as `path` and `hint`.
fn saved_recovery(prefix: &str, reference: &SpillReference) -> String {
    format!("{prefix}: {}. {}", reference.path.display(), reference.hint)
}

/// The footer used when the complete result could not be saved for `glob`.
///
/// Corresponds to upstream `glob.ts`'s fallback text. The result is still useful, so the footer
/// names the next narrowing step rather than an error.
fn missing_glob_recovery() -> String {
    "The complete result could not be saved; narrow pattern or path to see more.".to_owned()
}

/// The footer used when the complete result could not be saved for `grep`.
///
/// Corresponds to upstream `grep.ts`'s fallback text, which also names the `include` filter.
fn missing_grep_recovery() -> String {
    "The complete result could not be saved; narrow pattern, path, or include to see more."
        .to_owned()
}

/// `match` / `matches` for a count, so the header does not read `Found 1 matches`.
///
/// Corresponds to upstream `grep.ts`'s `matchNoun`.
fn match_noun(count: u64) -> &'static str {
    if count == 1 { "match" } else { "matches" }
}

/// Strips the displayed search-root prefix before choosing a top-level group.
///
/// Corresponds to upstream `glob.ts`'s `relativeToSearchRoot`. Trailing separators are tolerated so
/// `src/` and `src` name the same root; a path that is not under the root is left untouched rather
/// than silently regrouped.
fn relative_to_search_root(path: &str, root: &str) -> String {
    if root == "." {
        return path.strip_prefix("./").unwrap_or(path).to_owned();
    }
    let trimmed = root.trim_end_matches('/');
    if trimmed.is_empty() {
        return path.trim_start_matches('/').to_owned();
    }
    if path == trimmed {
        return String::new();
    }
    match path
        .strip_prefix(trimmed)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        Some(rest) => rest.to_owned(),
        None => path.to_owned(),
    }
}

/// The leading path segment of one display path — its top-level entry.
///
/// Corresponds to upstream `glob.ts`'s `topLevelSegment`. Leading separators are stripped first so
/// an absolute path (one outside the workdir, which [`to_workdir_relative`] leaves untouched) groups
/// by its first real name instead of collapsing every such path into one empty group.
fn top_level_segment(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    match trimmed.find('/') {
        Some(cut) => trimmed[..cut].to_owned(),
        None => trimmed.to_owned(),
    }
}

/// Computes a lexical relative path from `from` to `to`.
///
/// Mirrors Node's `path.relative` for the case this module needs: both paths must be absolute and
/// share a root, otherwise `None`. Resolution is purely lexical — the point is a display string, and
/// touching the filesystem here would make a search's presentation depend on what still exists.
fn lexical_relative(from: &Path, to: &Path) -> Option<String> {
    let (from_root, from_segments) = lexical_segments(from)?;
    let (to_root, to_segments) = lexical_segments(to)?;
    if from_root != to_root {
        return None;
    }
    let common = from_segments
        .iter()
        .zip(to_segments.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut segments: Vec<&str> = Vec::with_capacity(from_segments.len() + to_segments.len());
    // One `..` per segment that has to be climbed out of; `resize` expresses "this many copies"
    // without a loop whose body ignores its own counter.
    segments.resize(from_segments.len().saturating_sub(common), "..");
    for segment in &to_segments[common..] {
        segments.push(segment);
    }
    Some(segments.join("/"))
}

/// Splits an absolute path into its root identity and its normalized segments.
///
/// `None` for a relative path: a relative path has no root to compare, and `..` cannot be resolved
/// without one. `.` is dropped and `..` pops, which is what a lexical comparison requires.
fn lexical_segments(path: &Path) -> Option<(String, Vec<String>)> {
    if !path.is_absolute() {
        return None;
    }
    let mut root = String::new();
    let mut segments: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                root.push_str(&prefix.as_os_str().to_string_lossy());
            }
            Component::RootDir => {
                if !root.ends_with('/') {
                    root.push('/');
                }
            }
            Component::CurDir => {}
            Component::ParentDir => match segments.last() {
                Some(last) if last != ".." => {
                    segments.pop();
                }
                // Above the root there is nothing to pop: `/..` is `/`, and inventing a `..` here
                // would produce a display path that does not name a real location.
                _ => {}
            },
            Component::Normal(name) => segments.push(name.to_string_lossy().into_owned()),
        }
    }
    Some((root, segments))
}
