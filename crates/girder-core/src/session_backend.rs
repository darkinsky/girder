//! Which session catalog a deployment should use, and what that catalog can do.
//!
//! # Why this is a separate decision
//!
//! [`crate::SessionCatalog`] is a trait with exactly one implementation in this crate,
//! [`DirectoryCatalog`](crate::DirectoryCatalog), which stores one `*.jsonl.zst` file per session
//! under a root directory. That is also the only durable option: a catalog backed by a database or
//! an object store is *possible* against the trait — implementing it needs no change here — but it
//! does not exist yet, so nothing in this module can select it. Selecting a backend is therefore not
//! a matter of picking an implementation to construct; it is a matter of saying, before anything is
//! constructed, which properties the rest of the process may rely on.
//!
//! # Why the default is the directory and not memory
//!
//! An in-memory catalog is the easiest thing to reach for in a test, and a plausible-looking default
//! for a process that has not been told where sessions live. It is the wrong default. Sessions are
//! the only record of what an agent did, and a memory backend loses every one of them when the
//! process exits: a deployment that silently defaulted to it would look healthy for a whole session
//! and then come back empty, with nothing in the logs saying the data was never written. The
//! directory is the default *even when no directory has been configured*, because the alternative
//! is a harness that answers questions about a conversation it did not keep.
//!
//! Memory is therefore reachable only by asking for it: [`Config::ephemeral`] must be set. That is
//! an explicit statement — "this process is a test or a scratch run and its sessions are meant to
//! be thrown away" — and it is the kind of statement that should not be inferred on a caller's
//! behalf.
//!
//! # Why capabilities are reported instead of assumed
//!
//! Callers ask the catalog questions the trait answers per backend. A directory can be scanned, so
//! history search works; a process-local map cannot be scanned by anything else, so it does not.
//! A client that assumed search worked would show a search box that returns nothing, and would only
//! find out from a user whose query silently matched no sessions. [`Capabilities`] exists so that
//! such a gap is known *before* the request: a caller can hide the affordance, or refuse to start
//! and ask for [`Backend::Directory`] instead. The same reasoning applies to `shared`: a session
//! store that only one process can write is a store a second writer will silently diverge from.
//!
//! # Why the choice carries a reason
//!
//! An operator looking at a deployment that "lost its sessions" needs to see the sentence explaining
//! that memory was chosen and why, without reading this file or re-deriving it from the environment.
//! [`Choice::reason`] is therefore written for a log line or a health page, not for a developer: it
//! names the selected backend and the input that selected it.

use std::path::PathBuf;

/// What a session backend can do.
///
/// Reported rather than inferred so that a caller can check a capability *before* relying on it: a
/// backend that cannot search should cost a client a hidden search box, not an empty result set that
/// looks like "no matching sessions".
///
/// The fields are independent facts about a backend — not a set of knobs or a stage in a pipeline —
/// and are kept as named fields because a bitfield would need exactly the same names to be readable.
#[allow(clippy::struct_excessive_bools)] // Each bool is a distinct capability, not a flag to combine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// Whether sessions can be enumerated.
    ///
    /// Every backend that can be read at all can also be listed, so this is `true` in practice; it
    /// is reported separately because "list" is the one operation a UI cannot do without, and a
    /// future backend that can answer `read(id)` and not `list` must be able to say so.
    pub list: bool,
    /// Whether session history can be searched.
    ///
    /// This is a statement about the *medium*, not the trait: search reads the logs themselves, so a
    /// backend with no persistent text to scan cannot offer it.
    pub search: bool,
    /// Whether history can be read a page at a time.
    pub page: bool,
    /// Whether more than one process may write the same store.
    ///
    /// `false` means the backend is process-local: a second process would not see this one's writes,
    /// and two writers would race. A deployment that runs a single harness and a CLI side by side
    /// must check this before pointing both at the same store.
    pub shared: bool,
    /// The persistence medium, as a short name for logs and health output.
    ///
    /// The literal string, not an enum: it is written to logs and read by humans, and a name that
    /// survives a new backend being added is worth more here than exhaustiveness a compiler would
    /// enforce against a backend this module does not have.
    pub medium: &'static str,
}

impl Capabilities {
    /// Whether data written through this backend survives the process that wrote it.
    ///
    /// Derived from [`medium`](Self::medium) rather than stored as a fifth flag, because the two
    /// cannot disagree: a process-local map is the only non-durable medium this harness recognises.
    /// A caller that must not lose a session — anything serving a real user — should test this, and
    /// [`Backend::Directory`] is the answer when it is `false`.
    pub fn durable(&self) -> bool {
        self.medium != "memory"
    }
}

