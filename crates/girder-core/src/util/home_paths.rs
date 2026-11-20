//! Where the harness keeps its own data.
//!
//! A tool that writes state needs a directory, and the directory must be the same one on every run — a session
//! stored under a temp path that changes per run is a session nobody can reopen.
//!
//! # Why the home directory is passed in rather than read
//!
//! Reading the real `HOME` makes a test depend on the machine it runs on, and makes the harness's behaviour
//! unprovable. [`Paths::under`] takes a root, so a test uses a temporary one and production passes the real one.
//!
//! # Why a relative override is refused
//!
//! `GIRDER_HOME=./data` resolves against whatever directory the process happened to start in. The harness would write
//! its sessions somewhere different depending on how it was launched, and a later run would not find them.

use std::path::{Path, PathBuf};

/// The directory layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// The layout under `root`.
    ///
    /// A relative root is refused: it would resolve against the launch directory, and the harness would write its
    /// state somewhere different depending on how it was started.
    ///
    /// # Errors
    ///
    /// Returns the root when it is relative or empty.
    pub fn under(root: impl Into<PathBuf>) -> Result<Self, String> {
        let root = root.into();
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(format!(
                "the harness home must be an absolute path, got `{}`",
                root.display()
            ));
        }
        Ok(Self { root })
    }

    /// The layout under the real home directory.
    ///
    /// `GIRDER_HOME` when it is set and absolute; otherwise `$HOME/.girder`, and when even `HOME` is unset, the
    /// system temporary directory — the last resort, chosen so the harness starts rather than refusing to.
    ///
    /// # Errors
    ///
    /// Returns the reason `GIRDER_HOME` was unusable, when it was set to something relative.
    pub fn from_environment(
        home: Option<&str>,
        override_home: Option<&str>,
    ) -> Result<Self, String> {
        if let Some(value) = override_home {
            return Self::under(value);
        }
        if let Some(home) = home {
            return Self::under(Path::new(home).join(".girder"));
        }
        Ok(Self {
            root: std::env::temp_dir().join("girder"),
        })
    }

    /// The root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where sessions live.
    #[must_use]
    pub fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Where titles live.
    #[must_use]
    pub fn titles(&self) -> PathBuf {
        self.root.join("titles")
    }

    /// Where spilled output lives.
    #[must_use]
    pub fn spills(&self) -> PathBuf {
        self.root.join("spills")
    }

    /// Where credentials live.
    ///
    /// Separate from the rest so its permissions can be tighter: a directory that also holds logs cannot be `0700`
    /// without hiding the logs.
    #[must_use]
    pub fn credentials(&self) -> PathBuf {
        self.root.join("credentials")
    }

    /// Where the lock files live.
    #[must_use]
    pub fn locks(&self) -> PathBuf {
        self.root.join("locks")
    }

    /// Every directory the harness expects to exist.
    ///
    /// Returned as a list so a caller can create them in one place rather than each caller knowing the set.
    #[must_use]
    pub fn directories(&self) -> Vec<PathBuf> {
        vec![
            self.root.clone(),
            self.sessions(),
            self.titles(),
            self.spills(),
            self.credentials(),
            self.locks(),
        ]
    }
}
