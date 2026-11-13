//! Filesystem mutation tools: `move_path`, `copy_path`, `delete_path`, and `make_dir`.
//!
//! The read/write/edit/patch tools in `file_tools` create and change content and nothing else.
//! Renaming a file, duplicating one, removing one, and making a directory are the operations a
//! coding turn reaches for constantly, and without a tool each becomes a shell command whose
//! effects the harness cannot see — so the session's observations go stale and the boundary is
//! whatever the shell happens to enforce.
//!
//! # Why `delete` asks for confirmation instead of prompting
//!
//! A delete is the one mutation here with no inverse: every other tool can be undone by running
//! it again with the arguments swapped, and this one cannot. The guard is a required
//! `confirm: true` argument rather than a question to the person, because a question has to be
//! answered out of band and a tool that silently waits for an answer looks hung. Requiring the
//! argument puts the decision in the call the model composes — visible in the transcript, and
//! reviewable before it runs — and the refusal says what to pass rather than only failing.
//!
//! # Why the boundary is not re-implemented here
//!
//! Every path is resolved and confined through [`girder_core::workspace_path`]'s helpers, the
//! same ones the RPC surfaces use. A second containment check written in this file would be a
//! second boundary, and two boundaries drift: whichever is looser silently becomes the real one,
//! and whichever is stricter is the one a caller routes around.
//!
//! # Why a copy is staged
//!
//! A copy that fails halfway leaves a tree that looks complete and is not. The tree is built
//! beside the destination and published with one rename, so the destination is either absent or
//! the whole copy; a failure removes the staging tree. That is also why a copy never merges into
//! an existing directory: a merge has no single publish step, so it cannot be all-or-nothing.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

// `girder_core::` rather than `crate::`: this module is also compiled into an integration test
// through `#[path]`, where `crate` is the test binary and only the real crate name resolves.
use girder_core::{
    ToolDefinition, ToolRegistry, ToolResult, ToolRunContext, normalize_path, resolve_in_workspace,
    workspace_contains,
};

/// Distinguishes staging directories created by concurrent calls in one process.
static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Builds the `move_path` tool.
///
/// The source and the destination are both confined, and the destination's parent must already
/// exist: creating parents and then failing would leave directories behind that the caller never
/// asked for, which is the half-result this module avoids everywhere else.
#[must_use]
pub fn move_tool(workspace: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "move_path",
        "Moves or renames a file or directory inside the workspace. The destination's parent \
         directory must already exist (create it with make_dir). Refuses to replace an existing \
         destination unless `overwrite` is true, and refuses to move a directory inside itself.",
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Existing file or directory to move, absolute or workspace-relative"
                },
                "destination": {
                    "type": "string",
                    "description": "New path; the parent directory must exist"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Replace an existing destination file. An existing destination directory is always refused.",
                    "default": false
                }
            },
            "required": ["source", "destination"]
        }),
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            let workspace = workspace.clone();
            Box::pin(async move { Ok(finish(do_move(&args, &workspace, &ctx).await)) })
        }),
    )
}

/// Builds the `copy_path` tool.
#[must_use]
pub fn copy_tool(workspace: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "copy_path",
        "Copies a file or directory inside the workspace, leaving the source in place. Directories \
         are copied recursively. The copy is staged and published in one rename, so a failure \
         leaves no half-written destination. Refuses to copy a directory into itself and refuses \
         to merge into an existing destination directory.",
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Existing file or directory to copy, absolute or workspace-relative"
                },
                "destination": {
                    "type": "string",
                    "description": "New path; the parent directory must exist"
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Replace an existing destination file. An existing destination directory is always refused.",
                    "default": false
                }
            },
            "required": ["source", "destination"]
        }),
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            let workspace = workspace.clone();
            Box::pin(async move { Ok(finish(do_copy(&args, &workspace, &ctx).await)) })
        }),
    )
}

/// Builds the `delete_path` tool.
///
/// # Why `confirm` is required
///
/// See the module documentation: a delete is irreversible, so the call has to carry the decision
/// rather than inherit it from a default.
#[must_use]
pub fn delete_tool(workspace: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "delete_path",
        "Permanently deletes a file or directory inside the workspace; this cannot be undone. \
         Requires `confirm: true`. A directory requires `recursive: true` as well. The workspace \
         root itself is never deleted.",
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File or directory to delete, absolute or workspace-relative"
                },
                "recursive": {
                    "type": "boolean",
                    "description": "Required to delete a directory together with everything in it",
                    "default": false
                },
                "confirm": {
                    "type": "boolean",
                    "description": "Must be true; the deletion is permanent",
                    "default": false
                }
            },
            "required": ["path", "confirm"]
        }),
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            let workspace = workspace.clone();
            Box::pin(async move { Ok(finish(do_delete(&args, &workspace, &ctx).await)) })
        }),
    )
}

