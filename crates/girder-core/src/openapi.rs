//! `OpenAPI` 3.1 emission for the Remote type catalogue.
//!
//! The catalogue already describes every Remote method and every named type, and
//! [`girder_core::typert_json`] already renders it as JSON Schema. What neither of them does is
//! describe the methods as an *HTTP surface*, which is the shape an external tool wants: a code
//! generator, a gateway, an API console and a function-calling client all read `OpenAPI` and none
//! of them read a bespoke `x-girder-methods` block.
//!
//! Upstream generates no `OpenAPI` at all. `packages/typert/generator` emits a `zod` client
//! (`emitter.ts`, `renderer.ts`) and a lossy tool-schema subset (`tool-schema.ts`), and the only
//! `openapi-3.0` strings in the upstream tree are a `zod-to-json-schema` target switch inside a
//! vendored registry bundle. So this is not a port of a missing upstream file; it is a new output
//! that exists because "call this RPC from outside the harness" otherwise has no contract to read.
//!
//! # Why 3.1 and not 3.0
//!
//! The catalogue records absence as a *type union*: [`girder_core::typert_json`] renders
//! `Option<T>` as `"type": ["string", "null"]`, because that is how draft 2020-12 spells a nullable
//! value. That keyword does not exist in `OpenAPI` 3.0, whose Schema Object is a fixed dialect that
//! admits a single string in `type` and spells nullability as `"nullable": true`. Publishing a 3.0
//! document with an array in `type` produces a document a 3.0 validator rejects and a 3.0 client
//! generator silently mis-reads — the worst of both outcomes.
//!
//! 3.1 replaced the fixed Schema Object with "JSON Schema, with a few dialect additions", so a
//! 2020-12 schema can be embedded unchanged. That is the whole reason for the version choice:
//! 3.1 is the first `OpenAPI` release where the schemas this crate already produces are valid
//! as written. [`JSON_SCHEMA_DIALECT`] names the dialect explicitly rather than relying on the
//! `OpenAPI` base dialect, which adds keywords (`discriminator`, `xml`) this emitter never uses.
//!
//! # One path per method
//!
//! `session.list` becomes `POST /session/list`. `POST` rather than `GET` even for a method that
//! reads: arguments are a JSON object, and a `GET` would have to flatten them into query
//! parameters, which loses nested shapes, arrays and `null`. RPC over `POST` keeps the body
//! identical to what the method actually receives, so the generated client and the real transport
//! cannot disagree about argument encoding.
//!
//! # The paths are logical, not the wire
//!
//! This build serves every method through one route — `POST /api/remote.invoke` with an envelope of
//! `{namespace, method, args, peer}` — so no `/session/list` route exists today. Emitting the
//! envelope as the only path would describe one operation and hide the other twenty; emitting one
//! path per method describes what a caller can *do* and leaves addressing to the transport. The
//! envelope route is published as [`INVOKE_PATH_EXTENSION`] so a client that has to speak to this
//! server as it is can find it, instead of inferring it from a document that never mentions it.
//!
//! # Why the schemas are not re-derived here
//!
//! Two emitters for one type model drift, and the drift shows up as a client that validates in one
//! and not the other. This module therefore renders the catalogue through
//! [`catalogue_to_json_schema`] and only re-homes the result: `$defs` becomes
//! `components.schemas` and every `#/$defs/…` reference is rewritten to `#/components/schemas/…`.
//! The JSON Schema semantics — nullability, presence, recursion, pointer escaping — stay in one
//! place, and this module adds the three fields `OpenAPI` requires on top of a schema bundle:
//! `openapi`, `info` and `paths`.
//!
//! A reference the source bundle could not resolve is carried across as
//! [`UNRESOLVED_EXTENSION`]. Dropping it would leave a `$ref` that resolves to nothing, which
//! validates as "anything" and is therefore the most expensive kind of silent failure.

use serde_json::{Map, Value, json};

use girder_core::{Catalogue, Method, catalogue_to_json_schema};

