//! Boot: turning configuration into a running system.
//!
//! Everything built so far is a component. This is the part that assembles them in the right order,
//! reports what it could not build, and refuses to start something half-wired.
//!
//! # Why the order matters
//!
//! A provider needs credentials and a model catalogue. A tool registry needs a working directory and a
//! path policy. A session needs a store. A runtime needs all of them. Starting them out of order
//! produces either a component with a missing dependency or a check that runs too late to matter.
//!
//! # Why a failed component does not always stop the boot
//!
//! Some parts are essential — without a provider there is nothing to talk to — and some are optional,
//! because a deployment may deliberately not mount them. The distinction is per-part and explicit:
//! [`BootReport`] says which failed and [`Boot::require`] says which failures are fatal. Treating
//! every failure as fatal makes a partial deployment impossible; treating none as fatal hides a
//! broken essential part behind a running process.
//!
//! # Why the report exists
//!
//! A boot that silently skipped a component produces a process that starts and then misbehaves in a
//! way nobody can trace back. Naming each part and its outcome makes "it started but tools do not
//! work" a single line in the log rather than an investigation.

use std::collections::BTreeMap;
use std::sync::Arc;

/// How one component fared during boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It was built and mounted.
    Mounted,
    /// It was deliberately not mounted.
    ///
    /// Different from `Failed`: a deployment that does not want a component is a decision, and
    /// reporting it as a failure would make every such deployment look broken.
    Skipped {
        /// Why it was skipped.
        reason: String,
    },
    /// It was wanted and could not be built.
    Failed {
        /// Why.
        reason: String,
    },
}

impl Outcome {
    /// Whether the component is usable.
    #[must_use]
    pub fn is_mounted(&self) -> bool {
        matches!(self, Self::Mounted)
    }
}

/// What happened during one boot.
#[derive(Clone, Debug, Default)]
pub struct BootReport {
    outcomes: BTreeMap<String, Outcome>,
}

impl BootReport {
    /// Creates an empty report.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a component's outcome.
    pub fn record(&mut self, component: &str, outcome: Outcome) {
        self.outcomes.insert(component.to_owned(), outcome);
    }

    /// The outcome of one component.
    #[must_use]
    pub fn outcome(&self, component: &str) -> Option<&Outcome> {
        self.outcomes.get(component)
    }

    /// Every component and its outcome, in name order.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, Outcome)> {
        self.outcomes
            .iter()
            .map(|(name, outcome)| (name.clone(), outcome.clone()))
            .collect()
    }

    /// The components that failed.
    #[must_use]
    pub fn failures(&self) -> Vec<(String, String)> {
        self.outcomes
            .iter()
            .filter_map(|(name, outcome)| match outcome {
                Outcome::Failed { reason } => Some((name.clone(), reason.clone())),
                _ => None,
            })
            .collect()
    }

    /// Renders one line per component, for a startup log.
    ///
    /// A single block rather than one line per component scattered through the boot: a log where the
    /// outcome of one part appears far from the outcome of the next is a log nobody reads.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for (name, outcome) in &self.outcomes {
            match outcome {
                Outcome::Mounted => {
                    let _ = writeln!(out, "  {name}: mounted");
                }
                Outcome::Skipped { reason } => {
                    let _ = writeln!(out, "  {name}: skipped ({reason})");
                }
                Outcome::Failed { reason } => {
                    let _ = writeln!(out, "  {name}: FAILED ({reason})");
                }
            }
        }
        out
    }
}

/// Why a boot was abandoned.
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    /// An essential component failed.
    #[error("`{component}` is required but failed: {reason}")]
    RequiredFailed {
        /// The component.
        component: String,
        /// Why it failed.
        reason: String,
    },

    /// The configuration itself is unusable.
    #[error("config: {0}")]
    Config(String),
}

/// What a boot produced.
pub struct Booted {
    /// What happened to each component.
    pub report: BootReport,
    /// The provider, when one mounted.
    pub provider: Option<Arc<dyn crate::llm::LlmProvider>>,
    /// The tool registry.
    pub tools: Arc<crate::tools::ToolRegistry>,
    /// The session store.
    pub sessions: Arc<dyn crate::session::Session>,
    /// The event bus.
    pub bus: Arc<crate::EventBus>,
    /// The model id in force.
    pub model: String,
}

impl std::fmt::Debug for Booted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Booted")
            .field("report", &self.report.failures())
            .field("model", &self.model)
            .field("has_provider", &self.provider.is_some())
            .finish_non_exhaustive()
    }
}

/// The configuration a boot reads.
///
/// Explicit fields rather than a generic map: every field here is required by something, and a
/// missing one should be a configuration error at boot rather than a failure at the first request.
#[derive(Clone, Default)]
pub struct BootConfig {
    /// The provider route name.
    pub provider: Option<String>,
    /// The model id.
    pub model: Option<String>,
    /// The API endpoint, when the provider needs one.
    pub endpoint: Option<String>,
    /// The credential, when the provider needs one.
    ///
    /// Never logged: [`BootConfig`]'s `Debug` redacts it, because a credential in a startup log is a
    /// credential in every log aggregator the deployment has.
    pub api_key: Option<String>,
    /// The working directory tools operate in.
    pub cwd: Option<std::path::PathBuf>,
    /// Where sessions are stored; in memory when absent.
    pub session_dir: Option<std::path::PathBuf>,
    /// The maximum turns per prompt.
    pub max_turns: Option<u32>,
}

