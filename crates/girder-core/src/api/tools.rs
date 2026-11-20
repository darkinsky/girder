//! `@file` reference completion: the `@` token grammar and the workspace index behind it.
//!
//! # The gap, measured rather than asserted
//!
//! The report this module answers claimed nine upstream `@Remote` methods were missing — the
//! residue of thirty-one after twenty `account/*` ones were written off as unportable. Each of the
//! nine was checked against the tree, and the result splits three ways:
//!
//! | upstream method | local state | why |
//! |---|---|---|
//! | `settings.describe` | declared gap, **not missing** | `settings_rpc` registers it through `RemoteRegistry::declare_unimplemented`, and `crate::settings::config_edit::SchemaRegistry::describe` already renders the answer it needs |
//! | `terminal.rename` | declared gap, **not missing** | `terminal_rpc`'s `UNIMPLEMENTED` names it; `crate::terminal::meta::Store::rename` already validates and stores the title |
//! | `terminal.watch` | declared gap, **not missing** | `terminal_rpc_extra` declares it and a test asserts the declaration |
//! | `settings.openSettingsDocument` | declared gap, **not missing** | `crate::settings::rpc::UNIMPLEMENTED` names it; unportable (native text editor) |
//! | `session.control`, `session.wait` | absent, and correctly so | both are `stream` methods, and [`RemoteRegistry`](girder_core::RemoteRegistry) has no stream mode; `session_state_rpc` implements the single-shot `state` half that `wait` would settle against |
//! | `remoteAccess.follow` | absent, and deliberately so | a `stream` method, **and** its semantics are already ported: [`remote_access_gap`](crate::api::remote_access) covers per-call resolution and names `AccessFollower` in its own coverage table |
//! | `skills.list` | absent, but nearly covered | [`skill_catalog`](crate::skill::catalog) discovers and [`skill_gap`](crate::skill::policy) parses the invocation policy; only the wire projection is missing |
//! | `fileReferences.list` | **absent, with no local counterpart at all** | `context_gap`'s own coverage table says so for the whole `file-reference` + `file-reference-local` pair |
//!
//! So six of the nine are *not* missing: four are declared gaps with a local half already built,
//! and two are stream methods whose semantics live elsewhere. What follows implements the one that
//! is genuinely absent end to end — and the one whose port is pure decision logic rather than
//! plumbing.
//!
//! # What upstream has
//!
//! `fileReferences.list` is a thin Remote wrapper (`packages/api/session-controller/src/file-references.ts`,
//! 48 lines) over a service pair this crate had nothing for:
//!
//! - `packages/context/file-reference` (113 lines) — the `@` token grammar a client edits against,
//!   and the prompt guidance a selected path becomes.
//! - `packages/context/file-reference-local` (523 lines) — a bounded, cancellable fuzzy index over
//!   one workspace root, and the directory-scoped listing that bypasses it.
//!
//! This module ports both halves that hold decisions. It is not a service: [`list`] and
//! [`WorkspaceFileSearch::list`] take a root and a query and answer, so a caller can use them without
//! a Context, and the same functions back both the Remote method and the tests.
//!
//! # The four decisions that must not be guessed
//!
//! 1. **The query decides the mode, and the switch is the last `/`.** A query containing a slash is
//!    a *directory listing* of the text before it, filtered by the text after it — live state, one
//!    `readdir`, not the index. A query with no slash is a *global fuzzy search* ranked over the
//!    whole index. The empty query is a directory listing of the root, not an empty search.
//! 2. **`..` out of the root is refused, and the refusal is empty rather than an error.** The check
//!    is lexical first, then per segment against the real directory: a path that resolves outside
//!    the root yields no candidates, and a symlinked directory inside the root yields none either,
//!    because [`std::fs::symlink_metadata`] is what is asked. Autocomplete is advisory; a listing
//!    that failed is not worth an error the caret has to render.
//! 3. **The ranking is a total order, and its tie-breaks are load-bearing.** Score descending, then
//!    directories before files, then — for a non-empty query only — shorter path first, then byte
//!    order. Dropping the length tie-break reorders `main.rs` and `src/main.rs` in the answer, which
//!    is the difference between the first candidate being the file the person meant and not.
//! 4. **A hidden path is listed only when the query asked for it.** A bare query hides any path with
//!    a dot-segment unless the query itself starts with a dot or contains `/.`; a directory listing
//!    hides dot-entries unless the *fragment* starts with a dot. Two separate rules, because the
//!    two modes see different things.
//!
//! # What this module deliberately does not port
//!
//! - **The index's lifetime.** Upstream keeps one [`WorkspaceFileSearch`] per Agent, rebuilds it in
//!   the background when a tool result invalidates it, and keeps answering from stale entries while
//!   the rebuild runs. That is a cache policy over a service the caller owns; here the traversal is
//!   a function of `(root, config)` and a caller that wants caching owns it. What *is* ported is the
//!   bound the cache exists to protect: [`Config::max_entries`] stops the walk.
//! - **Cancellation.** Upstream threads an `AbortSignal` through every `readdir`. [`CancellationToken`](girder_core::CancellationToken)
//!   is checked at the same points the signal was (`throwIfAborted` before and after each directory,
//!   and before each entry), so a cancelled call stops where upstream's would.
//! - **`node_modules`-adjacent hygiene that is not a decision.** The exclusion list is data, not
//!   logic: [`DEFAULT_EXCLUDED_DIRECTORIES`] is upstream's array, and `lib` is absent from it for the
//!   reason upstream gives.
//!
//! # Why the traversal is blocking inside an async method
//!
//! [`WorkspaceFileSearch::list`] is `async` because its caller is, but the walk itself is
//! [`std::fs`] on the calling thread: the method is `async` for the cancellation points and for the
//! shape a `RemoteRegistry` handler needs, not because it awaits anything. A real deployment that
//! indexes a large tree should run it under [`tokio::task::spawn_blocking`]; the crate's other
//! filesystem-facing RPC modules make the same call, and inventing a second, narrower IO abstraction
//! here would make this module untestable without it.
//!
//! # Compilation note
//!
//! The crate root does not declare this module yet; the integrator adds `pub mod api_tools_gap;`
//! and the `pub use`. Library items are spelled `girder_core::…` because the test binary compiles
//! this exact file by `#[path]`, where a bare `crate::` would name the *test* crate.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use girder_core::{CancellationToken, Declaration, RemoteCall, RemoteError, RemoteRegistry};