/// The `OpenAPI` version this module emits.
///
/// 3.1 rather than 3.0 because 3.1 embeds draft 2020-12 schemas unchanged; see the module docs for
/// what 3.0 would do to a `"type": [..., "null"]` union.
const OPENAPI_VERSION: &str = "3.1.0";

/// The JSON Schema dialect the embedded schemas are written in.
///
/// Named explicitly so a consumer does not apply the `OpenAPI` base dialect, which is 2020-12 plus
/// keywords this emitter never produces.
const JSON_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// The prefix a reference has inside the source schema bundle.
const DEFS_PREFIX: &str = "#/$defs/";

/// The prefix a reference has once the definitions live under `components.schemas`.
const SCHEMAS_PREFIX: &str = "#/components/schemas/";

/// The route that accepts an RPC envelope, published as a document extension.
///
/// A literal rather than an import: `girder-rpc` depends on this crate, so the constant cannot be
/// borrowed back from it. The value is the one `girder_rpc::INVOKE_PATH` declares; if that route
/// moves, this hint has to move with it, which is why it is a single named constant instead of a
/// string inlined at the use site.
const INVOKE_PATH: &str = "/api/remote.invoke";

/// The extension keyword naming [`INVOKE_PATH`].
const INVOKE_PATH_EXTENSION: &str = "x-girder-invoke-path";

/// The extension keyword naming the wire revision the document describes.
const REVISION_EXTENSION: &str = "x-girder-revision";

/// The extension keyword carrying references the catalogue could not resolve.
const UNRESOLVED_EXTENSION: &str = "x-girder-unresolved-refs";

/// The key under `x-girder-methods` that holds a method's request body.
const INPUT_FIELD: &str = "input";

/// The key under `x-girder-methods` that holds a method's return schema.
const OUTPUT_FIELD: &str = "output";

/// Documentation metadata for the generated document.
///
/// `version` is the *API* version the caller chooses to advertise, not the catalogue revision:
/// `OpenAPI` requires a version string in `info`, and a client re-generates against a new document
/// far more often than the wire revision changes. The revision travels separately in
/// [`REVISION_EXTENSION`], so the two facts never have to be encoded into one string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    /// What the API is called, shown as the document title.
    pub title: String,
    /// The API version a caller advertises.
    pub version: String,
    /// An optional longer description of the API.
    pub description: Option<String>,
}

impl Info {
    /// Builds metadata with a title and a version.
    #[must_use]
    pub fn new(title: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            version: version.into(),
            description: None,
        }
    }

    /// Adds a description, which `OpenAPI` renders as the document's overview.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Renders a catalogue as an `OpenAPI` 3.1 document.
