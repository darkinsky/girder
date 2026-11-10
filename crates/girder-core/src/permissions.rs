//! Permission presets: one user-facing choice over two independent knobs.
//!
//! The harness has two separate answers to "what may this run do": a *sandbox mode* (whether a command
//! may write) and an *approval policy* (whether a human must confirm). Neither is something a user wants
//! to think about, so a preset bundles them into one choice — "read only", "ask before writing", "just
//! do it".
//!
//! # Why a preset is recorded rather than derived
//!
//! Two presets can share a bundle. "Read only" and "review only" may set the same two knobs today and
//! mean different things tomorrow, and a reader that derived the preset from the knobs would report the
//! wrong one the moment they diverge. The chosen preset is therefore recorded as intent, beside the
//! values it produced.
//!
//! # Why the knobs are written through setters
//!
//! Execution reads the knobs, not the preset. A switch that only recorded a name would leave the
//! enforcement reading the old values — a preset that says "read only" while the sandbox still permits
//! writes is worse than no preset at all, because the user believes they are protected.
//!
//! # Why a deployment pins some presets
//!
//! A hosted deployment may not want to offer "just do it" at all. Whether a preset is offered is a
//! deployment decision, separate from whether the harness supports it, so [`Catalogue`] can mark one
//! unavailable while still knowing how to apply it.

use std::collections::BTreeMap;

use crate::shell::SandboxMode;

/// Whether a human must confirm an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    /// Every write waits for a person.
    Always,
    /// A write inside the workspace proceeds; one outside waits.
    OutsideWorkspace,
    /// Nothing waits.
    Never,
}

impl Approval {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::OutsideWorkspace => "outside-workspace",
            Self::Never => "never",
        }
    }

    /// Whether a write at `path` needs a person, given `root`.
    ///
    /// The single place the policy is interpreted, so execution and a UI preview cannot disagree about
    /// what a policy means.
    #[must_use]
    pub fn requires_approval(self, root: &std::path::Path, path: &std::path::Path) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::OutsideWorkspace => !crate::workspace_path::contains(root, path),
        }
    }
}

/// The two knobs a preset sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Knobs {
    /// Whether a command may write.
    pub sandbox: SandboxMode,
    /// Whether a human must confirm.
    pub approval: Approval,
}

impl Knobs {
    /// Builds a bundle.
    #[must_use]
    pub fn new(sandbox: SandboxMode, approval: Approval) -> Self {
        Self { sandbox, approval }
    }

    /// The read-only bundle: no writes, no prompts, because nothing can happen.
    #[must_use]
    pub fn read_only() -> Self {
        Self::new(SandboxMode::ReadOnly, Approval::Never)
    }

    /// The workspace bundle: writes inside the workspace, prompts outside.
    #[must_use]
    pub fn workspace() -> Self {
        Self::new(SandboxMode::WorkspaceWrite, Approval::OutsideWorkspace)
    }

    /// The unrestricted bundle.
    #[must_use]
    pub fn unrestricted() -> Self {
        Self::new(SandboxMode::Full, Approval::Never)
    }
}

/// One named preset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preset {
    /// The name a user selects, which is also what a command receives.
    pub name: String,
    /// What it does, for a picker.
    pub description: String,
    /// The knobs it sets.
    pub knobs: Knobs,
}

impl Preset {
    /// Builds a preset.
    ///
    /// # Errors
    ///
    /// Returns `None` for a blank name or description: a preset with no name cannot be selected, and one
    /// with no description cannot be chosen knowingly.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        knobs: Knobs,
    ) -> Option<Self> {
        let name = name.into();
        let description = description.into();
        if name.trim().is_empty() || description.trim().is_empty() {
            return None;
        }
        Some(Self {
            name: name.trim().to_owned(),
            description,
            knobs,
        })
    }

    /// The three presets this build ships.
    ///
    /// Named rather than enumerated so a deployment that wants a fourth registers its own; these are the
    /// ones a caller can assume exist.
    #[must_use]
    pub fn builtin() -> Vec<Self> {
        vec![
            Self::new(
                "read-only",
                "Refuse every write. Nothing is asked because nothing can change.",
                Knobs::read_only(),
            )
            .expect("the builtin is valid"),
            Self::new(
                "workspace-write",
                "Write inside the workspace; ask before writing outside it.",
                Knobs::workspace(),
            )
            .expect("the builtin is valid"),
            Self::new(
                "danger-full-access",
                "Allow any write without asking. Use only when the task needs it.",
                Knobs::unrestricted(),
            )
            .expect("the builtin is valid"),
        ]
    }
}

