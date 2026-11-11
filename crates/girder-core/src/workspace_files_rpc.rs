//! The `workspace.tree` and `workspace.readFile` Remote methods: browsing one registered workspace.
//!
//! `fs.*` answers questions about a path a client can already name, and it deliberately has no notion
//! of a workspace: it will list `/` if asked. A file browser needs the opposite guarantee — every path
//! it shows belongs to one project. This module adds that guarantee. The registry supplies the root,
//! and every path handed to the filesystem is confined to it before the first syscall.
//!
//! Upstream's `packages/api/workspace-files` is the reference for the semantics: a bounded directory
//! listing, a page of text with an explicit `truncated`/`eof` marker, and a refusal for anything that
//! is not text. It offers more than these two methods (`readBytes`, `stat`, and a `changes` stream);
//! what is here is the read-only subset a tree view and a preview pane need.
//!
//! # Why a workspace id rather than a session scope
//!
//! Upstream derives the root from the Session header (`WorkspaceFileScope`), so a client cannot name
//! a root at all. Girder's [`RemoteCall`] carries no session header — only a peer string — so the root
//! arrives as a `workspaceId` argument resolved through [`WorkspaceRegistry`]. That keeps the
//! important half of upstream's guarantee: a client names a *registration*, not a directory, and it
//! cannot invent a root that was never registered.
//!
//! # Why containment is checked twice
//!
//! The lexical check on the normalized path is what refuses `../../etc/passwd`; it runs on the path
//! the caller supplied, before anything touches the disk, and it is the same check the tool layer
//! uses ([`workspace_contains`]). It cannot see a symlink: a link named `innocent.txt` inside the
//! workspace whose target is `/etc/shadow` passes a lexical check and then reads outside. So the
//! resolved (canonical) path is checked against the canonical root as well. Both checks are refusals,
//! not warnings, because a preview that leaked one file outside the project would make the whole
//! boundary untrustworthy.
//!
//! # Why truncation is a fact in the response and not a quiet cut
//!
//! A page that stopped at a byte cap reads exactly like a complete file unless it says otherwise, and
//! a caller cannot recover the difference by inspecting the text. So a cut page carries `truncated`,
//! the number of bytes returned, the file's full size, and a human-readable `notice`. A caller that
//! ignores the flags still sees a sentence saying the page is incomplete.
//!
//! The seam has no ranged read: `FileSystem::read_bytes` reads a whole file or refuses it. A preview
//! therefore reads the file whole up to [`MAX_PREVIEW_BYTES`] and cuts the text afterwards. A file
//! above that ceiling is *refused* with its size named rather than read whole to be cut, because
//! reading a multi-gigabyte file to show its first paragraph is not a preview.
//!
//! # Why binary content is an error
//!
//! Decoding arbitrary bytes as UTF-8 either fails or produces replacement characters that look like
//! content. Upstream throws `workspace-file/not-text`, and this does the same thing in the vocabulary
//! this build has: a failed call whose message carries the `workspace-file/not-text:` marker. Bytes
//! containing a NUL are rejected too — a NUL is what a binary file usually announces itself with, and
//! a text file never legitimately contains one.
//!
//! # Why the paths go through [`girder_core`]
//!
//! The crate aliases itself as `girder_core`, and the integration test includes this file by path
//! because wiring it into `lib.rs` belongs to a different change. Spelling paths through the crate
//! root keeps one copy of them working from inside the crate and from the test crate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use girder_core::api_config::{Workspace, WorkspaceRegistry};
use girder_core::{
    FileSystem, FsType, RemoteCall, RemoteError, RemoteRegistry, normalize_path, relative_to,
    resolve_in_workspace, target_at, workspace_contains,
};

/// The namespace both methods live under.
///
/// The same `workspace` namespace as `list`/`create`/`delete`, because a client that has a workspace
/// row is exactly the client that will ask what is inside it; a second namespace would make the
/// browser look up an id twice.
const NAMESPACE: &str = "workspace";

/// How many entries `workspace.tree` returns when the caller does not say.
///
/// The same bound `fs.list` uses. A browser view is read by a person, and a thousand rows is past the
/// point where reading them is possible — the tail would be dropped anyway, so it is not fetched.
pub const DEFAULT_TREE_ENTRY_LIMIT: usize = 500;

/// The largest entry count `workspace.tree` accepts.
///
/// A caller may ask for more than the default — exporting a file list is a real use — but not without
/// bound: every entry costs a directory read and, for files, one `stat`.
pub const MAX_TREE_ENTRY_LIMIT: usize = 5_000;

