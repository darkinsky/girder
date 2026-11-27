//! The `workspaceFiles.tree` and `workspaceFiles.readBytes` Remote methods: browsing one registered workspace.
//!
//! `fs.*` answers questions about a path a client can already name, and it deliberately has no notion
//! of a workspace: it will list `/` if asked. A file browser needs the opposite guarantee — every path
//! it shows belongs to one project. This module adds that guarantee. The registry supplies the root,
//! and every path handed to the filesystem is confined to it before the first syscall.
//!
//! Upstream's `packages/api/workspace-files` is the reference for the semantics: a bounded directory
//! listing, a page of text, and a raw-byte read with an explicit range and EOF marker. The byte
//! endpoint deliberately does not decode UTF-8 or reject NUL bytes; text paging lives in
//! `files_extra.rs`.
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
//! # Why byte ranges are explicit
//!
//! A complete read is bounded before allocation. A ranged read is bounded by the requested window,
//! returns fewer bytes at EOF, and carries the file's total byte count separately from the returned
//! attachment. That lets a renderer distinguish an empty range from an empty file without reading
//! or decoding the entire content.
//!
//! # Why the paths go through [`girder_core`]
//!
//! The crate aliases itself as `girder_core`, and the integration test includes this file by path
//! because wiring it into `lib.rs` belongs to a different change. Spelling paths through the crate
//! root keeps one copy of them working from inside the crate and from the test crate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::sync::CancellationToken;

use girder_core::api::config::{Workspace, WorkspaceRegistry};
use girder_core::{
    BinaryRemoteResult, FileSystem, FsType, RemoteAttachment, RemoteAttachmentSegment, RemoteCall,
    RemoteError, RemoteRegistry, normalize_path, relative_to, resolve_in_workspace, target_at,
    workspace_contains,
};

/// The namespace both methods live under.
///
/// `workspaceFiles`, which is the namespace upstream serves this surface under.
///
/// This was `workspace`, on the reasoning that a client holding a workspace row is the client that
/// will ask what is inside it. That reasoning is sound and the conclusion was still wrong: upstream
/// keeps the two surfaces apart (`packages/api/workspace-files` declares
/// `super(ctx, 'workspaceFiles')`), so a client written against upstream calls
/// `workspaceFiles.list` and would find `workspace.list` answering a different question entirely --
/// the registry's rows rather than a directory's entries.
const NAMESPACE: &str = "workspaceFiles";

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

/// The legacy preview budget retained for compatibility with older callers.
///
/// 256 KiB is several thousand lines of code — more than a preview needs and small enough that a JSON
/// response carrying it stays pleasant to parse.
pub const DEFAULT_READ_MAX_BYTES: u64 = 256 * 1024;

/// The largest complete binary file read accepted by this endpoint.
///
/// A complete `readBytes` call is refused above this ceiling before the file is read.
pub const MAX_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;

/// Registers the workspace tree, raw byte read, and change stream over one filesystem and workspace registry.
pub fn register_workspace_file_methods(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
) {
    register_workspace_file_methods_with_sessions(registry, fs, workspaces, None, None);
}