///
/// One `POST` operation per method at `/{namespace}/{method}`, the named types under
/// `components.schemas`, and one request-body schema per method beside them. An empty catalogue
/// produces a document with an empty `paths` map rather than no `paths` key: a consumer should not
/// have to distinguish "no methods" from "a document that forgot to describe them".
#[must_use]
pub fn document(catalogue: &Catalogue, info: Info) -> Value {
    // The single source of schema truth. Everything below re-homes this bundle; nothing re-derives
    // a schema from the type model, so a fix to nullability or presence lands in both documents.
    let bundle = catalogue_to_json_schema(catalogue);

    let mut schemas = Map::new();
    hoist_definitions(&bundle, &mut schemas);

    let mut paths = Map::new();
    // The bundle emits one entry per method, in declaration order, so the two sequences line up.
    // Pairing them this way keeps `Method`'s fields (namespace, name, docs) available, which the
    // emitted entry only carries pre-joined into one qualified name.
    for (method, entry) in catalogue.methods.iter().zip(methods_of(&bundle)) {
        let qualified = method.qualified();
        let request = request_schema(&qualified, entry, &mut schemas);
        let response = entry
            .get(OUTPUT_FIELD)
            .map_or_else(|| json!({}), |schema| rewrite_refs(schema.clone()));

        let operation = operation(&qualified, method, &request, &response);
        // A map, not a `Vec` of pairs: a catalogue that names one method twice would otherwise emit
        // one key twice, and a repeated key in a JSON object is either a parse error or a silent
        // last-wins. Last-wins is the choice here, matching `RemoteRegistry::register`, which
        // overwrites; making it explicit keeps the two from disagreeing.
        paths.insert(operation_path(&method.namespace, &method.name), operation);
    }

    let mut components = Map::new();
    components.insert("schemas".to_owned(), Value::Object(schemas));

    let mut document = Map::new();
    document.insert("openapi".to_owned(), json!(OPENAPI_VERSION));
    document.insert("info".to_owned(), info_json(info));
    document.insert("jsonSchemaDialect".to_owned(), json!(JSON_SCHEMA_DIALECT));
    document.insert("paths".to_owned(), Value::Object(paths));
    document.insert("components".to_owned(), Value::Object(components));
    document.insert(INVOKE_PATH_EXTENSION.to_owned(), json!(INVOKE_PATH));

    // Carried across from the bundle rather than recomputed: the set of unresolved names is exactly
    // what the schema emitter already decided, and a second opinion could only disagree with it.
    for extension in [REVISION_EXTENSION, UNRESOLVED_EXTENSION] {
        if let Some(value) = bundle.get(extension) {
            document.insert(extension.to_owned(), value.clone());
        }
    }

    Value::Object(document)
}

/// Renders the `info` object.
fn info_json(info: Info) -> Value {
    let mut object = Map::new();
    object.insert("title".to_owned(), json!(info.title));
    object.insert("version".to_owned(), json!(info.version));
    if let Some(description) = info.description {
        object.insert("description".to_owned(), json!(description));
    }
    Value::Object(object)
}

/// Copies the bundle's named types into `components.schemas`.
///
/// Every defined type is copied, including one no method mentions: a type is part of the contract
/// whether or not the current method set exercises it, and a client that has to construct one needs
/// its definition.
fn hoist_definitions(bundle: &Value, schemas: &mut Map<String, Value>) {
    let Some(Value::Object(definitions)) = bundle.get("$defs") else {
        return;
    };
    for (name, schema) in definitions {
        // Definitions refer to each other, so the rewrite has to run over each one too; a `$ref`
        // that keeps pointing at `$defs` resolves to nothing once the block has moved.
        schemas.insert(name.clone(), rewrite_refs(schema.clone()));
    }
}

/// Returns the bundle's method entries.
///
/// An absent list means an empty one: a catalogue with no methods is a valid document, and the
/// emitter omits nothing it does not need to.
fn methods_of(bundle: &Value) -> Vec<&Value> {
    bundle
        .get("x-girder-methods")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |entries| entries.iter().collect())
}

/// Installs a method's request body under `components.schemas` and returns a reference to it.
///
/// The body is a component rather than an inline schema because a generator that sees a named
/// request type can emit one model class for it, and because a `$ref` is what lets a caller confirm
/// the body it read is the body the document meant.
fn request_schema(qualified: &str, entry: &Value, schemas: &mut Map<String, Value>) -> Value {
    // A `.` cannot appear in a type name, so this name cannot collide with a defined type. Using
    // the qualified method name keeps the component recognisable in a generated client.
    let name = format!("{qualified}.Input");
    let body = entry
        .get(INPUT_FIELD)
        .map_or_else(empty_object_schema, |schema| rewrite_refs(schema.clone()));
    schemas.insert(name.clone(), body);
    json!({ "$ref": format!("{SCHEMAS_PREFIX}{}", escape_pointer(&name)) })
}