/// How deep `workspace.tree` descends when the caller does not say.
///
/// One level, which is exactly `fs.list`. A tree deeper than the caller needs costs syscalls for rows
/// it will collapse; a caller that wants structure asks for it with `depth`.
pub const DEFAULT_TREE_DEPTH: u32 = 1;

/// The deepest tree `workspace.tree` will walk.
///
/// A bound rather than a preference: depth multiplies breadth, and an unbounded walk over a
/// `node_modules` shaped tree is how a preview turns into a denial of service against its own host.
pub const MAX_TREE_DEPTH: u32 = 8;

/// How many bytes of text `workspace.readFile` returns when the caller does not say.
///
/// 256 KiB is several thousand lines of code — more than a preview needs and small enough that a JSON
/// response carrying it stays pleasant to parse.
pub const DEFAULT_READ_MAX_BYTES: u64 = 256 * 1024;

/// The largest file `workspace.readFile` will read at all.
///
/// The seam has no ranged read, so a preview must read the file whole before cutting it. This is the
/// ceiling on that read; above it the method refuses and names the size instead, which is honest and
/// does not risk the host's memory.
pub const MAX_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;

/// Registers `workspace.tree` and `workspace.readFile` over one filesystem and workspace registry.
pub fn register_workspace_file_methods(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
) {
    let tree_fs = Arc::clone(fs);
    let tree_workspaces = Arc::clone(workspaces);
    registry.register(NAMESPACE, "tree", move |call: RemoteCall| {
        let fs = Arc::clone(&tree_fs);
        let workspaces = Arc::clone(&tree_workspaces);
        Box::pin(async move { tree(&call, fs.as_ref(), workspaces.as_ref()).await })
    });

    let read_fs = Arc::clone(fs);
    let read_workspaces = Arc::clone(workspaces);
    registry.register(NAMESPACE, "readFile", move |call: RemoteCall| {
        let fs = Arc::clone(&read_fs);
        let workspaces = Arc::clone(&read_workspaces);
        Box::pin(async move { read_file(&call, fs.as_ref(), workspaces.as_ref()).await })
    });
}

/// A path that passed both containment checks.
///
/// `root` is the canonical workspace root: the walk compares children against it, and comparing a
/// canonical path against the registry's spelling of the root would fail on any host where the root
/// itself is reached through a link.
struct Confined {
    /// The workspace root, canonicalized.
    root: PathBuf,
    /// The requested path, canonical when it exists.
    target: PathBuf,
    /// The requested path as the client should see it: workspace-relative, `/` separated.
    relative: String,
}

/// Resolves `requested` inside `root`, refusing anything that leaves it.
///
/// Two checks, in this order: a lexical normalization catches `..` and absolute escapes without
/// touching the disk, then the canonical forms catch a symlink whose target is outside. Canonicalizing
/// is only attempted for a path that exists; a missing path keeps its lexical form so the caller gets
/// "does not exist" from the read rather than a confusing containment message.
async fn confine(root: &Path, requested: &str, method: &str) -> Result<Confined, RemoteError> {
    if requested.contains('\0') {
        return Err(invalid(method, "`path` contains a NUL byte"));
    }

    let lexical = normalize_path(&resolve_in_workspace(Some(root), requested));
    if !workspace_contains(root, &lexical) {
        return Err(invalid(
            method,
            format!("`{requested}` escapes the workspace root"),
        ));
    }

    let canonical_root = tokio::fs::canonicalize(root).await.map_err(|error| {
        invalid(
            method,
            format!("workspace root `{}` is unreadable: {error}", root.display()),
        )
    })?;

    // A file the process cannot resolve cannot be opened either, so a canonicalize failure that is
    // not about existence is left for `stat` to report with its own message.
    let canonical_target = tokio::fs::canonicalize(&lexical).await.ok();
    if let Some(resolved) = &canonical_target {
        if !workspace_contains(&canonical_root, resolved) {
            return Err(invalid(
                method,
                format!("`{requested}` resolves outside the workspace root"),
            ));
        }
    }

    let relative = relative_of(root, &lexical);
    Ok(Confined {
        root: canonical_root,
        target: canonical_target.unwrap_or(lexical),
        relative,
    })
}