/// The namespace `fileReferences.list` lives under.
pub const NAMESPACE: &str = "fileReferences";

/// The one method under it.
pub const LIST_METHOD: &str = "list";

/// The resource kind an access policy is told about.
///
/// `workspace` rather than `session`: the answer is computed from a registered root, and a policy
/// that refused one workspace while allowing another is exactly the shape this names.
pub const RESOURCE_KIND: &str = "workspace";

/// How many candidates one query returns when the caller does not say.
///
/// Upstream `DEFAULT_FILE_SEARCH_MAX_RESULTS`. 20 fills a completion popup several times over and
/// keeps a keystroke's answer small enough to send on every character typed.
pub const DEFAULT_MAX_RESULTS: usize = 20;

/// How many entries one workspace index keeps when the caller does not say.
///
/// Upstream `DEFAULT_FILE_SEARCH_MAX_ENTRIES`. 50,000 entries is a large monorepo; the bound exists
/// so that indexing a checkout of a giant repository costs a bounded amount of memory and time
/// rather than whatever the tree happens to contain.
pub const DEFAULT_MAX_ENTRIES: usize = 50_000;

/// Directory basenames never traversed and never offered, when the caller does not say otherwise.
///
/// Upstream's array, in upstream's order. `lib` is deliberately absent: Ruby gems and many npm
/// packages keep their sources there, and excluding it would make `@` miss those sources silently.
/// A workspace that builds into `lib` adds it through [`Config::excluded_directories`].
pub const DEFAULT_EXCLUDED_DIRECTORIES: [&str; 15] = [
    ".git",
    "node_modules",
    "dist",
    "build",
    "out",
    "coverage",
    "target",
    ".next",
    ".nuxt",
    ".turbo",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".gradle",
];

/// The control characters [`format_file_mention`] refuses, as upstream's regex spells them.
///
/// `[\u0000-\u001f\u007f-\u009f"]` — C0 controls, DEL, C1 controls, and the double quote that would
/// close the quoted form early. Written as code-point ranges rather than a byte table because the
/// check runs once per candidate, not once per byte.
fn is_unrepresentable(character: char) -> bool {
    matches!(character,
        '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}' | '"')
}

/// The JavaScript `\s` set, which is what upstream's `(?:^|\s)` means.
///
/// Spelled out rather than approximated with [`char::is_whitespace`]: the two differ. `char::is_whitespace`
/// accepts U+0085 and U+001C–U+001F, which JavaScript's `\s` does not, and upstream's separator is
/// the JavaScript one. A Rust approximation that accepted U+0085 would start a completion where the
/// real client does not.
const fn is_js_space(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// Whether one path belongs to a candidate list at all.
///
/// The two shapes the index holds, and nothing else. Spelling them as an enum rather than a `bool`
/// keeps the wire value (`"file"`/`"directory"`) from being re-derived at each of the three places
/// that need it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// A directory: accepting it keeps completion open.
    Directory,
    /// A file: accepting it finishes the mention.
    File,
}

