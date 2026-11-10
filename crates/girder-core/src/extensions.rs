//! Extensions: user-defined plugins that add tools and behaviour at runtime.
//!
//! An extension is not a compiled plugin. It is a *definition* — a name, a source, the services it
//! needs — stored durably and started on demand. That distinction is the whole design: a compiled
//! plugin cannot be added to a running deployment without rebuilding it, which defeats the point of
//! letting a user extend their own harness.
//!
//! # Why a definition and a run are separate
//!
//! | concept | lifetime |
//! |---|---|
//! | [`Definition`] | durable; survives a restart |
//! | [`Run`] | process-local; exists while the extension is active |
//!
//! Collapsing them would make "the extension is configured" indistinguishable from "the extension is
//! running". A deployment that restarts should keep its extensions configured and not start them
//! until asked — the same rule goals follow, for the same reason: a restart must not resume work
//! nobody authorized.
//!
//! # Why activation needs approval
//!
//! An extension runs with the harness's own authority: it can register tools, read sessions, and call
//! providers. A model that could activate one on its own could widen its own capabilities. Approval is
//! therefore a human step, recorded, and it is why [`Registry::activate`] takes an approver rather
//! than a boolean.
//!
//! # Why a missing service is a start failure rather than a late error
//!
//! An extension declares what it needs. Starting one without them would let it fail at the first call
//! instead of at start, and the failure would surface in whatever the extension was in the middle of.

use std::collections::BTreeMap;

use crate::scope::BoxFut;

/// Identifies one extension.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExtensionId(String);

impl ExtensionId {
    /// Builds an id.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty() {
            return None;
        }
        Some(Self(value))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExtensionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A durable extension definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    /// Its id.
    pub id: ExtensionId,
    /// A human-readable name.
    pub name: String,
    /// What it does, for a permission prompt.
    pub description: String,
    /// Its source.
    ///
    /// Stored as text rather than as a path: a path ties the definition to one host's filesystem, and
    /// a definition that a client submitted must be inspectable before it runs.
    pub source: String,
    /// The services it needs.
    ///
    /// Declared so a missing one fails at start rather than at the first call.
    pub requires: Vec<String>,
    /// Who approved it, when it was approved.
    ///
    /// Absent for a definition that has never been activated. Recorded rather than implied because
    /// "approved" is the fact that makes running it legitimate.
    pub approved_by: Option<String>,
}

impl Definition {
    /// Builds a definition.
    ///
    /// # Errors
    ///
    /// Returns `None` for a blank name or source: an unnamed extension cannot be reported in a
    /// permission prompt, and a sourceless one cannot run.
    #[must_use]
    pub fn new(
        id: ExtensionId,
        name: impl Into<String>,
        source: impl Into<String>,
    ) -> Option<Self> {
        let name = name.into();
        let source = source.into();
        if name.trim().is_empty() || source.trim().is_empty() {
            return None;
        }
        Some(Self {
            id,
            name,
            description: String::new(),
            source,
            requires: Vec::new(),
            approved_by: None,
        })
    }

    /// Describes what it does.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Declares a required service.
    #[must_use]
    pub fn requiring(mut self, service: impl Into<String>) -> Self {
        self.requires.push(service.into());
        self
    }

    /// Whether it was approved.
    #[must_use]
    pub fn is_approved(&self) -> bool {
        self.approved_by.is_some()
    }

    /// The services this definition needs that `available` does not hold.
    ///
    /// Returned sorted so a permission prompt lists them in a stable order.
    #[must_use]
    pub fn missing_services(&self, available: &[String]) -> Vec<String> {
        let mut missing: Vec<String> = self
            .requires
            .iter()
            .filter(|required| !available.iter().any(|service| service == *required))
            .cloned()
            .collect();
        missing.sort();
        missing.dedup();
        missing
    }
}

/// Where an extension is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Configured, not started.
    Stopped,
    /// Running.
    Running,
    /// Running, then stopped after a failure.
    Failed {
        /// Why.
        reason: String,
    },
}

impl Status {
    /// Whether it is running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }

    /// The wire name.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running => "running",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Why an extension operation failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExtensionError {
    /// No extension has that id.
    #[error("no extension `{id}`")]
    NotFound {
        /// The id.
        id: String,
    },

    /// A person refused the extension.
    ///
    /// Distinct from a failure: the approval system worked, and the answer was no.
    #[error("extension `{id}` was not approved")]
    Denied {
        /// The id.
        id: String,
    },

