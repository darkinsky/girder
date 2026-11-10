//! Workflows: an orchestration script that spawns and coordinates agents.
//!
//! A workflow is a script that decides what subagents to start, what to send them, and what to do with their
//! answers. It is the difference between asking one model to plan and having a plan that runs.
//!
//! # Why the run is observed rather than controlled
//!
//! A caller watches a run; it does not steer one. Steering would need a channel into a script that is already
//! executing, and a script whose control flow could change mid-run is one whose result nobody can reproduce.
//!
//! # Why progress is a sequence of phases rather than a percentage
//!
//! A script has phases — plan, fan out, gather, summarise — and a percentage would have to invent a
//! denominator. Phases are what a person can act on: "it is gathering" says more than "63%".
//!
//! # Why an agent's result is recorded separately from its output
//!
//! An agent's output is what it said. Its result is what the script asked for, in the shape the script
//! declared. A run that recorded only output would need the script to re-parse prose to find what it wanted.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::scope::BoxFut;

/// Identifies one run.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RunId(String);

impl RunId {
    /// Builds an id.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty()).then_some(Self(value))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a run is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Accepted, not started.
    Pending,
    /// Running.
    Running,
    /// Finished successfully.
    Completed,
    /// Stopped because someone asked.
    Cancelled,
    /// Stopped with an error.
    Failed {
        /// Why.
        reason: String,
    },
}

impl Status {
    /// Whether the run has finished, one way or another.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Cancelled | Self::Failed { .. }
        )
    }

    /// The wire name.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed { .. } => "failed",
        }
    }
}

/// What one agent in a run was asked to do, and what it produced.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRecord {
    /// Its label within the script.
    ///
    /// A label rather than a session id: the script names its agents, and a reader that saw session ids would
    /// have to map them back to the plan to understand the run.
    pub label: String,
    /// What it was asked.
    pub prompt: String,
    /// What it said.
    pub output: Option<String>,
    /// What the script asked for, in the shape it declared.
    ///
    /// Separate from `output` because the script wants a structured answer, and re-parsing prose to find it is
    /// what the structured form exists to avoid.
    pub result: Option<serde_json::Value>,
    /// When it finished, in milliseconds since the epoch.
    pub finished_at: Option<u64>,
}

/// One phase of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phase {
    /// Its name, as the script declared it.
    pub name: String,
    /// When it began.
    pub started_at: u64,
    /// When it ended, when it has.
    pub ended_at: Option<u64>,
}

/// One run's state.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// Its id.
    pub id: RunId,
    /// What it does, for a listing.
    pub title: String,
    /// Where it is.
    pub status: Status,
    /// Its phases, in order.
    pub phases: Vec<Phase>,
    /// Its agents, by label.
    pub agents: BTreeMap<String, AgentRecord>,
    /// The script's final value, when it produced one.
    pub result: Option<serde_json::Value>,
    /// When it began.
    pub started_at: u64,
}

impl Run {
    /// How many agents have finished.
    #[must_use]
    pub fn finished_agents(&self) -> usize {
        self.agents
            .values()
            .filter(|agent| agent.finished_at.is_some())
            .count()
    }

    /// The phase currently running, when one is.
    #[must_use]
    pub fn current_phase(&self) -> Option<&Phase> {
        self.phases
            .iter()
            .rev()
            .find(|phase| phase.ended_at.is_none())
    }
}

/// What a start request asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartRequest {
    /// The script.
    pub script: String,
    /// A title for a listing.
    pub title: String,
    /// What the script may name, when it declares its inputs.
    pub arguments: BTreeMap<String, String>,
}

impl StartRequest {
    /// Builds a request.
    ///
    /// # Errors
    ///
    /// Returns `None` for a blank script or title: a run with no script does nothing, and one with no title
    /// cannot be listed.
    #[must_use]
    pub fn new(script: impl Into<String>, title: impl Into<String>) -> Option<Self> {
        let script = script.into();
        let title = title.into();
        if script.trim().is_empty() || title.trim().is_empty() {
            return None;
        }
        Some(Self {
            script,
            title,
            arguments: BTreeMap::new(),
        })
    }

    /// Supplies one argument.
    #[must_use]
    pub fn with_argument(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.arguments.insert(name.into(), value.into());
        self
    }
}

/// Why a workflow operation failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WorkflowError {
    /// No run has that id.
    #[error("no workflow run `{id}`")]
    Unknown {
        /// The id.
        id: String,
    },

    /// The script failed.
    #[error("workflow `{id}` failed: {reason}")]
    Failed {
        /// The id.
        id: String,
        /// Why.
        reason: String,
    },

    /// The run is already finished.
    #[error("workflow `{id}` is already {status}")]
    AlreadyFinished {
        /// The id.
        id: String,
        /// Its status.
        status: &'static str,
    },
}

/// The engine that executes a script.
///
/// Separated from the registry so a deployment chooses its script language without changing how a run is
/// observed. The registry owns status and recording; the engine owns execution.
pub trait Engine: Send + Sync {
    /// Runs the script, reporting phases and agents as they happen.
    ///
    /// The callback shape rather than a stream: a run produces a handful of events over its life, and a stream
    /// would make the caller own backpressure for a volume that does not exist.
    fn run(
        &self,
        request: StartRequest,
        report: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> BoxFut<Result<serde_json::Value, String>>;
}

/// What a run reports while it executes.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A phase began.
    PhaseStarted {
        /// Its name.
        name: String,
        /// When.
        at: u64,
    },
    /// A phase ended.
    PhaseEnded {
        /// Its name.
        name: String,
        /// When.
        at: u64,
    },
    /// An agent was asked to do something.
    AgentStarted {
        /// Its label.
        label: String,
        /// What it was asked.
        prompt: String,
    },
    /// An agent finished.
    AgentFinished {
        /// Its label.
        label: String,
        /// What it said.
        output: String,
        /// What the script asked for, when it declared a shape.
        result: Option<serde_json::Value>,
        /// When.
        at: u64,
    },
}

