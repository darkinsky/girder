//! User-data roots: the harness home, the shared agent home, and `~` expansion.
//!
//! Upstream's `home-paths` package answers three questions that all of the storage layer needs:
//! where does the harness keep user data, where do agents keep shared config, and how does a
//! configured path that starts with `~` become absolute.
//!
//! # Precedence
//!
//! Highest first: an explicitly configured path, then the environment variable, then the default.
//! A whitespace-only environment value counts as unset — upstream learned that a blank `DSH_HOME`
//! would otherwise resolve the home to the process's working directory, which is never what the
//! user meant.
//!
//! # Why display and resolution are separate
//!
//! A resolved root is an absolute path; what a user sees is `~/.dsh` or `$DSH_HOME`. Keeping them
//! as two functions rather than one formatted string means a log line can be unambiguous and a
//! user-facing message can be short, from the same value.

use std::path::{Path, PathBuf};

/// The directory name the harness keeps user data under.
pub const HARNESS_HOME_DIR: &str = ".girder";

/// The symbolic form shown for the default harness home.
pub const HARNESS_HOME_DISPLAY: &str = "~/.girder";

/// The environment variable that overrides the harness home.
pub const HARNESS_HOME_ENV: &str = "GIRDER_HOME";

/// The directory name shared agent configuration lives under.
pub const AGENTS_HOME_DIR: &str = ".agents";

/// The symbolic form shown for the default agents home.
pub const AGENTS_HOME_DISPLAY: &str = "~/.agents";

/// The environment variable that overrides the agents home.
pub const AGENTS_HOME_ENV: &str = "GIRDER_AGENTS_HOME";

/// The operating-system home directory, or `None` when it cannot be determined.
///
/// Read from `HOME` on POSIX and `USERPROFILE` on Windows, matching what `dirs` and Node's
/// `os.homedir()` do. Returning `Option` rather than falling back to the working directory: a
/// harness that cannot find the user's home must fail loudly, because the default would silently
/// scatter state into whatever directory it was started in.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Expands a leading `~`, `~/`, or `~\` against the home directory.
///
/// Returns the input unchanged when it has no supported prefix or when the home directory is
/// unknown — the same behavior as upstream, and the only safe one: inventing a root would put data
/// somewhere the user did not ask for.
#[must_use]
pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest));
    }
    PathBuf::from(path)
}

/// The environment a root resolution reads.
///
/// Passed in rather than read from the process so a test states the environment it exercises
/// instead of mutating global state — the same reason `config::Environment` exists.
#[derive(Clone, Debug, Default)]
pub struct RootEnvironment {
    /// Variables by name.
    pub vars: std::collections::HashMap<String, String>,
    /// The operating-system home, when known.
    pub home: Option<PathBuf>,
}

impl RootEnvironment {
    /// Reads the current process environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            vars: std::env::vars().collect(),
            home: home_dir(),
        }
    }

    /// Sets one variable.
    #[must_use]
    pub fn with_var(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.vars.insert(name.into(), value.into());
        self
    }

    /// Sets the home directory.
    #[must_use]
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.vars
            .get(name)
            .map(String::as_str)
            // A blank override is treated as unset, so `DSH_HOME=""` cannot resolve the root to the
            // working directory.
            .filter(|value| !value.trim().is_empty())
    }
}

/// A resolved user-data root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    /// The absolute path.
    pub path: PathBuf,
    /// The symbolic label for user-facing display: `~/.girder` for the default, `$GIRDER_HOME` for
    /// any other configured root.
    pub display: String,
}

