//! Branded identifiers: structurally identical strings that cannot be interchanged.
//!
//! Upstream's `dsh-brand` makes the same point with a TypeScript phantom type: a `SessionId` is a
//! `string` at runtime and a distinct type to the compiler, so passing one where a `ToolCallId` is
//! expected does not compile. 86 such brands exist across the upstream packages.
//!
//! Rust's newtype is stronger than the phantom type, and the difference is worth stating because it
//! changes what this module has to do:
//!
//! - TypeScript's brand is erased, so `brandString()` is an unchecked cast at the boundary. Nothing
//!   stops a caller from branding an arbitrary string.
//! - A Rust newtype has no such hole if the constructor is not public. Here it is [`Brand::new`],
//!   which validates the shape, so an id that could never come from the domain cannot be built.
//!
//! # What a brand does not do
//!
//! It does not validate that an id *exists*, only that it is well formed. `SessionId::new("x")` is
//! a legal id even if no session is called `x`. Existence is the registry's question, not the
//! identifier's.
//!
//! # Serialization
//!
//! Brands serialize as their inner value, so the wire format is unchanged: a `SessionId` is still
//! `"abc"` in JSON and YAML. This is what lets a brand be added to an existing field without a
//! migration.

use std::fmt;

/// The maximum length an identifier may have.
///
/// Bounded because an unbounded id is a denial-of-service surface: it lands in log lines, directory
/// names, and error messages. 512 is far above any real id and far below anything that hurts.
pub const MAX_ID_LEN: usize = 512;

/// A failure raised while building an identifier.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdError {
    /// The value was empty.
    #[error("identifier must not be empty")]
    Empty,

    /// The value was longer than [`MAX_ID_LEN`].
    #[error("identifier is {len} bytes, above the {MAX_ID_LEN}-byte limit")]
    TooLong {
        /// The offending length.
        len: usize,
    },

    /// The value contained a character no identifier may hold.
    ///
    /// Restricting the alphabet is what makes an id safe to put in a path component or a log line
    /// without escaping. Upstream's ids come from `randomUUID` or a timestamp prefix, so the
    /// allowed set is generous rather than restrictive.
    #[error("identifier contains `{character}`, which is not allowed")]
    InvalidCharacter {
        /// The first offending character.
        character: char,
    },
}

/// What may appear in an identifier.
///
/// Alphanumerics plus `-`, `_`, `.`, `:`, `/`, and `+`. The last three exist because upstream ids
/// are sometimes composite: an agent id is `parent/child`, a scoped key carries `:`, and base64url
/// tokens carry `+`. Everything else — whitespace, control characters, quotes — is refused, which
/// is the point: an id goes into log lines and file names unescaped.
fn valid_character(byte: char) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, '-' | '_' | '.' | ':' | '/' | '+')
}

/// A string identifier that cannot be confused with a different kind of identifier.
///
/// Implementors are unit structs, which makes the price of a new id one `impl` block:
///
/// ```
/// use girder_core::{Brand, IdError};
///
/// pub struct SessionId;
///
/// impl Brand for SessionId {
///     const DOMAIN: &'static str = "session";
/// }
/// ```
///
/// and the value type arrives for free as [`Id<SessionId>`].
pub trait Brand: Send + Sync + 'static {
    /// The domain this id belongs to, used in error messages.
    const DOMAIN: &'static str;
}

/// An identifier in the domain of `B`.
///
/// A newtype around `String` rather than a type alias, so the only way to obtain one is
/// [`Id::new`] and the only way to lose the brand is [`Id::into_string`]. Both are explicit.
pub struct Id<B: Brand> {
    value: String,
    /// Zero-sized, so the brand costs nothing at runtime beyond the type check.
    domain: std::marker::PhantomData<B>,
}

impl<B: Brand> Id<B> {
    /// Builds an identifier, validating its shape.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::Empty`], [`IdError::TooLong`], or [`IdError::InvalidCharacter`].
    pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdError::Empty);
        }
        if value.len() > MAX_ID_LEN {
            return Err(IdError::TooLong { len: value.len() });
        }
        if let Some(character) = value.chars().find(|c| !valid_character(*c)) {
            return Err(IdError::InvalidCharacter { character });
        }
        Ok(Self {
            value,
            domain: std::marker::PhantomData,
        })
    }

    /// Borrows the identifier as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Consumes the identifier, yielding its string.
    ///
    /// Explicit because it is the one operation that loses the brand; a `Deref` to `str` would make
    /// it too easy to pass a `SessionId` where a `&str` is expected and back into a `ToolCallId`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.value
    }

    /// The domain this identifier belongs to, for diagnostics.
    #[must_use]
    pub fn domain(&self) -> &'static str {
        B::DOMAIN
    }
}

