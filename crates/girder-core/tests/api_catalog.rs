//! Acceptance tests for the introspection catalog.
//!
//! The one that matters is [`the_catalog_matches_what_the_registry_actually_published`]: it builds
//! a catalog from `ServiceKey::ID` constants and `Event::NAME` constants, then checks the result
//! against a live [`Registry`] and against the event declarations. A catalog that could drift from
//! the registry — the failure this module exists to prevent — fails that test.
//!
//! # Why the module is compiled by path
//!
//! The catalog is not exported from the crate root yet; the wiring is a separate change to
//! `lib.rs`. Compiling the exact source file here keeps these tests runnable and independent of
//! that wiring, while still exercising the same code a consumer will reach through
//! `girder_core::api_catalog`. Nothing is duplicated: this is the real file.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/api_catalog.rs"]
mod api_catalog;

use std::sync::Arc;

use api_catalog::{Catalog, CatalogError, EventEntry, MethodEntry, ServiceEntry};
use girder_core::{Bail, Event, Parallel, Registry, Serial, ServiceKey, Waterfall};

// ---- a real registration set ----------------------------------------------------------------

trait Greeter: Send + Sync {
    fn greet(&self) -> String;
}

struct GreeterKey;

impl ServiceKey for GreeterKey {
    type Service = dyn Greeter;
    const ID: &'static str = "greeter";
}

struct English;

impl Greeter for English {
    fn greet(&self) -> String {
        "hello".to_owned()
    }
}

trait Farewell: Send + Sync {
    fn bye(&self) -> String;
}

struct FarewellKey;

impl ServiceKey for FarewellKey {
    type Service = dyn Farewell;
    const ID: &'static str = "farewell";
}

struct Polite;

impl Farewell for Polite {
    fn bye(&self) -> String {
        "goodbye".to_owned()
    }
}

/// An `emit` event, declared the way `runtime::AgentTurn` declares its own.
struct Ping;

impl Event for Ping {
    const NAME: &'static str = "test/ping";
    type Args = u32;
}

/// A `parallel` event.
struct Fanout;

impl Parallel for Fanout {
    const NAME: &'static str = "test/fanout";
    type Args = u32;
}

/// A `serial` event.
struct Pipeline;

impl Serial for Pipeline {
    const NAME: &'static str = "test/pipeline";
    type Args = &'static str;
    type Out = bool;
}

/// A `bail` event.
struct Claim;

impl Bail for Claim {
    const NAME: &'static str = "test/claim";
    type Args = &'static str;
    type Out = bool;
}

/// A `waterfall` event.
struct Transform;

impl Waterfall for Transform {
    const NAME: &'static str = "test/transform";
    type Args = u32;
    type Out = u32;
}

// The dispatch mode follows from the trait an event implements, so it is carried by a marker
// requiring that trait: `Emit::<Claim>::MODE` does not compile, and the mode recorded for an event
// cannot drift away from the trait it is declared with.
struct Emit<T>(std::marker::PhantomData<T>);

impl<T: Event> Emit<T> {
    const MODE: &'static str = "emit";
}

struct Fan<T>(std::marker::PhantomData<T>);

impl<T: Parallel> Fan<T> {
    const MODE: &'static str = "parallel";
}

struct Ordered<T>(std::marker::PhantomData<T>);

impl<T: Serial> Ordered<T> {
    const MODE: &'static str = "serial";
}

struct Stop<T>(std::marker::PhantomData<T>);

impl<T: Bail> Stop<T> {
    const MODE: &'static str = "bail";
}

struct ChainMode<T>(std::marker::PhantomData<T>);

impl<T: Waterfall> ChainMode<T> {
    const MODE: &'static str = "waterfall";
}

/// The declared events, as `(NAME, mode)` pairs read from the key types themselves.
fn declared_events() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (Ping::NAME, vec![Emit::<Ping>::MODE]),
        (Fanout::NAME, vec![Fan::<Fanout>::MODE]),
        (Pipeline::NAME, vec![Ordered::<Pipeline>::MODE]),
        (Claim::NAME, vec![Stop::<Claim>::MODE]),
        (Transform::NAME, vec![ChainMode::<Transform>::MODE]),
    ]
}

