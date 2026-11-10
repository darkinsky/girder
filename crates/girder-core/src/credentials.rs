//! Credentials: stored secrets and what a client may know about them.
//!
//! The interesting part is not storage. It is that [`CredentialInfo`] has **no field a secret could
//! ride in**, which is what makes the whole read half safe to cross the Remote wire. A view that
//! carried the value "only for local callers" would eventually be serialized to a client by a caller
//! that did not read the doc comment.
//!
//! # Why a record is tagged
//!
//! An API key and an OAuth grant are both secrets but they are not interchangeable: one is a value the
//! seam can use directly, the other is a payload only its owning plugin can interpret. A single
//! `String` field would let a caller hand a grant to a provider that expected a key, and the failure
//! would appear as an authentication error rather than as the type mistake it is.
//!
//! # Why the grant payload is opaque
//!
//! The seam stores and returns it verbatim. Validating it would mean the seam knowing every plugin's
//! authorization format, and a plugin could then not change its own format without changing this
//! crate. The one constraint enforced is that it survives a JSON round trip, which is what a durable
//! store requires.
//!
//! # Why a missing credential is `None` rather than an error
//!
//! Most lookups ask "is this configured". An error would force every caller to branch on a condition
//! that is normal, and the branch is where a missed check becomes a request with no credential.

use std::collections::BTreeMap;

/// Identifies one credential reference.
///
/// Not the key name: a reference is what a caller holds, and the same logical credential may be
/// supplied by several sources over time.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CredentialRef(String);

impl CredentialRef {
    /// Builds a reference.
    ///
    /// # Errors
    ///
    /// Returns `None` for an empty reference: it would name every credential at once.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty() {
            return None;
        }
        Some(Self(value))
    }

    /// The reference as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stored credential.
///
/// Tagged rather than a bare string: an API key is a value the seam can use, a grant is a payload
/// only its owner can interpret, and handing one to a consumer expecting the other is a type mistake
/// the seam should refuse rather than let surface as an authentication failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// An API key, plus any environment values its provider needs.
    ApiKey {
        /// The secret, when this credential carries one at all.
        ///
        /// Absent for a credential that supplies only environment values, such as an AWS profile.
        key: Option<String>,
        /// Provider environment values, such as `AWS_PROFILE`.
        env: BTreeMap<String, String>,
    },

    /// The product of an authorization grant, kept verbatim for its owner.
    ///
    /// Opaque: the seam never reads, validates, or reshapes it. The only constraint is that it
    /// survives a JSON round trip, which is what a durable store requires.
    Grant {
        /// The owner-defined payload.
        payload: serde_json::Value,
    },
}

impl Record {
    /// Builds an API key record.
    #[must_use]
    pub fn api_key(key: impl Into<String>) -> Self {
        Self::ApiKey {
            key: Some(key.into()),
            env: BTreeMap::new(),
        }
    }

    /// Builds a record that supplies only environment values.
    #[must_use]
    pub fn environment(env: BTreeMap<String, String>) -> Self {
        Self::ApiKey { key: None, env }
    }

    /// Builds a grant record.
    #[must_use]
    pub fn grant(payload: serde_json::Value) -> Self {
        Self::Grant { payload }
    }

    /// The discrimination tag, for a log or a client.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ApiKey { .. } => "api-key",
            Self::Grant { .. } => "grant",
        }
    }

    /// The secret, when this record holds one.
    ///
    /// The only accessor that returns a secret, and it is named so a reviewer sees it. Everything
    /// else in this module returns [`Info`].
    #[must_use]
    pub fn secret(&self) -> Option<&str> {
        match self {
            Self::ApiKey { key, .. } => key.as_deref(),
            Self::Grant { .. } => None,
        }
    }
}

/// What a client may know about a credential.
///
/// There is deliberately **no field a secret could ride in**. A view that carried the value "only for
/// local callers" would eventually be serialized to a client by a caller that did not read the doc
/// comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    /// Whether resolving the reference would currently return a value.
    pub configured: bool,
    /// The source layer supplying the value; absent while unconfigured.
    ///
    /// A layer name such as `env` or `store`, never a value.
    pub source: Option<String>,
    /// Whether the active provider can write this reference.
    pub writable: bool,
}