/// The workspace-relative display form of `target`, `/` separated.
///
/// `/` rather than the platform separator because the value is a wire field a client joins into its
/// own paths; a Windows `\` would be read as an escape there.
fn relative_of(root: &Path, target: &Path) -> String {
    relative_to(&normalize_path(root), target).map_or_else(String::new, |relative| {
        relative.to_string_lossy().replace('\\', "/")
    })
}

/// What a walk collected.
struct Walked {
    /// Entries in breadth-first order.
    entries: Vec<Value>,
    /// Directories whose contents could not be read, with the reason.
    unreadable: Vec<String>,
    /// Whether the entry budget stopped the walk before the tree ended.
    truncated: bool,
}

/// Walks `start` breadth-first, collecting at most `limit` entries down to `depth`.
///
/// Breadth-first rather than depth-first: with a bounded budget, the shallow rows are the ones a
/// browser shows expanded, so a cut walk that kept one deep branch would be the least useful half.
///
/// A directory that cannot be read is recorded and skipped rather than failing the whole call — the
/// caller asked for a picture of the tree, and one unreadable corner does not make the rest untrue.
async fn walk(
    fs: &dyn FileSystem,
    root: &Path,
    start: &Path,
    relative: &str,
    depth: u32,
    limit: usize,
) -> Walked {
    let cancel = CancellationToken::new();
    let mut queue: std::collections::VecDeque<(PathBuf, String, u32)> =
        std::collections::VecDeque::new();
    queue.push_back((start.to_path_buf(), relative.to_owned(), 0));

    let mut walked = Walked {
        entries: Vec::new(),
        unreadable: Vec::new(),
        truncated: false,
    };

    while let Some((directory, directory_relative, level)) = queue.pop_front() {
        if walked.truncated {
            break;
        }
        let listing = match fs
            .list_dir(&target_at(directory.clone()), cancel.clone())
            .await
        {
            Ok(listing) => listing,
            Err(error) => {
                let shown = display_of(&directory_relative);
                walked.unreadable.push(format!("{shown}: {error}"));
                continue;
            }
        };

        for entry in listing {
            if walked.entries.len() >= limit {
                // Only reached with an entry actually in hand, so `truncated` means "there was more"
                // rather than "the budget might have been enough".
                walked.truncated = true;
                break;
            }
            let child_relative = join_relative(&directory_relative, &entry.name);
            let child_path = directory.join(&entry.name);
            let size = if entry.file_type == FsType::File {
                fs.stat(&target_at(child_path.clone()), cancel.clone())
                    .await
                    .ok()
                    .flatten()
                    .map(|info| info.size)
            } else {
                // A directory has no meaningful byte size, and a device has none at all. Sending `0`
                // would read as "empty" rather than "not applicable", so the field is null.
                None
            };

            walked.entries.push(json!({
                "name": entry.name,
                "path": child_relative,
                "kind": kind_name(entry.file_type),
                "depth": level + 1,
                "size": size,
            }));

            if entry.file_type == FsType::Directory && level + 1 < depth {
                // Defence in depth: a listing name cannot contain a separator on any backend this
                // build runs, but a backend that returned one would turn the join below into a path
                // outside the root. The check costs a comparison per directory.
                if !workspace_contains(root, &child_path) {
                    walked.unreadable.push(child_relative);
                    continue;
                }
                queue.push_back((child_path, child_relative, level + 1));
            }
        }
    }

    walked
}

/// Implements `workspace.tree`.
async fn tree(
    call: &RemoteCall,
    fs: &dyn FileSystem,
    workspaces: &WorkspaceRegistry,
) -> Result<Value, RemoteError> {
    const METHOD: &str = "tree";

    let workspace_id = required_str(&call.args, "workspaceId", METHOD)?;
    let workspace = registered(workspaces, workspace_id, METHOD)?;
    let requested = optional_str(&call.args, "path").unwrap_or("");
    let depth = depth_arg(&call.args)?;
    let limit = limit_arg(&call.args)?;

    let confined = confine(&workspace.path, requested, METHOD).await?;
    let info = fs
        .stat(
            &target_at(confined.target.clone()),
            CancellationToken::new(),
        )
        .await
        .map_err(|error| failed(error.to_string()))?
        .ok_or_else(|| missing(&confined.relative, workspace_id))?;
    if info.file_type != FsType::Directory {
        return Err(failed(format!(
            "`{}` is a {}, not a directory",
            display_of(&confined.relative),
            kind_name(info.file_type)
        )));
    }

    let walked = walk(
        fs,
        &confined.root,
        &confined.target,
        &confined.relative,
        depth,
        limit,
    )
    .await;

    Ok(json!({
        "workspaceId": workspace_id,
        "title": workspace.title,
        // The listed directory as a workspace path, empty for the root. A child's path is this value
        // joined with its `name` by `/`, which is what a tree view keys its rows on.
        "path": confined.relative,
        "depth": depth,
        "entries": walked.entries,
        // Reported, never inferred from a full page: a caller that received exactly `limit` entries
        // cannot otherwise tell a complete answer from a cut one.
        "truncated": walked.truncated,
        "limit": limit,
        "unreadable": walked.unreadable,
    }))
}