impl Kind {
    /// The wire spelling, which is also the sort key's first component as upstream spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
        }
    }

    /// The tie-break rank: directories before files.
    ///
    /// Not cosmetic. A query that matches a directory and a file equally well should offer the
    /// directory first, because a directory can be descended *and* names everything below it.
    #[must_use]
    fn rank(self) -> u8 {
        match self {
            Self::Directory => 0,
            Self::File => 1,
        }
    }
}

/// One path a client may insert into the prompt.
///
/// Path-only by construction: upstream keeps file *contents* behind the `read` tool, and a
/// candidate that carried contents would put the whole workspace through the completion socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The path as the client should show and insert it — root-relative, with forward slashes.
    pub path: String,
    /// Whether it is a file or a directory.
    pub kind: Kind,
}

impl Candidate {
    /// Builds a candidate.
    #[must_use]
    pub fn new(path: impl Into<String>, kind: Kind) -> Self {
        Self {
            path: path.into(),
            kind,
        }
    }

    /// The wire shape upstream's `FileReferenceCandidate` has: two fields, no `null`s.
    #[must_use]
    pub fn to_value(&self) -> Value {
        json!({ "path": self.path, "kind": self.kind.as_str() })
    }
}

/// The resolved limits and exclusions for one workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Maximum ranked candidates one query returns.
    pub max_results: usize,
    /// Maximum entries one index keeps.
    pub max_entries: usize,
    /// Directory basenames never traversed and never offered.
    pub excluded_directories: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_results: DEFAULT_MAX_RESULTS,
            max_entries: DEFAULT_MAX_ENTRIES,
            excluded_directories: DEFAULT_EXCLUDED_DIRECTORIES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        }
    }
}

/// Why a [`Config`] is unusable.
///
/// Upstream throws from the constructor and from the per-Agent service factory; the strings below
/// are its messages, kept because a deployment that misconfigured an exclusion should be told which
/// rule it broke rather than that "the config is invalid".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// `max_results` is not a positive, representable integer.
    MaxResults,
    /// `max_entries` is not a positive, representable integer.
    MaxEntries,
    /// An excluded name is empty or names a path rather than a basename.
    ExcludedDirectory {
        /// The offending entry.
        name: String,
    },
}

impl ConfigError {
    /// Upstream's message for this failure, so a deployment reading either gets the same sentence.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::MaxResults => "file search maxResults must be a positive safe integer".to_owned(),
            Self::MaxEntries => "file search maxEntries must be a positive safe integer".to_owned(),
            Self::ExcludedDirectory { .. } => {
                "file search excludedDirectories entries must be non-empty directory basenames"
                    .to_owned()
            }
        }
    }
}

impl Config {
    /// Checks the three rules upstream checks before it builds an index.
    ///
    /// `max_results` and `max_entries` are `usize`, so "a positive safe integer" reduces to "not
    /// zero": a value the caller could not have produced does not need a branch here.
    ///
    /// # Errors
    ///
    /// [`ConfigError::MaxResults`], [`ConfigError::MaxEntries`], or
    /// [`ConfigError::ExcludedDirectory`] naming the first entry that is empty or contains a
    /// separator.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_results == 0 {
            return Err(ConfigError::MaxResults);
        }
        if self.max_entries == 0 {
            return Err(ConfigError::MaxEntries);
        }
        for name in &self.excluded_directories {
            if name.is_empty() || name.contains('/') || name.contains('\\') {
                return Err(ConfigError::ExcludedDirectory { name: name.clone() });
            }
        }
        Ok(())
    }

    /// Whether one directory basename is excluded.
    #[must_use]
    pub fn excludes(&self, name: &str) -> bool {
        self.excluded_directories.iter().any(|entry| entry == name)
    }
}

// ---- the `@` token grammar ----------------------------------------------------------------------

/// The `@path` token the caret sits in.
///
/// Upstream `ActiveAtToken`. `prefix` is the whole token as it currently stands — what an accepted
/// candidate replaces — while `query` is only the path text after `@` or `@"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveToken {
    /// The complete token, replaced wholesale when the user accepts a completion.
    pub prefix: String,
    /// The path query after `@` or `@"`.
    pub query: String,
    /// Whether the user opened a quoted path.
    pub quoted: bool,
}

