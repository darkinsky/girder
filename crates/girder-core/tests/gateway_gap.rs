//! Tests for the peer-identity gap of upstream `packages/api/gateway`.
//!
//! Upstream reference: `packages/api/gateway/tests/gateway-access.host.spec.ts` and
//! `gateway-stream.host.spec.ts` (2,000+ lines between them), plus the `Peer`/`admit` contract in
//! `packages/client/connection`. The properties pinned here are the ones that make a peer *verified*
//! rather than *claimed*, and each would fail silently rather than loudly if it regressed:
//!
//! - a principal cannot be promoted by a wire field, and an argument naming one is refused;
//! - a disposed Peer is refused before its payload is read, and disposal is shared by every clone;
//! - admission is decided by the carrier, in upstream's order: trust, disposal, credential;
//! - a socket claims one Peer exclusively, and releasing it disposes that Peer;
//! - a forwarded-event stream belongs to the Peer that opened it, and only an owner answers;
//! - the owner bypasses the policy, and a non-owner without one is refused rather than allowed.
//!
//! # Why the module is included by path
//!
//! `girder-core`'s crate root does not declare `gateway_gap`; adding `pub mod gateway_gap;` and its
//! `pub use` belongs to the integrator. The module is compiled into this test binary directly, so it
//! must not reference `crate::` — inside a `#[path]` module that would name *this* test crate. Every
//! library reference goes through `girder_core::{...}` instead, which is why the module needs no shim
//! modules the way `storage_gap` does.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert by panicking; a failed unwrap is a failed test"
)]

#[path = "../src/gateway_gap.rs"]
mod gateway_gap;

use girder_core::{Access, AccessDecision, AccessRequest, AccessResource, Policy, RemoteCall};
use serde_json::{Map, Value, json};

use gateway_gap::{
    Admission, AuthorisationRequest, ConnectionOutcome, Credential, ERROR_CODES, EVENTS_ENDPOINT,
    EVENTS_RESULT_ENDPOINT, EventStreams, GatewayEndpoint, GatewayError, Interactions,
    OpenedStream, Peer, PeerDirectory, PeerId, Principal, RESERVED_IDENTITY_FIELDS, Rejection,
    Route, admitted_call, assert_no_identity_fields, authorise, endpoint_of, is_gateway_endpoint,
    is_gateway_error_code, route,
};

// ---- helpers ------------------------------------------------------------------------------------

/// A guest Peer named the way the upstream specs name one.
fn guest(name: &str) -> Peer {
    Peer::new(PeerId::new(name), Principal::named("test-guest", name))
}

/// A directory with one published guest, returning both.
fn directory_with_guest() -> (PeerDirectory, Peer) {
    let directory = PeerDirectory::new();
    let peer = directory.publish(Principal::named("test-guest", "api"), Interactions::Answer);
    (directory, peer)
}

/// Admits one request, returning the carrier-shaped result.
fn admitted(
    directory: &PeerDirectory,
    trusted: bool,
    credential: Credential<'_>,
) -> Result<Peer, Rejection> {
    let outcome: Result<Peer, Rejection> = directory.admit(trusted, credential).into();
    outcome
}

/// Admits one socket, returning the carrier-shaped result.
fn connected(
    directory: &PeerDirectory,
    trusted: bool,
    credential: Credential<'_>,
) -> Result<gateway_gap::Connection, Rejection> {
    let outcome: Result<gateway_gap::Connection, Rejection> =
        directory.admit_connection(trusted, credential).into();
    outcome
}

/// A policy that records every request it decides.
#[derive(Debug, Default)]
struct Recorder {
    seen: std::sync::Mutex<Vec<AccessRequest>>,
}

impl Recorder {
    fn requests(&self) -> Vec<AccessRequest> {
        self.seen.lock().expect("not poisoned").clone()
    }
}

impl Policy for Recorder {
    fn decide(&self, request: &AccessRequest) -> AccessDecision {
        self.seen
            .lock()
            .expect("not poisoned")
            .push(request.clone());
        AccessDecision::Allow
    }
}

// ---- measurement: the surface this module covers -------------------------------------------------

