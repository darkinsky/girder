//! The rest of the workspace file surface: `stat`, `list`, `read`, and `search`.
//!
//! Upstream publishes six methods and this build had two (`tree` and `readFile`). A client with a file browser needs
//! `list` for one directory and `stat` for one entry, which are the calls a tree is too coarse to answer: a tree at the
//! right depth is a whole subtree, and a browser refreshing one folder should not re-walk it.
//!
//! # Why these are separate from the tree
//!
//! `tree` walks breadth-first under a budget and answers "what is here". `list` answers "what is in this directory" and
//! has no depth to choose, and `stat` answers about one thing. Trying to serve all three through `tree` with a depth of
//! one would make the caller pay for the walk to build its budget machinery and would lose the entry's own metadata.
//!
//! # Why `read` is a page of lines
//!
//! The document preview asks for a 1-based line offset and renders `text`, `lines`, and `eof` together with the
//! same file identity `stat` returns. A byte cut would split a line and would reuse `bytes` for the page length,
//! which is the file's total size. A page that would exceed the byte cap, or a file that contains a NUL, is refused.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::Command;

use crate::api::remote::{RemoteCall, RemoteError, RemoteRegistry};
use crate::fs::FileSystem;
use crate::workspace::path::{normalize as normalize_path, relative_to};
use girder_core::api::config::WorkspaceRegistry;

/// The namespace these methods register under: `workspaceFiles`, as upstream serves it.
///
/// `packages/api/workspace-files/src/index.ts` declares `super(ctx, 'workspaceFiles')`. The
/// `workspace` namespace belongs to the session-orchestration controller, which answers about
/// registered workspaces rather than about the files inside one.
pub const NAMESPACE: &str = "workspaceFiles";

/// How many lines `read` returns when the caller names no limit.
///
/// The same default upstream uses for one text page. The page is cut on line boundaries, not bytes.
pub const DEFAULT_READ_LINES: u64 = 5_000;

/// The most lines one `read` accepts.
pub const MAX_READ_LINES: u64 = 5_000;

/// The most bytes one text page may contain.
///
/// A page above this is refused. Shortening it would make a cut page look complete.
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

/// The byte budget previously applied to `read`.
///
/// `read` now pages by line. The constants stay exported because callers still name them.
pub const DEFAULT_READ_BYTES: u64 = 1024 * 1024;

/// The former ceiling on one `read`.
///
/// Line pages are capped by [`MAX_READ_LINES`] and the page byte cap instead.
pub const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

/// How many directory entries `list` returns when the caller names no cap.
///
/// The same default upstream uses for one listing. The rest is dropped and reported with `truncated`.
pub const DEFAULT_LIST_ENTRIES: usize = 2_000;

/// The most entries one `list` call may return.
const MAX_LIST_ENTRIES: usize = 5_000;

/// How many search matches a call returns when the caller names no cap.
///
/// The same default upstream's workspace search uses (`search.maxResults`).
pub const DEFAULT_SEARCH_HITS: usize = 512;

/// How many files one search may list before it stops and reports itself incomplete.
const DEFAULT_MAX_ENTRIES: usize = 100_000;

/// Hard ceiling on a caller-supplied result cap.
const MAX_RESULTS_CAP: usize = 10_000;