/// Finds the `@` token ending at the caret.
///
/// Returns `None` outside a token. The leading `(?:^|\s)` in upstream's pattern is what makes an `@`
/// inside another token — an email address, most often — not a completion trigger, and it is why
/// `a@b` yields nothing while `a @b` yields `@b`.
///
/// # Why the quoted form is tried first
///
/// Upstream tries `(?:^|\s)(@"([^"]*))$` before `(?:^|\s)(@([^\s]*))$`. The second pattern also
/// matches inside a quoted token and would stop at the space, so trying it first would truncate
/// `@"src/my dir` to `@"src/my`. Order is the whole rule, not an optimisation.
///
/// `cursor_col` is a **character** index, matching JavaScript's UTF-16 code-unit index for the
/// text upstream deals with (paths and prose below the astral plane); [`char`] indexing is chosen
/// here because a byte index would split a multi-byte character and a code-unit index cannot be
/// expressed in safe Rust without re-encoding.
#[must_use]
pub fn active_at_token(line: &str, cursor_col: usize) -> Option<ActiveToken> {
    let before: String = line.chars().take(cursor_col).collect();
    if let Some(token) = quoted_token(&before) {
        return Some(token);
    }
    plain_token(&before)
}

/// The quoted form: `(?:^|\s)(@"([^"]*))$` anchored at the end.
fn quoted_token(before: &str) -> Option<ActiveToken> {
    let after_at = after_separator(before, true)?;
    // `[^"]*` between `@"` and the end: the token body may not contain a quote, and the `@` is
    // already consumed by `after_separator`.
    if after_at.contains('"') {
        return None;
    }
    Some(ActiveToken {
        prefix: format!("@\"{after_at}"),
        query: after_at.to_owned(),
        quoted: true,
    })
}

/// The plain form: `(?:^|\s)(@([^\s]*))$` anchored at the end.
fn plain_token(before: &str) -> Option<ActiveToken> {
    let after_at = after_separator(before, false)?;
    if after_at.chars().any(is_js_space) {
        return None;
    }
    Some(ActiveToken {
        prefix: format!("@{after_at}"),
        query: after_at.to_owned(),
        quoted: false,
    })
}

/// The body of the last `<start-of-line-or-space>@<body>` suffix of `before`.
///
/// Anchoring at the end is what makes this a scan rather than a match: only an `@` whose body runs to
/// the caret can start the token. Walking back from the end is equivalent to the regex engine's
/// leftmost scan here, and the equivalence is worth stating because it is not obvious: whenever an
/// earlier `@` also runs to the caret, the later `@` lies inside its body, so the earlier candidate's
/// body contains either a space (plain form) or a `"` (quoted form) and fails `before` ever reaching
/// it. Only the last candidate can satisfy the pattern's own body restriction.
fn after_separator(before: &str, quoted: bool) -> Option<&str> {
    let opener = if quoted { "@\"" } else { "@" };
    for (index, _) in before.match_indices('@').rev() {
        let after_at = before.get(index..)?;
        if !after_at.starts_with(opener) {
            continue;
        }
        let preceded_ok = index == 0
            || before
                .get(..index)
                .and_then(|prefix| prefix.chars().next_back())
                .is_some_and(is_js_space);
        if !preceded_ok {
            continue;
        }
        return before.get(index + opener.len()..);
    }
    None
}

/// Formats a selected candidate as prompt text.
///
/// Upstream `formatFileMention`. Whitespace in a path uses the quoted `@"path"` grammar; a quoted
/// *directory* keeps the quote open after its trailing slash so completion can descend another
/// level. Returns `None` for a path the grammar cannot represent — one carrying a control character
/// or a double quote — rather than emitting a token that would not parse back.
#[must_use]
pub fn format_file_mention(candidate: &Candidate, preserve_quote: bool) -> Option<String> {
    let path = if candidate.kind == Kind::Directory {
        format!("{}/", candidate.path)
    } else {
        candidate.path.clone()
    };
    if path.chars().any(is_unrepresentable) {
        return None;
    }
    let quoted = preserve_quote || path.chars().any(is_js_space);
    if !quoted {
        return Some(format!("@{path}"));
    }
    if candidate.kind == Kind::Directory {
        return Some(format!("@\"{path}"));
    }
    Some(format!("@\"{path}\""))
}

// ---- the workspace index ------------------------------------------------------------------------