#[test]
fn every_failure_code_is_a_stable_remote_spelling() {
    for code in ERROR_CODES {
        assert!(
            is_gateway_error_code(code),
            "{code} is declared but unknown"
        );
        // Upstream's Remote failure vocabulary is `<namespace>/<name>` in lower case.
        let (namespace, name) = code
            .split_once('/')
            .unwrap_or_else(|| panic!("{code} is not a namespaced code"));
        assert!(
            !namespace.is_empty() && !name.is_empty(),
            "{code} is malformed"
        );
        assert_eq!(*code, code.to_lowercase(), "{code} is not lower case");
    }
    assert!(!is_gateway_error_code("gateway/not-a-real-code"));
    assert!(!is_gateway_error_code("PEER_DISPOSED"));
}

#[test]
fn a_failure_carries_upstream_shaped_details() {
    let disposed = GatewayError::PeerDisposed {
        endpoint: "session/list".to_owned(),
    };
    assert_eq!(disposed.code(), "gateway/peer-disposed");
    assert_eq!(disposed.endpoint(), "session/list");
    assert_eq!(disposed.field(), None);
    assert_eq!(disposed.details(), json!({ "endpoint": "session/list" }));

    let invalid = GatewayError::ArgumentsInvalid {
        endpoint: EVENTS_ENDPOINT.to_owned(),
        field: "args".to_owned(),
    };
    assert_eq!(invalid.field(), Some("args"));
    assert_eq!(
        invalid.details(),
        json!({ "endpoint": EVENTS_ENDPOINT, "field": "args" })
    );
    // `field` is absent, not null: a Client that distinguishes the two would mis-render null.
    assert!(disposed.details().get("field").is_none());
    assert!(disposed.to_string().contains("session/list"));
}

// ---- endpoints -----------------------------------------------------------------------------------

#[test]
fn gateway_endpoints_are_resolved_before_the_method_split() {
    assert!(is_gateway_endpoint(EVENTS_ENDPOINT));
    assert!(is_gateway_endpoint(EVENTS_RESULT_ENDPOINT));
    assert!(!is_gateway_endpoint("$events/result/extra"));
    assert!(!is_gateway_endpoint("session/list"));

    assert_eq!(
        route(EVENTS_ENDPOINT).expect("the events endpoint"),
        Route::Gateway(GatewayEndpoint::Events)
    );
    // Without the gateway check this would split into namespace `$events`, method `result` — a method
    // no service may implement, so the check is what keeps the two names from colliding.
    assert_eq!(
        route(EVENTS_RESULT_ENDPOINT).expect("the result endpoint"),
        Route::Gateway(GatewayEndpoint::EventsResult)
    );
}

#[test]
fn a_method_endpoint_splits_into_exactly_one_pair() {
    assert_eq!(
        route("session/list").expect("a method"),
        Route::Method {
            namespace: "session".to_owned(),
            method: "list".to_owned(),
        }
    );
    for endpoint in ["session", "session/", "/list", "", "a/b/c", "$events/"] {
        let error = route(endpoint).expect_err("not a routable endpoint");
        assert_eq!(error.code(), "gateway/ambiguous-endpoint", "{endpoint}");
        assert_eq!(error.endpoint(), endpoint);
    }
    assert_eq!(endpoint_of("session", "list"), "session/list");
}

// ---- principals: the wire cannot promote a caller ------------------------------------------------

#[test]
fn the_owner_principal_is_the_only_owner() {
    assert!(Principal::owner().is_owner());
    assert!(!Principal::named("owner-ish", "x").is_owner());
    assert!(!Principal::named("test-guest", "x").is_owner());
    assert_eq!(Principal::owner().to_wire(), json!({ "kind": "owner" }));
}

#[test]
fn a_wire_payload_cannot_claim_the_owner_principal() {
    // Upstream: "Caller identity fixed at creation by in-process Host code; no wire field can set it."
    let error = Principal::from_wire(EVENTS_RESULT_ENDPOINT, &json!({ "kind": "owner" }))
        .expect_err("an owner claim is refused");
    assert_eq!(error.code(), "gateway/arguments-invalid");
    assert_eq!(error.field(), Some("principal"));
}

