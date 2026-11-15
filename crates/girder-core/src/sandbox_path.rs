//! Sandbox paths: whether a path, once the filesystem has had its say, stays inside a root.
//!
//! `workspace_path::contains` answers a related question lexically, and that is the right answer for
//! displaying a path: it never touches the filesystem, so it works for a file that does not exist. It
//! is the wrong answer for a decision about whether a write escapes the workspace, because the
//! filesystem does not resolve a path the way `normalize` does. Two cases are enough to show why.
//!
//! A symlink inside the workspace can point anywhere: `/workspace/link` is inside `/workspace` as a
//! string, but writing through it writes to whatever it names. Nothing lexical can see that.
//!
//! Worse, a `..` after a symlink is not the `..` the string suggests. If `/workspace/link` points at
//! `/tmp/elsewhere`, then `/workspace/link/../secret` is `/tmp/secret` for the kernel, while lexical
//! normalisation pops `link` and answers `/workspace/secret` — a path that is both wrong and inside.
//! That is a check that reports "allowed" for a path that lands outside, which is precisely the
//! failure a sandbox must not have.
//!
//! # Why the longest existing prefix
//!
//! `std::fs::canonicalize` fails on a path whose last components do not exist, and the paths worth
//! checking are usually the ones about to be created. So [`Sandbox::check`] walks the path one
//! component at a time, following each symlink it meets, and appends the rest lexically once it
//! reaches a component that is not there. That is the same answer `canonicalize` would give once
//! those components exist, because a component that does not exist cannot contain a symlink.
//!
//! # Keeping "cannot tell" separate from "outside"
//!
//! A path that loops through symlinks has no destination, and reporting it as inside would be a lie
//! while reporting it as outside would be a claim about where it lands. [`Verdict::Unknown`] exists
//! so a caller can decide for itself — refuse the write, or ask a person — instead of receiving an
//! answer the check could not actually compute.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

/// How many symlinks one check may follow before it reports that it cannot tell.
///
/// The kernel gives up after a handful of hops, so a path needing this many is a loop rather than a
/// deep tree. Counting here rather than trusting the host's limit keeps the verdict the same on every
/// platform and keeps a loop from being mistaken for a very long path.
const MAX_SYMLINK_HOPS: usize = 40;

/// What a check concluded about one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The path lands inside the sandbox.
    Inside,
    /// The path lands outside the sandbox, and a write through it would escape.
    ///
    /// The two fields are the place the path really reaches and the root it was measured against,
    /// both in the lossy display form: they are meant for a message to a person, and refusing to
    /// answer for a path with a non-UTF-8 byte in it would be worse than naming it approximately.
    Outside {
        /// Where the path resolves to.
        resolved: String,
        /// The sandbox root it was compared against, in canonical form.
        root: String,
    },
    /// The path has no single destination, so the check cannot say either way.
    Unknown {
        /// What stopped the check, phrased for a log or an error message.
        detail: String,
    },
}

/// A directory that a path is required to stay within.
#[derive(Clone, Debug)]
pub struct Sandbox {
    /// The root, already canonical: symlinks in it are followed and `.`/`..` are gone, so `check`
    /// compares against where the directory really is rather than against how it was spelled.
    pub root: PathBuf,
}

impl Sandbox {
    /// Builds a sandbox rooted at `root`.
    ///
    /// The root is canonicalised here, once, so that every later check compares canonical paths to a
    /// canonical root. A root spelled through a symlink would otherwise make every path inside it look
    /// outside, and the failure would be a refusal rather than a wrong permission — but a sandbox that
    /// refuses its own workspace is still a sandbox that does not work.
    ///
    /// # Errors
    ///
    /// [`SandboxError::NotAbsolute`] when the root is relative: a relative root means a different
    /// directory per process, so it cannot name a sandbox. [`SandboxError::Missing`] when the root does
    /// not exist as a directory, or cannot be resolved at all.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, SandboxError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(SandboxError::NotAbsolute { root: show(&root) });
        }
        // `canonicalize` is the only way to learn where a root really is; its failure means the root
        // is missing or unreadable, and both make the sandbox unusable for the same reason.
        let Ok(resolved) = std::fs::canonicalize(&root) else {
            return Err(SandboxError::Missing { root: show(&root) });
        };
        // A regular file cannot contain a path, so accepting one as a root would produce a sandbox in
        // which every check is Outside for a reason the caller never asked about.
        if !resolved.is_dir() {
            return Err(SandboxError::Missing { root: show(&root) });
        }
        Ok(Self { root: resolved })
    }

    /// Checks whether `path` lands inside this sandbox.
    ///
    /// The answer comes from the filesystem, not from the spelling: symlinks are followed before the
    /// comparison, so a link inside the root that points outside it is Outside, and a link that points
    /// back inside is Inside. Containment is decided per path segment, so `/work2` is not a child of
    /// `/work` even though the strings share a prefix.
    ///
    /// A relative `path` is taken as relative to the root. A check has no other base that would mean
    /// anything to its caller, and joining onto the root is the one interpretation under which `..`
    /// still has to be resolved honestly rather than silently dropped.
    ///
    /// A path that does not exist yet is still classified: the longest existing ancestor is resolved
    /// and the remainder is appended, which is where the path will land once it is created.
    #[must_use]
    pub fn check(&self, path: &str) -> Verdict {
        let raw = PathBuf::from(path);
        // An absolute path already names a place; prefixing the root would put the root in the middle
        // of it. This is the same rule `workspace_path::resolve` follows.
        let anchored = if raw.is_absolute() {
            raw
        } else {
            self.root.join(raw)
        };
        let resolved = match resolve(&anchored) {
            Ok(resolved) => resolved,
            // A failure here is a loop or a directory the process may not read: there is no place to
            // compare, so there is no honest Inside or Outside to report.
            Err(detail) => return Verdict::Unknown { detail },
        };
        if resolved.starts_with(&self.root) {
            return Verdict::Inside;
        }
        Verdict::Outside {
            resolved: show(&resolved),
            root: show(&self.root),
        }
    }
}