/// A bounded fuzzy index over one workspace root.
///
/// Not a service and not a cache by default: [`list`](Self::list) walks the tree when a bare query
/// needs the index. A caller that wants upstream's stale-while-rebuilding behaviour wraps this, which
/// is what upstream's own per-Agent map does.
#[derive(Clone, Debug)]
pub struct WorkspaceFileSearch {
    root: PathBuf,
    config: Config,
    excluded: BTreeSet<String>,
}

impl WorkspaceFileSearch {
    /// Builds an index over `root`.
    ///
    /// # Errors
    ///
    /// Whatever [`Config::validate`] rejects.
    pub fn new(root: impl Into<PathBuf>, config: Config) -> Result<Self, ConfigError> {
        config.validate()?;
        let excluded = config.excluded_directories.iter().cloned().collect();
        Ok(Self {
            root: root.into(),
            config,
            excluded,
        })
    }

    /// The config this index was built with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Whether one directory basename is excluded.
    fn excludes(&self, name: &str) -> bool {
        self.excluded.contains(name)
    }

    /// Ranked path candidates for the current token.
    ///
    /// The mode switch is the last `/`: a query that contains one is a listing of the text before it
    /// filtered by the text after it, and a query that does not is a ranked search over the index.
    /// The empty query lists the root.
    ///
    /// # Errors
    ///
    /// Nothing. An unreadable root, a directory outside the root, and a missing subdirectory all
    /// produce an empty list: completion is advisory, and upstream makes the same choice.
    #[must_use]
    pub fn list(&self, raw_query: &str, cancel: &CancellationToken) -> Vec<Candidate> {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        // Windows separators are normalised before anything else, so `src\ma` behaves as `src/ma`.
        let query = raw_query.replace('\\', "/");
        match query.rfind('/') {
            None if query.is_empty() => self.list_directory("", "", cancel),
            None => {
                let indexed = self.index(cancel);
                rank(
                    indexed
                        .into_iter()
                        .filter(|candidate| visible_for_global_query(&candidate.path, &query))
                        .collect(),
                    &query,
                    self.config.max_results,
                )
            }
            Some(slash) => {
                let directory = query.get(..=slash).unwrap_or_default();
                let fragment = query.get(slash + 1..).unwrap_or_default();
                self.list_directory(directory, fragment, cancel)
            }
        }
    }

    /// The entries of one real directory, filtered by `fragment` and ranked.
    fn list_directory(
        &self,
        display_directory: &str,
        fragment: &str,
        cancel: &CancellationToken,
    ) -> Vec<Candidate> {
        // Upstream refuses a display directory with an excluded segment before touching the disk, so
        // `@node_modules/` costs nothing and cannot be used to enumerate what the index skips.
        if display_directory
            .split('/')
            .any(|segment| self.excludes(segment))
        {
            return Vec::new();
        }
        let Some(absolute) = self.resolve_display_directory(display_directory, cancel) else {
            return Vec::new();
        };
        let mut candidates: Vec<Candidate> = Vec::new();
        for entry in read_sorted(&absolute, cancel) {
            let name = entry.0;
            if name.starts_with('.') && !fragment.starts_with('.') {
                continue;
            }
            if entry.1 == Kind::Directory && self.excludes(&name) {
                continue;
            }
            candidates.push(Candidate::new(
                format!("{display_directory}{name}"),
                entry.1,
            ));
        }
        rank(candidates, fragment, self.config.max_results)
    }

