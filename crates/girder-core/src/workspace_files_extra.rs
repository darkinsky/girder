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
//! # Why `read` is bounded
//!
//! A file can be a gigabyte. `read` takes a limit and reports whether the content was cut, because a client that
//! received the first megabyte of a large file with no indication it was truncated would render half a file as if it
//! were whole.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::fs::FileSystem;
use crate::remote::{RemoteCall, RemoteError, RemoteRegistry};
use crate::workspace_path::{normalize as normalize_path, relative_to};
use girder_core::api_config::WorkspaceRegistry;

/// The namespace these methods register under.
pub const NAMESPACE: &str = "workspace";

/// How much of a file `read` returns when the caller names no limit.
///
/// One megabyte: enough for a source file a person would read and small enough that a frame carrying it is still a
/// frame. A caller that wants more asks for more, which is the point of the parameter.
pub const DEFAULT_READ_BYTES: u64 = 1024 * 1024;

/// The most a single `read` may return.
///
/// Eight megabytes: a ceiling rather than a preference, so one call cannot ask for a file that would not fit in a
/// message. A caller that needs more reads in ranges.
pub const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

/// How many search hits a call returns by default.
pub const DEFAULT_SEARCH_HITS: usize = 50;

/// Registers `workspace.stat`, `list`, `read`, and `search`.
pub fn register_workspace_file_extras(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
) {
    register_stat(registry, fs, workspaces);
    register_list(registry, fs, workspaces);
    register_read(registry, fs, workspaces);
    register_search(registry, fs, workspaces);
}

/// Resolves a request path against the workspace it names.
///
/// The same containment rule the tree and readFile use, reached through the same helper: a path one of these methods
/// accepted and another refused would be a boundary that depends on which verb the client chose.
async fn confine(
    fs: &dyn FileSystem,
    workspaces: &WorkspaceRegistry,
    args: &Value,
    method: &str,
) -> Result<(std::path::PathBuf, std::path::PathBuf, String), RemoteError> {
    let workspace_id = args
        .get("workspaceId")
        .and_then(Value::as_str)
        .ok_or_else(|| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: "`workspaceId` is required".to_owned(),
        })?;
    let requested = args.get("path").and_then(Value::as_str).unwrap_or_default();

    let root = workspaces
        .list()
        .into_iter()
        .find(|workspace| workspace.id == workspace_id)
        .map(|workspace| workspace.path)
        .ok_or_else(|| RemoteError::InvalidArguments {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            message: format!("no workspace `{workspace_id}`"),
        })?;

    let resolved = crate::workspace_path::resolve(Some(&root), requested);
    let normalized = normalize_path(&resolved);
    if !crate::workspace_path::contains(&root, &normalized) {
        return Err(RemoteError::Refused {
            namespace: NAMESPACE.to_owned(),
            method: method.to_owned(),
            reason: format!("`{requested}` is outside the workspace"),
        });
    }
    // Canonicalised so a symlink inside the workspace cannot point outside it past the lexical check above. Only when
    // the path exists: `stat` on a missing path must answer that it is missing, not fail to canonicalise it.
    let target = match tokio::fs::canonicalize(&normalized).await {
        Ok(canonical) => {
            if !crate::workspace_path::contains(&root, &canonical) {
                return Err(RemoteError::Refused {
                    namespace: NAMESPACE.to_owned(),
                    method: method.to_owned(),
                    reason: format!("`{requested}` resolves outside the workspace"),
                });
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

/// Registers `workspace.stat`.
fn register_stat(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
) {
    let fs = Arc::clone(fs);
    let workspaces = Arc::clone(workspaces);
    registry.register(NAMESPACE, "stat", move |call: RemoteCall| {
        let fs = Arc::clone(&fs);
        let workspaces = Arc::clone(&workspaces);
        Box::pin(async move {
            let (_root, target, relative) =
                confine(fs.as_ref(), &workspaces, &call.args, "stat").await?;
            // A missing path is an answer, not a failure: a browser stat-ing a deleted file needs to hear "gone" rather
            // than a transport error it would show as a crash.
            let Ok(metadata) = tokio::fs::metadata(&target).await else {
                return Ok(json!({
                    "exists": false,
                    "path": relative,
                }));
            };
            Ok(json!({
                "exists": true,
                "path": relative,
                "isFile": metadata.is_file(),
                "isDirectory": metadata.is_dir(),
                "isSymlink": metadata.file_type().is_symlink(),
                // Only meaningful for a file; a directory's size is a platform artifact rather than an answer.
                "size": if metadata.is_file() { metadata.len() } else { 0 },
                "modifiedMs": modified_ms(&metadata),
            }))
        })
    });
    registry.declare(
        NAMESPACE,
        "stat",
        crate::access::Declaration::read("workspace"),
    );
}

/// A file's modification time in milliseconds since the epoch, when the platform reports one.
fn modified_ms(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
}

/// Registers `workspace.list`: the entries of one directory.
fn register_list(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
) {
    let fs = Arc::clone(fs);
    let workspaces = Arc::clone(workspaces);
    registry.register(NAMESPACE, "list", move |call: RemoteCall| {
        let fs = Arc::clone(&fs);
        let workspaces = Arc::clone(&workspaces);
        Box::pin(async move {
            let (_root, target, relative) = confine(fs.as_ref(), &workspaces, &call.args, "list").await?;
            let mut entries = Vec::new();
            let mut reader = match tokio::fs::read_dir(&target).await {
                Ok(reader) => reader,
                Err(error) => {
                    return Err(RemoteError::Failed(format!(
                        "cannot list `{relative}`: {error}"
                    )));
                }
            };
            while let Ok(Some(entry)) = reader.next_entry().await {
                let metadata = entry.metadata().await.ok();
                let is_dir = metadata.as_ref().is_some_and(std::fs::Metadata::is_dir);
                entries.push(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "path": format!("{relative}/{}", entry.file_name().to_string_lossy()).trim_start_matches('/').to_owned(),
                    "isDirectory": is_dir,
                    "size": metadata.as_ref().filter(|m| m.is_file()).map_or(0, std::fs::Metadata::len),
                }));
            }
            // Sorted by name with directories first: a browser showing a random order and mixing files into folders
            // is the one thing every file manager agrees not to do.
            entries.sort_by(|a, b| {
                let a_dir = a["isDirectory"].as_bool().unwrap_or(false);
                let b_dir = b["isDirectory"].as_bool().unwrap_or(false);
                b_dir.cmp(&a_dir).then_with(|| {
                    a["name"]
                        .as_str()
                        .unwrap_or_default()
                        .cmp(b["name"].as_str().unwrap_or_default())
                })
            });
            Ok(json!({ "path": relative, "entries": entries }))
        })
    });
    registry.declare(
        NAMESPACE,
        "list",
        crate::access::Declaration::read("workspace"),
    );
}

/// Registers `workspace.read`.
fn register_read(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
) {
    let fs = Arc::clone(fs);
    let workspaces = Arc::clone(workspaces);
    registry.register(NAMESPACE, "read", move |call: RemoteCall| {
        let fs = Arc::clone(&fs);
        let workspaces = Arc::clone(&workspaces);
        Box::pin(async move {
            let (_root, target, relative) =
                confine(fs.as_ref(), &workspaces, &call.args, "read").await?;
            let limit = call
                .args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_READ_BYTES)
                .min(MAX_READ_BYTES);
            let bytes = match tokio::fs::read(&target).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    return Err(RemoteError::Failed(format!(
                        "cannot read `{relative}`: {error}"
                    )));
                }
            };
            let total = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            let truncated = total > limit;
            let cut = usize::try_from(limit.min(total)).unwrap_or(usize::MAX);
            // A cut inside a multi-byte character would produce a string with a replacement character, which a client
            // would display as corruption. Walking back to a boundary costs at most three bytes.
            let mut end = cut;
            while end > 0 && !is_boundary(&bytes, end) {
                end -= 1;
            }
            Ok(json!({
                "path": relative,
                // Lossy on purpose: the method is for text a person reads, and a file with a stray byte should show
                // what it says rather than fail. `readBytes` is the method for exact content.
                "text": String::from_utf8_lossy(&bytes[..end]),
                "bytes": end,
                "totalBytes": total,
                "truncated": truncated,
            }))
        })
    });
    registry.declare(
        NAMESPACE,
        "read",
        crate::access::Declaration::read("workspace"),
    );
}