/// A root that cannot be used as a sandbox.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// The root is not an absolute path.
    #[error("sandbox root is not an absolute path: {root}")]
    NotAbsolute {
        /// The root as it was given.
        root: String,
    },
    /// The root does not exist as a directory, or cannot be resolved.
    ///
    /// A root the process may not read is reported here too: a missing root and an unreadable one are
    /// the same problem from the caller's side, and neither can support a check.
    #[error("sandbox root does not exist as a directory: {root}")]
    Missing {
        /// The root as it was given.
        root: String,
    },
}

/// One component of a path, kept in the order it still has to be processed.
///
/// Symlink targets are spliced back into this queue, so a path is walked as a flat list of steps no
/// matter how many links it passes through.
#[derive(Debug)]
enum Step {
    /// A root or drive prefix: the walk restarts here.
    Anchor(OsString),
    /// A plain name, which may be a symlink and therefore has to be asked about.
    Name(OsString),
    /// The current directory, which contributes nothing.
    Here,
    /// The parent directory.
    Up,
}

impl From<Component<'_>> for Step {
    fn from(component: Component<'_>) -> Self {
        match component {
            Component::RootDir | Component::Prefix(_) => {
                Self::Anchor(component.as_os_str().to_owned())
            }
            Component::Normal(name) => Self::Name(name.to_owned()),
            Component::CurDir => Self::Here,
            Component::ParentDir => Self::Up,
        }
    }
}

/// Resolves `path` to the place the filesystem would reach, tolerating components that do not exist.
///
/// The error is a `String` rather than a type because it has exactly one destination: the `detail` of
/// [`Verdict::Unknown`], shown to a person.
fn resolve(path: &Path) -> Result<PathBuf, String> {
    let mut pending: VecDeque<Step> = path.components().map(Step::from).collect();
    let mut resolved = PathBuf::new();
    let mut hops = 0usize;
    while let Some(step) = pending.pop_front() {
        match step {
            // An anchor restarts the walk. This is only ever reached from the first component or from
            // a symlink to an absolute path, and in the second case starting over is the whole point:
            // whatever was resolved before the link must not remain a prefix of the answer.
            Step::Anchor(anchor) => {
                resolved.clear();
                resolved.push(anchor);
            }
            Step::Here => {}
            Step::Up => {
                // Popping the root does nothing, which is what the kernel does with `/..`; keeping the
                // `..` instead would invent an escape the filesystem does not perform.
                let _ = resolved.pop();
            }
            Step::Name(name) => {
                let candidate = resolved.join(&name);
                match std::fs::symlink_metadata(&candidate) {
                    // `symlink_metadata` does not follow the link, which is what makes the follow
                    // below explicit and therefore countable.
                    Ok(meta) if meta.file_type().is_symlink() => {
                        hops += 1;
                        if hops > MAX_SYMLINK_HOPS {
                            return Err(format!(
                                "symlink chain starting at {} does not end",
                                show(&candidate)
                            ));
                        }
                        let target = std::fs::read_link(&candidate).map_err(|error| {
                            format!("cannot read symlink {}: {error}", show(&candidate))
                        })?;
                        // The target is relative to the directory holding the link, and `resolved` is
                        // still that directory, so the target's components go back on the queue
                        // unchanged. An absolute target carries its own anchor and restarts the walk.
                        let mut spliced: VecDeque<Step> =
                            target.components().map(Step::from).collect();
                        while let Some(step) = spliced.pop_back() {
                            pending.push_front(step);
                        }
                    }
                    Ok(_) => resolved.push(name),
                    // A component that does not exist cannot contain anything, so the remainder is a
                    // name rather than a place: appending it lexically is where a create would put
                    // it. `NotADirectory` means a non-directory is in the way, which has the same
                    // consequence for the rest of the path.
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::NotFound | ErrorKind::NotADirectory
                        ) =>
                    {
                        resolved.push(name);
                    }
                    Err(error) => {
                        return Err(format!("cannot inspect {}: {error}", show(&candidate)));
                    }
                }
            }
        }
    }
    Ok(resolved)
}

/// The display form of a path, for messages rather than for comparisons.
fn show(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
