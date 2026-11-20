//! Tests for per-call resource resolution in the access layer.
//!
//! Upstream reference: `packages/api/gateway/src/access.ts` (179 lines, `callResources` /
//! `eventResources`) plus the `decide()` contract in `packages/api/remote-access/src/index.ts`.
//! The properties pinned here are the ones that fail *silently* if they regress — a call authorized
//! against the wrong session still succeeds, and no log line says so:
//!
//! - a call's resources are the ids in its arguments, not a wildcard — the whole point of the module;
//! - the last source wins per kind, but the kind keeps the position of the first source;
//! - a malformed source is `resource-invalid`, never an empty or partial resource list;
//! - `global` is the answer only when no source named anything, never as a fallback for a failure;
//! - a forwarded event that cannot name its resources reaches nobody, rather than everybody;
//! - the decision quantifier is `all`, so one unreachable resource refuses the whole call.
//!
//! # Why the modules are compiled by path
//!
//! `girder-core`'s crate root does not declare `remote_access_gap`; adding it and its `pub use` is
//! the integrator's change. `typert_gap` is compiled the same way because it is private to the
//! library and this test needs its registry. Both are included from their real source files, so the
//! modules see each other exactly as they will once wired in.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test asserts by panicking; a failed expectation is the result it is reporting"
)]

#[path = "../src/typert/registry.rs"]
pub mod typert_gap;

#[path = "../src/api/remote_access.rs"]
pub mod remote_access_gap;

use serde_json::{Value, json};
use std::collections::BTreeMap;

use girder_core::{Access, AccessDecision, AccessResource, AllowList, Policy, ReadOnly};

use remote_access_gap::{
    CallRequest, MethodAccessResolver, NO_ALLOW, RESOURCE_INVALID, Refused, Resolved,
    ResolvedResource, ResourceIds, ResourceReader, all_allowed, declaration, kinds_of, reader,
    resolve_event_resources,
};
use typert_gap::{
    GLOBAL_RESOURCE_KIND, HostContextAdapter, InvocationDescriptor, InvocationParameter,
    LookupDeclaration, LookupProvider, ResourceDeclaration, TypertRegistry,
};

// ---- fixtures -----------------------------------------------------------------------------------

/// A descriptor for a direct, unary `session.delete`.
fn delete_descriptor() -> InvocationDescriptor {
    InvocationDescriptor::new("id", "sessions", "session", "delete")
        .with_parameter(InvocationParameter::json("sessionId"))
}

