//! Workspace paths: whether a path is absolute, and how one resolves against a root.
//!
//! The subtlety is that "absolute" has two spellings. A host that accepts `/a/b` and `C:\a\b` must
//! classify both, or a Windows client's absolute path is read as workspace-relative and joined onto the
//! root — producing a path that silently points somewhere else.
//!
//! # Why this is separate from `paths`
//!
//! `paths` answers "where is the harness's own data". This answers "does this path name a place on the
//! host, and where does it land inside a workspace". Mixing them would make the harness's home a
//! workspace, which it is not.

use std::path::{Path, PathBuf};

/// Whether a path uses a Windows drive prefix (`C:` or `C:/`).
#[must_use]
pub fn has_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Whether a path is a UNC path (`\\server\share`).
#[must_use]
pub fn is_unc(path: &str) -> bool {
    path.starts_with(r"\\")
}

/// Whether a path is absolute in either spelling a host accepts.
#[must_use]
pub fn is_absolute(path: &str) -> bool {
    path.starts_with('/') || has_drive_prefix(path) || is_unc(path)
}

/// Resolves a path against a workspace root.
///
/// An absolute path is returned unchanged: joining it would make the root a prefix of something that
/// already names a place. A relative path is joined, with leading `./` segments dropped so two spellings
/// of the same relative path produce one absolute form.
#[must_use]
pub fn resolve(root: Option<&Path>, path: &str) -> PathBuf {
    if is_absolute(path) {
        return PathBuf::from(path);
    }
    let trimmed = path.trim_start_matches("./");
    match root {
        Some(root) => root.join(trimmed),
        None => PathBuf::from(trimmed),
    }
}

/// Makes `path` relative to `root`, when it is inside it.
///
/// `None` rather than a best-effort relative path when the path is outside: a relative path that climbs
/// out with `..` segments looks like one that stays inside, and a caller that displayed it would show a
/// file as belonging to a workspace it is not in.
#[must_use]
pub fn relative_to(root: &Path, path: &Path) -> Option<PathBuf> {
    path.strip_prefix(root).ok().map(Path::to_path_buf)
}

/// The workspace-relative display form of a path.
///
/// Falls back to the absolute form when the path is outside the root, which is the honest answer: a
/// reader can see that it is not in the workspace.
#[must_use]
pub fn display(root: Option<&Path>, path: &Path) -> String {
    let Some(root) = root else {
        return path.display().to_string();
    };
    relative_to(root, path)
        .unwrap_or_else(|| path.to_path_buf())
        .display()
        .to_string()
}

/// Whether `child` is inside `parent` after lexical normalisation.
///
/// Lexical rather than canonical: a path that does not exist yet still has to be classified, and
/// resolving it through the filesystem would fail on the very case a caller is about to create.
#[must_use]
pub fn contains(parent: &Path, child: &Path) -> bool {
    let parent = normalize(parent);
    let child = normalize(child);
    child.starts_with(&parent)
}

/// Normalises `.` and `..` segments lexically.
///
/// A `..` that would climb above the root is dropped rather than kept: `../..` from `/` is still `/`, and
/// keeping it would make a containment check believe an escape happened when the filesystem says it did
/// not.
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Popping a normal segment undoes it; popping a root does nothing.
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}
