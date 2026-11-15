//! What one tool call does to the tool-result cache, and how far that damage reaches.
//!
//! `tool_cache` stamps every entry with the source file's mtime and refuses an entry whose mtime no
//! longer matches. That gate is exact for a change made by another process, because the filesystem
//! moves the mtime. It is blind to the one case that matters most: *our own* write, on a filesystem
//! whose timestamp resolution is coarser than the interval between the read and the write. A
//! same-tick write leaves `source_mtime_ms` unchanged, so the next `read` of that path is served the
//! pre-write text with the shape of a fresh read. This module is the answer to that hole: after a
//! call that may have written, the entries it can have invalidated are dropped explicitly instead
//! of waiting for a timestamp that may never move.
//!
//! # Why a shell command drops everything
//!
//! A command line is opaque. `echo x > f` and `rm -rf build` look like any other argv to this layer,
//! and no static reading of the text can tell what was written: the path can be built at run time,
//! a called binary can write, and a redirect is just another token. Enumerating the paths of a shell
//! command is not "hard", it is impossible, so `bash` is [`Effect::InvalidateAll`]. The same
//! reasoning covers every tool whose writes this layer cannot see — a subagent edits files, a
//! background job was started by a shell, a terminal session is a shell — and those fall through to
//! the same answer by default.
//!
//! The cost of being conservative is a cache miss and a re-read. The cost of being wrong is a model
//! that reasons about a file which no longer exists in that form. The asymmetry is the whole argument
//! for this module's bias: a wrong cache hit is worse than no cache at all, and a miss only spends
//! the disk read the cache was trying to save.
//!
//! # Why the paths are normalized
//!
//! A file has one identity and many spellings: `/a/b`, `/a/b/`, `/a/./b`, and `a/../a/b` all name
//! the same inode. Invalidation that compares raw strings therefore misses a spelling and leaves the
//! entry live, which is the same stale serve this module exists to prevent. Paths are folded
//! lexically here — separators collapsed, `.` removed, `..` resolved against the preceding segment,
//! trailing separators dropped — so every spelling in an argument vector lands on the same key.
//!
//! The folding is deliberately lexical: it never touches the filesystem. A write may name a file
//! that does not exist yet, and a `stat` here would make invalidation fallible and slow for no gain.
//! Symlinks are therefore not resolved, so two paths that reach one inode through different links
//! stay distinct; that gap is covered by the mtime gate, which does notice the file itself changing.
//!
//! # What `apply` can and cannot do
//!
//! `tool_cache::Cache` addresses entries by `Key` — the tool name plus the model's *raw* argument
//! text — and exposes no way to enumerate its keys or to remove one by path. The same file can
//! therefore be held under any number of keys (`/a/b`, `/a/b/`, a different key order), and none of
//! them can be reconstructed from a path. [`apply`] consequently drops the whole cache for a path
//! effect rather than pretend to a precision the store cannot provide. The [`Effect::Invalidate`]
//! variant still carries the paths: they are the precise answer for a caller that keeps a path
//! index, they make the decision testable, and they are what a future `Cache::remove_path` would
//! consume. Dropping everything is the only sound implementation available today.

use std::path::{Component, Path};

/// One tool call's impact on the cached read results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Nothing to do: the call cannot have changed a file (a read-only tool).
    None,
    /// These paths may no longer be current.
    ///
    /// Normalized; see the module documentation. An empty vector is never produced by [`effect`] —
    /// a mutating call whose paths could not be recovered reports [`Effect::InvalidateAll`] instead,
    /// because "no paths" is not evidence that "nothing changed".
    Invalidate {
        /// The affected paths, normalized and deduplicated.
        paths: Vec<String>,
    },
    /// Everything may have changed, because the call could write anywhere or its targets are opaque.
    InvalidateAll,
}

/// The tools that cannot have modified a file.
///
/// An allowlist, for the same reason `tool_cache` uses one: an unknown tool is not evidence of a
/// read-only one, and the safe default for the unknown case is the expensive-but-correct answer.
/// Every entry returns [`Effect::None`]; everything not listed here is treated as possibly-writing.
///
/// Only genuinely read-only tools are listed. Session and orchestration tools (`todo_write`, `goal`,
/// `skill`, `subagent*`, `workflow_*`, `job_*`, `terminal_*`) are deliberately absent even where
/// they write no *workspace* file: their writes happen through a path this layer does not see, so
/// guessing that the cache is still valid would be exactly the guess this module refuses to make.
const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "glob",
    "grep",
    "lsp",
    "session_search",
    "web_fetch",
    "web_search",
    "ask_user",
    "present",
    "skill",
];

/// The argument fields that name a file a tool writes, per tool.
///
/// `None` means "this is not a path-addressed writer", not "read-only": the caller falls back to
/// [`Effect::InvalidateAll`]. Copy and move are listed with both ends because the source is part of
/// the effect for `move_path`, which removes it, and a destination is always written.
fn mutated_fields(tool: &str) -> Option<&'static [&'static str]> {
    match tool {
        "write" | "edit" | "delete_path" | "make_dir" => Some(&["path"]),
        "copy_path" | "move_path" => Some(&["source", "destination"]),
        _ => None,
    }
}