/// How long one `rg --files` may run.
const SEARCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// VCS metadata directories. They never contribute files or inferred parents.
const VCS_NAMES: [&str; 6] = [".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Registers `workspace.stat`, `list`, `read`, and `search`.
pub fn register_workspace_file_extras(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
) {
    register_workspace_file_extras_with_sessions(registry, fs, workspaces, None, None);
}

/// Registers the file browser methods and resolves `workspaceFileScopeId` through a session catalog.
///
/// `fallback` is the deployment workspace root. A stored session with no working directory uses it.
/// A session id that does not exist still fails, because the fallback is not a substitute for a session.
pub fn register_workspace_file_extras_with_sessions(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
    sessions: Option<Arc<dyn crate::api::remote::SessionCatalog>>,
    fallback: Option<PathBuf>,
) {
    register_stat(registry, workspaces, &sessions, &fallback);
    register_list(registry, fs, workspaces, &sessions, &fallback);
    register_read(registry, workspaces, &sessions, &fallback);
    register_search(registry, fs, workspaces, &sessions, &fallback);
}

/// The directory a file call is rooted at.
///
/// `workspaceId` keeps the registry path used by existing callers. The browser sends
/// `workspaceFileScopeId`, which is a session id. The recorded working directory wins; a session
/// that never recorded one uses `fallback`, the deployment workspace root.
pub fn workspace_file_root(
    workspaces: &WorkspaceRegistry,
    sessions: Option<&dyn crate::api::remote::SessionCatalog>,
    fallback: Option<&Path>,
    args: &Value,
    method: &str,
) -> Result<std::path::PathBuf, RemoteError> {
    if let Some(id) = nonempty(args, "workspaceId") {
        return registered_root(workspaces, id, method);
    }
    if let Some(id) = nonempty(args, "workspaceFileScopeId").or_else(|| nonempty(args, "sessionId"))
    {
        if let Some(root) = registered_root(workspaces, id, method).ok() {
            return Ok(root);
        }
        if let Some(sessions) = sessions {
            if sessions.session_header(id).is_some() {
                if let Some(cwd) = sessions.working_directory(id).filter(|cwd| !cwd.is_empty()) {
                    return Ok(std::path::PathBuf::from(cwd));
                }
                if let Some(root) = deployment_root(fallback) {
                    return Ok(root);
                }
                return Err(RemoteError::Failed(format!(
                    "gateway/lookup-not-found: session `{id}` has no workspace"
                )));
            }
        }
        return Err(RemoteError::Failed(format!(
            "gateway/lookup-not-found: no session `{id}`"
        )));
    }
    Err(RemoteError::InvalidArguments {
        namespace: NAMESPACE.to_owned(),
        method: method.to_owned(),
        message: "`workspaceId` is required".to_owned(),
    })
}

fn deployment_root(fallback: Option<&Path>) -> Option<PathBuf> {
    let path = fallback?;
    let spelled = path.to_string_lossy();
    crate::is_execution_world_path(spelled.as_ref()).then(|| path.to_path_buf())
}

fn outside_workspace(requested: &str) -> RemoteError {
    RemoteError::Failed(format!(
        "workspace-file/outside-workspace: `{requested}` is outside the workspace"
    ))
}

async fn list_failure(requested: &str, target: &Path, error: std::io::Error) -> RemoteError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return RemoteError::Failed(format!(
            "workspace-file/not-found: no entry at `{requested}`"
        ));
    }
    let kind = tokio::fs::symlink_metadata(target)
        .await
        .ok()
        .map(|metadata| {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                "symlink"
            } else if file_type.is_dir() {
                "directory"
            } else if file_type.is_file() {
                "file"
            } else {
                "other"
            }
        });
    if error.kind() == std::io::ErrorKind::NotADirectory
        || kind.is_some_and(|kind| kind != "directory")
    {
        let kind = kind.unwrap_or("other");
        return RemoteError::Failed(format!(
            "workspace-file/not-directory: `{requested}` is a {kind}"
        ));
    }
    RemoteError::Failed(format!("cannot list `{requested}`: {error}"))
}

fn nonempty<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn registered_root(
    workspaces: &WorkspaceRegistry,
    id: &str,
    method: &str,
) -> Result<std::path::PathBuf, RemoteError> {
    workspaces
        .list()
        .into_iter()
        .find(|workspace| workspace.id == id)
        .map(|workspace| workspace.path)
        .ok_or_else(|| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: format!("unknown workspace `{id}`"),
        })
}

fn request_fields<'a>(args: &'a Value) -> &'a Value {
    args.get("request")
        .filter(|value| value.is_object())
        .unwrap_or(args)
}

fn range_fields<'a>(args: &'a Value) -> &'a Value {
    args.get("range")
        .filter(|value| value.is_object())
        .unwrap_or(args)
}