    /// The approval could not be obtained.
    ///
    /// Distinct from a refusal: nobody was asked, or the channel broke.
    #[error("cannot obtain approval for `{id}`: {reason}")]
    ApprovalFailed {
        /// The id.
        id: String,
        /// Why.
        reason: String,
    },

    /// The id is taken.
    #[error("an extension `{id}` already exists")]
    Duplicate {
        /// The id.
        id: String,
    },

    /// The extension needs a service this process does not hold.
    #[error("extension `{id}` needs services this process does not hold: {missing:?}")]
    MissingServices {
        /// The id.
        id: String,
        /// What is missing.
        missing: Vec<String>,
    },

    /// It cannot be activated because nobody approved it.
    #[error("extension `{id}` has not been approved")]
    NotApproved {
        /// The id.
        id: String,
    },

    /// It is already running.
    #[error("extension `{id}` is already running")]
    AlreadyRunning {
        /// The id.
        id: String,
    },

    /// The source could not be loaded.
    #[error("extension `{id}` failed to load: {reason}")]
    LoadFailed {
        /// The id.
        id: String,
        /// Why.
        reason: String,
    },
}

/// A running extension.
///
/// Holds the stopper rather than a handle to the source: the registry's only interest in a running
/// extension is how to stop it.
pub struct Run {
    /// Which definition it runs.
    pub id: ExtensionId,
    /// Stops it.
    pub stop: Box<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Run")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The seam a runtime implements to start an extension.
///
/// Separate from the registry so a test can drive the lifecycle without a real runtime, and so the
/// registry never learns how an extension is executed — which is what keeps a future dynamic loader
/// from changing the lifecycle rules.
pub trait Runtime: Send + Sync {
    /// Starts a definition and returns how to stop it.
    fn start(
        &self,
        definition: &Definition,
        available: &[String],
    ) -> BoxFut<Result<Run, ExtensionError>>;
}

/// An approved extension record, plus its live status.
struct Entry {
    definition: Definition,
    status: Status,
    /// The stopper of the running instance, when one is running.
    ///
    /// Held rather than discarded: `activate` used to drop the returned `Run`, which left an
    /// extension that could never be stopped — it kept its tools registered and its timers alive
    /// for the life of the process.
    run: Option<Run>,
}

/// The extension registry.
///
/// A `Mutex` around the whole map: every operation is a small edit, and the invariant "one run per
/// definition" spans the map rather than one entry.
#[derive(Default)]
pub struct Registry {
    entries: std::sync::Mutex<BTreeMap<String, Entry>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("extensions", &self.ids())
            .finish()
    }
}