/// Two ids of the same domain compare by value; ids of different domains do not compare at all.
impl<B: Brand> PartialEq for Id<B> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<B: Brand> Eq for Id<B> {}

impl<B: Brand> PartialOrd for Id<B> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<B: Brand> Ord for Id<B> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl<B: Brand> std::hash::Hash for Id<B> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<B: Brand> Clone for Id<B> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            domain: std::marker::PhantomData,
        }
    }
}

impl<B: Brand> fmt::Debug for Id<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The domain is included because a bare string in a debug log does not say which kind of id
        // it was, and that is exactly the confusion the brand exists to prevent.
        write!(f, "{}Id({:?})", B::DOMAIN, self.value)
    }
}

impl<B: Brand> fmt::Display for Id<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

/// Serializes as the inner string, so the wire format is unchanged by branding a field.
impl<B: Brand> serde::Serialize for Id<B> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.value)
    }
}

/// Deserializes by validating, so a malformed id cannot enter through JSON.
impl<'de, B: Brand> serde::Deserialize<'de> for Id<B> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<B: Brand>(std::marker::PhantomData<B>);

        impl<B: Brand> serde::de::Visitor<'_> for Visitor<B> {
            type Value = Id<B>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a {} identifier", B::DOMAIN)
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Id::new(value).map_err(|error| E::custom(error.to_string()))
            }
        }

        deserializer.deserialize_str(Visitor(std::marker::PhantomData))
    }
}

/// Defines a brand marker and its value type, under one module.
///
/// A bare `macro_rules!` cannot declare `SessionId` as both the marker and the value type — Rust
/// has one namespace for types — so the macro emits a module holding both and re-exports the value
/// type. The four lines of ceremony per id are what it removes.
///
/// ```ignore
/// brand!(session_id, SessionId);
/// // now: crate::ids::session_id::Marker and crate::SessionId
/// ```
#[macro_export]
macro_rules! brand {
    ($module:ident, $name:ident, $domain:literal) => {
        #[doc = concat!("Marker for the ", $domain, " identifier family.")]
        pub mod $module {
            /// The marker type.
            #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
            pub struct Marker;

            impl $crate::Brand for Marker {
                const DOMAIN: &'static str = $domain;
            }
        }

        #[doc = concat!("An identifier in the ", $domain, " family.")]
        pub type $name = $crate::Id<$module::Marker>;
    };
}

// ---- the ids the current build actually needs ---------------------------------------------------
//
// Only these exist so far. The other ~80 upstream brands belong to domains M3-M6 will add, and
// declaring them now would create unused types that drift from their eventual home.

/// A session's identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionIdBrand;

impl Brand for SessionIdBrand {
    const DOMAIN: &'static str = "session";
}

/// A session's identity.
pub type SessionId = Id<SessionIdBrand>;

/// A single tool call's identity, issued by the model provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ToolCallIdBrand;

impl Brand for ToolCallIdBrand {
    const DOMAIN: &'static str = "tool-call";
}

/// A single tool call's identity, issued by the model provider.
pub type ToolCallId = Id<ToolCallIdBrand>;

/// An agent's identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentIdBrand;

impl Brand for AgentIdBrand {
    const DOMAIN: &'static str = "agent";
}

/// An agent's identity.
pub type AgentId = Id<AgentIdBrand>;

/// A workspace's identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceIdBrand;

impl Brand for WorkspaceIdBrand {
    const DOMAIN: &'static str = "workspace";
}

/// A workspace's identity.
pub type WorkspaceId = Id<WorkspaceIdBrand>;

/// A message's durable identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageIdBrand;

impl Brand for MessageIdBrand {
    const DOMAIN: &'static str = "message";
}

/// A message's durable identity.
pub type MessageId = Id<MessageIdBrand>;