/// Resolves a request path against the workspace it names.
///
/// The same containment rule the tree and readFile use, reached through the same helper: a path one of these methods
/// accepted and another refused would be a boundary that depends on which verb the client chose.
async fn confine(
    fs: &dyn FileSystem,
    workspaces: &WorkspaceRegistry,
    sessions: Option<&dyn crate::api::remote::SessionCatalog>,
    fallback: Option<&Path>,
    args: &Value,
    method: &str,
) -> Result<(std::path::PathBuf, std::path::PathBuf, String), RemoteError> {
    let requested = args.get("path").and_then(Value::as_str).unwrap_or_default();
    let root = workspace_file_root(workspaces, sessions, fallback, args, method)?;

    let resolved = crate::workspace::path::resolve(Some(&root), requested);
    let normalized = normalize_path(&resolved);
    if !crate::workspace::path::contains(&root, &normalized) {
        return Err(outside_workspace(requested));
    }
    // Canonicalised so a symlink inside the workspace cannot point outside it past the lexical check above. Only when
    // the path exists: `stat` on a missing path must answer that it is missing, not fail to canonicalise it.
    let target = match tokio::fs::canonicalize(&normalized).await {
        Ok(canonical) => {
            if !crate::workspace::path::contains(&root, &canonical) {
                return Err(outside_workspace(requested));
            }
            canonical
        }
        Err(_) => normalized,
    };
    let relative = relative_to(&root, &target).map_or_else(String::new, |path| {
        path.to_string_lossy().replace('\\', "/")
    });
    let _ = fs;
    Ok((root, target, relative))
}

/// Registers `workspaceFiles.stat`.
fn register_stat(
    registry: &mut RemoteRegistry,
    workspaces: &Arc<WorkspaceRegistry>,
    sessions: &Option<Arc<dyn crate::api::remote::SessionCatalog>>,
    fallback: &Option<PathBuf>,
) {
    let workspaces = Arc::clone(workspaces);
    let sessions = sessions.clone();
    let fallback = fallback.clone();
    registry.register(NAMESPACE, "stat", move |call: RemoteCall| {
        let workspaces = Arc::clone(&workspaces);
        let sessions = sessions.clone();
        let fallback = fallback.clone();
        Box::pin(async move {
            let target = resolve_file_target(
                &workspaces,
                sessions.as_deref(),
                fallback.as_deref(),
                &call.args,
                "stat",
            )
            .await?;
            let metadata = tokio::fs::metadata(&target).await.map_err(|error| {
                RemoteError::Failed(format!(
                    "workspace-file/not-found: cannot stat `{}`: {error}",
                    target.display()
                ))
            })?;
            Ok(stat_value(&target, &metadata))
        })
    });
    registry.declare(
        NAMESPACE,
        "stat",
        crate::guard::access::Declaration::read("workspace"),
    );
}

/// The identity and freshness fields shared by `stat`, `read`, and `readBytes`.
fn stat_value(path: &Path, metadata: &std::fs::Metadata) -> Value {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos());
    json!({
        "absolutePath": path.to_string_lossy().replace('\\', "/"),
        "version": format!("{}:{}", metadata.len(), modified),
        "bytes": metadata.len(),
    })
}

/// Registers `workspace.list`: the entries of one directory.
fn register_list(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
    sessions: &Option<Arc<dyn crate::api::remote::SessionCatalog>>,
    fallback: &Option<PathBuf>,
) {
    let fs = Arc::clone(fs);
    let workspaces = Arc::clone(workspaces);
    let sessions = sessions.clone();
    let fallback = fallback.clone();
    registry.register(NAMESPACE, "list", move |call: RemoteCall| {
        let fs = Arc::clone(&fs);
        let workspaces = Arc::clone(&workspaces);
        let sessions = sessions.clone();
        let fallback = fallback.clone();
        Box::pin(async move {
            let (_root, target, relative) = confine(
                fs.as_ref(),
                &workspaces,
                sessions.as_deref(),
                fallback.as_deref(),
                &call.args,
                "list",
            )
            .await?;
            let max_entries = match bounded_count(
                &call.args,
                "list",
                &["maxEntries"],
                DEFAULT_LIST_ENTRIES,
                MAX_LIST_ENTRIES,
            ) {
                Ok(count) => count,
                Err(error) => return Err(error),
            };
            let requested = call
                .args
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(relative.as_str());
            let mut entries = Vec::new();
            let mut reader = match tokio::fs::read_dir(&target).await {
                Ok(reader) => reader,
                Err(error) => return Err(list_failure(requested, &target, error).await),
            };
            while let Ok(Some(entry)) = reader.next_entry().await {
                entries.push(directory_entry(&entry).await);
            }
            // Name order, the same order upstream's listing keeps. The file tree tells a directory
            // from a file by `type`, so putting directories first would hide a name the client sorts
            // against this answer.
            entries.sort_by(|left, right| {
                left["name"]
                    .as_str()
                    .unwrap_or_default()
                    .cmp(right["name"].as_str().unwrap_or_default())
            });
            let truncated = entries.len() > max_entries;
            entries.truncate(max_entries);
            Ok(json!({ "path": relative, "entries": entries, "truncated": truncated }))
        })
    });
    registry.declare(
        NAMESPACE,
        "list",
        crate::guard::access::Declaration::read("workspace"),
    );
}