/// Implements `workspace.readFile`.
async fn read_file(
    call: &RemoteCall,
    fs: &dyn FileSystem,
    workspaces: &WorkspaceRegistry,
) -> Result<Value, RemoteError> {
    const METHOD: &str = "readFile";

    let workspace_id = required_str(&call.args, "workspaceId", METHOD)?;
    let workspace = registered(workspaces, workspace_id, METHOD)?;
    let requested = required_path(&call.args, METHOD)?;
    let max_bytes = read_cap(&call.args)?;

    let confined = confine(&workspace.path, requested, METHOD).await?;
    let cancel = CancellationToken::new();
    let target = target_at(confined.target.clone());
    let info = fs
        .stat(&target, cancel.clone())
        .await
        .map_err(|error| failed(error.to_string()))?
        .ok_or_else(|| missing(&confined.relative, workspace_id))?;

    match info.file_type {
        FsType::File => {}
        FsType::Directory => {
            return Err(failed(format!(
                "`{}` is a directory",
                display_of(&confined.relative)
            )));
        }
        FsType::Other => {
            return Err(failed(format!(
                "`{}` is not a regular file",
                display_of(&confined.relative)
            )));
        }
    }

    let size = info.size;
    if size > MAX_PREVIEW_BYTES {
        return Err(failed(format!(
            "`{}` is {size} bytes, above the {MAX_PREVIEW_BYTES}-byte preview ceiling; it is refused \
             rather than read whole",
            display_of(&confined.relative)
        )));
    }

    // The limit is the file's own stat'd size: anything already observed is admitted, and a file that
    // grew between the stat and the read is refused by the seam rather than read unbounded.
    let bytes = fs
        .read_bytes(&target, size, cancel)
        .await
        .map_err(|error| failed(error.to_string()))?;

    // The scan runs over the whole buffer, not just the page: a file with a NUL past the byte cap is
    // still a binary file, and reporting its prefix as text would be the same lie with a smaller page.
    if bytes.contains(&0) {
        return Err(not_text(&confined.relative, "contains a NUL byte"));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| not_text(&confined.relative, "is not valid UTF-8"))?;

    let kept = char_boundary_at(text, max_bytes);
    let truncated = kept < text.len();
    let page = &text[..kept];
    let notice = if truncated {
        Some(format!("truncated: showed {kept} of {size} bytes"))
    } else {
        None
    };
    let version = info.version.0;

    Ok(json!({
        "workspaceId": workspace_id,
        "path": confined.relative,
        "kind": "file",
        "size": size,
        "version": version,
        "text": page,
        "returnedBytes": kept,
        // An explicit pair rather than one flag: `truncated` is what a client branches on, and
        // `notice` is what a person reads when the client does not.
        "truncated": truncated,
        "eof": !truncated,
        "maxBytes": max_bytes,
        "lines": page.lines().count(),
        "notice": notice,
    }))
}

/// The last character boundary at or below `limit`.
///
/// Cutting at an arbitrary byte would panic on slicing a multi-byte character, and cutting at a
/// replacement-character boundary would put `�` at the end of a page that is otherwise the file.
fn char_boundary_at(text: &str, limit: u64) -> usize {
    let mut cut = usize::try_from(limit).unwrap_or(usize::MAX).min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// The wire name of a file type.
const fn kind_name(kind: FsType) -> &'static str {
    match kind {
        FsType::File => "file",
        FsType::Directory => "directory",
        // Not "symlink": this layer cannot tell a link from a device or a socket, and claiming one
        // would be a guess presented as a fact.
        FsType::Other => "other",
    }
}

/// A workspace-relative path for a message, naming the root rather than showing an empty string.
fn display_of(relative: &str) -> &str {
    if relative.is_empty() {
        "<workspace root>"
    } else {
        relative
    }
}