/// Registers the file methods and resolves a browser `workspaceFileScopeId` through sessions.
///
/// `fallback` is the deployment workspace root used when a known session has no working directory.
pub fn register_workspace_file_methods_with_sessions(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
    sessions: Option<Arc<dyn girder_core::SessionCatalog>>,
    fallback: Option<std::path::PathBuf>,
) {
    let changes_workspaces = Arc::clone(workspaces);
    let changes_sessions = sessions.clone();
    let changes_fallback = fallback.clone();
    registry.register_stream(NAMESPACE, "changes", move |open| {
        let workspaces = Arc::clone(&changes_workspaces);
        let sessions = changes_sessions.clone();
        let fallback = changes_fallback.clone();
        Box::pin(async move {
            follow_changes(
                open,
                workspaces.as_ref(),
                sessions.as_deref(),
                fallback.as_deref(),
            )
            .await
        })
    });
    registry.declare(
        NAMESPACE,
        "changes",
        girder_core::Declaration::read("workspace"),
    );

    let read_fs = Arc::clone(fs);
    let read_workspaces = Arc::clone(workspaces);
    let read_sessions = sessions.clone();
    let read_fallback = fallback.clone();
    registry.register_binary(NAMESPACE, "readBytes", move |call: RemoteCall| {
        let fs = Arc::clone(&read_fs);
        let workspaces = Arc::clone(&read_workspaces);
        let sessions = read_sessions.clone();
        let fallback = read_fallback.clone();
        Box::pin(async move {
            read_file(
                &call,
                fs.as_ref(),
                workspaces.as_ref(),
                sessions.as_deref(),
                fallback.as_deref(),
            )
            .await
        })
    });

    let watch_workspaces = Arc::clone(workspaces);
    let watch_sessions = sessions;
    let watch_fallback = fallback;
    registry.register(NAMESPACE, "watch", move |call: RemoteCall| {
        let workspaces = Arc::clone(&watch_workspaces);
        let sessions = watch_sessions.clone();
        let fallback = watch_fallback.clone();
        Box::pin(async move {
            watch(
                &call,
                workspaces.as_ref(),
                sessions.as_deref(),
                fallback.as_deref(),
            )
            .await
        })
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

/// Implements the bounded directory walk used by Girder's workspace browser.
pub async fn tree(
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

/// Implements the binary `workspaceFiles.readBytes` endpoint.
async fn read_file(
    call: &RemoteCall,
    _fs: &dyn FileSystem,
    workspaces: &WorkspaceRegistry,
    sessions: Option<&dyn girder_core::SessionCatalog>,
    fallback: Option<&std::path::Path>,
) -> Result<BinaryRemoteResult, RemoteError> {
    const METHOD: &str = "readBytes";

    let root =
        girder_core::workspace_file_root(workspaces, sessions, fallback, &call.args, METHOD)?;
    let requested = required_path(&call.args, METHOD)?;
    let options = call.args.get("options").unwrap_or(&call.args);
    let target = resolve_binary_target(&root, requested, options, METHOD).await?;
    let link_metadata = tokio::fs::symlink_metadata(&target)
        .await
        .map_err(|error| {
            failed(format!(
                "workspace-file/not-found: cannot stat `{}`: {error}",
                target.display()
            ))
        })?;
    if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
        return Err(failed(format!(
            "workspace-file/not-regular-file: `{}` is not a regular file",
            target.display()
        )));
    }
    let metadata = tokio::fs::metadata(&target).await.map_err(|error| {
        failed(format!(
            "workspace-file/not-found: cannot stat `{}`: {error}",
            target.display()
        ))
    })?;
    let size = metadata.len();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos());
    let max_file_bytes = options
        .get("maxFileBytes")
        .or_else(|| options.get("maxBytes"))
        .and_then(Value::as_u64)
        .unwrap_or(MAX_PREVIEW_BYTES);
    if max_file_bytes == 0 || max_file_bytes > MAX_PREVIEW_BYTES {
        return Err(invalid(
            METHOD,
            format!("`maxFileBytes` must be between 1 and {MAX_PREVIEW_BYTES}"),
        ));
    }

    let range = options.get("range");
    let (offset, eof, bytes) = if let Some(range) = range {
        let offset = range.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let length = range
            .get("length")
            .and_then(Value::as_u64)
            .unwrap_or(max_file_bytes);
        if length > max_file_bytes {
            return Err(invalid(
                METHOD,
                format!("`range.length` must be at most {max_file_bytes}"),
            ));
        }
        let bytes = read_byte_range(&target, offset, length).await?;
        let eof = offset.saturating_add(bytes.len() as u64) >= size;
        (offset, eof, bytes)
    } else {
        if size > max_file_bytes {
            return Err(failed(format!(
                "workspace-file/too-large: `{requested}` exceeds the {max_file_bytes}-byte full-file cap"
            )));
        }
        let bytes = tokio::fs::read(&target).await.map_err(|error| {
            failed(format!(
                "workspace-file/read-failed: cannot read `{}`: {error}",
                target.display()
            ))
        })?;
        (0, true, bytes)
    };

    let value = json!({
        "absolutePath": target.to_string_lossy().replace('\\', "/"),
        "version": format!("{size}:{modified}"),
        "bytes": size,
        "offset": offset,
        "data": Value::Null,
        "eof": eof,
    });
    Ok(BinaryRemoteResult {
        value,
        attachments: vec![RemoteAttachment {
            path: vec![RemoteAttachmentSegment::Key("data".to_owned())],
            bytes,
        }],
    })
}

/// Resolves a byte-read path. Absolute targets are allowed, while relative targets use the
/// registered workspace. A `baseFile` is itself workspace-scoped and supplies the directory for
/// a relative target.
async fn resolve_binary_target(
    root: &Path,
    requested: &str,
    options: &Value,
    method: &str,
) -> Result<PathBuf, RemoteError> {
    if requested.contains('\0') {
        return Err(invalid(method, "`path` contains a NUL byte"));
    }
    let path = if let Some(base) = options.get("baseFile").and_then(Value::as_str) {
        let base = confine(root, base, method).await?;
        if Path::new(requested).is_absolute() {
            PathBuf::from(requested)
        } else {
            base.target
                .parent()
                .unwrap_or_else(|| Path::new(root))
                .join(requested)
        }
    } else if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        root.join(requested)
    };
    let normalized = normalize_path(&path);
    if !Path::new(requested).is_absolute() && !workspace_contains(root, &normalized) {
        return Err(invalid(
            method,
            format!("`{requested}` escapes the workspace root"),
        ));
    }
    let link_metadata = tokio::fs::symlink_metadata(&normalized)
        .await
        .map_err(|error| {
            failed(format!(
                "workspace-file/not-found: cannot resolve `{requested}`: {error}"
            ))
        })?;
    if link_metadata.file_type().is_symlink() {
        return Ok(normalized);
    }
    let target = tokio::fs::canonicalize(&normalized)
        .await
        .map_err(|error| {
            failed(format!(
                "workspace-file/not-found: cannot resolve `{requested}`: {error}"
            ))
        })?;
    Ok(target)
}

async fn read_byte_range(target: &Path, offset: u64, length: u64) -> Result<Vec<u8>, RemoteError> {
    if length == 0 {
        return Ok(Vec::new());
    }
    let mut file = tokio::fs::File::open(target).await.map_err(|error| {
        failed(format!(
            "workspace-file/read-failed: cannot read `{}`: {error}",
            target.display()
        ))
    })?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|error| {
            failed(format!(
                "workspace-file/read-failed: cannot seek `{}`: {error}",
                target.display()
            ))
        })?;
    let capacity = usize::try_from(length).unwrap_or(usize::MAX);
    let mut bytes = vec![0_u8; capacity];
    let read = file.read(&mut bytes).await.map_err(|error| {
        failed(format!(
            "workspace-file/read-failed: cannot read `{}`: {error}",
            target.display()
        ))
    })?;
    bytes.truncate(read);
    Ok(bytes)
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
                .ok_or_else(|| invalid("changes", "`depth` must be a non-negative integer"))?;
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
                .ok_or_else(|| invalid("changes", "`limit` must be a non-negative integer"))?;
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

async fn follow_changes(
    open: girder_core::StreamOpen,
    workspaces: &WorkspaceRegistry,
    sessions: Option<&dyn girder_core::SessionCatalog>,
    fallback: Option<&std::path::Path>,
) {
    let root = match girder_core::workspace_file_root(
        workspaces,
        sessions,
        fallback,
        &open.call.args,
        "changes",
    ) {
        Ok(root) => root,
        Err(error) => {
            let _ = open.fail(error);
            return;
        }
    };
    let requested = optional_str(&open.call.args, "path").unwrap_or(".");
    let initial = target_info(&root.join(requested));
    let plan =
        match girder_core::workspace::watch::plan_watch(Some(&root), requested, initial.as_ref()) {
            Ok(plan) => plan,
            Err(error) => {
                let _ = open.fail(failed(format!("{}: {error}", error.code())));
                return;
            }
        };
    if !open.push(json!({ "kind": "ready" })) {
        return;
    }
    let mut previous = fingerprint(&plan.process_path);
    let native = girder_core::workspace::native_watch::NativeWatcher::open(
        &plan.process_path,
        plan.kind == girder_core::FsType::Directory,
    );
    let (_native_guard, mut native_rx) = native.map_or((None, None), |(guard, receiver)| {
        (Some(guard), Some(receiver))
    });
    loop {
        let notified = if let Some(receiver) = native_rx.as_mut() {
            tokio::select! {
                () = open.cancel.cancelled() => return,
                incoming = open.uplink.recv() => {
                    if !matches!(incoming, Ok(Some(_))) {
                        return;
                    }
                    false
                }
                notification = receiver.recv() => {
                    if notification.is_none() {
                        native_rx = None;
                    }
                    true
                }
            }
        } else {
            tokio::select! {
                () = open.cancel.cancelled() => return,
                incoming = open.uplink.recv() => {
                    if !matches!(incoming, Ok(Some(_))) {
                        return;
                    }
                    false
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(250)) => true
            }
        };
        if !notified {
            continue;
        }
        let next = fingerprint(&plan.process_path);
        if next == previous {
            continue;
        }
        let frame = match &next {
            Some(version) => json!({
                "kind": "change",
                "change": {
                    "absolutePath": plan.process_path,
                    "version": version,
                },
            }),
            None => json!({
                "kind": "change",
                "change": {
                    "absolutePath": plan.process_path,
                    "absent": true,
                }
            }),
        };
        previous = next;
        if !open.push(frame) {
            return;
        }
    }
}

fn target_info(path: &Path) -> Option<girder_core::FsInfo> {
    let metadata = std::fs::metadata(path).ok()?;
    let file_type = if metadata.is_dir() {
        girder_core::FsType::Directory
    } else if metadata.is_file() {
        girder_core::FsType::File
    } else {
        girder_core::FsType::Other
    };
    Some(girder_core::FsInfo {
        version: girder_core::FsVersion(metadata.len().to_string()),
        file_type,
        size: metadata.len(),
    })
}

fn fingerprint(path: &str) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |time| time.as_nanos());
    if metadata.is_dir() {
        let mut children = std::fs::read_dir(path)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| {
                let child = entry.metadata().ok();
                format!(
                    "{}:{}:{}",
                    entry.file_name().to_string_lossy(),
                    child.as_ref().map_or(0, std::fs::Metadata::len),
                    child
                        .and_then(|metadata| metadata.modified().ok())
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |time| time.as_nanos())
                )
            })
            .collect::<Vec<_>>();
        children.sort();
        return Some(children.join("\n"));
    }
    Some(format!("{}:{modified}", metadata.len()))
}