impl Registry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a definition.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionError::Duplicate`] when the id is taken. Replacing silently would swap the
    /// source behind an approval a human gave for different code.
    pub fn define(&self, definition: Definition) -> Result<(), ExtensionError> {
        let mut entries = self.lock();
        if entries.contains_key(definition.id.as_str()) {
            return Err(ExtensionError::Duplicate {
                id: definition.id.to_string(),
            });
        }
        entries.insert(
            definition.id.to_string(),
            Entry {
                definition,
                status: Status::Stopped,
                run: None,
            },
        );
        Ok(())
    }

    /// Records a human's approval of one definition.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionError::NotFound`] for an unknown id.
    pub fn approve(&self, id: &str, approver: impl Into<String>) -> Result<(), ExtensionError> {
        let mut entries = self.lock();
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| ExtensionError::NotFound { id: id.to_owned() })?;
        entry.definition.approved_by = Some(approver.into());
        Ok(())
    }

    /// Asks a person to approve a definition, and records the answer.
    ///
    /// The definition is described to the approver before anything runs: an approval granted for a definition
    /// nobody looked at is a signature on a blank page, which is what [`Self::approve`] alone would produce.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionError::NotFound`] for an unknown id, and [`ExtensionError::Denied`] when the person said
    /// no — a refusal is a decision about this extension, not a failure of the approval system.
    pub async fn request_approval(
        &self,
        id: &str,
        approver: &dyn crate::approval::Approver,
    ) -> Result<crate::approval::Decision, ExtensionError> {
        let definition = {
            let entries = self.lock();
            entries
                .get(id)
                .map(|entry| entry.definition.clone())
                .ok_or_else(|| ExtensionError::NotFound { id: id.to_owned() })?
        };

        let request = crate::approval::Request::new(
            format!("activate extension `{id}`"),
            format!(
                "`{}` from {} wants to run in this session",
                definition.name, definition.source
            ),
        )
        .map(|request| {
            // The definition's own description is the detail: it is what the person reads to decide.
            request.with_detail(definition.description.clone())
        })
        .ok_or_else(|| ExtensionError::NotFound { id: id.to_owned() })?;

        let decision =
            approver
                .ask(request)
                .await
                .map_err(|error| ExtensionError::ApprovalFailed {
                    id: id.to_owned(),
                    reason: error.to_string(),
                })?;

        if !decision.is_approved() {
            return Err(ExtensionError::Denied { id: id.to_owned() });
        }

        let by = match decision.decided_by() {
            crate::approval::DecidedBy::Human => "human",
            crate::approval::DecidedBy::Policy => "policy",
            crate::approval::DecidedBy::TimedOut => "timed-out",
        };
        self.approve(id, format!("{by} ({})", definition.name))?;
        Ok(decision)
    }

    /// Removes a definition, stopping it first if it runs.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionError::NotFound`] for an unknown id.
    pub fn remove(&self, id: &str) -> Result<Definition, ExtensionError> {
        let mut entries = self.lock();
        let mut entry = entries
            .remove(id)
            .ok_or_else(|| ExtensionError::NotFound { id: id.to_owned() })?;
        // Stop it before dropping the stopper: removing a running extension otherwise leaves its
        // tools registered and its timers alive with nothing left that could stop them.
        if let Some(run) = entry.run.take() {
            (run.stop)();
        }
        Ok(entry.definition)
    }

    /// Every definition, in id order.
    #[must_use]
    pub fn definitions(&self) -> Vec<Definition> {
        self.lock()
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    /// One definition.
    #[must_use]
    pub fn definition(&self, id: &str) -> Option<Definition> {
        self.lock().get(id).map(|entry| entry.definition.clone())
    }

    /// One extension's status.
    #[must_use]
    pub fn status(&self, id: &str) -> Option<Status> {
        self.lock().get(id).map(|entry| entry.status.clone())
    }

    /// Every id, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// The ids currently running.
    #[must_use]
    pub fn running(&self) -> Vec<String> {
        self.lock()
            .values()
            .filter(|entry| entry.status.is_running())
            .map(|entry| entry.definition.id.to_string())
            .collect()
    }

    /// Starts one extension.
    ///
    /// The order is deliberate: approval, then services, then start. Checking approval first means an
    /// unapproved extension never reaches the runtime at all, and checking services before starting
    /// means a missing dependency costs nothing rather than leaving a half-started extension.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionError::NotApproved`], [`ExtensionError::MissingServices`],
    /// [`ExtensionError::AlreadyRunning`], or whatever the runtime reports.
    pub async fn activate(
        &self,
        id: &str,
        available: &[String],
        runtime: &dyn Runtime,
    ) -> Result<Status, ExtensionError> {
        let definition = {
            let entries = self.lock();
            let entry = entries
                .get(id)
                .ok_or_else(|| ExtensionError::NotFound { id: id.to_owned() })?;
            if entry.status.is_running() {
                return Err(ExtensionError::AlreadyRunning { id: id.to_owned() });
            }
            if !entry.definition.is_approved() {
                return Err(ExtensionError::NotApproved { id: id.to_owned() });
            }
            entry.definition.clone()
        };

        let missing = definition.missing_services(available);
        if !missing.is_empty() {
            let reason = format!("needs {missing:?}");
            self.set_status(id, Status::Failed { reason });
            return Err(ExtensionError::MissingServices {
                id: id.to_owned(),
                missing,
            });
        }

        match runtime.start(&definition, available).await {
            Ok(run) => {
                // Keep the stopper: `deactivate` and `remove` both need it, and a dropped one leaves
                // the extension running with no way to end it.
                if let Some(entry) = self.lock().get_mut(id) {
                    entry.run = Some(run);
                }
                self.set_status(id, Status::Running);
                Ok(Status::Running)
            }
            Err(error) => {
                self.set_status(
                    id,
                    Status::Failed {
                        reason: error.to_string(),
                    },
                );
                Err(error)
            }
        }
    }

    /// Stops one extension.
    ///
    /// # Errors
    ///
    /// Returns [`ExtensionError::NotFound`] for an unknown id. Stopping one that is not running is a
    /// success: the caller wanted it stopped.
    pub fn deactivate(&self, id: &str) -> Result<Status, ExtensionError> {
        let mut entries = self.lock();
        let entry = entries
            .get_mut(id)
            .ok_or_else(|| ExtensionError::NotFound { id: id.to_owned() })?;
        if let Some(run) = entry.run.take() {
            (run.stop)();
        }
        entry.status = Status::Stopped;
        Ok(Status::Stopped)
    }

    fn set_status(&self, id: &str, status: Status) {
        if let Some(entry) = self.lock().get_mut(id) {
            entry.status = status;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
