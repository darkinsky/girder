//! Service identity and lookup keys.

use std::any::Any;
use std::fmt;

/// Stable identifier for a service, used as the registry lookup key.
///
/// A service id is a dotted, lowercase path such as `"llm.provider"`. It is deliberately a
/// string rather than a type id: plugins compiled in a different crate must be able to resolve
/// each other's services by the same key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceId(String);

impl ServiceId {
    /// Creates a service id from a static name.
    ///
    /// # Panics
    ///
    /// Panics when `name` is empty or contains a character outside `[a-z0-9._-]`. Service ids are
    /// compared literally, so a malformed id would be unreachable by its intended consumers;
    /// construction is the only point where the mistake is still attributable.
    #[must_use]
    pub fn new(name: &'static str) -> Self {
        assert!(!name.is_empty(), "service id must not be empty");
        assert!(
            name.bytes().all(|b| b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'.' | b'_' | b'-')),
            "service id {name:?} must match [a-z0-9._-]+"
        );
        Self(name.to_owned())
    }

    /// Returns the id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A capability published by a plugin and consumed by other plugins.
///
/// Implementors are the service providers of the harness: the LLM client, the shell executor,
/// the session store, and so on. Consumers resolve `Arc<dyn Any>` and downcast to the declared
/// provider type, so a provider can be replaced without recompiling its consumers.
pub trait Service: Any + Send + Sync {
    /// The registry key this service is published under.
    ///
    /// Two providers claiming the same id is a registration error, not a silent replacement.
    fn id(&self) -> ServiceId;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_lowercase_names() {
        assert_eq!(ServiceId::new("llm.provider").as_str(), "llm.provider");
        assert_eq!(ServiceId::new("shell-exec").as_str(), "shell-exec");
    }

    #[test]
    fn new_rejects_empty_uppercase_and_spaced_names() {
        assert!(std::panic::catch_unwind(|| ServiceId::new("")).is_err());
        assert!(std::panic::catch_unwind(|| ServiceId::new("Llm")).is_err());
        assert!(std::panic::catch_unwind(|| ServiceId::new("llm provider")).is_err());
    }
}