#[test]
fn a_malformed_principal_is_refused_rather_than_defaulted() {
    for value in [
        json!("owner"),
        json!({}),
        json!({ "kind": 7 }),
        json!({ "kind": "" }),
    ] {
        let error = Principal::from_wire(EVENTS_RESULT_ENDPOINT, &value)
            .expect_err("a malformed principal cannot name a caller");
        assert_eq!(error.code(), "gateway/arguments-invalid", "{value}");
    }
}

#[test]
fn a_named_principal_keeps_its_discriminant_and_subject() {
    let principal = Principal::from_wire(
        EVENTS_RESULT_ENDPOINT,
        &json!({ "kind": "test-guest", "guest": "subagent-controls" }),
    )
    .expect("a non-owner principal is readable");
    assert_eq!(principal.kind(), "test-guest");
    assert_eq!(principal.subject(), Some("subagent-controls"));
    assert_eq!(
        principal,
        Principal::named("test-guest", "subagent-controls")
    );
    assert!(principal.to_wire().get("kind").is_some());
}

#[test]
fn an_identity_field_in_arguments_is_refused_not_ignored() {
    for field in RESERVED_IDENTITY_FIELDS {
        let mut args = Map::new();
        args.insert(field.to_owned(), json!("owner"));
        let error = assert_no_identity_fields("session/list", &json!(args))
            .expect_err("an identity field is refused");
        assert_eq!(error.code(), "gateway/arguments-invalid");
        assert_eq!(error.field(), Some(field));
    }
    // A field that merely resembles one is ordinary data.
    assert!(
        assert_no_identity_fields(
            "session/list",
            &json!({ "peerId": "p1", "principalKind": "owner" })
        )
        .is_ok()
    );
    // Non-object arguments are the codec's problem, not this check's.
    assert!(assert_no_identity_fields("session/list", &json!([1, 2])).is_ok());
}

#[test]
fn a_nested_identity_field_is_not_a_top_level_claim() {
    // Upstream resolves the resources a *decoded* argument names, so a nested `principal` names no
    // caller; only the top level is the transport's business.
    let args = json!({ "request": { "principal": "owner" } });
    assert!(assert_no_identity_fields("session/list", &args).is_ok());
}

// ---- peers and lifetime --------------------------------------------------------------------------

#[test]
fn a_fresh_peer_is_live_and_defaults_to_answering() {
    let peer = guest("guest-1");
    assert!(peer.is_live());
    assert_eq!(peer.interactions(), Interactions::Answer);
    assert_eq!(peer.id().as_str(), "guest-1");
    assert_eq!(peer.principal(), &Principal::named("test-guest", "guest-1"));
}

#[test]
fn disposal_is_shared_by_every_clone_and_idempotent() {
    let peer = guest("guest-1");
    let second = peer.clone();
    let third = second.clone();
    assert!(second.dispose());
    assert!(!peer.is_live());
    assert!(!third.is_live());
    // The second dispose reports that it disposed nothing, so a teardown path can be idempotent.
    assert!(!peer.dispose());
    assert_eq!(
        peer.assert_live(EVENTS_ENDPOINT)
            .expect_err("disposed")
            .code(),
        "gateway/peer-disposed"
    );
}

#[test]
fn a_disposed_peer_is_refused_before_its_arguments_are_read() {
    // The argument is also malformed; disposal must win, because a dead Peer's payload is not worth
    // validating and the ordering is what stops a disposal race from admitting one more call.
    let peer = guest("guest-1");
    peer.dispose();
    let Err(error) = admitted_call(&peer, json!({ "principal": "owner" })) else {
        panic!("a disposed peer builds no call")
    };
    assert_eq!(error.code(), "gateway/peer-disposed");
}

#[test]
fn an_admitted_call_takes_its_peer_from_the_peer() {
    let peer = guest("guest-1");
    let Ok(call): Result<RemoteCall, _> = admitted_call(&peer, json!({ "sessionId": "s1" })) else {
        panic!("a live peer builds a call")
    };
    assert_eq!(call.peer, "guest-1");
    assert_eq!(call.args, json!({ "sessionId": "s1" }));
}