/// Builds the `make_dir` tool.
#[must_use]
pub fn make_dir_tool(workspace: PathBuf) -> ToolDefinition {
    ToolDefinition::new(
        "make_dir",
        "Creates a directory inside the workspace. Missing parents are created unless `parents` is \
         false. An existing directory is reported as such rather than treated as a failure, because \
         a caller that only needs the directory to exist has got what it asked for.",
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to create, absolute or workspace-relative"
                },
                "parents": {
                    "type": "boolean",
                    "description": "Create missing parent directories",
                    "default": true
                }
            },
            "required": ["path"]
        }),
        Arc::new(move |args: Value, ctx: ToolRunContext| {
            let workspace = workspace.clone();
            Box::pin(async move { Ok(finish(do_make_dir(&args, &workspace, &ctx).await)) })
        }),
    )
}

/// Registers all four filesystem mutation tools against `workspace`.
pub fn register_fs_tools(tools: &ToolRegistry, workspace: PathBuf) {
    tools.register(move_tool(workspace.clone()));
    tools.register(copy_tool(workspace.clone()));
    tools.register(delete_tool(workspace.clone()));
    tools.register(make_dir_tool(workspace));
}

/// Turns an inner outcome into a tool result.
///
/// A refusal is a result rather than an error: the model can read it and try a different call,
/// whereas an error makes the turn look broken and invites a retry of the same call.
fn finish(outcome: Result<String, String>) -> ToolResult {
    match outcome {
        Ok(text) => ToolResult::text(text),
        Err(message) => ToolResult::error(message),
    }
}

// ---- move -------------------------------------------------------------------------------------

/// Moves or renames a path, or explains why it will not.
async fn do_move(args: &Value, workspace: &Path, ctx: &ToolRunContext) -> Result<String, String> {
    let source = confine(workspace, required_str(args, "source")?, "source").await?;
    let destination = confine(workspace, required_str(args, "destination")?, "destination").await?;
    let overwrite = flag(args, "overwrite");

    if source == destination {
        return Err(format!(
            "`source` and `destination` are the same path: `{}`",
            source.display()
        ));
    }
    let metadata = require_existing(&source, "source").await?;
    if metadata.is_dir() && workspace_contains(&source, &destination) {
        // A directory cannot become its own descendant: the rename would orphan the tree.
        return Err(format!(
            "cannot move `{}` inside itself (`{}`)",
            source.display(),
            destination.display()
        ));
    }
    require_parent(&destination).await?;

    let existing = tokio::fs::symlink_metadata(&destination).await.ok();
    if let Some(metadata) = &existing {
        if metadata.is_dir() {
            return Err(format!(
                "`{}` is an existing directory; move to a new name rather than onto it",
                destination.display()
            ));
        }
        if !overwrite {
            return Err(format!(
                "`{}` already exists; pass \"overwrite\": true to replace it",
                destination.display()
            ));
        }
    }

    if let Err(error) = tokio::fs::rename(&source, &destination).await {
        if error.kind() == ErrorKind::CrossesDevices {
            // `rename` cannot cross a mount boundary. Publishing by copy and then removing the
            // source keeps the order that matters: the destination is complete before the source
            // disappears, so an interruption leaves two copies rather than none. A removal that
            // fails is reported as the half-state it is instead of being swallowed.
            publish_copy(&source, &destination, overwrite).await?;
            if let Err(error) = remove_path(&source).await {
                return Err(format!(
                    "copied `{}` to `{}` but could not remove the source: {error}; both exist now",
                    source.display(),
                    destination.display()
                ));
            }
        } else if overwrite && existing.is_some() {
            // Unix replaces an existing file inside `rename`; Windows refuses. Removing first is
            // the portable spelling, and it only runs when the caller asked to replace.
            tokio::fs::remove_file(&destination)
                .await
                .map_err(|error| format!("cannot replace `{}`: {error}", destination.display()))?;
            tokio::fs::rename(&source, &destination)
                .await
                .map_err(|error| {
                    format!(
                        "cannot move `{}` to `{}`: {error}",
                        source.display(),
                        destination.display()
                    )
                })?;
        } else {
            return Err(format!(
                "cannot move `{}` to `{}`: {error}",
                source.display(),
                destination.display()
            ));
        }
    }

    // Both paths changed under the session's view: the source is gone and the destination is
    // content at a path it may have observed as something else.
    ctx.forget(&source);
    ctx.forget(&destination);
    Ok(format!(
        "moved `{}` to `{}`",
        source.display(),
        destination.display()
    ))
}