/// Whether `index` begins a UTF-8 character.
fn is_boundary(bytes: &[u8], index: usize) -> bool {
    index == bytes.len() || (bytes[index] & 0b1100_0000) != 0b1000_0000
}

/// Registers `workspace.search`: a filename search across a workspace.
///
/// Filenames rather than contents: searching content is what `grep` is for and what the model already has a tool for,
/// while a file browser's search box looks for names. Doing both here would make one method answer two questions and
/// make the slow one the default.
fn register_search(
    registry: &mut RemoteRegistry,
    fs: &Arc<dyn FileSystem>,
    workspaces: &Arc<WorkspaceRegistry>,
) {
    let fs = Arc::clone(fs);
    let workspaces = Arc::clone(workspaces);
    registry.register(NAMESPACE, "search", move |call: RemoteCall| {
        let fs = Arc::clone(&fs);
        let workspaces = Arc::clone(&workspaces);
        Box::pin(async move {
            let (_root, target, _relative) = confine(fs.as_ref(), &workspaces, &call.args, "search").await?;
            let query = call
                .args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_lowercase();
            if query.is_empty() {
                return Err(RemoteError::InvalidArguments {
                    namespace: NAMESPACE.to_owned(),
                    method: "search".to_owned(),
                    message: "`query` is required".to_owned(),
                });
            }
            let limit = call
                .args
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(DEFAULT_SEARCH_HITS);
            let root = target.clone();
            let mut hits = Vec::new();
            let mut truncated = false;
            let mut stack = vec![target];
            while let Some(directory) = stack.pop() {
                let Ok(mut reader) = tokio::fs::read_dir(&directory).await else {
                    continue;
                };
                while let Ok(Some(entry)) = reader.next_entry().await {
                    let name = entry.file_name().to_string_lossy().to_lowercase();
                    let path = entry.path();
                    // The query matches the name anywhere, which is what a search box does; a prefix-only match would
                    // miss `main.rs` for `rs`.
                    if name.contains(&query) {
                        if hits.len() >= limit {
                            truncated = true;
                            break;
                        }
                        hits.push(json!({
                            "name": entry.file_name().to_string_lossy(),
                            "path": relative_to(&root, &path).map_or_else(String::new, |p| p.to_string_lossy().replace('\\', "/")),
                            "isDirectory": entry.metadata().await.is_ok_and(|m| m.is_dir()),
                        }));
                    }
                    if entry.metadata().await.is_ok_and(|m| m.is_dir()) {
                        stack.push(path);
                    }
                }
                if truncated {
                    break;
                }
            }
            Ok(json!({ "query": query, "hits": hits, "truncated": truncated }))
        })
    });
    registry.declare(
        NAMESPACE,
        "search",
        crate::access::Declaration::read("workspace"),
    );
}
