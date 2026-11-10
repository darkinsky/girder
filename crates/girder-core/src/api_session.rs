//! The session Remote surface: request and response types, and the catalogue they produce.
//!
//! This is the first `api` subsystem. Two things are being established here rather than one:
//!
//! 1. The wire shapes a browser client sends and receives for a session.
//! 2. That the mechanism built for them — `#[derive(Typert)]` for types, `#[remote]` for methods,
//!    [`crate::typert::to_typescript`] for the client — produces something coherent end to end.
//!
//! # Why the request types are deliberately narrow
//!
//! Every field here exists because a client sends it. A request type that mirrored a server-side
//! structure would leak fields the client must not set, and a client that set one would be able to
//! change something the method never intended to expose. The wire type is a contract, not a view.
//!
//! # Why `SessionId` is a plain string on the wire
//!
//! A branded identifier is a Rust-side guarantee that two id families cannot be confused. On the
//! wire it is a string, and the schema says `string` rather than pretending the brand survives JSON.
//! The guarantee is enforced where it can be — at the boundary — and not claimed where it cannot.

use crate::typert::{Catalogue, Method, ObjectSchema, Parameter, Schema};

/// Requests one session's events, newest last.
#[derive(Debug, Clone, crate::Typert)]
pub struct SessionPageRequest {
    /// The session to page.
    pub session_id: String,
    /// The maximum number of events to return.
    pub limit: u64,
    /// Continue after this sequence number; omitted starts from the first event.
    pub after_seq: Option<u64>,
}

/// One page of a session's events.
#[derive(Debug, Clone, crate::Typert)]
pub struct SessionPageValue {
    /// The events, in sequence order.
    pub events: Vec<serde_json::Value>,
    /// Whether more events exist past this page.
    pub has_more: bool,
}

/// Requests one session's folded projections.
#[derive(Debug, Clone, crate::Typert)]
pub struct SessionProjectionsRequest {
    /// The session.
    pub session_id: String,
}

/// The projections a client reads to render a session.
///
/// A map rather than one field per projection: a projection is registered by name, and a request type
/// with a field per projection would need changing every time one is added.
#[derive(Debug, Clone, crate::Typert)]
pub struct SessionProjectionsValue {
    /// Projection state by key.
    pub projections: serde_json::Map<String, serde_json::Value>,
}

/// Asks to cancel a session's live turn.
#[derive(Debug, Clone, crate::Typert)]
pub struct SessionCancelRequest {
    /// The session.
    pub session_id: String,
    /// Whether queued input survives the cancellation.
    ///
    /// Defaults to keeping it: an interrupted user usually wants what they typed to remain, and a
    /// discard is the deliberate choice.
    pub keep_inbox: Option<bool>,
}

/// The receipt after a cancellation is admitted.
#[derive(Debug, Clone, crate::Typert)]
pub struct SessionCancelValue {
    /// Always true: a refusal is an error, not a `false`.
    ///
    /// A boolean that could be false would make every client check it, and a client that did not
    /// would treat a refusal as success.
    pub accepted: bool,
}

/// Asks to rename a session.
#[derive(Debug, Clone, crate::Typert)]
pub struct SessionRenameRequest {
    /// The session.
    pub session_id: String,
    /// The new title.
    pub title: String,
}

/// The result of a rename.
#[derive(Debug, Clone, crate::Typert)]
pub struct SessionRenameValue {
    /// The title as stored, after trimming.
    pub title: String,
}

/// Asks to create a session.
#[derive(Debug, Clone, crate::Typert)]
pub struct SessionCreateRequest {
    /// The working directory the session runs in; omitted inherits the host's.
    pub cwd: Option<String>,
}

/// The new session's identity and starting state.
#[derive(Debug, Clone, crate::Typert)]
pub struct SessionCreateValue {
    /// Its id.
    pub session_id: String,
    /// Its title, empty until one is derived.
    pub title: String,
}

/// A model a client may select.
#[derive(Debug, Clone, crate::Typert)]
pub struct ModelChoice {
    /// The provider route.
    pub provider: String,
    /// The model id.
    pub model: String,
    /// The context window, when the catalogue declares one.
    pub context_window: Option<u64>,
}

/// The models available to this deployment.
#[derive(Debug, Clone, crate::Typert)]
pub struct ModelCatalogue {
    /// The choices, in a stable order.
    pub models: Vec<ModelChoice>,
}

