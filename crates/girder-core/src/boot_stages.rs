//! Boot stages: the order a start-up moves through, and what it left behind.
//!
//! [`crate::boot`] reports *components* — provider, tools, sessions — and answers "was this piece
//! built?". A running process usually needs a weaker question answered first: "how far did start-up
//! get, and may it keep going?" Those are different questions. A component can be mounted and the
//! stage that mounts it still be degraded, and the interesting failure is often the one that stopped
//! the sequence rather than the one that broke a single part.
//!
//! # Why stages are ordered rather than merely named
//!
//! Restoring persisted state before the store exists, or listening before extensions are loaded,
//! produces a process that accepts traffic with half a system behind it. Ordering is part of the
//! contract, so [`Stage`] compares by boot position and not by declaration luck: an operator or a
//! runner can ask `restore_state > configure` and get a meaningful answer.
//!
//! # Why a failure does not always mean "stop"
//!
//! Some stages can finish with a hole: an extension fails to load, a state directory is missing and
//! the process starts empty. Refusing to start there would turn a working deployment with one broken
//! plugin into no deployment at all. Other stages cannot: a configuration that does not parse means
//! there is nothing to mount. [`StageOutcome`] carries that decision at the point where it is known,
//! and [`BootTrace::should_continue`] answers it once for the whole sequence.
//!
//! # Why the trace keeps history as well as a verdict
//!
//! The verdict is what a caller acts on; the history is what a human reads when the verdict looks
//! wrong. Keeping both means a retried stage shows up as a retry in the log instead of vanishing
//! behind its final outcome.

use std::cmp::Ordering;

/// One stage of start-up.
///
/// The variants are listed in execution order, and [`Stage::rank`] pins that order so moving a
/// variant in the source cannot silently move it in the boot sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Reading configuration.
    Configure,
    /// Mounting services.
    MountServices,
    /// Loading plugins and extensions.
    LoadExtensions,
    /// Restoring persisted state.
    RestoreState,
    /// Starting to accept connections.
    Listen,
}

impl Stage {
    /// Every stage, in execution order.
    ///
    /// A report walks this rather than whatever happened to be recorded, so a stage that never ran
    /// is visible as a gap instead of being absent from the picture.
    pub const ALL: [Self; 5] = [
        Self::Configure,
        Self::MountServices,
        Self::LoadExtensions,
        Self::RestoreState,
        Self::Listen,
    ];

    /// The position of this stage in the boot sequence.
    ///
    /// Explicit numbers rather than a derived discriminant: declaration order is an implementation
    /// detail, and the boot sequence is not.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Configure => 0,
            Self::MountServices => 1,
            Self::LoadExtensions => 2,
            Self::RestoreState => 3,
            Self::Listen => 4,
        }
    }

    /// The name this stage is reported under, in the vocabulary of start-up logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Configure => "configure",
            Self::MountServices => "mount-services",
            Self::LoadExtensions => "load-extensions",
            Self::RestoreState => "restore-state",
            Self::Listen => "listen",
        }
    }
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl Ord for Stage {
    fn cmp(&self, other: &Self) -> Ordering {
        // Ordering by boot position, so `>` reads as "later in start-up" and a sort of recorded
        // stages is the boot sequence rather than the order they were written down.
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for Stage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// How one stage ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageOutcome {
    /// The stage completed.
    Ok,
    /// The stage failed, but the rest of start-up can proceed with a known hole.
    Degraded {
        /// Why it degraded, in terms the operator can act on.
        reason: String,
    },
    /// The stage failed and start-up must stop here.
    Fatal {
        /// Why it is fatal.
        reason: String,
    },
}

impl StageOutcome {
    /// A degradation with a reason.
    #[must_use]
    pub fn degraded(reason: impl Into<String>) -> Self {
        Self::Degraded {
            reason: reason.into(),
        }
    }

    /// A fatal failure with a reason.
    #[must_use]
    pub fn fatal(reason: impl Into<String>) -> Self {
        Self::Fatal {
            reason: reason.into(),
        }
    }

    /// Whether the stage completed cleanly.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// Whether the stage failed but start-up may continue.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded { .. })
    }

    /// Whether the stage failed and start-up must stop.
    #[must_use]
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal { .. })
    }

    /// The reason, when there is one.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Ok => None,
            Self::Degraded { reason } | Self::Fatal { reason } => Some(reason),
        }
    }

    /// How badly the stage ended, so two records can be compared without matching on variants.
    ///
    /// Kept private: the numbers are an ordering device, not a vocabulary a caller should depend on.
    fn severity(&self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Degraded { .. } => 1,
            Self::Fatal { .. } => 2,
        }
    }
}

/// The record of one start-up: what each stage did, and whether it may go on.
#[derive(Clone, Debug, Default)]
pub struct BootTrace {
    /// One entry per stage, holding the worst outcome recorded for it.
    outcomes: std::collections::BTreeMap<Stage, StageOutcome>,
    /// Every record, in the order it was made, including retries of a stage.
    history: Vec<(Stage, StageOutcome)>,
}