/// One argument map.
fn wire_args(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

/// The reader map a call is resolved against.
type Readers = BTreeMap<String, ResourceReader>;

/// No method readers.
fn no_readers() -> Readers {
    BTreeMap::new()
}

/// A registry with one `session` kind read from the `sessionId` field.
fn registry_with_sessions() -> TypertRegistry {
    let mut registry = TypertRegistry::new();
    registry
        .register_resource("session", ResourceDeclaration::field("sessionId"))
        .unwrap();
    registry
}

/// The single resource list, or a panic naming the failure.
fn expect_resources(resolved: &Resolved) -> Vec<ResolvedResource> {
    match resolved {
        Resolved::Resources(resources) => resources.clone(),
        Resolved::Invalid => panic!("expected resolved resources, got invalid"),
    }
}

// ---- the gap itself -----------------------------------------------------------------------------

/// The property the module exists for: a policy is asked about the id the caller named.
///
/// Before this, `Declaration` gave the policy `session:` (any) for every call, so a policy could
/// not tell deleting one session from deleting another.
#[test]
fn a_call_resolves_to_the_id_its_argument_names() {
    let registry = registry_with_sessions();
    let resolver = MethodAccessResolver::new(&registry);
    let descriptor = delete_descriptor();

    let answer = resolver.resolve_call(
        &descriptor,
        &no_readers(),
        &wire_args(&[("sessionId", json!("session-a"))]),
        None,
    );

    assert_eq!(
        expect_resources(&answer),
        vec![ResolvedResource::new("session", "session-a")]
    );
    assert_eq!(answer.refusal(), None);
}

/// Two different arguments are two different resources — the distinction a wildcard destroys.
#[test]
fn two_calls_on_different_arguments_resolve_to_different_resources() {
    let registry = registry_with_sessions();
    let resolver = MethodAccessResolver::new(&registry);
    let descriptor = delete_descriptor();

    let first = resolver.resolve_call(
        &descriptor,
        &no_readers(),
        &wire_args(&[("sessionId", json!("session-a"))]),
        None,
    );
    let second = resolver.resolve_call(
        &descriptor,
        &no_readers(),
        &wire_args(&[("sessionId", json!("session-b"))]),
        None,
    );

    assert_ne!(expect_resources(&first), expect_resources(&second));
    assert_eq!(
        expect_resources(&second),
        vec![ResolvedResource::new("session", "session-b")]
    );
}

/// A policy now sees a real id and can refuse one session while allowing another.
///
/// `ReadOnly` is used because it needs no state: the point is that the per-call resource travelled
/// all the way into the decision, which the assertion on the recorded request proves.
#[test]
fn the_decision_receives_the_resolved_resource_not_a_wildcard() {
    let registry = registry_with_sessions();
    let resolver = MethodAccessResolver::new(&registry);
    let descriptor = delete_descriptor();

    let request = resolver
        .request(
            &descriptor,
            &no_readers(),
            "peer-1",
            Access::Write,
            &wire_args(&[("sessionId", json!("session-a"))]),
            None,
        )
        .unwrap();

    // The id survives the projection into the type a mounted policy is actually asked about.
    assert_eq!(
        request.to_access_request().resources,
        vec![AccessResource::new("session", "session-a")]
    );
    assert_eq!(request.method, "session/delete");
    // And a read-only policy refuses the write *by name*, not merely "some session".
    match request.decide(&ReadOnly) {
        AccessDecision::Refuse { reason } => assert!(reason.contains("session/delete")),
        AccessDecision::Allow => panic!("a write must not pass a read-only policy"),
    }
}

/// An unset argument names nothing, so the call falls back to the global resource.
#[test]
fn no_argument_naming_a_resource_resolves_to_global() {
    let registry = registry_with_sessions();
    let resolver = MethodAccessResolver::new(&registry);

    let answer = resolver.resolve_call(&delete_descriptor(), &no_readers(), &wire_args(&[]), None);

    assert_eq!(expect_resources(&answer), vec![ResolvedResource::global()]);
    assert!(expect_resources(&answer)[0].is_global());
}

// ---- merging rules ------------------------------------------------------------------------------

/// The last source wins per kind, but the kind keeps the position its first source gave it.
///
/// Order is what a snapshot's grant list and any structured log preserve, so a kind that moved
/// position between two evaluations would make the two look unrelated.
#[test]
fn a_later_source_replaces_a_kind_without_moving_it() {
    let mut registry = TypertRegistry::new();
    registry
        .register_resource("session", ResourceDeclaration::field("sessionId"))
        .unwrap();
    registry
        .register_resource("workspace", ResourceDeclaration::field("workspaceId"))
        .unwrap();
    let resolver = MethodAccessResolver::new(&registry);
    // Both ids are declared parameters, which is upstream's precondition: a field declaration only
    // reads a top-level argument that the descriptor declares.
    let descriptor = InvocationDescriptor::new("id", "sessions", "session", "move")
        .with_parameter(InvocationParameter::json("sessionId"))
        .with_parameter(InvocationParameter::json("workspaceId"));

    // `session` is mentioned first by its default declaration, then replaced by the reader; the
    // registry's order puts `session` before `workspace`, and the replacement must not change that.
    let mut readers = no_readers();
    readers.insert(
        "session".to_owned(),
        reader(|_| Some(ResourceIds::One("session-from-reader".to_owned()))),
    );

    let answer = resolver.resolve_call(
        &descriptor,
        &readers,
        &wire_args(&[
            ("sessionId", json!("session-from-field")),
            ("workspaceId", json!("workspace-a")),
        ]),
        None,
    );

    assert_eq!(
        expect_resources(&answer),
        vec![
            ResolvedResource::new("session", "session-from-reader"),
            ResolvedResource::new("workspace", "workspace-a"),
        ]
    );
}

/// A reader receives the declared parameters positionally, in call order.
///
/// A reader written against the source signature would silently read the wrong argument if the
/// local call site built its list differently, and the wrong argument means the wrong resource.
#[test]
fn a_reader_receives_positional_arguments_in_declaration_order() {
    let registry = TypertRegistry::new();
    let resolver = MethodAccessResolver::new(&registry);
    let descriptor = InvocationDescriptor::new("id", "sessions", "session", "move")
        .with_parameter(InvocationParameter::json("from"))
        .with_parameter(InvocationParameter::json("to"));

    let mut readers = no_readers();
    readers.insert(
        "session".to_owned(),
        reader(|positional| {
            assert_eq!(positional, &[json!("session-a"), json!("workspace-b")]);
            Some(ResourceIds::One(positional[0].as_str().unwrap().to_owned()))
        }),
    );

    let answer = resolver.resolve_call(
        &descriptor,
        &readers,
        &wire_args(&[("from", json!("session-a")), ("to", json!("workspace-b"))]),
        None,
    );

    assert_eq!(
        expect_resources(&answer),
        vec![ResolvedResource::new("session", "session-a")]
    );
}

/// The cancellation slot is appended after every declared parameter.
#[test]
fn the_cancellation_slot_is_the_last_positional_argument() {
    let registry = TypertRegistry::new();
    let resolver = MethodAccessResolver::new(&registry);
    let descriptor = InvocationDescriptor::new("id", "sessions", "session", "watch")
        .with_parameter(InvocationParameter::json("sessionId"))
        .with_cancellation();

    let mut readers = no_readers();
    readers.insert(
        "session".to_owned(),
        reader(|positional| {
            assert_eq!(positional.len(), 2);
            assert_eq!(positional[1], json!("signal-token"));
            Some(ResourceIds::One("session-a".to_owned()))
        }),
    );

    let answer = resolver.resolve_call(
        &descriptor,
        &readers,
        &wire_args(&[("sessionId", json!("session-a"))]),
        Some(&json!("signal-token")),
    );

    assert!(answer.is_resolved());
}

/// A lookup parameter whose provider names a kind contributes its wire identity.
#[test]
fn a_lookup_parameter_names_a_resource_of_its_kind() {
    let mut registry = TypertRegistry::new();
    registry
        .register_lookup(
            LookupProvider::new(
                LookupDeclaration::new("session", "session", "sessionId", "Session", "SessionId"),
                |_| Some(json!("resolved")),
            )
            .naming_resource("session"),
        )
        .unwrap();
    let resolver = MethodAccessResolver::new(&registry);
    let descriptor =
        InvocationDescriptor::new("id", "sessions", "session", "delete").with_parameter(
            InvocationParameter::lookup("session", "sessionId", "session"),
        );

    let answer = resolver.resolve_call(
        &descriptor,
        &no_readers(),
        &wire_args(&[("sessionId", json!("session-a"))]),
        None,
    );

    assert_eq!(
        expect_resources(&answer),
        vec![ResolvedResource::new("session", "session-a")]
    );
}

/// A Context-receiver method's identity names a resource of the adapter's kind.
#[test]
fn the_context_receiver_identity_names_a_resource() {
    let mut registry = TypertRegistry::new();
    registry
        .register_host_context(
            "session",
            HostContextAdapter::new("sessionId", "SessionId", |_| Some("session".to_owned()))
                .naming_resource("session"),
        )
        .unwrap();
    let resolver = MethodAccessResolver::new(&registry);
    let descriptor = InvocationDescriptor::new("id", "sessions", "session", "list")
        .on_context("session", "sessionId");

    let answer = resolver.resolve_call(
        &descriptor,
        &no_readers(),
        &wire_args(&[("sessionId", json!("session-a"))]),
        None,
    );

    assert_eq!(
        expect_resources(&answer),
        vec![ResolvedResource::new("session", "session-a")]
    );
}

// ---- malformed answers are refusals, not wildcards ----------------------------------------------

/// An empty id is a malformed identity, not "any".
///
/// This is the rule that keeps the module from recreating the wildcard it removes: an empty string
/// is exactly how `Declaration` spells "any", so accepting one here would undo the whole port.
#[test]
fn an_empty_id_is_invalid_rather_than_a_wildcard() {
    let mut registry = TypertRegistry::new();
    registry
        .register_resource("session", ResourceDeclaration::field("sessionId"))
        .unwrap();
    let resolver = MethodAccessResolver::new(&registry);

    let answer = resolver.resolve_call(
        &delete_descriptor(),
        &no_readers(),
        &wire_args(&[("sessionId", json!(""))]),
        None,
    );

    assert_eq!(answer, Resolved::Invalid);
    assert_eq!(answer.refusal(), Some(RESOURCE_INVALID));
}

/// A reader that returns an empty list has contradicted itself, so the call is unresolvable.
#[test]
fn an_empty_reader_list_is_invalid() {
    let registry = TypertRegistry::new();
    let resolver = MethodAccessResolver::new(&registry);
    let mut readers = no_readers();
    readers.insert(
        "session".to_owned(),
        reader(|_| Some(ResourceIds::Many(Vec::new()))),
    );

    let answer = resolver.resolve_call(
        &delete_descriptor(),
        &readers,
        &wire_args(&[("sessionId", json!("session-a"))]),
        None,
    );

    assert_eq!(answer, Resolved::Invalid);
}

/// A single-resource declaration that yields several identities is refused, not truncated.
///
/// Authorizing against the first element would be a hole: the other identities would be touched
/// by a call that was never checked against them.
#[test]
fn a_multi_identity_declaration_is_invalid() {
    let mut registry = TypertRegistry::new();
    registry
        .register_resource(
            "session",
            ResourceDeclaration::resolver(|_| {
                typert_gap::ResourceIdentity::Many(vec!["a".to_owned(), "b".to_owned()])
            }),
        )
        .unwrap();
    let resolver = MethodAccessResolver::new(&registry);

    let answer = resolver.resolve_call(
        &delete_descriptor(),
        &no_readers(),
        &wire_args(&[("sessionId", json!("session-a"))]),
        None,
    );

    assert_eq!(answer, Resolved::Invalid);
}

/// The nested-object fallback: a resource carried inside a request object is still found.
///
/// Without this, a method that takes a whole request object would resolve to global and every such
/// call would be authorized against the host rather than against the session it names.
#[test]
fn a_resource_inside_a_nested_object_argument_is_found() {
    let mut registry = TypertRegistry::new();
    registry
        .register_resource("session", ResourceDeclaration::field("sessionId"))
        .unwrap();
    let resolver = MethodAccessResolver::new(&registry);
    let descriptor = InvocationDescriptor::new("id", "sessions", "session", "rename")
        .with_parameter(InvocationParameter::json("request"));

    let answer = resolver.resolve_call(
        &descriptor,
        &no_readers(),
        &wire_args(&[(
            "request",
            json!({ "sessionId": "session-a", "title": "new" }),
        )]),
        None,
    );

    assert_eq!(
        expect_resources(&answer),
        vec![ResolvedResource::new("session", "session-a")]
    );
}

/// A resolver-backed declaration is called with the call's arguments.
#[test]
fn a_resolver_declaration_sees_the_named_arguments() {
    let mut registry = TypertRegistry::new();
    registry
        .register_resource(
            "session",
            declaration(|input| match input.value("sessionId") {
                Some(Value::String(id)) => typert_gap::ResourceIdentity::Identity(id.clone()),
                _ => typert_gap::ResourceIdentity::Absent,
            }),
        )
        .unwrap();
    let resolver = MethodAccessResolver::new(&registry);

    let answer = resolver.resolve_call(
        &delete_descriptor(),
        &no_readers(),
        &wire_args(&[("sessionId", json!("session-a"))]),
        None,
    );

    assert_eq!(
        expect_resources(&answer),
        vec![ResolvedResource::new("session", "session-a")]
    );
}

/// A top-level argument the descriptor does not declare names no resource.
///
/// Upstream refuses an undeclared argument outright, so it can never reach resolution; pinning the
/// resolver's half of that rule keeps a hand-built call from smuggling a resource in through a
/// field the method never declared.
#[test]
fn an_undeclared_top_level_argument_names_nothing() {
    let registry = registry_with_sessions();
    let resolver = MethodAccessResolver::new(&registry);

    // `delete` declares only `sessionId`, so the extra `sessionId` look-alike is not read.
    let answer = resolver.resolve_call(
        &delete_descriptor(),
        &no_readers(),
        &wire_args(&[("undeclared", json!("session-a"))]),
        None,
    );

    assert_eq!(expect_resources(&answer), vec![ResolvedResource::global()]);
}

/// A malformed call is refused before a policy is asked, so a refused call activates nothing.
#[test]
fn an_invalid_call_is_refused_before_any_policy_sees_it() {
    let mut registry = TypertRegistry::new();
    registry
        .register_resource("session", ResourceDeclaration::field("sessionId"))
        .unwrap();
    let resolver = MethodAccessResolver::new(&registry);

    let refused = resolver
        .request(
            &delete_descriptor(),
            &no_readers(),
            "peer-1",
            Access::Write,
            &wire_args(&[("sessionId", json!(""))]),
            None,
        )
        .err();

    assert_eq!(refused, Some(Refused::new(RESOURCE_INVALID)));
}

/// The global resource is never a fallback for a failure — only for "nothing was named".
#[test]
fn global_is_not_a_fallback_for_a_malformed_source() {
    let registry = TypertRegistry::new();
    let resolver = MethodAccessResolver::new(&registry);
    let mut readers = no_readers();
    readers.insert(
        "session".to_owned(),
        reader(|_| Some(ResourceIds::One(String::new()))),
    );

    // No declaration ran and the only reader was malformed: the answer is invalid, not global.
    let answer = resolver.resolve_call(&delete_descriptor(), &readers, &wire_args(&[]), None);
    assert_eq!(answer, Resolved::Invalid);
}

// ---- forwarded events ---------------------------------------------------------------------------

/// A forwarded event resolves to what its declaration names, so delivery can be filtered per id.
///
/// `api-session/removed` declares one reader: its first argument is the session id.
#[test]
fn an_event_resolves_to_the_resources_its_declaration_names() {
    let answer = resolve_event_resources("api-session/removed", &[json!("session-a")]);

    assert_eq!(
        expect_resources(&answer),
        vec![ResolvedResource::new("session", "session-a")]
    );
}

/// A summary-carrying event reads the id out of the argument's `sessionId` field.
#[test]
fn an_event_reads_the_id_out_of_a_summary_argument() {
    let answer =
        resolve_event_resources("api-session/added", &[json!({ "sessionId": "session-a" })]);

    assert_eq!(
        expect_resources(&answer),
        vec![ResolvedResource::new("session", "session-a")]
    );
}

/// An event that declares no reader is a global event, not an invalid one.
///
/// The distinction matters: refusing every readerless event would drop notifications that are
/// genuinely host-wide, while treating a *failed* read as global would broadcast a per-session
/// event to everyone.
#[test]
fn an_event_without_readers_is_global() {
    let answer = resolve_event_resources("commands/change", &[]);

    assert_eq!(expect_resources(&answer), vec![ResolvedResource::global()]);
}

/// An event whose declared argument is missing is unresolvable, not global.
#[test]
fn an_event_with_a_missing_argument_is_invalid() {
    // `api-session/removed` declares a session id reader, so an empty argument list cannot name it.
    assert_eq!(
        resolve_event_resources("api-session/removed", &[]),
        Resolved::Invalid
    );
}

/// An event that cannot name its resources reaches nobody.
///
/// The alternative — falling back to global — would broadcast an unresolvable notification to every
/// non-owner, which is the one failure mode worse than dropping it.
#[test]
fn an_unresolvable_event_is_invalid_rather_than_global() {
    let answer = resolve_event_resources("no-such-event", &[json!("session-a")]);

    assert_eq!(answer, Resolved::Invalid);
    assert_eq!(answer.refusal(), Some(RESOURCE_INVALID));
}

// ---- the decision quantifier --------------------------------------------------------------------

/// A call touching two resources is allowed only when *both* are allowed.
///
/// `all` rather than `any`: a policy that wrote `any` would hand out a partial grant, and the
/// unauthorized half of the call would still run.
#[test]
fn the_decision_quantifier_is_all_not_any() {
    let resources = vec![
        ResolvedResource::new("session", "session-a"),
        ResolvedResource::new("session", "session-b"),
    ];

    assert!(all_allowed(&resources, |_| true));
    assert!(!all_allowed(&resources, |resource| {
        resource.id() == Some("session-a")
    }));
    // An empty resource list is not a vacuous truth: nothing was resolved, so nothing is allowed.
    assert!(!all_allowed(&[], |_| true));
}

/// The kinds of a call are reported once each, for a policy that branches per kind.
#[test]
fn kinds_are_reported_without_duplicates() {
    let resources = vec![
        ResolvedResource::new("session", "a"),
        ResolvedResource::new("session", "b"),
        ResolvedResource::new("workspace", "w"),
    ];

    let kinds: Vec<&str> = kinds_of(&resources).into_iter().collect();
    assert_eq!(kinds, vec!["session", "workspace"]);
}

// ---- the end-to-end shape -----------------------------------------------------------------------

/// A policy that reads the resolved resource can allow one session and refuse another.
///
/// This is the concrete capability the gap cost: before it, both calls looked identical.
#[test]
fn a_policy_can_allow_one_session_and_refuse_another() {
    struct OnlySessionA;

    impl Policy for OnlySessionA {
        fn decide(&self, request: &girder_core::AccessRequest) -> AccessDecision {
            let allowed = request
                .resources
                .iter()
                .all(|resource| resource.kind == "session" && resource.id == "session-a");
            if allowed {
                AccessDecision::Allow
            } else {
                AccessDecision::Refuse {
                    reason: NO_ALLOW.to_owned(),
                }
            }
        }
    }

    let registry = registry_with_sessions();
    let resolver = MethodAccessResolver::new(&registry);
    let policy = OnlySessionA;

    let allowed = resolver
        .request(
            &delete_descriptor(),
            &no_readers(),
            "peer-1",
            Access::Write,
            &wire_args(&[("sessionId", json!("session-a"))]),
            None,
        )
        .unwrap();
    let refused = resolver
        .request(
            &delete_descriptor(),
            &no_readers(),
            "peer-1",
            Access::Write,
            &wire_args(&[("sessionId", json!("session-b"))]),
            None,
        )
        .unwrap();

    assert!(allowed.decide(&policy).is_allowed());
    assert!(!refused.decide(&policy).is_allowed());
}

/// The global resource keeps the empty id a policy already understands as "the whole host".
#[test]
fn the_global_resource_projects_to_the_empty_id_a_policy_expects() {
    let request = CallRequest {
        peer: "peer-1".to_owned(),
        method: "session/list".to_owned(),
        access: Access::Read,
        resources: vec![ResolvedResource::global()],
        args: BTreeMap::new(),
    };

    let projected = request.to_access_request();
    assert_eq!(
        projected.resources,
        vec![AccessResource::any_of(GLOBAL_RESOURCE_KIND)]
    );
    // The peer name and the endpoint travel with it unchanged, so an allowlist keyed on either
    // still works.
    assert_eq!(projected.peer, "peer-1");
    assert_eq!(projected.method, "session/list");
    assert!(
        AllowList::new()
            .allowing("peer-1")
            .decide(&projected)
            .is_allowed()
    );
    assert!(!AllowList::new().decide(&projected).is_allowed());
}


// Path aliases so a file compiled with #[path] still resolves crate::<domain>::<module>.
mod api {
    pub use super::remote_access_gap as remote_access;
}

mod typert {
    pub use super::typert_gap as registry;
}