/// Renders one path item.
fn operation(qualified: &str, method: &Method, request: &Value, response: &Value) -> Value {
    let mut operation = Map::new();
    operation.insert("operationId".to_owned(), json!(qualified));
    // Grouping by namespace is what turns the generated console into a readable index; it is an
    // annotation for tooling and carries no wire meaning.
    operation.insert("tags".to_owned(), json!([method.namespace]));
    if let Some(docs) = &method.docs {
        operation.insert("description".to_owned(), json!(docs));
    }

    let mut body = Map::new();
    // A body is required exactly when some parameter is; a method whose parameters are all
    // optional accepts an absent body, and the server then decodes the same defaults it would
    // apply to `{}`. Marking every body required would make a generator emit `{}` for a method
    // that takes nothing at all.
    body.insert(
        "required".to_owned(),
        json!(method.parameters.iter().any(|parameter| parameter.required)),
    );
    body.insert(
        "content".to_owned(),
        json!({ "application/json": { "schema": request } }),
    );
    operation.insert("requestBody".to_owned(), Value::Object(body));

    let mut responses = Map::new();
    responses.insert(
        "200".to_owned(),
        json!({
            "description": format!("The value `{qualified}` returned."),
            "content": { "application/json": { "schema": response } },
        }),
    );
    // No schema: a failure is projected by the transport into a `{code, message}` object whose
    // codes grow with the method set, and a schema that listed today's codes would reject
    // tomorrow's. The description is what a caller can rely on without being lied to.
    responses.insert(
        "default".to_owned(),
        json!({ "description": "The call failed; the body carries a stable error `code`." }),
    );
    operation.insert("responses".to_owned(), Value::Object(responses));

    json!({ "post": Value::Object(operation) })
}

/// Builds the path a method is published at.
///
/// `session.list` becomes `/session/list`: the dotted call name is how every client refers to the
/// method, and turning the separator into a path segment gives an address that is unique without
/// a lookup table and readable in a request log.
fn operation_path(namespace: &str, name: &str) -> String {
    let mut path = String::with_capacity(namespace.len() + name.len() + 2);
    // Trimmed so a namespace that arrives with stray separators cannot produce `//session`.
    let namespace = namespace.trim_matches('/');
    if !namespace.is_empty() {
        path.push('/');
        path.push_str(namespace);
    }
    let name = name.trim_matches('/');
    if !name.is_empty() {
        path.push('/');
        path.push_str(name);
    }
    // A method with an empty namespace and name would otherwise render as the empty string, which
    // is not a path at all.
    if path.is_empty() {
        path.push('/');
    }
    path
}

/// The schema for a body that carries no arguments.
fn empty_object_schema() -> Value {
    json!({ "type": "object", "properties": {}, "required": [] })
}

/// Rewrites every `#/$defs/…` reference in a value to point at `components.schemas`.
///
/// Recursive because a definition may contain one: a reference to a named type survives the move
/// only if the reference inside its *own* definition is rewritten as well.
fn rewrite_refs(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut rewritten = Map::with_capacity(object.len());
            for (key, value) in object {
                // `$ref` is a string, so it is rewritten rather than walked into; every other key
                // holds a schema or an annotation, both of which may themselves contain a
                // reference.
                let value = if key == "$ref" {
                    rewrite_reference(value)
                } else {
                    rewrite_refs(value)
                };
                rewritten.insert(key, value);
            }
            Value::Object(rewritten)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(rewrite_refs).collect()),
        other => other,
    }
}

/// Rewrites one reference string, leaving anything that is not a local definition alone.
///
/// Only the prefix changes: the segment after it is a JSON Pointer and already escaped, so copying
/// it verbatim keeps pointer escaping correct for a name that contains `/` or `~`.
fn rewrite_reference(value: Value) -> Value {
    let rewritten = value
        .as_str()
        .and_then(|reference| reference.strip_prefix(DEFS_PREFIX))
        .map(|pointer| format!("{SCHEMAS_PREFIX}{pointer}"));
    match rewritten {
        Some(reference) => Value::String(reference),
        None => value,
    }
}

/// Escapes a name for use as a JSON Pointer segment.
///
/// A component name containing `/` would otherwise address a different path entirely, and the
/// `$ref` would parse and resolve to nothing.
fn escape_pointer(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}