impl Info {
    /// The view of an unconfigured reference.
    #[must_use]
    pub fn missing() -> Self {
        Self {
            configured: false,
            source: None,
            writable: false,
        }
    }

    /// The view of a configured reference supplied by `source`.
    #[must_use]
    pub fn configured(source: impl Into<String>, writable: bool) -> Self {
        Self {
            configured: true,
            source: Some(source.into()),
            writable,
        }
    }
}

/// Why a credential operation failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CredentialError {
    /// No credential is stored under that reference.
    #[error("no credential `{reference}`")]
    NotFound {
        /// The reference.
        reference: String,
    },

    /// The reference cannot be written by the active provider.
    #[error("credential `{reference}` is read-only")]
    ReadOnly {
        /// The reference.
        reference: String,
    },

    /// An environment variable name is not a POSIX identifier.
    #[error("`{name}` is not a valid environment variable name")]
    InvalidEnvName {
        /// The name.
        name: String,
    },

    /// A reference was blank.
    #[error("a credential reference cannot be blank")]
    BlankReference,
}

/// The in-memory credential store.
///
/// A `Mutex` around one map: every operation is a small read or write, and the invariant — a
/// configured reference always has a source — spans the entry rather than the map.
#[derive(Default)]
pub struct Store {
    records: std::sync::Mutex<BTreeMap<String, (Record, String)>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the values: a credential in a debug line is a credential in every log aggregator.
        f.debug_struct("Store")
            .field("references", &self.references())
            .finish()
    }
}

impl Store {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores a record, overwriting an existing one.
    ///
    /// Overwriting is allowed here, unlike in the registries that refuse a duplicate: a rotated key
    /// is the normal case, and refusing it would make rotation impossible.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::InvalidEnvName`] for a name that is not a POSIX identifier.
    pub fn set(
        &self,
        reference: &CredentialRef,
        record: Record,
        source: impl Into<String>,
    ) -> Result<(), CredentialError> {
        if let Record::ApiKey { env, .. } = &record {
            for name in env.keys() {
                if !is_posix_name(name) {
                    return Err(CredentialError::InvalidEnvName { name: name.clone() });
                }
            }
        }
        self.lock()
            .insert(reference.to_string(), (record, source.into()));
        Ok(())
    }

    /// Removes a record.
    pub fn unset(&self, reference: &CredentialRef) -> bool {
        self.lock().remove(reference.as_str()).is_some()
    }

    /// Reads a record.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialError::NotFound`] when the reference is unset.
    pub fn get(&self, reference: &CredentialRef) -> Result<Record, CredentialError> {
        self.lock()
            .get(reference.as_str())
            .map(|(record, _)| record.clone())
            .ok_or_else(|| CredentialError::NotFound {
                reference: reference.to_string(),
            })
    }

    /// Describes a record without revealing it.
    ///
    /// The half that crosses the wire, which is why it returns [`Info`] rather than the record.
    #[must_use]
    pub fn describe(&self, reference: &CredentialRef) -> Info {
        match self.lock().get(reference.as_str()) {
            Some((_, source)) => Info::configured(source.clone(), true),
            None => Info::missing(),
        }
    }

    /// Every stored reference, sorted.
    ///
    /// A reference is not a secret: it names which credential is configured, which a client needs to
    /// render a settings page.
    #[must_use]
    pub fn references(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// Whether a reference would resolve.
    #[must_use]
    pub fn is_configured(&self, reference: &CredentialRef) -> bool {
        self.lock().contains_key(reference.as_str())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, (Record, String)>> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Whether a name is a POSIX environment variable identifier.
///
/// Checked because an environment map is handed to a subprocess: a name containing `=` or a newline
/// would be interpreted by the shell rather than by the program.
fn is_posix_name(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Whether an environment value map has only valid names.
///
/// Exposed so a caller building a record can check before calling `set`, rather than recovering from
/// the error.
#[must_use]
pub fn valid_environment(env: &BTreeMap<String, String>) -> bool {
    env.keys().all(|name| is_posix_name(name))
}
