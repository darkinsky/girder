//! An introspection catalog: which services and events this build exposes.
//!
//! Upstream ships `tool-cordis`'s `api-catalog.ts`, a generated table of every `ctx.<key>`
//! service, every harness event, and the types their signatures mention, so a model can ask "what
//! can I use?" instead of guessing at names. This module is the Rust counterpart for the two facts
//! this crate can actually answer: which service ids exist and which event names exist.
//!
//! # Why there is no built-in list
//!
//! A handwritten catalog drifts: someone adds a service, forgets the table, and the model is told
//! either that a capability does not exist or — worse — that one does. That failure is why
//! [`Catalog::from_registry`] takes the ids that were *actually* published instead of shipping a
//! constant table. The catalog can only describe what it was handed, so it cannot invent an entry,
//! and every enrichment call refuses a name that is not already in it.
//!
//! # What is real, and what is not
//!
//! Real, because it is read from the live registry: the service ids. The registry's table is keyed
//! by `ServiceKey::ID`, so the id a provider published is exactly the id this catalog reports.
//! Real, because it is read from the declaration: event names and their dispatch modes, which are
//! `NAME` constants on the event key types.
//!
//! Not real, and deliberately left empty: descriptions and method signatures. The registry stores
//! type-erased objects (`Arc<dyn Any>`) behind a `&'static str`, so a table row cannot name its own
//! methods, and `ServiceKey` has no description associated item. Events are compile-time
//! declarations rather than runtime registrations, so they have no description either. Rather than
//! invent prose that would silently rot, the constructor leaves both empty and
//! [`Catalog::describe_service`], [`Catalog::add_method`], and [`Catalog::describe_event`] let a
//! caller that owns the documentation attach it.
//!
//! # Why the caller supplies the names
//!
//! `Registry` exposes `len`, `contains::<K>()`, and `get::<K>()`, but no enumeration: the table is
//! reachable only through a key type. The caller is the only one that knows the key types it
//! registered, so it passes their ids; this module turns them into a queryable, serializable
//! catalog. That is also why the input is a slice of `&str` rather than a registry handle — the
//! names have to survive the call, and the ids are `&'static str` anyway.

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// One public method of a service.
///
/// `signature` is prose, not a parsed type: this crate has no reflection, so the shape of a call
/// is documentation a provider writes down, not something the catalog can derive. It exists so a
/// model can see how to call a method without reading the trait, and it is empty when nobody
/// supplied it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MethodEntry {
    /// Method name as written at the call site, for example `greet`.
    pub name: String,
    /// The call shape a caller must match, for example `greet(name: &str) -> String`.
    pub signature: String,
    /// What the method does, or empty when no documentation was supplied.
    pub description: String,
}

/// One published service.
///
/// The `name` is the registry id the provider registered under, so looking it up here and
/// resolving it from the registry must agree. `methods` starts empty because the type-erased
/// table cannot enumerate them; see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceEntry {
    /// The registry id, matching `ServiceKey::ID`.
    pub name: String,
    /// What the service is for, or empty when no documentation was supplied.
    pub description: String,
    /// Documented methods, in the order they were added.
    pub methods: Vec<MethodEntry>,
}

/// One event this build dispatches.
///
/// `modes` holds the dispatch modes the event is declared with (`emit`, `parallel`, `serial`,
/// `bail`, `waterfall`). It is a list rather than a single value because the upstream catalog
/// models the mode as one field per declaration while this crate's names are strings; keeping the
/// list lets a caller record every mode it found for the name without this module guessing which
/// one is right.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventEntry {
    /// The dispatch name, matching the event type's `NAME` constant.
    pub name: String,
    /// Dispatch modes, as supplied by the caller, in order.
    pub modes: Vec<String>,
    /// What the event reports, or empty when no documentation was supplied.
    pub description: String,
}

/// A rejected catalog operation.
///
/// Enrichment fails loudly rather than silently creating an entry: a catalog that could grow a
/// service nobody published would be exactly the drift this module exists to prevent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    /// The catalog has no service of that name, so there is nothing to describe.
    #[error("no service named `{name}` is in the catalog")]
    UnknownService {
        /// The name that was looked up.
        name: String,
    },
    /// The catalog has no event of that name, so there is nothing to describe.
    #[error("no event named `{name}` is in the catalog")]
    UnknownEvent {
        /// The name that was looked up.
        name: String,
    },
    /// The service already documents a method with that name.
    ///
    /// Two methods with one name would make [`MethodEntry::signature`] ambiguous for a reader, and
    /// a lookup by name would silently return whichever was added first.
    #[error("service `{service}` already documents a method named `{method}`")]
    DuplicateMethod {
        /// The service being described.
        service: String,
        /// The method name that is already present.
        method: String,
    },
}

/// This build's services and events, queryable by name and serializable for a model or client.
///
/// Entries are keyed by name, so iteration and [`Catalog::to_json`] are sorted and stable: two
/// catalogs built from the same names serialize identically, which is what makes a diff of the
/// output meaningful.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    services: BTreeMap<String, ServiceEntry>,
    events: BTreeMap<String, EventEntry>,
}

