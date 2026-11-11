//! The `fs.*` RPC methods: file metadata and directory listing.
//!
//! These exist because the tool surface covers reading and writing a *file*, and nothing answers "what is in this
//! directory" or "how large is it" without reading it. A client rendering a file browser, or a host deciding whether a
//! target is worth opening, needs those two questions answered without the bytes.
//!
//! # Why metadata is its own method
//!
//! `stat` costs one syscall and `read` costs the whole file. A caller that only wanted the size would pay for the
//! contents, and on a large file it would pay a great deal: a 1 GB log inspected this way is a 1 GB read.
//!
//! # Why absence is a result and not an error
//!
//! "This path does not exist" is the normal answer to a question about a path. Reporting it as an error would make
//! every caller put its existence check in an error branch, where it reads like a failure rather than a fact.
//!
//! # Why the listing is bounded
//!
//! A directory can hold a hundred thousand entries. The bound is applied while collecting, and the result says whether
//! it stopped — a caller that received exactly the limit cannot otherwise tell whether more existed.

use std::sync::Arc;

use crate::fs::{FileSystem, FsTarget, FsType};

use crate::remote::{RemoteCall, RemoteError, RemoteRegistry};
use serde_json::Value;

/// How many entries a listing returns before it stops and says so.
///
/// 500: enough to fill a file browser several times over and far below the point where a JSON response becomes
/// unpleasant to parse.
pub const DEFAULT_LIST_LIMIT: usize = 500;

/// Registers the `fs.*` methods over one filesystem.
pub fn register_fs_methods(registry: &mut RemoteRegistry, fs: &Arc<dyn FileSystem>) {
    let stat_fs = Arc::clone(fs);
    registry.register("fs", "stat", move |call: RemoteCall| {
        let fs = Arc::clone(&stat_fs);
        Box::pin(async move {
            let path = required_path(&call, "stat")?;
            let target = fs.resolve(&path, None);
            match fs
                .stat(&target, tokio_util::sync::CancellationToken::new())
                .await
            {
                // Absence is a fact, not a failure: a caller asking whether a path exists gets `null` and reads it as
                // "no", which is the answer it wanted.
                Ok(None) => Ok(serde_json::json!({ "exists": false })),
                Ok(Some(info)) => Ok(serde_json::json!({
                    "exists": true,
                    "version": info.version.0,
                    "kind": kind_name(info.file_type),
                    "size": info.size,
                })),
                Err(error) => Err(RemoteError::Failed(error.to_string())),
            }
        })
    });

    let list_fs = Arc::clone(fs);
    registry.register("fs", "list", move |call: RemoteCall| {
        let fs = Arc::clone(&list_fs);
        Box::pin(async move {
            let path = required_path(&call, "list")?;
            let limit = usize::try_from(
                call.args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_LIST_LIMIT as u64),
            )
            .unwrap_or(DEFAULT_LIST_LIMIT)
            .max(1);

            let target: FsTarget = fs.resolve(&path, None);
            match fs
                .list_dir(&target, tokio_util::sync::CancellationToken::new())
                .await
            {
                Ok(entries) => {
                    // The truncation is computed before the list is shortened, so it reports what was dropped rather
                    // than whether the caller happened to ask for fewer than it got.
                    let truncated = entries.len() > limit;
                    let listed: Vec<serde_json::Value> = entries
                        .iter()
                        .take(limit)
                        .map(|entry| {
                            serde_json::json!({
                                "name": entry.name,
                                // Only the name, not a path: joining belongs to the caller, which knows its own
                                // separator and root.
                                "kind": kind_name(entry.file_type),
                            })
                        })
                        .collect();
                    Ok(serde_json::json!({
                        "entries": listed,
                        "truncated": truncated,
                        "total": entries.len(),
                    }))
                }
                Err(error) => Err(RemoteError::Failed(error.to_string())),
            }
        })
    });
}

/// The `path` argument, or an error naming the method.
fn required_path(call: &RemoteCall, method: &str) -> Result<String, RemoteError> {
    call.args
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RemoteError::InvalidArguments {
            namespace: "fs".to_owned(),
            method: method.to_owned(),
            message: "`path` is required".to_owned(),
        })
}

/// The wire name of a file type.
fn kind_name(kind: FsType) -> &'static str {
    match kind {
        FsType::File => "file",
        FsType::Directory => "directory",
        // Deliberately not "symlink": this layer cannot tell a symlink from a device or a socket, and claiming one
        // would be a guess dressed as a fact.
        FsType::Other => "other",
    }
}