/// A session backend to use.
///
/// Deliberately small: these are the two stores that exist. A database or object-store backend would
/// need no change to [`crate::SessionCatalog`] — implementing the trait is the whole extension point
/// — but it would need a variant here, so this enum is expected to grow rather than to be complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Backend {
    /// A directory of session logs (default).
    Directory,
    /// Process memory (tests and scratch runs only).
    Memory,
}

impl Backend {
    /// What this backend can do.
    ///
    /// The values are a claim about the store, not about this module: a directory of logs can be
    /// listed, scanned for search, paged, and written by more than one process, while an in-process
    /// map can be listed and paged and nothing else. Search is `false` for memory because there is
    /// no text on disk to scan and no index to query — a search box backed by it would report "no
    /// matches" for sessions that are right there.
    pub fn capabilities(self) -> Capabilities {
        match self {
            Self::Directory => Capabilities {
                list: true,
                search: true,
                page: true,
                shared: true,
                medium: "directory",
            },
            Self::Memory => Capabilities {
                list: true,
                search: false,
                page: true,
                shared: false,
                medium: "memory",
            },
        }
    }
}

/// The inputs that select a backend.
///
/// Only the facts that actually change the answer live here. Where the default directory is, and how
/// it is created, belong to the caller that constructs the catalog: this type must be answerable
/// from a test with no filesystem access.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// An explicitly configured session directory.
    ///
    /// `Some` means a deployment (or a caller) has said where sessions belong. It selects
    /// [`Backend::Directory`] and its path appears in the reason, so an operator can see which
    /// directory was used rather than guessing at the default.
    pub session_dir: Option<PathBuf>,
    /// Whether this process is a test or scratch run whose sessions may be discarded.
    ///
    /// This is the *only* way to select [`Backend::Memory`]. It is opt-in rather than inferred from
    /// "no directory configured" precisely because an unconfigured directory is the common
    /// mis-configuration, and inferring memory from it turns that mistake into data loss.
    pub ephemeral: bool,
}

/// A backend selection: what to use, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// The backend to construct.
    pub backend: Backend,
    /// The sentence explaining the choice, for logs and health output.
    ///
    /// Never empty. It names the backend and the input that selected it, so that a reader who sees
    /// sessions disappear can tell whether memory was chosen deliberately.
    pub reason: String,
}

impl Choice {
    /// Records a selection.
    pub fn new(backend: Backend, reason: impl Into<String>) -> Self {
        Self {
            backend,
            reason: reason.into(),
        }
    }

    /// What the selected backend can do.
    ///
    /// A convenience for callers that hold only the [`Choice`] — the capabilities travel with the
    /// selection so that the same sentence explains both which store was used and what is missing.
    pub fn capabilities(&self) -> Capabilities {
        self.backend.capabilities()
    }
}

/// Selects a backend from `config`.
///
/// Precedence, and the reason for it:
///
/// 1. [`Config::ephemeral`] selects [`Backend::Memory`]. It is checked first because it is an
///    explicit statement that this process's data is disposable, and honouring a directory in that
///    case would have a test write into a real sessions directory — the failure mode most likely to
///    be noticed late, after real logs have been overwritten. When a directory is also configured,
///    the reason says it was ignored, so the caller is not left believing it took effect.
/// 2. [`Config::session_dir`] selects [`Backend::Directory`] at that path.
/// 3. Otherwise the default is [`Backend::Directory`] with the caller's default root. Memory is
///    never a default: a harness that loses every session on restart is a harness that cannot answer
///    "what did the agent do", and that failure is silent until someone goes looking.
pub fn choose(config: &Config) -> Choice {
    if config.ephemeral {
        let reason = match &config.session_dir {
            Some(path) => format!(
                "memory backend: ephemeral mode (test/scratch run) is enabled, so sessions are not \
                 persisted; configured session directory {} was ignored",
                path.display()
            ),
            None => {
                "memory backend: ephemeral mode (test/scratch run) is enabled, so sessions live \
                     only for the life of this process and are lost on exit"
                    .to_owned()
            }
        };
        return Choice::new(Backend::Memory, reason);
    }

    match &config.session_dir {
        Some(path) => Choice::new(
            Backend::Directory,
            format!(
                "directory backend: sessions persisted under the configured directory {}",
                path.display()
            ),
        ),
        None => Choice::new(
            Backend::Directory,
            "directory backend: no ephemeral mode was requested and no session directory was \
             configured, so the default on-disk session store is used"
                .to_owned(),
        ),
    }
}