/// Registers `workspaceFiles.read`: one page of lines from a UTF-8 file.
///
/// `offset` is the 1-based first line and `limit` is how many lines the page may hold. The answer
/// carries the same `absolutePath`, `version`, and `bytes` as `stat`, plus the page itself.
fn register_read(
    registry: &mut RemoteRegistry,
    workspaces: &Arc<WorkspaceRegistry>,
    sessions: &Option<Arc<dyn crate::api::remote::SessionCatalog>>,
    fallback: &Option<PathBuf>,
) {
    let workspaces = Arc::clone(workspaces);
    let sessions = sessions.clone();
    let fallback = fallback.clone();
    registry.register(NAMESPACE, "read", move |call: RemoteCall| {
        let workspaces = Arc::clone(&workspaces);
        let sessions = sessions.clone();
        let fallback = fallback.clone();
        Box::pin(async move {
            let target = resolve_file_target(
                &workspaces,
                sessions.as_deref(),
                fallback.as_deref(),
                &call.args,
                "read",
            )
            .await?;
            let range = range_fields(&call.args);
            let offset = page_bound(range, "offset", 1, 1, u64::MAX)?;
            let limit = page_bound(range, "limit", DEFAULT_READ_LINES, 1, MAX_READ_LINES)?;
            read_page(&target, offset, limit).await
        })
    });
    registry.declare(
        NAMESPACE,
        "read",
        crate::guard::access::Declaration::read("workspace"),
    );
}

/// Resolves a regular file for `stat`/`read`.
///
/// Relative paths stay inside the registered workspace. Absolute paths are allowed by the
/// upstream file API, but a symlink is still rejected before canonicalization so its target
/// cannot silently change the file identity.
async fn resolve_file_target(
    workspaces: &WorkspaceRegistry,
    sessions: Option<&dyn crate::api::remote::SessionCatalog>,
    fallback: Option<&Path>,
    args: &Value,
    method: &str,
) -> Result<PathBuf, RemoteError> {
    let workspace_root = workspace_file_root(workspaces, sessions, fallback, args, method)?;
    let requested =
        args.get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| RemoteError::InvalidArguments {
                namespace: NAMESPACE.to_owned(),
                method: method.to_owned(),
                message: "`path` is required".to_owned(),
            })?;
    if requested.contains('\0') {
        return Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: "`path` contains a NUL byte".to_owned(),
        });
    }
    let absolute = Path::new(requested).is_absolute();
    let candidate = if absolute {
        PathBuf::from(requested)
    } else {
        workspace_root.join(requested)
    };
    let normalized = normalize_path(&candidate);
    if !absolute && !crate::workspace::path::contains(&workspace_root, &normalized) {
        return Err(RemoteError::Refused {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            reason: format!("`{requested}` escapes the workspace"),
        });
    }
    let link_metadata = tokio::fs::symlink_metadata(&normalized)
        .await
        .map_err(|error| {
            RemoteError::Failed(format!(
                "workspace-file/not-found: cannot stat `{requested}`: {error}"
            ))
        })?;
    if link_metadata.file_type().is_symlink() {
        return Err(RemoteError::Failed(format!(
            "workspace-file/not-regular-file: `{requested}` is a symlink"
        )));
    }
    let target = tokio::fs::canonicalize(&normalized)
        .await
        .map_err(|error| {
            RemoteError::Failed(format!(
                "workspace-file/not-found: cannot stat `{requested}`: {error}"
            ))
        })?;
    let metadata = tokio::fs::metadata(&target).await.map_err(|error| {
        RemoteError::Failed(format!(
            "workspace-file/not-found: cannot stat `{requested}`: {error}"
        ))
    })?;
    if !metadata.is_file() {
        return Err(RemoteError::Failed(format!(
            "workspace-file/not-regular-file: `{requested}` is not a regular file"
        )));
    }
    Ok(target)
}

