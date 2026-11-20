//! Who may call what: the policy layer between a transport and a method.
//!
//! `RemoteCall::peer` is carried and never verified, which is the honest description of a single-user harness: there is
//! one peer and checking it would be theatre. The moment a harness serves two clients the same field becomes a claim
//! that must be checked, and the check belongs in one place rather than repeated in every handler.
//!
//! # Why a policy rather than a check inside each method
//!
//! A handler that checked its own caller would encode the same rule once per method, and the first one somebody
//! forgot would be the hole. A policy is consulted by `dispatch` before the handler runs, so a method that forgets to
//! think about access still cannot be reached by a peer the policy refuses.
//!
//! # Why the decision names a reason
//!
//! "Refused" alone is unactionable. A caller that learns `peer` is not permitted can tell its operator; a caller told
//! nothing retries. The reason is also what a log needs to explain an audit.
//!
//! # Why the default is permissive
//!
//! A harness that has not configured a policy serves one local client, and a default that refused everything would
//! break every existing caller to protect a case that does not exist yet. The default is therefore "no policy mounted",
//! which is different from "a policy that allows" — the latter would hide the fact that nothing was configured.

use std::collections::BTreeSet;

/// What one call asks to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Read a value.
    Read,
    /// Change something.
    Write,
    /// Change something that cannot be undone.
    ///
    /// Separate from `Write` because deleting a session and renaming one are both writes, and a deployment may want to
    /// allow the second for a peer it would not trust with the first.
    Destructive,
}

/// One resource a call touches.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Resource {
    /// What kind of thing it is: `session`, `extension`, `workspace`.
    pub kind: String,
    /// Which one. An empty id means "any", which is what a listing asks for.
    pub id: String,
}

impl Resource {
    /// Builds a resource.
    #[must_use]
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
        }
    }

    /// The resource kind alone, for a listing.
    #[must_use]
    pub fn any_of(kind: impl Into<String>) -> Self {
        Self::new(kind, String::new())
    }
}

/// A decision about one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// It may proceed.
    Allow,
    /// It may not, for this reason.
    Refuse {
        /// Why, in words a caller can show its operator.
        reason: String,
    },
}

impl Decision {
    /// Whether the call may proceed.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// What a policy is told about one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Who is asking.
    pub peer: String,
    /// The method, as `namespace.method`.
    pub method: String,
    /// What it does.
    pub access: Access,
    /// What it touches.
    pub resources: Vec<Resource>,
}

/// Decides whether a call may proceed.
///
/// A trait rather than a closure so a deployment can swap in a real authorisation check without changing the
/// registry: the registry knows how to ask, and the deployment knows what the answer depends on.
pub trait Policy: Send + Sync {
    /// Decides one call.
    fn decide(&self, request: &Request) -> Decision;
}

/// A policy that refuses everything.
///
/// Useful as a test double and as the shape a deployment uses while building a real one: mounting this makes the
/// refusal visible immediately rather than at the first surprising call.
#[derive(Debug, Default)]
pub struct DenyAll;

impl Policy for DenyAll {
    fn decide(&self, request: &Request) -> Decision {
        Decision::Refuse {
            reason: format!("`{}` is not permitted for anyone", request.method),
        }
    }
}

/// A policy that allows reads and refuses writes.
///
/// The common shape for a read-only mirror: a peer that can list and inspect a deployment without being able to change
/// it.
#[derive(Debug, Default)]
pub struct ReadOnly;

impl Policy for ReadOnly {
    fn decide(&self, request: &Request) -> Decision {
        match request.access {
            Access::Read => Decision::Allow,
            Access::Write | Access::Destructive => Decision::Refuse {
                reason: format!(
                    "`{}` changes state, and this peer may only read",
                    request.method
                ),
            },
        }
    }
}

/// A policy that allows named peers.
///
/// Named rather than numbered because a peer name is what a transport can assert: an operator writes down who may do
/// what, and a policy that took an address would have to be updated whenever one changed.
#[derive(Debug, Default)]
pub struct AllowList {
    peers: BTreeSet<String>,
    /// Methods allowed regardless of peer, for the calls a harness needs to start.
    public: BTreeSet<String>,
}

impl AllowList {
    /// An allowlist with no peers and no public methods.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a peer that may do anything.
    #[must_use]
    pub fn allowing(mut self, peer: impl Into<String>) -> Self {
        self.peers.insert(peer.into());
        self
    }

    /// Adds a method any peer may call.
    #[must_use]
    pub fn public(mut self, method: impl Into<String>) -> Self {
        self.public.insert(method.into());
        self
    }
}

impl Policy for AllowList {
    fn decide(&self, request: &Request) -> Decision {
        if self.public.contains(&request.method) {
            return Decision::Allow;
        }
        if self.peers.contains(&request.peer) {
            return Decision::Allow;
        }
        Decision::Refuse {
            reason: format!(
                "peer `{}` is not permitted to call `{}`",
                request.peer, request.method
            ),
        }
    }
}

/// What a method declares about itself.
///
/// Registered beside the handler so the policy is asked before the handler runs: a declaration kept in a separate
/// table would drift, and the drift would be a method the policy never saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    /// What it does.
    pub access: Access,
    /// What it touches.
    pub resources: Vec<Resource>,
}

impl Declaration {
    /// A read of one resource.
    #[must_use]
    pub fn read(kind: impl Into<String>) -> Self {
        Self {
            access: Access::Read,
            resources: vec![Resource::any_of(kind)],
        }
    }

    /// A write to one resource.
    #[must_use]
    pub fn write(kind: impl Into<String>) -> Self {
        Self {
            access: Access::Write,
            resources: vec![Resource::any_of(kind)],
        }
    }

    /// A change that cannot be undone.
    #[must_use]
    pub fn destructive(kind: impl Into<String>) -> Self {
        Self {
            access: Access::Destructive,
            resources: vec![Resource::any_of(kind)],
        }
    }
}