fn populated() -> Registry {
    let registry = Registry::new();
    let ctx = registry.context("catalog-test-plugin");
    ctx.register::<GreeterKey>(Arc::new(English)).unwrap();
    ctx.register::<FarewellKey>(Arc::new(Polite)).unwrap();
    registry
}

// ---- the test that matters ------------------------------------------------------------------

#[test]
fn the_catalog_matches_what_the_registry_actually_published() {
    let registry = populated();

    // Both sides come from the declarations, never from a hand-written list: the ids are the
    // `ServiceKey::ID` constants of the very keys that were registered.
    let ids = [GreeterKey::ID, FarewellKey::ID];
    let catalog = Catalog::from_registry(&ids, &declared_events());

    // Every name the caller declared is resolvable in the real table, and vice versa.
    for (id, present) in [
        (GreeterKey::ID, registry.contains::<GreeterKey>()),
        (FarewellKey::ID, registry.contains::<FarewellKey>()),
    ] {
        assert!(
            present,
            "`{id}` must really be registered for this test to mean anything"
        );
        assert!(
            catalog.service(id).is_some(),
            "registered service `{id}` is missing from the catalog"
        );
    }
    assert!(
        registry.get::<GreeterKey>().is_ok(),
        "the catalog claims `greeter` exists, so it must resolve"
    );
    assert_eq!(registry.get::<GreeterKey>().unwrap().greet(), "hello");
    assert_eq!(registry.get::<FarewellKey>().unwrap().bye(), "goodbye");

    // The counts agree across the type-erasure boundary: the registry exposes only `len`, so equal
    // counts plus the membership checks above leave no room for an invented or dropped entry.
    assert_eq!(catalog.services().count(), ids.len());
    assert_eq!(catalog.services().count(), registry.len());

    // Services that exist are exactly the published set: nothing else is catalogued.
    let names = catalog
        .services()
        .map(|service| service.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["farewell", "greeter"]);

    // The empty description is deliberate, not a placeholder that pretends to be documentation.
    for service in catalog.services() {
        assert!(service.description.is_empty());
        assert!(service.methods.is_empty());
    }
}

#[test]
fn the_catalog_matches_the_declared_events() {
    let catalog = Catalog::from_registry(&[], &declared_events());

    for (name, modes) in declared_events() {
        let entry = catalog
            .event(name)
            .unwrap_or_else(|| panic!("declared event `{name}` is missing from the catalog"));
        assert_eq!(entry.name, name);
        assert_eq!(entry.modes, modes, "mode recorded for `{name}`");
        assert!(entry.description.is_empty());
    }

    assert_eq!(catalog.events().count(), 5);
    // A name close to a real one but not declared is absent, so the catalog is not fuzzy.
    assert!(catalog.event("test/ping-not-declared").is_none());
}

#[test]
fn an_event_name_comes_from_the_constant_not_a_literal() {
    // If this catalog were built from a copy of the string, renaming the event would leave the
    // catalog behind. Building it from `NAME` makes the two impossible to disagree.
    let catalog = Catalog::from_registry(&[], &[(Ping::NAME, vec![])]);

    assert!(catalog.event("test/ping").is_some());
    assert_eq!(
        catalog.event(Ping::NAME).map(|event| event.name.as_str()),
        Some(Ping::NAME)
    );
}

#[test]
fn a_service_absent_from_the_registry_is_absent_from_the_catalog() {
    // The inverse direction of the consistency test: the catalog was built from `greeter` alone
    // while the registry published both, so the catalog must not gain `farewell` from anywhere —
    // in particular not from a built-in list of names it was never told about.
    let registry = populated();
    let catalog = Catalog::from_registry(&[GreeterKey::ID], &[]);

    assert!(
        registry.contains::<FarewellKey>(),
        "the registry really holds the second service"
    );
    assert!(catalog.service(FarewellKey::ID).is_none());
    assert!(catalog.service("Greeter").is_none());
}

// ---- lookups ---------------------------------------------------------------------------------

#[test]
fn a_present_service_and_event_are_found_by_name() {
    let catalog = Catalog::from_registry(&["greeter", "farewell"], &[("agent/turn", vec!["emit"])]);

    let service = catalog.service("greeter").unwrap();
    assert_eq!(service.name, "greeter");
    assert_eq!(service.methods, Vec::new());

    let event = catalog.event("agent/turn").unwrap();
    assert_eq!(event.name, "agent/turn");
    assert_eq!(event.modes, vec!["emit"]);
}