fn page_bound(
    args: &Value,
    name: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, RemoteError> {
    let Some(value) = args.get(name) else {
        return Ok(default);
    };
    let Some(count) = value.as_u64() else {
        return Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: "read".to_owned(),
            message: format!("`{name}` must be an integer of at least {min}"),
        });
    };
    if count < min || count > max {
        return Err(RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: "read".to_owned(),
            message: format!("`{name}` must be from {min} to {max}"),
        });
    }
    Ok(count)
}

async fn read_page(target: &Path, offset: u64, limit: u64) -> Result<Value, RemoteError> {
    let symlink = tokio::fs::symlink_metadata(target).await.map_err(|error| {
        RemoteError::Failed(format!(
            "workspace-file/not-found: cannot read `{}`: {error}",
            target.display()
        ))
    })?;
    if symlink.file_type().is_symlink() || !symlink.is_file() {
        return Err(RemoteError::Failed(format!(
            "workspace-file/not-regular-file: `{}` is not a regular file",
            target.display()
        )));
    }
    let file = tokio::fs::File::open(target).await.map_err(|error| {
        RemoteError::Failed(format!(
            "workspace-file/not-found: cannot read `{}`: {error}",
            target.display()
        ))
    })?;
    let metadata = file.metadata().await.map_err(|error| {
        RemoteError::Failed(format!(
            "workspace-file/not-found: cannot read `{}`: {error}",
            target.display()
        ))
    })?;
    let mut value = stat_value(target, &metadata);
    let mut reader = tokio::io::BufReader::new(file);
    let mut number = 1_u64;
    let last = offset.saturating_add(limit).saturating_sub(1);
    let mut lines = Vec::new();
    let mut page_bytes = 0_usize;
    let mut eof = true;
    loop {
        let Some(line) = take_line(&mut reader, target).await? else {
            break;
        };
        if number >= offset {
            let content = line.trim_end_matches(['\n', '\r']);
            page_bytes = page_bytes.saturating_add(content.len());
            if !lines.is_empty() {
                page_bytes = page_bytes.saturating_add(1);
            }
            if page_bytes > MAX_PAGE_BYTES {
                return Err(RemoteError::Failed(format!(
                    "workspace-file/too-large: lines {offset}-{last} of `{}` exceed the {MAX_PAGE_BYTES} byte cap",
                    target.display()
                )));
            }
            lines.push(content.to_owned());
        }
        if number == last {
            let peeked = reader.fill_buf().await.map_err(|error| {
                RemoteError::Failed(format!(
                    "workspace-file/not-text: `{}` is not valid UTF-8 ({error})",
                    target.display()
                ))
            })?;
            eof = peeked.is_empty();
            break;
        }
        number += 1;
    }
    value["offset"] = json!(offset);
    value["text"] = json!(lines.join("\n"));
    value["lines"] = json!(lines.len());
    value["eof"] = json!(eof);
    Ok(value)
}

/// One line, including its newline, or nothing at the end of the file.
///
/// The read stops at the page cap. A longer line is refused instead of being buffered whole.
async fn take_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    target: &Path,
) -> Result<Option<String>, RemoteError> {
    let mut buf = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|error| {
            RemoteError::Failed(format!(
                "workspace-file/not-text: `{}` is not valid UTF-8 ({error})",
                target.display()
            ))
        })?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if buf.len().saturating_add(take) > MAX_PAGE_BYTES {
            return Err(RemoteError::Failed(format!(
                "workspace-file/too-large: a line of `{}` exceeds the {MAX_PAGE_BYTES} byte cap",
                target.display()
            )));
        }
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            break;
        }
    }
    if buf.contains(&0) {
        return Err(RemoteError::Failed(format!(
            "workspace-file/not-text: `{}` contains NUL bytes",
            target.display()
        )));
    }
    String::from_utf8(buf).map(Some).map_err(|_| {
        RemoteError::Failed(format!(
            "workspace-file/not-text: `{}` is not valid UTF-8",
            target.display()
        ))
    })
}