impl BootTrace {
    /// Creates an empty trace.
    ///
    /// Empty means nothing has been attempted yet, and [`Self::should_continue`] says yes: a runner
    /// that has not started should not be told to stop.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the result of one stage.
    ///
    /// A repeated record for the same stage keeps the *worst* outcome: a retry that succeeds does not
    /// un-happen the failure that preceded it, and a start-up log that reports only the last attempt
    /// hides exactly the event the operator needs. The full sequence stays available through
    /// [`Self::records`], so the retry itself is still visible.
    ///
    /// Records made after a fatal failure are kept rather than rejected. Reaching this method after
    /// [`Self::should_continue`] said no is a bug in the runner, and a trace that swallows the
    /// evidence would make that bug impossible to find; the blocker is decided by boot order, so
    /// late records cannot overwrite it.
    pub fn record(&mut self, stage: Stage, outcome: StageOutcome) {
        let replaces = self
            .outcomes
            .get(&stage)
            .is_none_or(|existing| outcome.severity() > existing.severity());
        if replaces {
            self.outcomes.insert(stage, outcome.clone());
        }
        self.history.push((stage, outcome));
    }

    /// Whether start-up should continue past what has been recorded.
    ///
    /// False as soon as any stage reported [`StageOutcome::Fatal`]: stages after the blocker must not
    /// run, because they would run against a system the blocker just proved unusable.
    #[must_use]
    pub fn should_continue(&self) -> bool {
        self.blocker().is_none()
    }

    /// The stage that stopped start-up, if one did.
    ///
    /// The earliest fatal stage in *boot order* rather than record order: when records are replayed
    /// out of sequence, the stage the sequence could not get past is still the one to blame.
    #[must_use]
    pub fn blocker(&self) -> Option<Stage> {
        Stage::ALL
            .into_iter()
            .find(|stage| self.outcomes.get(stage).is_some_and(StageOutcome::is_fatal))
    }

    /// The effective outcome of one stage, after collapsing repeats.
    #[must_use]
    pub fn outcome(&self, stage: Stage) -> Option<&StageOutcome> {
        self.outcomes.get(&stage)
    }

    /// Every record in the order it was made, including retries.
    #[must_use]
    pub fn records(&self) -> &[(Stage, StageOutcome)] {
        &self.history
    }

    /// The stages that degraded, in boot order, with their reasons.
    ///
    /// Separate from [`Self::report`] because a caller often wants to surface a degradation
    /// somewhere other than a log line — a health endpoint, an exit-code decision.
    #[must_use]
    pub fn degradations(&self) -> Vec<(Stage, String)> {
        Stage::ALL
            .into_iter()
            .filter_map(|stage| match self.outcomes.get(&stage) {
                Some(StageOutcome::Degraded { reason }) => Some((stage, reason.clone())),
                _ => None,
            })
            .collect()
    }

    /// Renders the trace as a report a human can read at 3am.
    ///
    /// Every stage appears, in boot order, whether or not it was recorded: a stage missing from a
    /// report is a question ("did it run?"), and answering it in the report costs one line. Stages
    /// after the blocker are marked `not reached`, because that is the difference between "it ran and
    /// said nothing" and "we never got there".
    #[must_use]
    pub fn report(&self) -> String {
        use std::fmt::Write as _;

        if self.outcomes.is_empty() {
            return "boot: no stages recorded\n".to_owned();
        }

        let blocker = self.blocker();
        let mut out = String::from("boot trace:\n");
        for stage in Stage::ALL {
            let line = match self.outcomes.get(&stage) {
                Some(StageOutcome::Ok) => "ok".to_owned(),
                Some(StageOutcome::Degraded { reason }) => format!("DEGRADED ({reason})"),
                Some(StageOutcome::Fatal { reason }) => format!("FATAL ({reason})"),
                None if blocker.is_some_and(|blocked| stage > blocked) => "not reached".to_owned(),
                None => "not run".to_owned(),
            };
            let _ = writeln!(out, "  {}: {line}", stage.label());
        }

        // The verdict line is last so that a reader who stops at the first line of a log tail still
        // sees the answer, and a reader who reads the block arrives at it as a conclusion.
        if let Some(stage) = blocker {
            let _ = writeln!(out, "verdict: stopped at {stage}");
        } else {
            let degraded = self
                .degradations()
                .into_iter()
                .map(|(stage, _)| stage.label())
                .collect::<Vec<_>>();
            if degraded.is_empty() {
                let _ = writeln!(out, "verdict: continue");
            } else {
                let _ = writeln!(out, "verdict: continue; degraded: {}", degraded.join(", "));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Stage;

    #[test]
    fn stages_compare_by_boot_position() {
        assert!(Stage::Configure < Stage::Listen);
        assert!(Stage::Configure < Stage::MountServices);
        assert!(Stage::Listen > Stage::RestoreState);
    }

    #[test]
    fn stage_labels_are_stable() {
        assert_eq!(Stage::Configure.label(), "configure");
        assert_eq!(Stage::Listen.label(), "listen");
        assert_eq!(Stage::Listen.to_string(), "listen");
    }
}