/// The runs this deployment knows about.
#[derive(Default)]
pub struct Registry {
    /// The runs, behind an `Arc` so a report callback can hold a weak reference to them.
    ///
    /// Held weakly by the callback: a strong reference would keep the table alive for the process's life, and
    /// a registry that outlived its owner would accumulate runs nobody reads.
    shared: Arc<std::sync::Mutex<BTreeMap<String, Run>>>,
    engine: Option<Arc<dyn Engine>>,
}

/// The handle a report callback uses to record against a run.
///
/// A clone of the `Arc` rather than a wrapper: the callback needs exactly one operation, and a struct for it would
/// be a name without a meaning.
type SharedRuns = Arc<std::sync::Mutex<BTreeMap<String, Run>>>;

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            // The runs by id rather than the table: a debug line should be readable, and the ids are what a
            // reader can act on.
            .field("runs", &self.ids())
            .field("has_engine", &self.engine.is_some())
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// Creates a registry with no engine.
    ///
    /// A registry without an engine records runs but cannot start one: a deployment that mounts the observation
    /// half without the execution half should refuse a start rather than accept one it cannot perform.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a registry with an engine.
    #[must_use]
    pub fn with_engine(engine: Arc<dyn Engine>) -> Self {
        Self {
            shared: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            engine: Some(engine),
        }
    }

    /// Every run id, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// One run.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Run> {
        self.lock().get(id).cloned()
    }

    /// Starts a run, or records it as failed when no engine is mounted.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowError::Failed`] when no engine is mounted: a run that silently did nothing would report
    /// completion for work that never happened.
    pub async fn start(
        &self,
        id: RunId,
        request: StartRequest,
        now: u64,
    ) -> Result<Run, WorkflowError> {
        let Some(engine) = self.engine.clone() else {
            // Recorded as failed rather than refused outright, so a listing shows the attempt and why it did
            // nothing.
            let run = Run {
                id: id.clone(),
                title: request.title,
                status: Status::Failed {
                    reason: "no workflow engine is mounted".to_owned(),
                },
                phases: Vec::new(),
                agents: BTreeMap::new(),
                result: None,
                started_at: now,
            };
            self.lock().insert(id.to_string(), run.clone());
            return Err(WorkflowError::Failed {
                id: id.to_string(),
                reason: "no workflow engine is mounted".to_owned(),
            });
        };

        self.lock().insert(
            id.to_string(),
            Run {
                id: id.clone(),
                title: request.title.clone(),
                status: Status::Running,
                phases: Vec::new(),
                agents: BTreeMap::new(),
                result: None,
                started_at: now,
            },
        );

        // The callback keys on an owned copy: the borrow of `id` cannot outlive this function, and the report
        // fires while the engine runs.
        let key = id.to_string();
        let shared: SharedRuns = Arc::clone(&self.shared);
        let report: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(move |event| {
            let mut runs = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(run) = runs.get_mut(&key) {
                apply(run, event);
            }
        });

        let outcome = engine.run(request, report).await;
        let mut runs = self.lock();
        let run = runs.get_mut(id.as_str()).expect("inserted above");
        match outcome {
            Ok(value) => {
                run.status = Status::Completed;
                run.result = Some(value);
            }
            Err(reason) => {
                run.status = Status::Failed {
                    reason: reason.clone(),
                };
                return Err(WorkflowError::Failed {
                    id: id.to_string(),
                    reason,
                });
            }
        }
        Ok(run.clone())
    }

    /// Cancels a run.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowError::Unknown`] or [`WorkflowError::AlreadyFinished`].
    pub fn cancel(&self, id: &str) -> Result<Status, WorkflowError> {
        let mut runs = self.lock();
        let run = runs
            .get_mut(id)
            .ok_or_else(|| WorkflowError::Unknown { id: id.to_owned() })?;
        if run.status.is_terminal() {
            return Err(WorkflowError::AlreadyFinished {
                id: id.to_owned(),
                status: run.status.as_str(),
            });
        }
        run.status = Status::Cancelled;
        Ok(Status::Cancelled)
    }

    /// Every run that has not finished.
    #[must_use]
    pub fn active(&self) -> Vec<String> {
        self.lock()
            .values()
            .filter(|run| !run.status.is_terminal())
            .map(|run| run.id.to_string())
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Run>> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Records one event against a run.
fn apply(run: &mut Run, event: Event) {
    match event {
        Event::PhaseStarted { name, at } => {
            run.phases.push(Phase {
                name,
                started_at: at,
                ended_at: None,
            });
        }
        Event::PhaseEnded { name, at } => {
            if let Some(phase) = run.phases.iter_mut().rev().find(|phase| phase.name == name) {
                phase.ended_at = Some(at);
            }
        }
        Event::AgentStarted { label, prompt } => {
            run.agents.insert(
                label.clone(),
                AgentRecord {
                    label,
                    prompt,
                    output: None,
                    result: None,
                    finished_at: None,
                },
            );
        }
        Event::AgentFinished {
            label,
            output,
            result,
            at,
        } => {
            if let Some(agent) = run.agents.get_mut(&label) {
                agent.output = Some(output);
                agent.result = result;
                agent.finished_at = Some(at);
            }
        }
    }
}