/// Registers `workspaceFiles.search`: name and path discovery through `rg --files`.
///
/// The browser searches names, not file contents. `rg --files` is what upstream runs, so ignore
/// rules, hidden files, and VCS directories follow ripgrep. A machine without `rg` fails with
/// `workspace-file/search-failed` rather than walking the tree and pretending the same filters ran.
fn register_search(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
    sessions: &Option<Arc<dyn crate::api::remote::SessionCatalog>>,
    fallback: &Option<PathBuf>,
) {
    let fs = Arc::clone(fs);
    let workspaces = Arc::clone(workspaces);
    let sessions = sessions.clone();
    let fallback = fallback.clone();
    registry.register(NAMESPACE, "search", move |call: RemoteCall| {
        let fs = Arc::clone(&fs);
        let workspaces = Arc::clone(&workspaces);
        let sessions = sessions.clone();
        let fallback = fallback.clone();
        Box::pin(async move {
            let (_root, target, _relative) = confine(
                fs.as_ref(),
                &workspaces,
                sessions.as_deref(),
                fallback.as_deref(),
                &call.args,
                "search",
            )
            .await?;
            let fields = request_fields(&call.args);
            let query = fields
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            if query.is_empty() {
                return Err(RemoteError::Failed(
                    "gateway/bad-request: `query` is required".to_owned(),
                ));
            }
            if !tokio::fs::metadata(&target)
                .await
                .is_ok_and(|metadata| metadata.is_dir())
            {
                return Err(RemoteError::Failed(format!(
                    "workspace-file/not-directory: `{target}` is not a directory",
                    target = target.display()
                )));
            }
            let max_results = bounded_count(
                fields,
                "search",
                &["maxResults", "limit"],
                DEFAULT_SEARCH_HITS,
                MAX_RESULTS_CAP,
            )?;
            let max_entries = bounded_count(
                fields,
                "search",
                &["maxEntries"],
                DEFAULT_MAX_ENTRIES,
                DEFAULT_MAX_ENTRIES,
            )?;
            let include_hidden = flag(fields, "includeHidden", false);
            let respect_gitignore = flag(fields, "respectGitignore", true);
            let excluded = string_list(fields, "excludedPaths");
            match tokio::time::timeout(
                SEARCH_TIMEOUT,
                search_with_rg(
                    &target,
                    &query,
                    &excluded,
                    include_hidden,
                    respect_gitignore,
                    max_results,
                    max_entries,
                ),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(RemoteError::Failed(
                    "workspace-file/search-failed: Workspace file search timed out".to_owned(),
                )),
            }
        })
    });
    registry.declare(
        NAMESPACE,
        "search",
        crate::guard::access::Declaration::read("workspace"),
    );
}

fn flag(args: &Value, name: &str, default: bool) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(default)
}

fn string_list(args: &Value, name: &str) -> Vec<String> {
    args.get(name)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn bounded_count(
    args: &Value,
    method: &str,
    names: &[&str],
    default: usize,
    cap: usize,
) -> Result<usize, RemoteError> {
    for name in names {
        if let Some(value) = args.get(*name) {
            let Some(count) = value.as_u64().and_then(|count| usize::try_from(count).ok()) else {
                return Err(RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: method.to_owned(),
                    message: format!("`{name}` must be a positive integer"),
                });
            };
            if count == 0 || count > cap {
                return Err(RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: method.to_owned(),
                    message: format!("`{name}` must be from 1 to {cap}"),
                });
            }
            return Ok(count);
        }
    }
    Ok(default)
}

/// One child in the shape the file tree reads: `type`, and `size` only for a regular file.
///
/// `metadata` follows a symlink, so a link is reported as its target. A dangling link has no
/// target and is `other`.
async fn directory_entry(entry: &tokio::fs::DirEntry) -> Value {
    let name = entry.file_name().to_string_lossy().into_owned();
    let Ok(metadata) = entry.metadata().await else {
        return json!({ "name": name, "type": "other" });
    };
    if metadata.is_dir() {
        return json!({ "name": name, "type": "directory" });
    }
    if metadata.is_file() {
        return json!({ "name": name, "type": "file", "size": metadata.len() });
    }
    json!({ "name": name, "type": "other" })
}

/// One literal exclusion, escaped so ripgrep treats it as a name rather than a glob.
fn exclusion(path: &str) -> String {
    let mut literal = String::new();
    for character in path.chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']' | '{' | '}' | '!') {
            literal.push('\\');
        }
        literal.push(character);
    }
    if path.contains('/') {
        format!("--glob=!/{literal}")
    } else {
        format!("--glob=!{literal}")
    }
}