/// One poll of `workspaceFiles.watch`: the paths that appeared, changed, or vanished since the last poll.
///
/// The first poll of a path is the baseline and reports nothing. Later polls scan again. This is a
/// directory walk, not an operating-system notifier: a file that appears and disappears between two
/// polls is never reported.
async fn watch(
    call: &RemoteCall,
    workspaces: &WorkspaceRegistry,
    sessions: Option<&dyn girder_core::SessionCatalog>,
    fallback: Option<&std::path::Path>,
) -> Result<Value, RemoteError> {
    let root =
        girder_core::workspace_file_root(workspaces, sessions, fallback, &call.args, "watch")?;
    let requested = optional_str(&call.args, "path").unwrap_or(".");
    let target = root.join(requested);
    let info = std::fs::metadata(&target).ok().map(|metadata| {
        let file_type = if metadata.is_dir() {
            girder_core::FsType::Directory
        } else if metadata.is_file() {
            girder_core::FsType::File
        } else {
            girder_core::FsType::Other
        };
        girder_core::FsInfo {
            version: girder_core::FsVersion(metadata.len().to_string()),
            file_type,
            size: metadata.len(),
        }
    });
    let plan = girder_core::workspace::watch::plan_watch(Some(&root), requested, info.as_ref())
        .map_err(|error| invalid("watch", error.to_string()))?;
    // A file is not a directory. The scan walks its parent and keeps the one name, so a save of
    // that file is reported and a sibling edit is not.
    let target_path = std::path::PathBuf::from(&plan.process_path);
    let (scan_root, only_file) = if plan.kind == girder_core::FsType::File {
        let name = target_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let parent = target_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| target_path.clone());
        (parent, name)
    } else {
        (target_path, None)
    };
    let key = plan.absolute_path.clone();
    let changes = tokio::task::spawn_blocking(move || {
        let mut watchers = watchers()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let watcher = watchers.entry(key).or_insert_with(|| {
            girder_core::fs::file_watch::PollingWatcher::new(
                scan_root,
                std::time::Duration::from_millis(500),
            )
        });
        let mut changes = watcher.scan();
        if let Some(name) = only_file {
            changes.retain(|change| change.path == name);
        }
        changes
    })
    .await
    .unwrap_or_default();
    let changes = changes
        .into_iter()
        .map(|change| {
            serde_json::json!({
                "path": change.path,
                "kind": change.kind.as_str(),
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "path": plan.absolute_path,
        "changes": changes,
        "done": false,
    }))
}

fn watchers() -> &'static std::sync::Mutex<
    std::collections::BTreeMap<String, girder_core::fs::file_watch::PollingWatcher>,
> {
    static WATCHERS: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::BTreeMap<String, girder_core::fs::file_watch::PollingWatcher>,
        >,
    > = std::sync::OnceLock::new();
    WATCHERS.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
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
/// stable prefix instead.
fn failed(message: impl Into<String>) -> RemoteError {
    RemoteError::Failed(message.into())
}

/// A refusal for a path that names nothing.
fn missing(relative: &str, workspace_id: &str) -> RemoteError {
    failed(format!(
        "`{}` does not exist in workspace `{workspace_id}`",
        display_of(relative)
    ))
}