impl Catalog {
    /// Creates an empty catalog.
    ///
    /// Useful as a starting point when the caller wants to populate entries one at a time, and as
    /// the identity for the empty case: querying it returns `None` and serializing it produces two
    /// empty arrays rather than failing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the catalog from the service ids and event declarations that are really registered.
    ///
    /// `services` are the ids the caller published (each `ServiceKey::ID`), and `events` are the
    /// names and modes of the events it dispatches (each `NAME` constant). Both are read as given:
    /// nothing is filtered by a built-in list, so an entry can only exist because it was passed in.
    ///
    /// Empty names are skipped because no registry id or event name is empty, and a duplicate name
    /// collapses to a single entry with the values from its last occurrence — the alternative, two
    /// entries under one key, is not representable and would make lookups depend on insert order.
    ///
    /// Descriptions are left empty here: the registry does not carry them and events do not either.
    /// See the module docs for why that is deliberate rather than an omission.
    #[must_use]
    pub fn from_registry(services: &[&str], events: &[(&str, Vec<&str>)]) -> Self {
        let services = services
            .iter()
            .filter(|name| !name.is_empty())
            .map(|name| {
                (
                    (*name).to_owned(),
                    ServiceEntry {
                        name: (*name).to_owned(),
                        description: String::new(),
                        methods: Vec::new(),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();

        let mut catalog = Self {
            services,
            events: BTreeMap::new(),
        };
        for (name, modes) in events {
            if name.is_empty() {
                continue;
            }
            catalog.events.insert(
                (*name).to_owned(),
                EventEntry {
                    name: (*name).to_owned(),
                    modes: modes.iter().map(|mode| (*mode).to_owned()).collect(),
                    description: String::new(),
                },
            );
        }
        catalog
    }

    /// Looks a service up by the id it was published under.
    ///
    /// Returns `None` for a name that was not passed to [`Catalog::from_registry`] — a caller can
    /// therefore use this as the answer to "does this build have that service?".
    #[must_use]
    pub fn service(&self, name: &str) -> Option<&ServiceEntry> {
        self.services.get(name)
    }

    /// Looks an event up by its dispatch name.
    ///
    /// Returns `None` for a name that was not declared.
    #[must_use]
    pub fn event(&self, name: &str) -> Option<&EventEntry> {
        self.events.get(name)
    }

    /// Iterates the services in name order.
    ///
    /// Ordered rather than insertion-ordered so that a consumer rendering the catalog sees a
    /// stable list, and so a test can assert against the whole set without depending on how the
    /// caller happened to build the slice.
    pub fn services(&self) -> impl Iterator<Item = &ServiceEntry> {
        self.services.values()
    }

    /// Iterates the events in name order.
    pub fn events(&self) -> impl Iterator<Item = &EventEntry> {
        self.events.values()
    }

    /// Returns true when no service and no event is catalogued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.services.is_empty() && self.events.is_empty()
    }

    /// Attaches a description to a service that is already in the catalog.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::UnknownService`] when `name` was never published. Refusing is the
    /// point: enrichment must not be able to create an entry that the registry does not have.
    pub fn describe_service(
        &mut self,
        name: &str,
        description: impl Into<String>,
    ) -> Result<(), CatalogError> {
        let entry = self
            .services
            .get_mut(name)
            .ok_or_else(|| CatalogError::UnknownService {
                name: name.to_owned(),
            })?;
        entry.description = description.into();
        Ok(())
    }

    /// Documents one method of a service that is already in the catalog.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::UnknownService`] when `service` was never published, and
    /// [`CatalogError::DuplicateMethod`] when a method of that name is already documented.
    pub fn add_method(&mut self, service: &str, method: MethodEntry) -> Result<(), CatalogError> {
        let entry = self
            .services
            .get_mut(service)
            .ok_or_else(|| CatalogError::UnknownService {
                name: service.to_owned(),
            })?;
        if entry
            .methods
            .iter()
            .any(|existing| existing.name == method.name)
        {
            return Err(CatalogError::DuplicateMethod {
                service: service.to_owned(),
                method: method.name,
            });
        }
        entry.methods.push(method);
        Ok(())
    }

    /// Attaches a description to an event that is already in the catalog.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::UnknownEvent`] when `name` was never declared.
    pub fn describe_event(
        &mut self,
        name: &str,
        description: impl Into<String>,
    ) -> Result<(), CatalogError> {
        let entry = self
            .events
            .get_mut(name)
            .ok_or_else(|| CatalogError::UnknownEvent {
                name: name.to_owned(),
            })?;
        entry.description = description.into();
        Ok(())
    }

    /// Serializes the catalog for a model or a client.
    ///
    /// The shape is `{"services": [...], "events": [...]}`, both arrays sorted by name, each
    /// service carrying `name`, `description`, and `methods`, and each event carrying `name`,
    /// `modes`, and `description`. Always present rather than omitted when empty: a consumer that
    /// has to distinguish "no services" from "field missing" is a consumer that will get it wrong.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let services = self
            .services
            .values()
            .map(|service| {
                json!({
                    "name": service.name,
                    "description": service.description,
                    "methods": service
                        .methods
                        .iter()
                        .map(|method| {
                            json!({
                                "name": method.name,
                                "signature": method.signature,
                                "description": method.description,
                            })
                        })
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();

        let events = self
            .events
            .values()
            .map(|event| {
                json!({
                    "name": event.name,
                    "modes": event.modes,
                    "description": event.description,
                })
            })
            .collect::<Vec<_>>();

        json!({ "services": services, "events": events })
    }
}