#[test]
fn a_call_for_a_peer_that_speaks_for_another_cannot_be_built() {
    // The only input that decides `RemoteCall::peer` is the admitted Peer: there is no overload that
    // takes a claimed name, so "spoofing" is a compile-time impossibility rather than a check.
    let liar = guest("guest-1");
    let Ok(call) = admitted_call(&liar, json!({})) else {
        panic!("a live peer builds a call")
    };
    assert_ne!(call.peer, "owner");
    assert_eq!(call.peer, liar.id().as_str());
}

#[test]
fn a_peer_without_interactions_may_call_but_not_open_an_event_stream() {
    let peer = guest("guest-1").with_interactions(Interactions::None);
    assert!(admitted_call(&peer, json!({})).is_ok());
    assert_eq!(
        peer.require_interactions(EVENTS_ENDPOINT)
            .expect_err("interactions=none")
            .code(),
        "gateway/interactions-forbidden"
    );
}

// ---- admission -----------------------------------------------------------------------------------

#[test]
fn the_interactions_declaration_is_parsed_or_refused_whole() {
    assert_eq!(
        Interactions::from_declarations(&[], "/connect"),
        Ok(Interactions::None),
        "an API operator defaults to none"
    );
    assert_eq!(
        Interactions::from_declarations(&["answer"], "/connect"),
        Ok(Interactions::Answer)
    );
    assert_eq!(
        Interactions::from_declarations(&["none"], "/connect"),
        Ok(Interactions::None)
    );
    for declarations in [vec!["answer", "none"], vec!["maybe"], vec![""]] {
        let error = Interactions::from_declarations(&declarations, "/connect")
            .expect_err("an illegal declaration is refused");
        assert_eq!(error.code(), "gateway/protocol");
        assert_eq!(error.endpoint(), "/connect");
    }
    assert_eq!(Interactions::default(), Interactions::Answer);
    assert!(Interactions::Answer.allows_events());
    assert!(!Interactions::None.allows_events());
}

#[test]
fn admission_checks_trust_before_the_credential() {
    let directory = PeerDirectory::new();
    // An untrusted Host with a valid cookie is still a 403: the fence protects the listener, and the
    // credential protects the identity — upstream refuses them with different statuses on purpose.
    assert_eq!(
        admitted(&directory, false, Credential::Browser).err(),
        Some(Rejection::Forbidden)
    );
    assert_eq!(
        admitted(&directory, true, Credential::None).err(),
        Some(Rejection::Unauthorized)
    );
    let peer = admitted(&directory, true, Credential::Browser)
        .unwrap_or_else(|rejection| panic!("a trusted browser is admitted, got {rejection:?}"));
    assert!(peer.principal().is_owner());
}

#[test]
fn an_unknown_api_credential_names_no_peer() {
    let directory = PeerDirectory::new();
    assert_eq!(
        admitted(&directory, true, Credential::Api("peer-404")).err(),
        Some(Rejection::Unauthorized)
    );
    assert!(directory.is_empty());
}

#[test]
fn a_published_peer_is_admitted_by_its_own_credential_only() {
    let (directory, published) = directory_with_guest();
    assert_eq!(directory.len(), 1);
    let caller = admitted(&directory, true, Credential::Api(published.id().as_str()))
        .unwrap_or_else(|rejection| panic!("the published id is admitted, got {rejection:?}"));
    assert_eq!(caller.id(), published.id());
    assert_eq!(caller.interactions(), Interactions::Answer);
    // A different id is not a wildcard for the same peer.
    assert_eq!(
        admitted(&directory, true, Credential::Api("peer-2")).err(),
        Some(Rejection::Unauthorized)
    );
    assert_eq!(
        directory.peer(published.id()).map(|peer| peer.id().clone()),
        Some(published.id().clone())
    );
}

#[test]
fn the_browser_peer_is_one_shared_owner() {
    let directory = PeerDirectory::new();
    let first = admitted(&directory, true, Credential::Browser)
        .unwrap_or_else(|rejection| panic!("browser admission, got {rejection:?}"));
    let second = admitted(&directory, true, Credential::Browser)
        .unwrap_or_else(|rejection| panic!("browser admission, got {rejection:?}"));
    assert_eq!(first.id(), second.id());
    assert_eq!(first.id(), directory.browser().id());
    assert!(
        directory.peer(first.id()).is_none(),
        "the browser peer is not published"
    );
}