/// Joins a directory's relative path with a child's name, `/` separated.
fn join_relative(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

/// The registration an id names, or a refusal.
fn registered(
    workspaces: &WorkspaceRegistry,
    workspace_id: &str,
    method: &str,
) -> Result<Workspace, RemoteError> {
    workspaces.get(workspace_id).ok_or_else(|| {
        invalid(
            method,
            format!("unknown workspace `{workspace_id}`; it is not registered"),
        )
    })
}

/// The `workspaceId` argument.
fn required_str<'a>(args: &'a Value, field: &str, method: &str) -> Result<&'a str, RemoteError> {
    args.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(method, format!("`{field}` is required")))
}

/// The `path` argument, which for a preview may not be omitted.
fn required_path<'a>(args: &'a Value, method: &str) -> Result<&'a str, RemoteError> {
    args.get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(method, "`path` is required"))
}

/// An optional string argument.
fn optional_str<'a>(args: &'a Value, field: &str) -> Option<&'a str> {
    args.get(field).and_then(Value::as_str)
}

/// The `depth` argument, bounded and refused rather than clamped.
///
/// A clamp would silently answer a different question than the one asked; a refusal tells the caller
/// the bound exists.
fn depth_arg(args: &Value) -> Result<u32, RemoteError> {
    match args.get("depth") {
        None | Some(Value::Null) => Ok(DEFAULT_TREE_DEPTH),
        Some(value) => {
            let depth = value
                .as_u64()
                .and_then(|raw| u32::try_from(raw).ok())
                .ok_or_else(|| invalid("tree", "`depth` must be a non-negative integer"))?;
            if depth == 0 || depth > MAX_TREE_DEPTH {
                return Err(invalid(
                    "tree",
                    format!("`depth` must be between 1 and {MAX_TREE_DEPTH}"),
                ));
            }
            Ok(depth)
        }
    }
}

/// The `limit` argument, bounded and refused rather than clamped.
fn limit_arg(args: &Value) -> Result<usize, RemoteError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(DEFAULT_TREE_ENTRY_LIMIT),
        Some(value) => {
            let limit = value
                .as_u64()
                .and_then(|raw| usize::try_from(raw).ok())
                .ok_or_else(|| invalid("tree", "`limit` must be a non-negative integer"))?;
            if limit == 0 || limit > MAX_TREE_ENTRY_LIMIT {
                return Err(invalid(
                    "tree",
                    format!("`limit` must be between 1 and {MAX_TREE_ENTRY_LIMIT}"),
                ));
            }
            Ok(limit)
        }
    }
}

/// The `maxBytes` argument of a preview, bounded and refused rather than clamped.
fn read_cap(args: &Value) -> Result<u64, RemoteError> {
    match args.get("maxBytes") {
        None | Some(Value::Null) => Ok(DEFAULT_READ_MAX_BYTES),
        Some(value) => {
            let cap = value
                .as_u64()
                .ok_or_else(|| invalid("readFile", "`maxBytes` must be a positive integer"))?;
            if cap == 0 || cap > MAX_PREVIEW_BYTES {
                return Err(invalid(
                    "readFile",
                    format!("`maxBytes` must be between 1 and {MAX_PREVIEW_BYTES}"),
                ));
            }
            Ok(cap)
        }
    }
}

/// A refusal for input the caller can fix.
fn invalid(method: &str, message: impl Into<String>) -> RemoteError {
    RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: message.into(),
    }
}

/// A failure the caller cannot fix by changing the request.
///
/// `RemoteError` has no data-carrying variant, so the message is the whole failure. Upstream's
/// structured codes are not reproduced, and the messages that a client may want to branch on carry a
/// stable prefix instead (see [`not_text`]).
fn failed(message: impl Into<String>) -> RemoteError {
    RemoteError::Failed(message.into())
}

/// A refusal for content that is not text.
///
/// The `workspace-file/not-text:` prefix is this build's stand-in for upstream's error code: a client
/// that wants to branch on "this is binary, stop previewing it" matches the prefix, and a person
/// reading the message gets the same sentence either way.
fn not_text(relative: &str, reason: &str) -> RemoteError {
    failed(format!(
        "workspace-file/not-text: `{}` {reason}",
        display_of(relative)
    ))
}

/// A refusal for a path that names nothing.
fn missing(relative: &str, workspace_id: &str) -> RemoteError {
    failed(format!(
        "`{}` does not exist in workspace `{workspace_id}`",
        display_of(relative)
    ))
}