/// What a catalogue reports about one preset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The preset.
    pub preset: Preset,
    /// Whether this deployment offers it.
    ///
    /// Separate from whether the harness supports it: a hosted deployment may not want to offer
    /// "danger-full-access" while still knowing how to apply it.
    pub available: bool,
}

/// The presets a deployment offers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalogue {
    entries: BTreeMap<String, Entry>,
}

impl Catalogue {
    /// A catalogue holding the builtin presets, all available.
    #[must_use]
    pub fn builtin() -> Self {
        let mut catalogue = Self::default();
        for preset in Preset::builtin() {
            catalogue.add(preset, true);
        }
        catalogue
    }

    /// Adds a preset.
    pub fn add(&mut self, preset: Preset, available: bool) {
        self.entries
            .insert(preset.name.clone(), Entry { preset, available });
    }

    /// Marks a preset unavailable, or removes the mark.
    pub fn set_available(&mut self, name: &str, available: bool) {
        if let Some(entry) = self.entries.get_mut(name) {
            entry.available = available;
        }
    }

    /// Every preset, sorted by name.
    ///
    /// Sorted because a picker renders from it, and an order that changed between runs would make the
    /// list jump.
    #[must_use]
    pub fn entries(&self) -> Vec<Entry> {
        self.entries.values().cloned().collect()
    }

    /// The presets this deployment offers.
    #[must_use]
    pub fn available(&self) -> Vec<Preset> {
        self.entries
            .values()
            .filter(|entry| entry.available)
            .map(|entry| entry.preset.clone())
            .collect()
    }

    /// One preset by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.get(name)
    }
}

/// Why a preset could not be applied.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PermissionError {
    /// No preset has that name.
    #[error("no permission preset `{name}`")]
    Unknown {
        /// The name.
        name: String,
    },

    /// The preset exists but this deployment does not offer it.
    ///
    /// Distinct from unknown: the name is real and the deployment refused it, which a user reports
    /// differently from a typo.
    #[error("permission preset `{name}` is not available in this deployment")]
    Unavailable {
        /// The name.
        name: String,
    },
}

/// The current permission state.
///
/// The recorded preset is held beside the knobs rather than derived from them: two presets can share a
/// bundle, and a reader that derived the name would report the wrong one the moment they diverge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    preset: Option<String>,
    knobs: Knobs,
}

impl Default for State {
    fn default() -> Self {
        // The workspace bundle rather than the unrestricted one: a fresh session should not be able to
        // write outside its workspace without asking.
        Self {
            preset: None,
            knobs: Knobs::workspace(),
        }
    }
}

impl State {
    /// The recorded preset, when one was chosen.
    #[must_use]
    pub fn preset(&self) -> Option<&str> {
        self.preset.as_deref()
    }

    /// The knobs in force.
    ///
    /// What execution reads. A switch writes these, so enforcement follows the user's choice rather than
    /// the name they clicked.
    #[must_use]
    pub fn knobs(&self) -> Knobs {
        self.knobs
    }

    /// Applies a preset, returning the knobs that changed.
    ///
    /// Returns the change so a caller can write the knobs through their setters: a switch that only
    /// recorded a name would leave enforcement reading the old values, and a preset that says "read only"
    /// while the sandbox still permits writes is worse than none.
    ///
    /// # Errors
    ///
    /// Returns [`PermissionError::Unknown`] or [`PermissionError::Unavailable`].
    pub fn apply(&mut self, catalogue: &Catalogue, name: &str) -> Result<Knobs, PermissionError> {
        let entry = catalogue
            .get(name)
            .ok_or_else(|| PermissionError::Unknown {
                name: name.to_owned(),
            })?;
        if !entry.available {
            return Err(PermissionError::Unavailable {
                name: name.to_owned(),
            });
        }
        self.preset = Some(entry.preset.name.clone());
        self.knobs = entry.preset.knobs;
        Ok(self.knobs)
    }

    /// Sets the knobs directly, clearing the recorded preset.
    ///
    /// A caller that changed one knob by hand has left the preset it came from, and keeping the name would
    /// report an intent the values no longer match.
    pub fn set_knobs(&mut self, knobs: Knobs) {
        self.preset = None;
        self.knobs = knobs;
    }
}