    /// Maps a display path onto a real one inside the root, or refuses it.
    ///
    /// Two checks, in the order upstream performs them: the resolved path must be inside the root
    /// (lexically, which is what refuses `../etc`), and every segment must be a real directory
    /// (**not** a symlink, which is what refuses a link planted inside the workspace). The second
    /// check cannot be folded into the first: [`std::fs::canonicalize`] would resolve the link
    /// instead of refusing it, which is the hole upstream's `lstat` walk exists to close.
    ///
    /// The root-relative arithmetic is textual because the *display* form is textual: `src/../docs`
    /// and `docs` must compare equal, and a `PathBuf` comparison alone would not say so.
    fn resolve_display_directory(
        &self,
        display_directory: &str,
        cancel: &CancellationToken,
    ) -> Option<PathBuf> {
        let root_text = normalize_text(&self.root);
        let joined = if display_directory.is_empty() {
            root_text.clone()
        } else if display_directory.starts_with('/') {
            // Node's `path.resolve(root, absolute)` returns the absolute argument, ignoring the root.
            collapse(display_directory)
        } else {
            collapse(&format!(
                "{}/{display_directory}",
                root_text.trim_end_matches('/')
            ))
        };
        let relative = joined.strip_prefix(&root_text)?;
        // `strip_prefix` on text is not a path operation: `/ws2` is a prefix of `/ws2extra`. The
        // remainder must be empty or begin with the separator, which is what makes it a descendant.
        if !relative.is_empty() && !relative.starts_with('/') {
            return None;
        }
        let relative = relative.strip_prefix('/').unwrap_or(relative);
        let mut current = self.root.clone();
        for segment in relative.split('/').filter(|segment| !segment.is_empty()) {
            if cancel.is_cancelled() {
                return None;
            }
            current.push(segment);
            let metadata = std::fs::symlink_metadata(&current).ok()?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return None;
            }
        }
        Some(current)
    }

    /// Every entry the index holds, bounded by [`Config::max_entries`].
    ///
    /// Breadth-first, in the order a queue produces, because that is what upstream's `directories`
    /// array does — and the order decides *which* entries survive the bound when a tree is larger
    /// than the budget. Upstream's claim is documented in its own test: `maxEntries` below the number
    /// of root entries yields nothing, because the root's own listing fills the budget before any
    /// subdirectory is queued.
    fn index(&self, cancel: &CancellationToken) -> Vec<Candidate> {
        let mut indexed: Vec<Candidate> = Vec::new();
        let mut queue: Vec<(PathBuf, String)> = vec![(self.root.clone(), String::new())];
        let mut cursor = 0;
        while cursor < queue.len() && indexed.len() < self.config.max_entries {
            if cancel.is_cancelled() {
                break;
            }
            let Some((absolute, relative)) = queue.get(cursor).cloned() else {
                break;
            };
            let is_root = cursor == 0;
            cursor += 1;
            // The root is not a subtree: an unreadable branch costs its own candidates, but an
            // unreadable root means the traversal learned nothing, and upstream settles nothing
            // rather than publishing an empty index over entries that are still good.
            let entries = if is_root {
                match read_root(&absolute) {
                    Ok(entries) => entries,
                    Err(()) => return Vec::new(),
                }
            } else {
                read_sorted(&absolute, cancel)
            };
            for (name, kind) in entries {
                if cancel.is_cancelled() {
                    break;
                }
                let path = if relative.is_empty() {
                    name.clone()
                } else {
                    format!("{relative}/{name}")
                };
                if kind == Kind::Directory {
                    if self.excludes(&name) {
                        continue;
                    }
                    indexed.push(Candidate::new(path.clone(), kind));
                    queue.push((absolute.join(&name), path));
                } else {
                    indexed.push(Candidate::new(path, kind));
                }
                if indexed.len() >= self.config.max_entries {
                    break;
                }
            }
        }
        indexed
    }
}

/// Every entry of one directory, sorted by name, or nothing when it cannot be read.
///
/// Sorted because enumeration order is a fact about the filesystem, not about the answer: two hosts
/// with the same tree must rank identically, and the tie-break by path is only a tie-break if the
/// input is deterministic.
fn read_sorted(absolute: &Path, cancel: &CancellationToken) -> Vec<(String, Kind)> {
    if cancel.is_cancelled() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(absolute) else {
        return Vec::new();
    };
    let mut out: Vec<(String, Kind)> = Vec::new();
    for entry in entries.flatten() {
        if cancel.is_cancelled() {
            return Vec::new();
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        // `file_type` from a directory entry is what `lstat` reports, so a symlink is neither a file
        // nor a directory here and is skipped — which is exactly upstream's behaviour for the bare
        // index: a linked directory's contents are not indexed under either name.
        if file_type.is_dir() {
            out.push((name, Kind::Directory));
        } else if file_type.is_file() {
            out.push((name, Kind::File));
        }
    }
    out.sort_by(|left, right| left.0.cmp(&right.0));
    out
}

/// The root listing, where failure is fatal rather than empty.
///
/// Separate from [`read_sorted`] because the two have different contracts, not because they differ in
/// code: an unreadable subdirectory contributes nothing and the rest of the answer is still useful,
/// while an unreadable root means the index would be published empty and never retried.
fn read_root(absolute: &Path) -> Result<Vec<(String, Kind)>, ()> {
    let entries = std::fs::read_dir(absolute).map_err(|_| ())?;
    let mut out: Vec<(String, Kind)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            out.push((name, Kind::Directory));
        } else if file_type.is_file() {
            out.push((name, Kind::File));
        }
    }
    out.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(out)
}