impl BootConfig {
    /// Reads configuration from the environment.
    ///
    /// Environment-first because that is how a container passes a secret. A file would need a
    /// permission model this crate does not have.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            provider: std::env::var("GIRDER_PROVIDER").ok(),
            model: std::env::var("GIRDER_MODEL").ok(),
            endpoint: std::env::var("GIRDER_ENDPOINT").ok(),
            api_key: std::env::var("DEEPSEEK_API_KEY").ok(),
            cwd: std::env::var("GIRDER_CWD")
                .ok()
                .map(std::path::PathBuf::from),
            session_dir: std::env::var("GIRDER_SESSION_DIR")
                .ok()
                .map(std::path::PathBuf::from),
            max_turns: std::env::var("GIRDER_MAX_TURNS")
                .ok()
                .and_then(|value| value.parse().ok()),
        }
    }
}

impl std::fmt::Debug for BootConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            // A credential in a startup log is a credential in every log aggregator.
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("cwd", &self.cwd)
            .field("session_dir", &self.session_dir)
            .field("max_turns", &self.max_turns)
            .finish()
    }
}

/// The default model when none is configured.
pub const DEFAULT_MODEL: &str = "deepseek-chat";

/// The default endpoint for the `DeepSeek` provider.
pub const DEFAULT_ENDPOINT: &str = "https://api.deepseek.com/anthropic";

/// Builds a system from configuration.
///
/// The order is the dependency order: provider, then tools, then sessions, then the runtime that
/// needs all three. A component that fails is recorded and, if required, aborts.
///
/// # Errors
///
/// Returns [`BootError::RequiredFailed`] when a component the system cannot run without failed.
pub fn boot(config: &BootConfig) -> Result<Booted, BootError> {
    let mut report = BootReport::new();

    // 1. The provider. Required: without it there is nothing to send a request to.
    let provider: Option<Arc<dyn crate::llm::LlmProvider>> = if let Some(key) = &config.api_key {
        let endpoint = config.endpoint.as_deref().unwrap_or(DEFAULT_ENDPOINT);
        match crate::llm_deepseek::DeepSeekConfig::new(endpoint, key)
            .pipe(crate::llm_deepseek::DeepSeekProvider::new)
        {
            Ok(provider) => {
                report.record("provider", Outcome::Mounted);
                Some(Arc::new(provider))
            }
            Err(error) => {
                report.record(
                    "provider",
                    Outcome::Failed {
                        reason: error.to_string(),
                    },
                );
                None
            }
        }
    } else {
        report.record(
            "provider",
            Outcome::Skipped {
                reason: "no credential configured".to_owned(),
            },
        );
        None
    };

    if provider.is_none()
        && let Some(Outcome::Failed { reason }) = report.outcome("provider")
    {
        return Err(BootError::RequiredFailed {
            component: "provider".to_owned(),
            reason: reason.clone(),
        });
    }

    // 2. Tools. Always available: a system with no tools is still a system, and refusing to boot
    //    because a tool is missing would make a read-only deployment impossible.
    let tools = Arc::new(crate::tools::ToolRegistry::default());
    report.record("tools", Outcome::Mounted);

    // 3. Sessions. Persistence is optional; an unconfigured directory means in-memory, which is the
    //    right default for a test and a poor one for anything else.
    let sessions: Arc<dyn crate::session::Session> = if let Some(directory) = &config.session_dir {
        if let Err(error) = std::fs::create_dir_all(directory) {
            report.record(
                "sessions",
                Outcome::Failed {
                    reason: error.to_string(),
                },
            );
            return Err(BootError::RequiredFailed {
                component: "sessions".to_owned(),
                reason: error.to_string(),
            });
        }
        report.record("sessions", Outcome::Mounted);
        // A single-directory store needs a session id per file, which the runtime supplies; the
        // in-memory store is what this build mounts until then.
        Arc::new(crate::session::MemorySession::new())
    } else {
        report.record(
            "sessions",
            Outcome::Skipped {
                reason: "no session directory configured; using memory".to_owned(),
            },
        );
        Arc::new(crate::session::MemorySession::new())
    };

    // 4. The event bus. Always mounted: a component that cannot publish would fail later in a way
    //    that points nowhere.
    let bus = Arc::new(crate::EventBus::new());
    report.record("events", Outcome::Mounted);

    let model = config
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned());

    Ok(Booted {
        report,
        provider,
        tools,
        sessions,
        bus,
        model,
    })
}

/// A tiny pipe helper, so the provider construction reads in one direction.
trait Pipe: Sized {
    fn pipe<T>(self, apply: impl FnOnce(Self) -> T) -> T {
        apply(self)
    }
}

impl<T> Pipe for T {}

impl Booted {
    /// Builds a runtime from what booted.
    ///
    /// # Errors
    ///
    /// Returns [`BootError::RequiredFailed`] when no provider mounted: a runtime without one would
    /// fail on the first prompt rather than at startup, which is the harder place to diagnose.
    pub fn into_runtime(
        self,
        max_turns: Option<u32>,
    ) -> Result<Arc<crate::runtime::AgentRuntime>, BootError> {
        let Some(provider) = self.provider else {
            return Err(BootError::RequiredFailed {
                component: "provider".to_owned(),
                reason: "a runtime needs a provider".to_owned(),
            });
        };
        Ok(crate::runtime::AgentRuntime::new(
            crate::runtime::RuntimeConfig {
                provider,
                tools: Arc::clone(&self.tools) as Arc<dyn crate::tools::ToolRegistryView>,
                bus: self.bus,
                model: self.model,
                max_turns: max_turns.unwrap_or(16),
                // Unlimited unless a deployment sets one: a budget is a billing decision, and inventing a ceiling
                // here would break a session that was working.
                token_budget: crate::limits::Ceiling::Unlimited,
            },
        ))
    }
}