#[test]
fn a_disposed_directory_refuses_every_later_admission() {
    let directory = PeerDirectory::new();
    let (directory, published) = {
        let published =
            directory.publish(Principal::named("test-guest", "api"), Interactions::None);
        (directory, published)
    };
    assert!(!directory.is_disposed());
    assert!(directory.dispose());
    assert!(!directory.dispose(), "disposal is idempotent");
    assert!(directory.is_disposed());
    assert!(!published.is_live(), "every published peer is disposed");
    for credential in [
        Credential::Browser,
        Credential::Api(published.id().as_str()),
        Credential::None,
    ] {
        assert_eq!(
            admitted(&directory, true, credential).err(),
            Some(Rejection::Unauthorized)
        );
    }
}

#[test]
fn one_socket_owns_one_peer_and_release_disposes_it() {
    let (directory, published) = directory_with_guest();
    let id = published.id().as_str().to_owned();
    let connection = connected(&directory, true, Credential::Api(&id))
        .unwrap_or_else(|rejection| panic!("expected an admission, got {rejection:?}"));
    assert_eq!(connection.peer().id(), published.id());
    // A second socket on the same Peer is a conflict, not a shared claim.
    assert_eq!(
        connected(&directory, true, Credential::Api(&id)).err(),
        Some(Rejection::Conflict)
    );
    assert_eq!(Rejection::Conflict.status(), 409);
    assert_eq!(Rejection::Forbidden.status(), 403);
    assert_eq!(Rejection::Unauthorized.status(), 401);

    assert!(connection.release());
    assert!(!connection.release(), "release is idempotent");
    assert!(!published.is_live(), "release disposes the peer");
    // Release removes the record as well as disposing the Peer, so the credential names nothing
    // afterwards. A closed socket's id is therefore not a reusable ticket.
    assert_eq!(
        connected(&directory, true, Credential::Api(&id)).err(),
        Some(Rejection::Unauthorized)
    );
}

#[test]
fn the_browser_peer_is_shared_so_a_socket_does_not_claim_it() {
    let directory = PeerDirectory::new();
    let first = directory.admit_connection(true, Credential::Browser);
    let second = directory.admit_connection(true, Credential::Browser);
    assert!(matches!(first, ConnectionOutcome::Admitted(_)));
    assert!(
        matches!(second, ConnectionOutcome::Admitted(_)),
        "no 409 for the shared browser peer"
    );
}

#[test]
fn an_untrusted_upgrade_is_rejected_before_any_peer_is_named() {
    let directory = PeerDirectory::new();
    let outcome = directory.admit_connection(false, Credential::Browser);
    assert!(matches!(
        outcome,
        ConnectionOutcome::Rejected(Rejection::Forbidden)
    ));
    assert_eq!(directory.len(), 0);
}

#[test]
fn admission_is_a_result_a_carrier_can_write_back() {
    let directory = PeerDirectory::new();
    let refused: Result<Peer, Rejection> = Admission::Rejected(Rejection::Forbidden).into();
    assert_eq!(refused.err(), Some(Rejection::Forbidden));
    assert!(admitted(&directory, true, Credential::Browser).is_ok());
}

// ---- authorisation -------------------------------------------------------------------------------

#[test]
fn the_owner_is_not_asked_about_its_own_host() {
    let peer = PeerDirectory::new().browser();
    let recorder = Recorder::default();
    assert_eq!(
        authorise(
            &peer,
            &AuthorisationRequest::destructive("session", "session.delete"),
            Some(&recorder)
        ),
        Ok(())
    );
    assert!(
        recorder.requests().is_empty(),
        "the policy must not even be consulted for the owner"
    );
    // And an owner needs no policy at all.
    assert_eq!(
        authorise(
            &peer,
            &AuthorisationRequest::read("session", "session.list"),
            None
        ),
        Ok(())
    );
}

