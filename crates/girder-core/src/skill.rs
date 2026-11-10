//! Skills: named instructions a provider supplies and a consumer can invoke.
//!
//! A skill is a document with a name and a description, plus the files that go with it. The model sees the
//! name and description when deciding whether to use it, and the body when it does.
//!
//! # Why a registry rather than a directory
//!
//! Several providers may supply skills — a filesystem directory, a plugin, a built-in set. Each owns its own
//! discovery, and merging them here means a consumer asks one question instead of knowing every source.
//!
//! # Why a name collision is resolved by precedence rather than refused
//!
//! Two providers can legitimately offer a skill with the same name: a project's own `review` skill should
//! shadow a built-in one of the same name, because the more specific source is the one the person meant. A
//! registry that refused the collision would force every project to rename its skills.
//!
//! The precedence is by layer order, and the winner is recorded so a consumer can report which source
//! answered — a user who expected their project's skill and got the built-in one needs to see why.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::scope::BoxFut;

/// One skill's front matter.
///
/// The description is what the model reads when deciding whether to invoke; the body is what it reads when
/// it does. Keeping them apart is what lets a listing show many skills while a prompt shows one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// The name a consumer invokes.
    pub name: String,
    /// One line, shown in a catalog.
    pub description: String,
    /// Which provider supplied it.
    pub provider: String,
    /// The layer it came from, used to resolve a collision.
    ///
    /// Higher wins. A project layer above a builtin layer is what makes a project's `review` shadow the
    /// built-in one.
    pub layer: u32,
}

/// A skill's full content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    /// Its front matter.
    pub summary: Summary,
    /// Its body, as the model reads it.
    pub body: String,
    /// Files that go with it, by path relative to the skill's directory.
    ///
    /// Carried so a consumer can show them without a second lookup: a skill whose body references a template
    /// is unusable if the template has to be fetched separately.
    pub files: BTreeMap<String, String>,
}

impl Definition {
    /// Builds a definition.
    ///
    /// # Errors
    ///
    /// Returns `None` for a blank name, description, or body: a skill with no name cannot be invoked, one
    /// with no description cannot be chosen, and one with no body teaches nothing.
    #[must_use]
    pub fn new(summary: Summary, body: impl Into<String>) -> Option<Self> {
        let body = body.into();
        if summary.name.trim().is_empty()
            || summary.description.trim().is_empty()
            || body.trim().is_empty()
        {
            return None;
        }
        Some(Self {
            summary,
            body,
            files: BTreeMap::new(),
        })
    }

    /// Attaches a file.
    #[must_use]
    pub fn with_file(mut self, path: impl Into<String>, contents: impl Into<String>) -> Self {
        self.files.insert(path.into(), contents.into());
        self
    }

    /// The name a consumer invokes.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.summary.name
    }
}

/// Why a skill lookup failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SkillError {
    /// No provider offers that name.
    #[error("no skill named `{name}`")]
    Unknown {
        /// The name.
        name: String,
    },

    /// The provider failed while loading.
    #[error("provider `{provider}` failed: {reason}")]
    Provider {
        /// Which provider.
        provider: String,
        /// Why.
        reason: String,
    },
}

/// A source of skills.
///
/// A trait so a deployment chooses where skills come from. The registry owns the merging and the
/// precedence; a provider owns discovery, which is the part that differs between a directory, a plugin, and a
/// built-in set.
pub trait Provider: Send + Sync {
    /// Its name, for attribution and for a collision message.
    fn name(&self) -> &str;

    /// The layer it occupies. Higher wins a collision.
    fn layer(&self) -> u32;

    /// Every skill it offers.
    fn list(&self) -> BoxFut<Result<Vec<Summary>, SkillError>>;

    /// One skill's full content.
    fn load(&self, name: &str) -> BoxFut<Result<Option<Definition>, SkillError>>;
}

/// The merged catalog.
#[derive(Default)]
pub struct Registry {
    providers: std::sync::Mutex<Vec<Arc<dyn Provider>>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("providers", &self.provider_names())
            .finish()
    }
}

impl Registry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a provider.
    ///
    /// # Errors
    ///
    /// Returns the name when it is taken. Two providers under one name would make a collision message name a
    /// source that is not the one that answered.
    pub fn register(&self, provider: Arc<dyn Provider>) -> Result<(), String> {
        let name = provider.name().to_owned();
        let mut providers = self.lock();
        if providers.iter().any(|existing| existing.name() == name) {
            return Err(name);
        }
        providers.push(provider);
        // Sorted by layer so the winner of a collision is the first match, and the sort makes two runs agree.
        providers.sort_by_key(|provider| std::cmp::Reverse(provider.layer()));
        Ok(())
    }

    /// The registered provider names, in precedence order.
    #[must_use]
    pub fn provider_names(&self) -> Vec<String> {
        self.lock()
            .iter()
            .map(|provider| provider.name().to_owned())
            .collect()
    }

    /// The merged catalog.
    ///
    /// One entry per name, the highest layer winning. The winner records its provider, so a consumer can say
    /// which source answered — a user who expected their project's skill and got the built-in one needs to see
    /// why.
    ///
    /// # Errors
    ///
    /// Returns the first provider failure. A partially merged catalog would hide which names are missing.
    pub async fn catalog(&self) -> Result<Vec<Summary>, SkillError> {
        let providers = self.lock().clone();
        let mut merged: BTreeMap<String, Summary> = BTreeMap::new();
        for provider in providers {
            for summary in provider.list().await? {
                // The providers are sorted by descending layer, so the first one to claim a name wins and
                // later ones are the ones it shadows.
                merged.entry(summary.name.clone()).or_insert(summary);
            }
        }
        Ok(merged.into_values().collect())
    }

    /// Loads one skill.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError::Unknown`] when no provider offers the name.
    pub async fn load(&self, name: &str) -> Result<Definition, SkillError> {
        let providers = self.lock().clone();
        for provider in providers {
            if let Some(definition) = provider.load(name).await? {
                return Ok(definition);
            }
        }
        Err(SkillError::Unknown {
            name: name.to_owned(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<dyn Provider>>> {
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A provider over an in-memory set, for tests and for a build with no filesystem.
pub struct Static {
    name: String,
    layer: u32,
    skills: BTreeMap<String, Definition>,
}

impl Static {
    /// Creates a provider holding nothing.
    #[must_use]
    pub fn new(name: impl Into<String>, layer: u32) -> Self {
        Self {
            name: name.into(),
            layer,
            skills: BTreeMap::new(),
        }
    }

    /// Adds a skill.
    pub fn add(&mut self, definition: Definition) {
        self.skills
            .insert(definition.summary.name.clone(), definition);
    }
}

impl Provider for Static {
    fn name(&self) -> &str {
        &self.name
    }

    fn layer(&self) -> u32 {
        self.layer
    }

    fn list(&self) -> BoxFut<Result<Vec<Summary>, SkillError>> {
        let summaries: Vec<Summary> = self
            .skills
            .values()
            .map(|definition| definition.summary.clone())
            .collect();
        Box::pin(async move { Ok(summaries) })
    }

    fn load(&self, name: &str) -> BoxFut<Result<Option<Definition>, SkillError>> {
        let definition = self.skills.get(name).cloned();
        Box::pin(async move { Ok(definition) })
    }
}