// ---- copy -------------------------------------------------------------------------------------

/// Copies a path, or explains why it will not.
async fn do_copy(args: &Value, workspace: &Path, ctx: &ToolRunContext) -> Result<String, String> {
    let source = confine(workspace, required_str(args, "source")?, "source").await?;
    let destination = confine(workspace, required_str(args, "destination")?, "destination").await?;
    let overwrite = flag(args, "overwrite");

    if source == destination {
        return Err(format!(
            "`source` and `destination` are the same path: `{}`",
            source.display()
        ));
    }
    let metadata = require_existing(&source, "source").await?;
    if metadata.is_dir() && workspace_contains(&source, &destination) {
        // Copying a directory into itself recurses forever: the copy contains a copy of the
        // directory that contains it, at every depth.
        return Err(format!(
            "cannot copy `{}` inside itself (`{}`); the copy would contain itself",
            source.display(),
            destination.display()
        ));
    }
    require_parent(&destination).await?;

    if let Ok(metadata) = tokio::fs::symlink_metadata(&destination).await {
        if metadata.is_dir() {
            return Err(format!(
                "`{}` is an existing directory; a merge has no single publish step, so pass a new \
                 path instead",
                destination.display()
            ));
        }
        if !overwrite {
            return Err(format!(
                "`{}` already exists; pass \"overwrite\": true to replace it",
                destination.display()
            ));
        }
    }

    publish_copy(&source, &destination, overwrite).await?;
    ctx.forget(&destination);
    Ok(format!(
        "copied `{}` to `{}`",
        source.display(),
        destination.display()
    ))
}

/// Builds the copy beside `destination` and publishes it with one rename.
///
/// Staging rather than writing straight to the destination is what makes "a failed copy leaves
/// nothing behind" true: the visible path either does not exist or holds the complete tree.
async fn publish_copy(source: &Path, destination: &Path, overwrite: bool) -> Result<(), String> {
    let staging = staging_path(destination)?;
    if let Err(message) = build_copy(source.to_path_buf(), staging.clone()).await {
        discard(&staging).await;
        return Err(message);
    }

    // The destination is re-checked inside the staging window. A destination that appeared while
    // the tree was being built must not be replaced by a caller who did not ask for that.
    if let Ok(metadata) = tokio::fs::symlink_metadata(destination).await {
        if metadata.is_dir() {
            discard(&staging).await;
            return Err(format!(
                "`{}` is an existing directory; refusing to merge the copy into it",
                destination.display()
            ));
        }
        if !overwrite {
            discard(&staging).await;
            return Err(format!(
                "`{}` already exists; pass \"overwrite\": true to replace it",
                destination.display()
            ));
        }
        if let Err(error) = tokio::fs::remove_file(destination).await {
            discard(&staging).await;
            return Err(format!(
                "cannot replace `{}`: {error}",
                destination.display()
            ));
        }
    }

    if let Err(error) = tokio::fs::rename(&staging, destination).await {
        discard(&staging).await;
        return Err(format!(
            "cannot publish the copy at `{}`: {error}",
            destination.display()
        ));
    }
    Ok(())
}

/// Copies `source` to `destination`, recursing through directories.
///
/// Boxed because it recurses: an unboxed recursive future would have an infinite size. Symlinks
/// are refused rather than followed — a link can point back at an ancestor (endless recursion) or
/// out of the workspace (copying content the boundary exists to keep out), and naming the one entry
/// lets the caller copy its target deliberately.
fn build_copy(source: PathBuf, destination: PathBuf) -> girder_core::BoxFut<Result<(), String>> {
    Box::pin(async move {
        let metadata = tokio::fs::symlink_metadata(&source)
            .await
            .map_err(|error| format!("cannot inspect `{}`: {error}", source.display()))?;

        if metadata.is_dir() {
            tokio::fs::create_dir(&destination)
                .await
                .map_err(|error| format!("cannot create `{}`: {error}", destination.display()))?;
            let mut entries = tokio::fs::read_dir(&source)
                .await
                .map_err(|error| format!("cannot list `{}`: {error}", source.display()))?;
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|error| format!("cannot list `{}`: {error}", source.display()))?
            {
                let entry_type = entry.file_type().await.map_err(|error| {
                    format!("cannot inspect `{}`: {error}", entry.path().display())
                })?;
                if entry_type.is_symlink() {
                    return Err(format!(
                        "`{}` is a symbolic link; copy its target explicitly",
                        entry.path().display()
                    ));
                }
                build_copy(entry.path(), destination.join(entry.file_name())).await?;
            }
            Ok(())
        } else if metadata.is_file() {
            tokio::fs::copy(&source, &destination)
                .await
                .map(|_| ())
                .map_err(|error| {
                    format!(
                        "cannot copy `{}` to `{}`: {error}",
                        source.display(),
                        destination.display()
                    )
                })
        } else {
            Err(format!(
                "`{}` is neither a regular file nor a directory",
                source.display()
            ))
        }
    })
}