#[test]
fn a_non_owner_with_no_policy_is_refused_rather_than_allowed() {
    // The opposite of `RemoteRegistry`'s default: a directory that admitted a second peer has already
    // left the single-local-client case behind, so "nothing configured" must mean "not authorised".
    let peer = guest("guest-1");
    let error = authorise(
        &peer,
        &AuthorisationRequest::read("session", "session.list"),
        None,
    )
    .expect_err("a non-owner needs a policy");
    assert_eq!(error.code(), "remote-access/denied");
    assert_eq!(error.endpoint(), "session.list");
    assert!(error.to_string().contains("no access policy"));
}

#[test]
fn the_policy_is_told_the_admitted_peer_the_method_and_the_resources() {
    let peer = guest("guest-1");
    let recorder = Recorder::default();
    let request = AuthorisationRequest::write("session", "session.rename");
    assert_eq!(authorise(&peer, &request, Some(&recorder)), Ok(()));
    let seen = recorder.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].peer, "guest-1");
    assert_eq!(seen[0].method, "session.rename");
    assert_eq!(seen[0].access, Access::Write);
    assert_eq!(seen[0].resources, vec![AccessResource::any_of("session")]);
}

#[test]
fn the_peer_name_the_policy_sees_cannot_be_supplied_by_the_caller() {
    // There is no path from `args` to `AccessRequest::peer`: the only source is the Peer handle, whose
    // principal was fixed at construction. This asserts the link by name, so a future overload that
    // took a string would have to change this test.
    let admitted = guest("guest-1");
    let recorder = Recorder::default();
    assert_eq!(
        authorise(
            &admitted,
            &AuthorisationRequest::read("session", "session.list"),
            Some(&recorder)
        ),
        Ok(())
    );
    assert_eq!(recorder.requests()[0].peer, "guest-1");
}

#[test]
fn an_allowlist_decides_by_the_admitted_name() {
    let peer = guest("guest-1");
    let allowed = girder_core::AllowList::new().allowing("guest-1");
    assert_eq!(
        authorise(
            &peer,
            &AuthorisationRequest::read("session", "session.list"),
            Some(&allowed)
        ),
        Ok(())
    );
    let refusal = authorise(
        &peer,
        &AuthorisationRequest::read("session", "session.list"),
        Some(&girder_core::AllowList::new()),
    )
    .expect_err("an unlisted peer is refused");
    assert!(refusal.to_string().contains("guest-1"), "{refusal}");
}

#[test]
fn a_read_only_policy_refuses_a_non_owner_write_but_allows_a_read() {
    let peer = guest("guest-1");
    let read_only = girder_core::ReadOnly;
    assert_eq!(
        authorise(
            &peer,
            &AuthorisationRequest::read("session", "session.list"),
            Some(&read_only)
        ),
        Ok(())
    );
    for request in [
        AuthorisationRequest::write("session", "session.rename"),
        AuthorisationRequest::destructive("session", "session.delete"),
    ] {
        let error = authorise(&peer, &request, Some(&read_only)).expect_err("refused");
        assert_eq!(error.code(), "remote-access/denied");
    }
}

#[test]
fn a_deny_all_policy_still_lets_the_owner_through() {
    let owner = PeerDirectory::new().browser();
    let deny_all = girder_core::DenyAll;
    assert_eq!(
        authorise(
            &owner,
            &AuthorisationRequest::read("session", "session.list"),
            Some(&deny_all)
        ),
        Ok(())
    );
    assert_eq!(
        authorise(
            &guest("guest-1"),
            &AuthorisationRequest::read("session", "session.list"),
            Some(&deny_all)
        )
        .expect_err("refused")
        .code(),
        "remote-access/denied"
    );
}

#[test]
fn disposal_is_reported_before_any_policy_decision() {
    let peer = guest("guest-1");
    peer.dispose();
    let recorder = Recorder::default();
    let error = authorise(
        &peer,
        &AuthorisationRequest::read("session", "session.list"),
        Some(&recorder),
    )
    .expect_err("a disposed peer is refused");
    assert_eq!(error.code(), "gateway/peer-disposed");
    assert!(recorder.requests().is_empty());
}

#[test]
fn an_authorisation_request_builds_each_access_level() {
    assert_eq!(
        AuthorisationRequest::read("session", "session.list").access,
        Access::Read
    );
    assert_eq!(
        AuthorisationRequest::write("session", "session.rename").access,
        Access::Write
    );
    assert_eq!(
        AuthorisationRequest::destructive("session", "session.delete").access,
        Access::Destructive
    );
}