async fn search_with_rg(
    root: &std::path::Path,
    query: &str,
    excluded: &[String],
    include_hidden: bool,
    respect_gitignore: bool,
    max_results: usize,
    max_entries: usize,
) -> Result<Value, RemoteError> {
    let mut command = Command::new("rg");
    command
        .arg("--files")
        .arg("--sort=path")
        .arg("--null")
        .arg("--path-separator=/")
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if include_hidden {
        command.arg("--hidden");
    }
    if !respect_gitignore {
        command.arg("--no-ignore");
    }
    for name in VCS_NAMES {
        command.arg(exclusion(name));
    }
    for path in excluded {
        command.arg(exclusion(path));
    }
    let mut child = command.spawn().map_err(|error| {
        RemoteError::Failed(format!(
            "workspace-file/search-failed: Could not launch workspace file search ({error})"
        ))
    })?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        RemoteError::Failed(
            "workspace-file/search-failed: workspace file search produced no output".to_owned(),
        )
    })?;
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut sink = Vec::new();
        let Some(mut stderr) = stderr else {
            return sink;
        };
        let mut chunk = [0_u8; 1024];
        while let Ok(read) = stderr.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            if sink.len() < 65_536 {
                let room = 65_536 - sink.len();
                sink.extend_from_slice(&chunk[..read.min(room)]);
            }
        }
        sink
    });
    let needle = query.to_lowercase();
    let by_path = query.contains('/');
    let mut entries: BTreeMap<String, &'static str> = BTreeMap::new();
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut listed = 0_usize;
    let mut stopped = false;
    loop {
        let read = stdout.read(&mut chunk).await.map_err(|error| {
            RemoteError::Failed(format!(
                "workspace-file/search-failed: Could not read workspace file search ({error})"
            ))
        })?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        while let Some(end) = buffer.iter().position(|byte| *byte == 0) {
            let path = String::from_utf8_lossy(&buffer[..end]).into_owned();
            buffer.drain(..=end);
            if path.is_empty() {
                continue;
            }
            if listed == max_entries {
                stopped = true;
                break;
            }
            admit(&mut entries, &path, "file", &needle, by_path);
            let mut slash = path.rfind('/');
            while let Some(end) = slash {
                admit(&mut entries, &path[..end], "directory", &needle, by_path);
                slash = path[..end].rfind('/');
            }
            listed += 1;
        }
        if stopped {
            break;
        }
    }
    if stopped {
        let _ = child.kill().await;
    }
    let status = child.wait().await.map_err(|error| {
        RemoteError::Failed(format!(
            "workspace-file/search-failed: workspace file search did not exit ({error})"
        ))
    })?;
    let code = status.code();
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    let stderr_text = String::from_utf8_lossy(&stderr_bytes).trim().to_owned();
    if !stopped && !matches!(code, Some(0 | 1 | 2)) {
        let detail = if stderr_text.is_empty() {
            String::new()
        } else {
            format!(": {stderr_text}")
        };
        return Err(RemoteError::Failed(format!(
            "workspace-file/search-failed: workspace file search exited with code {}{detail}",
            code.map_or_else(|| "none".to_owned(), |code| code.to_string())
        )));
    }
    let mut matches: Vec<(String, &'static str)> = entries.into_iter().collect();
    matches.sort_by(|left, right| {
        left.0
            .split('/')
            .count()
            .cmp(&right.0.split('/').count())
            .then_with(|| left.0.cmp(&right.0))
    });
    let truncated = matches.len() > max_results;
    let listed_matches = matches
        .into_iter()
        .take(max_results)
        .map(|(path, kind)| json!({ "path": path, "type": kind }))
        .collect::<Vec<_>>();
    Ok(json!({
        "matches": listed_matches,
        "truncated": truncated,
        "incomplete": stopped || code == Some(2),
    }))
}

fn admit(
    entries: &mut BTreeMap<String, &'static str>,
    path: &str,
    kind: &'static str,
    needle: &str,
    by_path: bool,
) {
    let name = path.rsplit('/').next().unwrap_or(path);
    let haystack = if by_path { path } else { name };
    if haystack.to_lowercase().contains(needle) {
        entries.entry(path.to_owned()).or_insert(kind);
    }
}