/// The staging path a copy is built at, beside its destination so the publish is one `rename`.
fn staging_path(destination: &Path) -> Result<PathBuf, String> {
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "`{}` has no parent directory to stage the copy in",
            destination.display()
        )
    })?;
    let name = destination.file_name().ok_or_else(|| {
        format!(
            "`{}` does not name a file or directory",
            destination.display()
        )
    })?;
    let unique = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".{}.{}.{unique}.partial",
        name.to_string_lossy(),
        std::process::id()
    )))
}

/// Removes a path that a failed operation created.
///
/// Best effort, and deliberately silent: the caller is already reporting the original failure, and
/// a cleanup error would replace a useful message with a less useful one. What matters is that the
/// attempt is made, so a failed copy does not leave a tree the destination name would have had.
async fn discard(path: &Path) {
    if let Ok(metadata) = tokio::fs::symlink_metadata(path).await {
        if metadata.is_dir() {
            let _ = tokio::fs::remove_dir_all(path).await;
        } else {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

// ---- delete -----------------------------------------------------------------------------------

/// Deletes a path, or explains why it will not.
async fn do_delete(args: &Value, workspace: &Path, ctx: &ToolRunContext) -> Result<String, String> {
    let path = confine(workspace, required_str(args, "path")?, "path").await?;
    let recursive = flag(args, "recursive");

    // Checked before anything touches the disk: this is the decision itself, not a formality.
    if !flag(args, "confirm") {
        return Err(format!(
            "`delete_path` removes `{}` permanently; pass \"confirm\": true to proceed",
            path.display()
        ));
    }

    let root = tokio::fs::canonicalize(workspace).await.map_err(|error| {
        format!(
            "workspace root `{}` is unreadable: {error}",
            workspace.display()
        )
    })?;
    // Compared in canonical form rather than lexically: a root supplied as a symlinked or
    // non-normalised spelling would otherwise not match the target's canonical spelling, and the
    // guard would miss the one path it exists to protect.
    if canonical_form(&path).await == root {
        // Deleting the root deletes every other path the session can reach, including the tools'
        // own boundary. There is no version of that request to honour.
        return Err(format!(
            "refusing to delete the workspace root `{}`",
            path.display()
        ));
    }

    let metadata = require_existing(&path, "path").await?;
    if metadata.is_dir() {
        if !recursive {
            return Err(format!(
                "`{}` is a directory; pass \"recursive\": true to delete it with everything in it",
                path.display()
            ));
        }
        tokio::fs::remove_dir_all(&path)
            .await
            .map_err(|error| format!("cannot delete `{}`: {error}", path.display()))?;
    } else {
        tokio::fs::remove_file(&path)
            .await
            .map_err(|error| format!("cannot delete `{}`: {error}", path.display()))?;
    }

    // The path is gone, so whatever was observed there no longer describes anything.
    ctx.forget(&path);
    Ok(format!("deleted `{}`", path.display()))
}

// ---- make_dir ---------------------------------------------------------------------------------

/// Creates a directory, or explains why it will not.
async fn do_make_dir(
    args: &Value,
    workspace: &Path,
    _ctx: &ToolRunContext,
) -> Result<String, String> {
    let path = confine(workspace, required_str(args, "path")?, "path").await?;
    // Parents by default: a caller that named a deep path wanted the path, and refusing because an
    // intermediate directory is missing only makes it issue the same call again with more steps.
    let parents = args.get("parents").and_then(Value::as_bool).unwrap_or(true);

    if let Ok(metadata) = tokio::fs::metadata(&path).await {
        if metadata.is_dir() {
            // Idempotent: the caller wanted the directory to exist, and it does.
            return Ok(format!("`{}` already exists", path.display()));
        }
        return Err(format!(
            "`{}` exists and is not a directory",
            path.display()
        ));
    }

    let created = if parents {
        tokio::fs::create_dir_all(&path).await
    } else {
        tokio::fs::create_dir(&path).await
    };
    created.map_err(|error| format!("cannot create `{}`: {error}", path.display()))?;
    Ok(format!("created `{}`", path.display()))
}

// ---- shared path and argument handling ---------------------------------------------------------

/// Resolves one argument and proves it stays inside the workspace.
///
/// Two checks, in this order. The lexical one normalises `..` and absolute escapes without
/// touching the disk, so an escape is refused even when nothing exists at the target. The
/// canonical one runs after, so a symlinked directory cannot smuggle the operation outside the
/// root — and it also handles a target that does not exist yet by resolving the deepest ancestor
/// that does, which is the only way to classify a path that is about to be created.
async fn confine(workspace: &Path, raw: &str, field: &str) -> Result<PathBuf, String> {
    if raw.trim().is_empty() {
        return Err(format!("`{field}` must be a non-empty path"));
    }
    if raw.contains('\0') {
        return Err(format!("`{field}` contains a NUL byte"));
    }

    let lexical = normalize_path(&resolve_in_workspace(Some(workspace), raw));
    if !workspace_contains(workspace, &lexical) {
        return Err(format!("`{field}` = `{raw}` is outside the workspace"));
    }

    let root = tokio::fs::canonicalize(workspace).await.map_err(|error| {
        format!(
            "workspace root `{}` is unreadable: {error}",
            workspace.display()
        )
    })?;
    let resolved = canonical_form(&lexical).await;
    if !workspace_contains(&root, &resolved) {
        return Err(format!(
            "`{field}` = `{raw}` resolves outside the workspace"
        ));
    }
    Ok(lexical)
}

/// The canonical spelling of `path`, for a path that may not exist yet.
///
/// `canonicalize` fails on a missing path, so the deepest ancestor that exists is resolved and the
/// missing tail is reattached. Resolving the ancestor rather than skipping the check is the point:
/// a symlinked parent directory is exactly the way a lexically-inside path leads outside.
fn canonical_form(path: &Path) -> girder_core::BoxFut<PathBuf> {
    let path = path.to_path_buf();
    Box::pin(async move {
        if let Ok(resolved) = tokio::fs::canonicalize(&path).await {
            return resolved;
        }

        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut cursor = path.clone();
        loop {
            let Some(name) = cursor.file_name().map(std::ffi::OsStr::to_os_string) else {
                // Reached the root without finding anything that exists; the lexical form is the
                // only answer left, and the lexical check has already approved it.
                return path;
            };
            tail.push(name);
            let Some(parent) = cursor.parent().map(Path::to_path_buf) else {
                return path;
            };
            cursor = parent;
            if let Ok(resolved) = tokio::fs::canonicalize(&cursor).await {
                let mut out = resolved;
                for part in tail.iter().rev() {
                    out.push(part);
                }
                return out;
            }
        }
    })
}

/// Reads a required string argument, naming the field in the refusal.
fn required_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, String> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{field}` is required and must be a string"))
}

/// Reads a boolean argument, defaulting to `false`.
fn flag(args: &Value, field: &str) -> bool {
    args.get(field).and_then(Value::as_bool).unwrap_or(false)
}

/// Stats a path the operation needs to exist, naming the field when it does not.
async fn require_existing(path: &Path, field: &str) -> Result<std::fs::Metadata, String> {
    tokio::fs::metadata(path)
        .await
        .map_err(|error| match error.kind() {
            ErrorKind::NotFound => format!("`{field}` = `{}` does not exist", path.display()),
            _ => format!("cannot inspect `{field}` = `{}`: {error}", path.display()),
        })
}

/// Requires the destination's parent directory to exist already.
///
/// Not created implicitly: a call that then fails would leave directories behind that the caller
/// never asked for, and a caller that wants them has `make_dir`.
async fn require_parent(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err(format!("`{}` has no parent directory", path.display()));
    };
    match tokio::fs::metadata(parent).await {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "`{}` exists and is not a directory",
            parent.display()
        )),
        Err(_) => Err(format!(
            "`{}` does not exist; create it with make_dir first",
            parent.display()
        )),
    }
}

/// Removes a file or a directory tree, reporting the `io` failure unchanged.
fn remove_path(path: &Path) -> girder_core::BoxFut<std::io::Result<()>> {
    let path = path.to_path_buf();
    Box::pin(async move {
        if tokio::fs::symlink_metadata(&path).await?.is_dir() {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_file(&path).await
        }
    })
}