// ---- forwarded-event streams ---------------------------------------------------------------------

#[test]
fn a_gateway_stream_reads_no_uplink_and_refuses_anything_else() {
    let streams = EventStreams::new();
    let owner = PeerDirectory::new().browser();
    for payload in [
        json!({}),
        json!({ "args": {} , "extra": 1 }),
        json!({ "args": { "sessionId": "s1" } }),
        json!("args"),
        json!(null),
    ] {
        let error = streams
            .open(&owner, EVENTS_ENDPOINT, &payload)
            .expect_err("a malformed stream payload is refused");
        assert_eq!(error.code(), "gateway/arguments-invalid", "{payload}");
    }
    assert!(streams.is_empty());
}

#[test]
fn an_owner_opens_an_answering_stream_and_a_guest_opens_a_subscription() {
    let streams = EventStreams::new();
    let owner = PeerDirectory::new().browser();
    let owner_stream: OpenedStream = streams
        .open(&owner, EVENTS_ENDPOINT, &json!({ "args": {} }))
        .expect("the owner opens a stream");
    assert!(owner_stream.answers);
    assert_eq!(streams.answers(&owner_stream.client_id), Some(true));

    let subscriber = guest("guest-1");
    let guest_stream = streams
        .open(&subscriber, EVENTS_ENDPOINT, &json!({ "args": {} }))
        .expect("a guest opens a stream");
    assert!(
        !guest_stream.answers,
        "a non-owner never answers a waterfall"
    );
    assert_ne!(guest_stream.client_id, owner_stream.client_id);
    assert_eq!(streams.len(), 2);
}

#[test]
fn each_generation_is_owned_by_the_peer_that_opened_it() {
    let streams = EventStreams::new();
    let first = guest("guest-1");
    let second = guest("guest-2");
    let opened = streams
        .open(&first, EVENTS_ENDPOINT, &json!({ "args": {} }))
        .expect("a stream");
    assert_eq!(
        streams.owner_of(&opened.client_id),
        Some(first.id().clone())
    );
    assert_eq!(
        streams.assert_answers(&first, &opened.client_id, EVENTS_RESULT_ENDPOINT),
        Err(GatewayError::InvocationUnavailable {
            endpoint: EVENTS_RESULT_ENDPOINT.to_owned(),
        }),
        "a guest may not answer even its own generation"
    );
    assert_eq!(
        streams
            .assert_answers(&second, &opened.client_id, EVENTS_RESULT_ENDPOINT)
            .expect_err("another peer's generation"),
        GatewayError::InvocationUnavailable {
            endpoint: EVENTS_RESULT_ENDPOINT.to_owned(),
        }
    );
}

#[test]
fn only_the_owner_answers_and_only_for_its_own_generation() {
    let streams = EventStreams::new();
    let directory = PeerDirectory::new();
    let owner = directory.browser();
    let other_owner = directory.publish(Principal::owner(), Interactions::Answer);
    let opened: OpenedStream = streams
        .open(&owner, EVENTS_ENDPOINT, &json!({ "args": {} }))
        .expect("the owner opens a stream");
    assert_eq!(
        streams.assert_answers(&owner, &opened.client_id, EVENTS_RESULT_ENDPOINT),
        Ok(true)
    );
    // A second owner Peer is still a different Peer: a generation belongs to the one that opened it.
    assert_eq!(
        streams
            .assert_answers(&other_owner, &opened.client_id, EVENTS_RESULT_ENDPOINT)
            .expect_err("another peer's generation is not answerable"),
        GatewayError::InteractionsForbidden {
            endpoint: EVENTS_RESULT_ENDPOINT.to_owned(),
        }
    );
}

#[test]
fn a_result_naming_no_active_stream_is_a_protocol_failure() {
    let streams = EventStreams::new();
    let owner = PeerDirectory::new().browser();
    let error = streams
        .assert_answers(&owner, "stream-404", EVENTS_RESULT_ENDPOINT)
        .expect_err("no such generation");
    assert_eq!(error.code(), "gateway/protocol");
    assert!(error.to_string().contains("no active event stream"));
    assert_eq!(streams.owner_of("stream-404"), None);
    assert_eq!(streams.answers("stream-404"), None);
}