/// Lexically collapses `.` and `..` segments without touching the filesystem.
///
/// Applied after the root and the display directory are joined, so `src/../docs` and `docs` are the
/// same path and the containment test compares two normalised strings. A leading `..` that would
/// climb above the root is kept, which is what makes the `strip_prefix` test fail and the query be
/// refused rather than answered against `/`.
fn collapse(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// The root as a forward-slashed string, which is the form display paths are compared against.
fn normalize_text(path: &Path) -> String {
    let mut out = String::new();
    for component in path.components() {
        match component {
            Component::RootDir => out.push('/'),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str("..");
            }
            Component::Normal(part) => {
                if !out.is_empty() && !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str(&part.to_string_lossy());
            }
            Component::Prefix(prefix) => {
                out.push_str(&prefix.as_os_str().to_string_lossy());
            }
        }
    }
    if out.is_empty() { "/".to_owned() } else { out }
}

/// Whether a bare query may see this indexed path.
///
/// A query that itself mentions a dot directory opts into hidden paths; otherwise any path with a
/// dot-segment is withheld. Two directions of one rule: `query.starts_with('.')` opts in for the
/// first segment, `query.contains("/.")` for a later one.
fn visible_for_global_query(path: &str, query: &str) -> bool {
    if query.starts_with('.') || query.contains("/.") {
        return true;
    }
    !path.split('/').any(|segment| segment.starts_with('.'))
}

/// Sorts by the four tie-breaks upstream sorts by, then keeps the first `limit`.
///
/// A total order, so the answer does not depend on the traversal's order once the scores tie. The
/// length tie-break applies only to a non-empty query: for the empty query every candidate scores
/// zero and the length order would put `a` before `docs`, which is not what a listing means.
fn rank(candidates: Vec<Candidate>, query: &str, limit: usize) -> Vec<Candidate> {
    let mut scored: Vec<(Candidate, u32)> = candidates
        .into_iter()
        .filter_map(|candidate| score(&candidate, query).map(|score| (candidate, score)))
        .collect();
    scored.sort_by(|(left, left_score), (right, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.kind.rank().cmp(&right.kind.rank()))
            .then_with(|| {
                if query.is_empty() {
                    std::cmp::Ordering::Equal
                } else {
                    left.path.len().cmp(&right.path.len())
                }
            })
            .then_with(|| left.path.cmp(&right.path))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(candidate, _)| candidate)
        .collect()
}

/// One candidate's score, or `None` when it does not match at all.
///
/// The bands are upstream's, and their relative sizes are the decision: an exact basename match beats
/// a prefix beats a substring of the basename beats a substring of the whole path beats a
/// subsequence. A directory carries `+25` in every band, so a directory that ties a file wins — and
/// cannot overtake one a band above it, since the bands are 200 apart.
///
/// Lowercased before comparison because a path is a name on a case-insensitive filesystem as often as
/// not, and a user who typed `README` meant `readme.md`.
fn score(candidate: &Candidate, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let lowered_path = candidate.path.to_lowercase();
    let name = lowered_path
        .rsplit_once('/')
        .map_or(lowered_path.as_str(), |(_, name)| name);
    let needle = query.to_lowercase();
    let directory_bonus = if candidate.kind == Kind::Directory {
        25
    } else {
        0
    };
    if name == needle {
        return Some(1_000 + directory_bonus);
    }
    if name.starts_with(&needle) {
        return Some(900 + directory_bonus);
    }
    if name.contains(&needle) {
        return Some(700 + directory_bonus);
    }
    if lowered_path.contains(&needle) {
        return Some(500 + directory_bonus);
    }
    subsequence_score(&lowered_path, &needle).map(|score| 300 + score + directory_bonus)
}

/// How tightly `query` appears as a subsequence of `target`, or `None` when it does not.
///
/// `100 - gap`, floored at zero, where `gap` is the number of skipped characters. A tighter match
/// scores higher, and a match with 100 or more skipped characters scores zero — still a match, so it
/// is offered last rather than dropped.
fn subsequence_score(target: &str, query: &str) -> Option<u32> {
    let target: Vec<char> = target.chars().collect();
    let mut target_index = 0_usize;
    let mut gap = 0_usize;
    for character in query.chars() {
        let found = target
            .get(target_index..)?
            .iter()
            .position(|candidate| *candidate == character)?;
        gap += found;
        target_index += found + 1;
    }
    Some(u32::try_from(100_usize.saturating_sub(gap)).unwrap_or(0))
}

// ---- the Remote method ---------------------------------------------------------------------------

