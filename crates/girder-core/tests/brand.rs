//! Tests for branded identifiers (M1 of the v2 plan).
//!
//! The property that matters is a negative one: two identifier families must be impossible to
//! confuse. A test can demonstrate that at compile time only by *not compiling*, so what is checked
//! here is everything the compiler cannot: validation, serialization shape, and the conversions
//! that deliberately drop the brand.

use girder_core::{
    AgentId, Brand, Id, IdError, MAX_ID_LEN, MessageId, SessionId, ToolCallId, WorkspaceId,
};

#[test]
fn an_identifier_is_built_from_a_valid_string() {
    let id = SessionId::new("abc-123").expect("valid");
    assert_eq!(id.as_str(), "abc-123");
    assert_eq!(id.to_string(), "abc-123");
}

#[test]
fn an_empty_identifier_is_refused() {
    assert_eq!(SessionId::new(""), Err(IdError::Empty));
}

#[test]
fn an_identifier_above_the_limit_is_refused() {
    let too_long = "a".repeat(MAX_ID_LEN + 1);
    let error = SessionId::new(too_long).expect_err("overflow");
    assert!(matches!(error, IdError::TooLong { .. }));
}

#[test]
fn an_identifier_at_the_limit_is_accepted() {
    // The boundary itself must be allowed, or the limit is off by one.
    let exact = "a".repeat(MAX_ID_LEN);
    assert!(SessionId::new(exact).is_ok());
}

#[test]
fn base64_padding_is_refused() {
    // base64url omits `=`, so a padded token is not an id upstream would ever mint; accepting it
    // would let two spellings of one token both round-trip.
    assert!(SessionId::new("a+b=").is_err());
}

#[test]
fn a_character_outside_the_alphabet_is_refused() {
    // An id goes into log lines and file names unescaped; whitespace and quotes must not survive.
    for bad in [
        "has space",
        "has\tnewline",
        "quote\"inside",
        "semi;colon",
        "back\\slash",
    ] {
        let error = SessionId::new(bad).expect_err("must be refused");
        assert!(matches!(error, IdError::InvalidCharacter { .. }), "{bad:?}");
    }
}

#[test]
fn the_allowed_alphabet_matches_upstream_id_shapes() {
    // Alphanumerics plus the separators upstream ids actually use: an agent id is `parent/child`,
    // a scoped key carries `:`, and base64url tokens carry `-` and `_` (never `=`: upstream uses
    // `toString('base64url')`, which omits padding).
    for good in [
        "abc",
        "ABC123",
        "a-b_c.d",
        "parent/child",
        "scope:key",
        "a+b",
        "0123456789abcdef",
    ] {
        assert!(SessionId::new(good).is_ok(), "{good:?} must be accepted");
    }
}

#[test]
fn two_identifiers_of_the_same_family_compare_by_value() {
    let a = SessionId::new("x").unwrap();
    let b = SessionId::new("x").unwrap();
    assert_eq!(a, b);
    assert_eq!(a.cmp(&b), std::cmp::Ordering::Equal);
}

#[test]
fn identifiers_sort_and_hash_like_their_strings() {
    // A brand is a type-level wrapper, not an ordering change: a set keyed by id behaves as it did
    // with plain strings.
    let ids: std::collections::BTreeSet<SessionId> = ["b", "a", "c"]
        .iter()
        .map(|s| SessionId::new(*s).unwrap())
        .collect();
    let ordered: Vec<&str> = ids.iter().map(SessionId::as_str).collect();
    assert_eq!(ordered, vec!["a", "b", "c"]);

    let mut hashed = std::collections::HashSet::new();
    hashed.insert(SessionId::new("dup").unwrap());
    hashed.insert(SessionId::new("dup").unwrap());
    assert_eq!(hashed.len(), 1, "equal ids are one entry");
}

#[test]
fn an_identifier_serializes_as_its_inner_string() {
    // The wire format must not change when a field is branded, or branding would be a migration.
    let id = SessionId::new("abc").unwrap();
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, "\"abc\"");
}

#[test]
fn deserializing_validates() {
    // A malformed id must not enter through JSON, where no constructor ran.
    let ok: SessionId = serde_json::from_str("\"abc\"").unwrap();
    assert_eq!(ok.as_str(), "abc");

    let error = serde_json::from_str::<SessionId>("\"has space\"")
        .expect_err("whitespace must be refused on the wire too");
    assert!(error.to_string().contains("not allowed"), "{error}");
}

#[test]
fn the_brand_survives_a_round_trip_through_json() {
    let original = ToolCallId::new("call-1").unwrap();
    let text = serde_json::to_string(&original).unwrap();
    let restored: ToolCallId = serde_json::from_str(&text).unwrap();
    assert_eq!(original, restored);
}

#[test]
fn into_string_is_the_only_lossy_conversion() {
    // Explicit because it is the one operation that loses the brand; a `Deref<str>` would make an
    // accidental round trip through `&str` too easy.
    let id = AgentId::new("agent-1").unwrap();
    let raw: String = id.into_string();
    assert_eq!(raw, "agent-1");
}

#[test]
fn debug_names_the_domain() {
    // A bare string in a debug log does not say which kind of id it was, which is the confusion the
    // brand exists to prevent.
    let id = SessionId::new("abc").unwrap();
    let shown = format!("{id:?}");
    assert!(shown.contains("session"), "{shown}");
    assert!(shown.contains("abc"), "{shown}");
}

#[test]
fn each_family_reports_its_own_domain() {
    assert_eq!(SessionId::new("a").unwrap().domain(), "session");
    assert_eq!(ToolCallId::new("a").unwrap().domain(), "tool-call");
    assert_eq!(AgentId::new("a").unwrap().domain(), "agent");
    assert_eq!(WorkspaceId::new("a").unwrap().domain(), "workspace");
    assert_eq!(MessageId::new("a").unwrap().domain(), "message");
}

#[test]
fn a_custom_family_can_be_declared_outside_the_crate() {
    // The mechanism must be open: M3-M6 add ~80 more ids, and each must be declarable where its
    // domain lives.
    struct JobId;

    impl Brand for JobId {
        const DOMAIN: &'static str = "job";
    }

    let id: Id<JobId> = Id::new("job-1").unwrap();
    assert_eq!(id.domain(), "job");
    assert_eq!(id.as_str(), "job-1");
    assert!(format!("{id:?}").contains("job"));
}