#[test]
fn a_passive_peer_opens_no_stream_even_as_owner() {
    let streams = EventStreams::new();
    let owner = PeerDirectory::new()
        .browser()
        .with_interactions(Interactions::None);
    let error = streams
        .open(&owner, EVENTS_ENDPOINT, &json!({ "args": {} }))
        .expect_err("interactions=none");
    assert_eq!(error.code(), "gateway/interactions-forbidden");
    assert!(streams.is_empty());
}

#[test]
fn a_disposed_peer_opening_a_stream_is_refused_before_the_payload() {
    let streams = EventStreams::new();
    let owner = PeerDirectory::new().browser();
    owner.dispose();
    let error = streams
        .open(&owner, EVENTS_ENDPOINT, &json!("malformed"))
        .expect_err("a disposed peer is refused first");
    assert_eq!(error.code(), "gateway/peer-disposed");
}

#[test]
fn revoking_a_peer_drops_only_its_generations() {
    // Upstream discards the events buffered for a revoked non-owner Peer and keeps other streams.
    let streams = EventStreams::new();
    let revoked = guest("guest-1");
    let kept = guest("guest-2");
    let first = streams
        .open(&revoked, EVENTS_ENDPOINT, &json!({ "args": {} }))
        .expect("a stream");
    let second = streams
        .open(&kept, EVENTS_ENDPOINT, &json!({ "args": {} }))
        .expect("a stream");
    assert_eq!(streams.len(), 2);
    assert_eq!(streams.revoke(revoked.id()), 1);
    assert_eq!(streams.len(), 1);
    assert_eq!(streams.owner_of(&first.client_id), None);
    assert_eq!(streams.owner_of(&second.client_id), Some(kept.id().clone()));
    assert_eq!(
        streams.revoke(revoked.id()),
        0,
        "revoking twice drops nothing"
    );
}

// ---- the end-to-end property this module exists for -----------------------------------------------

#[test]
fn a_wire_claim_cannot_become_an_authorised_call() {
    // The whole gap in one test: everything a caller controls — a resident peer string, a claimed
    // principal, an `interactions` declaration — either has no path to the decision or is refused.
    let directory = PeerDirectory::new();
    let guest_peer = directory.publish(Principal::named("test-guest", "web"), Interactions::Answer);

    // 1. The wire field `peer` is never an input to admission.
    let claimed: Value = json!({ "peer": "browser", "principal": { "kind": "owner" } });
    assert!(assert_no_identity_fields(EVENTS_RESULT_ENDPOINT, &claimed).is_err());

    // 2. Admission is by credential, and yields the published, non-owner Peer.
    let caller = admitted(&directory, true, Credential::Api(guest_peer.id().as_str()))
        .unwrap_or_else(|rejection| panic!("the published credential, got {rejection:?}"));
    assert!(!caller.principal().is_owner());

    // 3. The call's peer is that Peer's identity, not the claimed one.
    let Ok(call) = admitted_call(&caller, json!({ "sessionId": "s1" })) else {
        panic!("a live peer builds a call")
    };
    assert_eq!(call.peer, guest_peer.id().as_str());
    assert_ne!(call.peer, "browser");

    // 4. A policy that only allows the owner refuses it, and the reason names the real peer.
    let owner_only = girder_core::AllowList::new().allowing("browser");
    let error = authorise(
        &caller,
        &AuthorisationRequest::destructive("session", "session.delete"),
        Some(&owner_only),
    )
    .expect_err("a guest cannot delete");
    assert_eq!(error.code(), "remote-access/denied");
    assert!(error.to_string().contains(call.peer.as_str()), "{error}");

    // 5. The owner is unaffected by the guest's refusal.
    let owner = admitted(&directory, true, Credential::Browser)
        .unwrap_or_else(|rejection| panic!("owner admission, got {rejection:?}"));
    assert_eq!(
        authorise(
            &owner,
            &AuthorisationRequest::destructive("session", "session.delete"),
            Some(&owner_only)
        ),
        Ok(())
    );
}