impl Root {
    /// Resolves a root from an explicit path, an environment override, a default, and a label.
    ///
    /// # Errors
    ///
    /// Returns [`RootError::NoHome`] when the default is needed but the home directory is unknown.
    pub fn resolve(
        configured: Option<&str>,
        env: &RootEnvironment,
        env_name: &str,
        default_dir: &str,
        default_display: &str,
    ) -> Result<Self, RootError> {
        // Explicit wins, then the environment, then the default.
        let (selected, is_default) = match configured.filter(|value| !value.trim().is_empty()) {
            Some(explicit) => (explicit.to_owned(), false),
            None => {
                if let Some(from_env) = env.get(env_name) {
                    (from_env.to_owned(), false)
                } else {
                    let home = env.home.clone().ok_or(RootError::NoHome {
                        env_name: env_name.to_owned(),
                    })?;
                    (home.join(default_dir).display().to_string(), true)
                }
            }
        };

        // `~` expands against the supplied home, not the process's, so a test can pin it.
        let expanded = if selected == "~" {
            env.home.clone().ok_or(RootError::NoHome {
                env_name: env_name.to_owned(),
            })?
        } else if let Some(rest) = selected
            .strip_prefix("~/")
            .or_else(|| selected.strip_prefix("~\\"))
        {
            env.home
                .clone()
                .ok_or(RootError::NoHome {
                    env_name: env_name.to_owned(),
                })?
                .join(rest)
        } else {
            PathBuf::from(&selected)
        };

        // Upstream normalizes to an absolute path; a relative configured root would otherwise depend
        // on the process's working directory.
        let path = if expanded.is_absolute() {
            expanded
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&expanded))
                .unwrap_or(expanded)
        };

        Ok(Self {
            path,
            display: if is_default {
                default_display.to_owned()
            } else {
                format!("${env_name}")
            },
        })
    }
}

/// A failure while resolving a root.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RootError {
    /// The home directory is unknown and the default was needed.
    #[error("cannot resolve the default root: no home directory for `{env_name}` to override")]
    NoHome {
        /// The variable that could have supplied a path instead.
        env_name: String,
    },

    /// A path that would escape its root.
    #[error("`{path}` resolves outside the root")]
    Escapes {
        /// The offending path.
        path: String,
    },
}

/// Resolves the harness home: `configured`, then `$GIRDER_HOME`, then `~/.girder`.
///
/// # Errors
///
/// Returns [`RootError::NoHome`] when the default is needed and the home is unknown.
pub fn harness_home(configured: Option<&str>, env: &RootEnvironment) -> Result<Root, RootError> {
    Root::resolve(
        configured,
        env,
        HARNESS_HOME_ENV,
        HARNESS_HOME_DIR,
        HARNESS_HOME_DISPLAY,
    )
}

/// Resolves the shared agents home: `configured`, then `$GIRDER_AGENTS_HOME`, then `~/.agents`.
///
/// # Errors
///
/// Same as [`harness_home`].
pub fn agents_home(configured: Option<&str>, env: &RootEnvironment) -> Result<Root, RootError> {
    Root::resolve(
        configured,
        env,
        AGENTS_HOME_ENV,
        AGENTS_HOME_DIR,
        AGENTS_HOME_DISPLAY,
    )
}

/// Shortens `path` for display by replacing a leading home directory with `~`.
///
/// Used where a path is shown to a user or written to a log that a user reads. The inverse of
/// [`expand_home`], and deliberately total: a path outside the home is returned unchanged rather
/// than made relative to something arbitrary.
#[must_use]
pub fn abbreviate_home(path: &Path, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return path.display().to_string();
    };
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Joins `path` under `root`, refusing one that would escape it.
///
/// A tool that takes a path from a model must not be talked into `../../etc/passwd`. The check is on
/// the normalized form, so `a/../b` cannot pass by string trickery.
///
/// # Errors
///
/// Returns [`RootError::Escapes`] when the result leaves `root`.
pub fn join_within(root: &Path, path: &str) -> Result<PathBuf, RootError> {
    use std::path::Component;

    let candidate = root.join(path);
    // Normalize lexically: `..` is resolved without touching the filesystem, so a path that does
    // not exist yet is still judged.
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }

    let normalized_root = {
        let mut base = PathBuf::new();
        for component in root.components() {
            match component {
                Component::ParentDir => {
                    base.pop();
                }
                Component::CurDir => {}
                other => base.push(other),
            }
        }
        base
    };

    if normalized.starts_with(&normalized_root) {
        Ok(normalized)
    } else {
        Err(RootError::Escapes {
            path: path.to_owned(),
        })
    }
}