#[test]
fn a_name_that_was_never_registered_is_none() {
    let catalog = Catalog::from_registry(&["greeter"], &[("agent/turn", vec!["emit"])]);

    assert!(catalog.service("tools").is_none());
    assert!(catalog.service("").is_none());
    assert!(catalog.event("agent/step").is_none());
    assert!(catalog.event("Agent/turn").is_none());
}

#[test]
fn services_and_events_iterate_in_name_order() {
    let catalog = Catalog::from_registry(
        &["zeta", "alpha", "mid"],
        &[("z/event", vec![]), ("a/event", vec![])],
    );

    let services = catalog
        .services()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    let events = catalog
        .events()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(services, vec!["alpha", "mid", "zeta"]);
    assert_eq!(events, vec!["a/event", "z/event"]);
}

// ---- serialization ---------------------------------------------------------------------------

#[test]
fn to_json_has_the_documented_shape() {
    let mut catalog = Catalog::from_registry(&["greeter"], &[("agent/turn", vec!["emit"])]);
    catalog
        .describe_service("greeter", "Greets people.")
        .unwrap();
    catalog
        .add_method(
            "greeter",
            MethodEntry {
                name: "greet".to_owned(),
                signature: "greet() -> String".to_owned(),
                description: "Returns a greeting.".to_owned(),
            },
        )
        .unwrap();

    let value = catalog.to_json();

    assert_eq!(
        value,
        serde_json::json!({
            "services": [{
                "name": "greeter",
                "description": "Greets people.",
                "methods": [{
                    "name": "greet",
                    "signature": "greet() -> String",
                    "description": "Returns a greeting.",
                }],
            }],
            "events": [{
                "name": "agent/turn",
                "modes": ["emit"],
                "description": "",
            }],
        })
    );
}

#[test]
fn to_json_keeps_both_arrays_present_even_when_empty() {
    let value = Catalog::new().to_json();

    assert_eq!(value, serde_json::json!({"services": [], "events": []}));
    // A consumer that has to distinguish "absent" from "empty" would get this wrong; there is
    // nothing to distinguish, because the fields are always arrays.
    assert!(
        value
            .get("services")
            .is_some_and(serde_json::Value::is_array)
    );
    assert!(value.get("events").is_some_and(serde_json::Value::is_array));
}

#[test]
fn to_json_orders_entries_so_two_catalogs_from_one_set_agree() {
    let forward = Catalog::from_registry(&["a", "b", "c"], &[("x/y", vec![]), ("a/b", vec![])]);
    let backward = Catalog::from_registry(&["c", "b", "a"], &[("a/b", vec![]), ("x/y", vec![])]);

    assert_eq!(forward.to_json(), backward.to_json());
}

// ---- the empty catalog -----------------------------------------------------------------------

#[test]
fn an_empty_catalog_answers_nothing_and_does_not_panic() {
    let catalog = Catalog::from_registry(&[], &[]);

    assert!(catalog.is_empty());
    assert_eq!(catalog.services().count(), 0);
    assert_eq!(catalog.events().count(), 0);
    assert!(catalog.service("greeter").is_none());
    assert!(catalog.event("agent/turn").is_none());
    assert_eq!(
        catalog.to_json(),
        serde_json::json!({"services": [], "events": []})
    );
}

#[test]
fn empty_names_are_skipped_and_duplicates_collapse() {
    // Neither an empty id nor an empty event name can exist in the registry, so admitting one
    // would make the catalog claim a capability that cannot be resolved.
    let catalog = Catalog::from_registry(&["", "greeter", "greeter"], &[("", vec!["emit"])]);

    assert_eq!(catalog.services().count(), 1);
    assert!(catalog.service("").is_none());
    assert_eq!(catalog.events().count(), 0);

    let duplicated =
        Catalog::from_registry(&[], &[("e/dupe", vec!["emit"]), ("e/dupe", vec!["bail"])]);
    assert_eq!(duplicated.events().count(), 1);
    // Last occurrence wins, deterministically: a map cannot hold two entries under one key.
    assert_eq!(duplicated.event("e/dupe").unwrap().modes, vec!["bail"]);
}