/// The arguments of one `fileReferences.list` call.
///
/// Upstream's wire shape, read off `typert.remote-client.d.ts`: the positional parameters
/// `(agent: Agent, query: string, signal)` are named `agentId` and `query` on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListRequest {
    /// The Session whose workspace the query is resolved against.
    pub agent_id: String,
    /// The path text after `@` or `@"`.
    pub query: String,
}

/// Reads the arguments of a `fileReferences.list` call.
///
/// Hand-read rather than `serde`-derived so a missing or mis-typed field names itself: a client that
/// sent `{ query: 3 }` should be told the query is not a string, not that the request object did not
/// match. `agentId` is required for the reason it exists — the root comes from the Session — and an
/// empty one is refused, because `""` is what a missing field degrades to in a language that
/// stringifies `undefined`.
///
/// # Errors
///
/// [`RemoteError::InvalidArguments`] when the arguments are not an object, when `agentId` is absent,
/// empty, or not a string, or when `query` is present but not a string.
pub fn parse_request(args: &Value) -> Result<ListRequest, RemoteError> {
    let invalid = |message: String| RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: LIST_METHOD.to_owned(),
        message,
    };
    let Some(object) = args.as_object() else {
        return Err(invalid(format!(
            "the arguments of `{NAMESPACE}.{LIST_METHOD}` must be a JSON object, not {args}"
        )));
    };
    let agent_id = match object.get("agentId") {
        Some(Value::String(id)) if !id.is_empty() => id.clone(),
        Some(Value::String(_)) => {
            return Err(invalid(
                "`agentId` must be a non-empty Session identity".to_owned(),
            ));
        }
        Some(other) => {
            return Err(invalid(format!("`agentId` must be a string, not {other}")));
        }
        None => {
            return Err(invalid("`agentId` is required".to_owned()));
        }
    };
    let query = match object.get("query") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(query)) => query.clone(),
        Some(other) => {
            return Err(invalid(format!("`query` must be a string, not {other}")));
        }
    };
    Ok(ListRequest { agent_id, query })
}

/// Registers `fileReferences.list` against a resolver from Session identity to workspace root.
///
/// The root is a *lookup*, not an argument: upstream derives it from the Session header, and a client
/// that could name a root could ask about any directory on the host. A `None` from the resolver is
/// answered as an empty list for the same advisory reason every other absence in this module is —
/// a Session that is not live has no workspace to complete against, which is not a client error.
pub fn register_file_reference_methods<F>(registry: &mut RemoteRegistry, root_of: F)
where
    F: Fn(&str) -> Option<PathBuf> + Send + Sync + 'static,
{
    registry.declare(NAMESPACE, LIST_METHOD, Declaration::read(RESOURCE_KIND));
    let resolver = Arc::new(root_of);
    registry.register(NAMESPACE, LIST_METHOD, move |call: RemoteCall| {
        let resolver = Arc::clone(&resolver);
        Box::pin(async move {
            let request = parse_request(&call.args)?;
            let Some(root) = resolver(&request.agent_id) else {
                return Ok(json!({ "candidates": Vec::<Value>::new() }));
            };
            // A malformed config is a deployment error, and the honest report is the one upstream's
            // constructor makes: a message naming the rule, not an empty completion list.
            let search = WorkspaceFileSearch::new(root, Config::default()).map_err(|error| {
                RemoteError::Failed(format!(
                    "`{NAMESPACE}.{LIST_METHOD}` could not index the workspace: {}",
                    error.message()
                ))
            })?;
            let cancel = CancellationToken::new();
            let candidates: Vec<Value> = search
                .list(&request.query, &cancel)
                .iter()
                .map(Candidate::to_value)
                .collect();
            Ok(json!({ "candidates": candidates }))
        })
    });
}

/// Answers one `fileReferences.list` call without a registry, for a caller that already has a root.
///
/// The same path [`register_file_reference_methods`] takes, exposed so a test or a host that does not
/// want a `RemoteRegistry` can ask the question directly.
///
/// # Errors
///
/// [`RemoteError::InvalidArguments`] for a malformed request, or [`RemoteError::Failed`] when the root
/// cannot be indexed at all.
pub fn list(root: &Path, request: &ListRequest) -> Result<Vec<Candidate>, RemoteError> {
    let search = WorkspaceFileSearch::new(root, Config::default()).map_err(|error| {
        RemoteError::Failed(format!(
            "`{NAMESPACE}.{LIST_METHOD}` could not index the workspace: {}",
            error.message()
        ))
    })?;
    Ok(search.list(&request.query, &CancellationToken::new()))
}