/// The session subsystem's method descriptors.
///
/// A function rather than a constant because a `Method` is built at runtime from the derive output;
/// a constant would need a `const fn` constructor that the schema types do not have.
#[must_use]
pub fn session_methods() -> Vec<Method> {
    vec![
        Method::new("session", "create")
            .with_parameter(Parameter::optional(
                "cwd",
                Schema::Optional(Box::new(Schema::String)),
            ))
            .returning(Schema::Ref("SessionCreateValue".to_owned()))
            .with_docs("Opens a new session."),
        Method::new("session", "page")
            .with_parameter(Parameter::required("sessionId", Schema::String))
            .with_parameter(Parameter::required("limit", Schema::Int64))
            .with_parameter(Parameter::optional(
                "afterSeq",
                Schema::Optional(Box::new(Schema::Int64)),
            ))
            .returning(Schema::Ref("SessionPageValue".to_owned()))
            .with_docs("Pages one session's events."),
        Method::new("session", "projections")
            .with_parameter(Parameter::required("sessionId", Schema::String))
            .returning(Schema::Ref("SessionProjectionsValue".to_owned()))
            .with_docs("Reads a session's folded projections."),
        Method::new("session", "cancel")
            .with_parameter(Parameter::required("sessionId", Schema::String))
            .with_parameter(Parameter::optional(
                "keepInbox",
                Schema::Optional(Box::new(Schema::Bool)),
            ))
            .returning(Schema::Ref("SessionCancelValue".to_owned()))
            .with_docs("Cancels a session's live turn."),
        Method::new("session", "rename")
            .with_parameter(Parameter::required("sessionId", Schema::String))
            .with_parameter(Parameter::required("title", Schema::String))
            .returning(Schema::Ref("SessionRenameValue".to_owned()))
            .with_docs("Renames a session."),
        Method::new("session", "modelCatalog")
            .returning(Schema::Ref("ModelCatalogue".to_owned()))
            .with_docs("Lists the models this deployment serves."),
    ]
}

/// The session subsystem's catalogue, at wire revision 1.
///
/// The catalogue is what the TypeScript generator consumes, so building it here is what makes the
/// whole chain testable without a running server.
#[must_use]
pub fn session_catalogue() -> Catalogue {
    let mut catalogue = Catalogue::new(1);

    catalogue.define(choose_name(
        SessionPageRequest::typert_schema(),
        "SessionPageRequest",
    ));
    catalogue.define(choose_name(
        SessionPageValue::typert_schema(),
        "SessionPageValue",
    ));
    catalogue.define(choose_name(
        SessionProjectionsRequest::typert_schema(),
        "SessionProjectionsRequest",
    ));
    catalogue.define(choose_name(
        SessionProjectionsValue::typert_schema(),
        "SessionProjectionsValue",
    ));
    catalogue.define(choose_name(
        SessionCancelRequest::typert_schema(),
        "SessionCancelRequest",
    ));
    catalogue.define(choose_name(
        SessionCancelValue::typert_schema(),
        "SessionCancelValue",
    ));
    catalogue.define(choose_name(
        SessionRenameRequest::typert_schema(),
        "SessionRenameRequest",
    ));
    catalogue.define(choose_name(
        SessionRenameValue::typert_schema(),
        "SessionRenameValue",
    ));
    catalogue.define(choose_name(
        SessionCreateRequest::typert_schema(),
        "SessionCreateRequest",
    ));
    catalogue.define(choose_name(
        SessionCreateValue::typert_schema(),
        "SessionCreateValue",
    ));
    catalogue.define(choose_name(ModelChoice::typert_schema(), "ModelChoice"));
    catalogue.define(choose_name(
        ModelCatalogue::typert_schema(),
        "ModelCatalogue",
    ));

    for method in session_methods() {
        catalogue.add(method);
    }
    catalogue
}

/// The name a schema is filed under.
///
/// The derive already produces the Rust name, so this exists only to turn a mismatch into a
/// visible failure rather than a silently wrong `$ref` target: a client resolving
/// `#/$defs/Foo` against a catalogue that filed it as `Bar` fails at runtime with nothing pointing
/// here.
fn choose_name(mut schema: ObjectSchema, expected: &str) -> ObjectSchema {
    assert_eq!(
        schema.name, expected,
        "the derive and the reference name must agree, or every $ref to it is wrong"
    );
    schema.name.clear();
    schema.name.push_str(expected);
    schema
}