// ---- enrichment cannot invent entries --------------------------------------------------------

#[test]
fn describing_an_unpublished_service_is_refused() {
    let mut catalog = Catalog::from_registry(&["greeter"], &[]);

    let error = catalog
        .describe_service("tools", "not published")
        .unwrap_err();
    assert_eq!(
        error,
        CatalogError::UnknownService {
            name: "tools".to_owned()
        }
    );
    assert!(
        catalog.service("tools").is_none(),
        "a refusal must not add the entry"
    );
}

#[test]
fn describing_an_undeclared_event_is_refused() {
    let mut catalog = Catalog::from_registry(&[], &[("agent/turn", vec!["emit"])]);

    let error = catalog.describe_event("agent/step", "nope").unwrap_err();
    assert_eq!(
        error,
        CatalogError::UnknownEvent {
            name: "agent/step".to_owned()
        }
    );
    assert!(catalog.event("agent/step").is_none());
}

#[test]
fn a_method_can_only_be_added_to_a_published_service() {
    let mut catalog = Catalog::from_registry(&["greeter"], &[]);

    let error = catalog
        .add_method(
            "tools",
            MethodEntry {
                name: "run".to_owned(),
                signature: "run()".to_owned(),
                description: String::new(),
            },
        )
        .unwrap_err();

    assert_eq!(
        error,
        CatalogError::UnknownService {
            name: "tools".to_owned()
        }
    );
    assert!(catalog.services().all(|service| service.methods.is_empty()));
}

#[test]
fn a_duplicate_method_name_is_refused_and_the_original_is_kept() {
    let mut catalog = Catalog::from_registry(&["greeter"], &[]);
    let first = MethodEntry {
        name: "greet".to_owned(),
        signature: "greet() -> String".to_owned(),
        description: "original".to_owned(),
    };
    catalog.add_method("greeter", first.clone()).unwrap();

    let error = catalog
        .add_method(
            "greeter",
            MethodEntry {
                name: "greet".to_owned(),
                signature: "greet(who: &str) -> String".to_owned(),
                description: "second".to_owned(),
            },
        )
        .unwrap_err();

    assert_eq!(
        error,
        CatalogError::DuplicateMethod {
            service: "greeter".to_owned(),
            method: "greet".to_owned(),
        }
    );
    let methods = &catalog.service("greeter").unwrap().methods;
    assert_eq!(methods, &vec![first]);
}

#[test]
fn descriptions_and_methods_round_trip_through_json() {
    let mut catalog = Catalog::from_registry(&["greeter"], &[("agent/turn", vec!["emit"])]);
    catalog.describe_service("greeter", "Greets.").unwrap();
    catalog.describe_event("agent/turn", "One turn.").unwrap();
    catalog
        .add_method(
            "greeter",
            MethodEntry {
                name: "greet".to_owned(),
                signature: "greet() -> String".to_owned(),
                description: "Say hello.".to_owned(),
            },
        )
        .unwrap();

    let value = catalog.to_json();

    assert_eq!(value["services"][0]["description"], "Greets.");
    assert_eq!(value["services"][0]["methods"][0]["name"], "greet");
    assert_eq!(
        value["services"][0]["methods"][0]["signature"],
        "greet() -> String"
    );
    assert_eq!(
        value["services"][0]["methods"][0]["description"],
        "Say hello."
    );
    assert_eq!(value["events"][0]["description"], "One turn.");
}

// ---- shape-level guarantees ------------------------------------------------------------------

#[test]
fn entries_keep_the_names_they_were_built_with() {
    // The struct fields are public, so a consumer may construct an entry directly; the catalog's
    // own constructor must agree with what it stores under the key.
    let catalog = Catalog::from_registry(&["greeter"], &[("agent/turn", vec!["emit", "serial"])]);
    let ServiceEntry { name, .. } = catalog.service("greeter").unwrap().clone();
    let EventEntry { name: event, .. } = catalog.event("agent/turn").unwrap().clone();

    assert_eq!(name, "greeter");
    assert_eq!(event, "agent/turn");
    assert_eq!(
        catalog.event("agent/turn").unwrap().modes,
        vec!["emit", "serial"]
    );
}