/// Infers what one call did to the cached files.
///
/// Pure: it reads the tool name and the raw argument text, and never touches the filesystem.
///
/// **`bash` is always [`Effect::InvalidateAll`].** Its command line cannot be read for paths — the
/// path may be built at run time, a called binary may write, and a redirect is an ordinary token —
/// so no amount of parsing would be exhaustive, and a partial answer would leave a stale entry
/// live. The same applies to every tool this module does not recognise: an unknown writer is
/// assumed to have written everywhere.
///
/// For a path-addressed writer, the arguments are parsed and the named paths are normalized. If they
/// cannot be recovered (malformed JSON, a missing field, an empty path, a patch with no file
/// marker), the answer is [`Effect::InvalidateAll`] rather than an empty list.
#[must_use]
pub fn effect(tool: &str, arguments: &str) -> Effect {
    if READ_ONLY_TOOLS.contains(&tool) {
        return Effect::None;
    }
    if tool == "bash" {
        // Stated as its own case rather than left to the fallback: this is the conservative choice
        // the module is built around, and a reader should find it named, not inferred.
        return Effect::InvalidateAll;
    }
    if tool == "apply_patch" {
        return patch_effect(arguments);
    }
    let Some(fields) = mutated_fields(tool) else {
        return Effect::InvalidateAll;
    };
    match named_paths(arguments, fields) {
        Some(paths) if !paths.is_empty() => Effect::Invalidate { paths },
        // The arguments did not say which files this call writes. A parse failure is not a promise
        // that nothing was written, so the conservative answer is the whole cache.
        _ => Effect::InvalidateAll,
    }
}

/// Applies an effect to the cache.
///
/// Idempotent, and cheap in the common case: a read-only call does nothing at all. See the module
/// documentation for why a path effect drops more than the paths name — the cache cannot address an
/// entry by path, and dropping everything is the only answer that cannot serve stale text.
pub fn apply(effect: &Effect, cache: &mut girder_core::ToolCache) {
    match effect {
        // The file the model read is exactly as it was, so the entry is still the right answer.
        Effect::None => {}
        // A shell command, an unrecognised tool, or a writer whose targets could not be read.
        Effect::InvalidateAll => cache.clear(),
        Effect::Invalidate { paths } => {
            if !paths.is_empty() {
                // Coarser than the effect: `ToolCache` has no path-indexed removal (see the module
                // documentation), so this drops entries the write did not touch as well. That is the
                // harmless direction -- it costs a re-read, while a surviving entry would hand the
                // model text that is no longer on disk.
                cache.clear();
            }
        }
    }
}

/// Collects the normalized paths a writer names in `fields`.
///
/// `None` when the arguments are not a JSON object, so the caller can fall back to the whole cache
/// instead of reading a malformed call as "wrote nowhere".
fn named_paths(arguments: &str, fields: &[&str]) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let mut paths = Vec::with_capacity(fields.len());
    for field in fields {
        if let Some(raw) = value.get(*field).and_then(serde_json::Value::as_str) {
            let normalized = normalize(raw);
            if !normalized.is_empty() {
                paths.push(normalized);
            }
        }
    }
    Some(dedupe(paths))
}

/// The paths a patch names, or [`Effect::InvalidateAll`] when it names none.
///
/// The markers are the patch format's own grammar (`*** Add File:`, `*** Update File:`,
/// `*** Delete File:`), plus `*** Move to:`, whose destination is written even though its source is
/// only read. Content lines cannot be mistaken for a marker: the format prefixes them.
fn patch_effect(arguments: &str) -> Effect {
    let value: Option<serde_json::Value> = serde_json::from_str(arguments).ok();
    let text = value
        .as_ref()
        .and_then(|value| value.get("patch"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let paths = patch_paths(text);
    if paths.is_empty() {
        // A patch with no marker writes nothing, but it also parsed into nothing this layer can see
        // a path for; the same reasoning as an unreadable writer applies.
        return Effect::InvalidateAll;
    }
    Effect::Invalidate { paths }
}

/// The normalized paths named by a patch body's marker lines.
fn patch_paths(text: &str) -> Vec<String> {
    const MARKERS: [&str; 4] = [
        "*** Add File: ",
        "*** Update File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    let mut paths = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        for marker in MARKERS {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                let normalized = normalize(rest);
                if !normalized.is_empty() {
                    paths.push(normalized);
                }
            }
        }
    }
    dedupe(paths)
}

/// Keeps the first occurrence of each path.
///
/// A patch can name one file twice (an update followed by a move, two hunks of one file), and a
/// repeated path in the effect would be meaningless work for the caller that consumes it.
fn dedupe(paths: Vec<String>) -> Vec<String> {
    let mut unique: Vec<String> = Vec::with_capacity(paths.len());
    for path in paths {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

/// Folds every spelling of one path onto a single string.
///
/// Lexical only, because the target may not exist yet and because invalidation must not be able to
/// fail on a filesystem error. Repeated separators collapse, `.` segments disappear, `..` pops the
/// preceding segment (staying at the root when it cannot pop), and trailing separators are dropped.
fn normalize(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut absolute = false;
    let mut segments: Vec<String> = Vec::new();
    for component in Path::new(trimmed).components() {
        match component {
            // Unreachable on Unix and kept for portability: a drive or share name is an ordinary
            // first segment here rather than something to resolve.
            Component::Prefix(prefix) => {
                segments.push(prefix.as_os_str().to_string_lossy().into_owned());
            }
            Component::RootDir => absolute = true,
            Component::CurDir => {}
            Component::ParentDir => {
                // Popping a real segment is the resolution; at the root there is nothing to pop and
                // `/..` is `/`, so the root stays. A relative path that runs out of segments keeps
                // the `..` it cannot resolve.
                if segments.pop().is_none() && !absolute {
                    segments.push("..".to_owned());
                }
            }
            Component::Normal(name) => segments.push(name.to_string_lossy().into_owned()),
        }
    }
    let joined = segments.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// The path of `raw` in the form [`normalize`] produces.
///
/// Exposed so a caller comparing a path it holds against an [`Effect`] does not have to re-derive
/// the folding rule, which would drift from this one.
#[must_use]
pub fn normalized_path(raw: &str) -> String {
    normalize(raw)
}
