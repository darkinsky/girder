//! Complete `OpenAPI` 3.1 emission, generated from the live [`girder_core::RemoteRegistry`].
//!
//! [`crate::openapi_document`] already emits `OpenAPI`, but it is driven by a
//! [`girder_core::Catalogue`], and the only catalogues this build *has* are the three hand-written
//! `api_*` modules (`api_session`, `api_config`, `api_job`). A running server registers far more
//! than those three: the CLI's `serve` path also mounts projection, job, terminal, workspace,
//! filesystem, settings, schedule, lifecycle, queue, host and extension methods. A document built
//! from the catalogues alone therefore describes a fraction of the surface, and a generated client
//! reads that silence as "this endpoint does not exist".
//!
//! This module takes the other source of truth: the registry itself. It asks the registry which
//! methods exist and publishes exactly those. Nothing here is a list of method names, because a
//! hardcoded list is stale the moment a `register_*` function gains a method — and the failure is
//! silent, since a missing path looks identical to a method that was never implemented.
//!
//! # One path per method, matching [`crate::openapi_document`]
//!
//! The transport serves every method through one route — `POST /api/remote.invoke` with an envelope
//! of `{namespace, method, args, peer}` — but the document publishes one `POST /{namespace}/{method}`
//! per method, exactly as `openapi.rs` does. The reason is the same one recorded there: an envelope
//! path would describe one operation and hide the other hundred, while a per-method path describes
//! what a caller can *do* and leaves addressing to the transport. Consistency matters here because a
//! client generator may be pointed at either document, and two documents that disagree about path
//! shape produce two incompatible clients from the same server.
//!
//! The envelope route is still published, as `x-girder-invoke-path`, so a caller that must speak to
//! this server as it is does not have to infer the route from a document that never names it.
//!
//! # What the descriptions are, and what they are not
//!
//! A registry stores handlers, not documentation: [`girder_core::RemoteRegistry::names`] returns the
//! `(namespace, method)` pairs and nothing else. Where a descriptor already exists — the three
//! catalogue modules — its parameters, docs and return schema are reused verbatim. Where one does
//! not, the operation is emitted with an unconstrained request body and an empty return schema and a
//! summary that says only what the method is called.
//!
//! That asymmetry is deliberate and is reported rather than papered over: inventing a parameter list
//! for a method nobody documented would produce a client that compiles and sends the wrong body, and
//! a document that lies is worse than a document that is thin. Operations with no descriptor are
//! still *reachable* — they are the reason this module exists — they are just not described.
//!
//! # Why the constants below are duplicated
//!
//! `openapi.rs` keeps its version, dialect and extension names private to that module, and this
//! module may only add files. The values are copied so the two documents carry the same version
//! string and the same dialect; a test in `tests/openapi_full.rs` pins them against the literals so a
//! change to one is caught rather than silently diverging.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use girder_core::{Catalogue, Method, ObjectSchema, RemoteRegistry, catalogue_to_json_schema};

/// The `OpenAPI` version this module emits.
///
/// 3.1 rather than 3.0 for the reason `openapi.rs` records: 3.1 embeds draft 2020-12 schemas
/// unchanged, and this crate renders a nullable value as a `"type": [..., "null"]` union that a 3.0
/// Schema Object rejects.
const OPENAPI_VERSION: &str = "3.1.0";

/// The JSON Schema dialect the embedded schemas are written in.
///
/// Named explicitly so a consumer does not apply the `OpenAPI` base dialect, which adds keywords the
/// shared schema emitter never produces.
const JSON_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// The route that accepts an RPC envelope.
///
/// A literal rather than an import, for the same reason `openapi.rs` uses one: `girder-rpc` depends
/// on this crate, so the constant cannot be borrowed back from it. Both documents name it, so a
/// change to the route has to change both spellings.
const INVOKE_PATH: &str = "/api/remote.invoke";

/// The extension keyword naming [`INVOKE_PATH`].
const INVOKE_PATH_EXTENSION: &str = "x-girder-invoke-path";

/// The extension keyword naming the wire revision the document describes.
const REVISION_EXTENSION: &str = "x-girder-revision";

/// The extension keyword carrying references the schema emitter could not resolve.
const UNRESOLVED_EXTENSION: &str = "x-girder-unresolved-refs";

/// The extension keyword carrying the number of operations the document publishes.
///
/// Emitted so a consumer can check completeness without counting `paths` itself: the number is
/// written by the same loop that fills `paths`, so a disagreement is a real defect rather than a
/// counting mistake in the consumer.
const OPERATION_COUNT_EXTENSION: &str = "x-girder-operation-count";

/// The prefix a reference has inside the schema emitter's bundle.
const DEFS_PREFIX: &str = "#/$defs/";

/// The prefix a reference has once the definitions live under `components.schemas`.
const SCHEMAS_PREFIX: &str = "#/components/schemas/";

/// The wire revision this document describes.
///
/// The three catalogue modules this module reads descriptors from all declare revision 1, and a
/// document that published a different number would tell a client to expect a wire shape the
/// descriptors do not describe.
const WIRE_REVISION: u32 = 1;

/// The document title used when the caller supplies no `info.title`.
const DEFAULT_TITLE: &str = "Girder";

/// The document version used when the caller supplies no `info.version`.
///
/// `OpenAPI` requires a version string, so a document cannot simply omit it.
const DEFAULT_VERSION: &str = "0.0.0";

/// One method's place in the published surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    /// The namespace the method is registered under.
    pub namespace: String,
    /// The method name as registered.
    pub method: String,
    /// A one-line description.
    pub summary: String,
}

impl Operation {
    /// The path the operation is published at.
    ///
    /// `session.list` becomes `/session/list`: the dotted call name is how every client refers to
    /// the method, and turning the separator into a path segment gives an address that is unique
    /// without a lookup table.
    #[must_use]
    fn path(&self) -> String {
        path_of(&self.namespace, &self.method)
    }

    /// The qualified name a client calls.
    #[must_use]
    fn qualified(&self) -> String {
        format!("{}.{}", self.namespace, self.method)
    }
}

/// A registry's surface, as the document needs it.
///
/// Built once per document so the operation list and the schema bundle cannot disagree about which
/// methods exist: both are derived from the same `entries` sequence, in the same order.
struct Surface {
    /// Every registered method, paired with the descriptor used to render it.
    entries: Vec<(Operation, Method)>,
    /// The named types those descriptors refer to.
    types: BTreeMap<String, ObjectSchema>,
}

/// Extracts every registered method, in the order the document publishes them.
///
/// **This is where completeness comes from.** The names are read from `registry.names()`, never
/// written down: a `register_*` function that gains a method adds it to the document on the next
/// call, and one that loses a method drops it. A hardcoded list would pass a test written against
/// the same list and still be wrong about the server.
#[must_use]
pub fn operations(registry: &RemoteRegistry) -> Vec<Operation> {
    surface(registry)
        .entries
        .into_iter()
        .map(|(operation, _descriptor)| operation)
        .collect()
}

/// Renders a registry as an `OpenAPI` 3.1 document.
///
/// One `POST` operation per registered method at `/{namespace}/{method}`, the named types the
/// descriptors mention under `components.schemas`, and one request-body component per method beside
/// them. An empty registry produces a document with an empty `paths` map rather than no `paths` key:
/// a consumer should not have to distinguish "no methods" from "a document that forgot to describe
/// them".
#[must_use]
pub fn document(registry: &RemoteRegistry, info: &Value) -> Value {
    let Surface { entries, types } = surface(registry);

    let mut catalogue = Catalogue::new(WIRE_REVISION);
    catalogue.methods = entries
        .iter()
        .map(|(_operation, descriptor)| descriptor.clone())
        .collect();
    catalogue.types = types;

    // The single source of schema truth, shared with `openapi.rs`: this module re-homes the bundle
    // and adds the fields `OpenAPI` requires on top of a schema bundle, but never re-derives a
    // schema from the type model. Two emitters for one type model drift, and the drift shows up as
    // a client that validates against one document and not the other.
    let bundle = catalogue_to_json_schema(&catalogue);

    let mut schemas = Map::new();
    hoist_definitions(&bundle, &mut schemas);

    let mut paths = Map::new();
    // The bundle emits one entry per method, in declaration order, so the two sequences line up.
    // Pairing them this way keeps each `Operation`'s own fields available, which the bundled entry
    // only carries pre-joined into one qualified name.
    for ((operation, descriptor), entry) in entries.iter().zip(methods_of(&bundle)) {
        let qualified = operation.qualified();
        let request = request_schema(&qualified, entry, &mut schemas);
        let response = entry
            .get("output")
            .map_or_else(|| json!({}), |schema| rewrite_refs(schema.clone()));

        paths.insert(
            operation.path(),
            operation_json(operation, &qualified, descriptor, &request, &response),
        );
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
    document.insert(OPERATION_COUNT_EXTENSION.to_owned(), json!(entries.len()));

    // Carried across from the bundle rather than recomputed: the set of unresolved names is exactly
    // what the schema emitter already decided, and a second opinion could only disagree with it.
    for extension in [REVISION_EXTENSION, UNRESOLVED_EXTENSION] {
        if let Some(value) = bundle.get(extension) {
            document.insert(extension.to_owned(), value.clone());
        }
    }

    Value::Object(document)
}

/// Builds the surface the document publishes.
///
/// Descriptors come from the three catalogue modules, keyed by qualified name; a registered method
/// with no descriptor is still published, with an unconstrained body. The registry decides *which*
/// methods exist and the catalogues only decide how much is known about them, which is the
/// direction that keeps the document complete as the registry grows.
fn surface(registry: &RemoteRegistry) -> Surface {
    let mut described: BTreeMap<String, Method> = BTreeMap::new();
    let mut types: BTreeMap<String, ObjectSchema> = BTreeMap::new();

    for catalogue in [
        girder_core::api_session::session_catalogue(),
        girder_core::api_config::config_catalogue(),
        girder_core::api_job::job_catalogue(),
    ] {
        for method in catalogue.methods {
            described.insert(method.qualified(), method);
        }
        for (name, schema) in catalogue.types {
            // First definition wins: the catalogues are written separately and a later one
            // redeclaring a name must not silently replace a definition another method's `$ref`
            // already resolves against.
            types.entry(name).or_insert(schema);
        }
    }

    // `names()` is backed by a `BTreeMap`, so this order is already sorted and stable; relying on
    // it keeps the document reproducible across runs, which is what makes a diff of two generated
    // documents meaningful.
    let mut entries = Vec::with_capacity(registry.len());
    for (namespace, method) in registry.names() {
        let qualified = format!("{namespace}.{method}");
        let descriptor = described.remove(&qualified).unwrap_or_else(|| {
            // No parameters and `Schema::Any` in and out: the honest description of a method whose
            // handler exists but whose signature was never written down.
            Method::new(namespace.clone(), method.clone())
        });
        let summary = descriptor
            .docs
            .as_deref()
            .and_then(first_line)
            .map_or_else(|| format!("Calls `{qualified}`."), str::to_owned);
        entries.push((
            Operation {
                namespace,
                method,
                summary,
            },
            descriptor,
        ));
    }

    Surface { entries, types }
}

/// Renders the `info` object.
///
/// Keys the caller supplied are preserved as written, because a caller that passes `contact` or
/// `license` means them. `title` and `version` are the two `OpenAPI` requires, so a document missing
/// either one gets a default rather than being emitted invalid — a consumer that rejected the whole
/// document over one absent key would learn nothing about the methods.
fn info_json(info: &Value) -> Value {
    let mut object = match info {
        Value::Object(supplied) => supplied.clone(),
        // Not an object, so there is nothing to preserve; the defaults below are all that can be
        // said. Returning the value unchanged would put a non-object in `info` and fail validation
        // for every consumer.
        _ => Map::new(),
    };
    object
        .entry("title".to_owned())
        .or_insert_with(|| json!(DEFAULT_TITLE));
    object
        .entry("version".to_owned())
        .or_insert_with(|| json!(DEFAULT_VERSION));
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
/// An absent list means an empty one: a registry with no methods is a valid document, and the
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
    // A `.` cannot appear in a type name, so this name cannot collide with a defined type. Using the
    // qualified method name keeps the component recognisable in a generated client.
    let name = format!("{qualified}.Input");
    let body = entry
        .get("input")
        .map_or_else(empty_object_schema, |schema| rewrite_refs(schema.clone()));
    schemas.insert(name.clone(), body);
    json!({ "$ref": format!("{SCHEMAS_PREFIX}{}", escape_pointer(&name)) })
}

/// Renders one path item.
fn operation_json(
    operation: &Operation,
    qualified: &str,
    descriptor: &Method,
    request: &Value,
    response: &Value,
) -> Value {
    let mut item = Map::new();
    item.insert("operationId".to_owned(), json!(qualified));
    // Grouping by namespace is what turns the generated console into a readable index; it is an
    // annotation for tooling and carries no wire meaning.
    item.insert("tags".to_owned(), json!([operation.namespace]));
    item.insert("summary".to_owned(), json!(operation.summary));
    // The full documentation, when there is more of it than the summary line already showed. A
    // method with a one-paragraph doc comment would otherwise lose everything past its first line.
    if let Some(docs) = &descriptor.docs
        && docs.lines().count() > 1
    {
        item.insert("description".to_owned(), json!(docs));
    }

    let mut body = Map::new();
    // A body is required exactly when some parameter is; a method whose parameters are all optional
    // accepts an absent body, and the server then decodes the same defaults it would apply to `{}`.
    // Marking every body required would make a generator emit `{}` for a method that takes nothing.
    body.insert(
        "required".to_owned(),
        json!(
            descriptor
                .parameters
                .iter()
                .any(|parameter| parameter.required)
        ),
    );
    body.insert(
        "content".to_owned(),
        json!({ "application/json": { "schema": request } }),
    );
    item.insert("requestBody".to_owned(), Value::Object(body));

    let mut responses = Map::new();
    responses.insert(
        "200".to_owned(),
        json!({
            "description": format!("The value `{qualified}` returned."),
            "content": { "application/json": { "schema": response } },
        }),
    );
    // No schema: a failure is projected by the transport into a `{code, message}` object whose codes
    // grow with the method set, and a schema that listed today's codes would reject tomorrow's. The
    // description is what a caller can rely on without being lied to.
    responses.insert(
        "default".to_owned(),
        json!({ "description": "The call failed; the body carries a stable error `code`." }),
    );
    item.insert("responses".to_owned(), Value::Object(responses));

    json!({ "post": Value::Object(item) })
}

/// Builds the path a method is published at.
///
/// Trimmed so a namespace that arrives with stray separators cannot produce `//session`; a method
/// with an empty namespace and name would otherwise render as the empty string, which is not a path
/// at all.
fn path_of(namespace: &str, name: &str) -> String {
    let mut path = String::with_capacity(namespace.len() + name.len() + 2);
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
    if path.is_empty() {
        path.push('/');
    }
    path
}

/// The schema for a body that carries no arguments.
fn empty_object_schema() -> Value {
    json!({ "type": "object", "properties": {}, "required": [] })
}

/// The first non-empty line of a doc comment.
///
/// The summary has to be one line because that is what an operation summary is; taking the whole
/// doc would push a paragraph into a field a generated console renders as a heading.
fn first_line(docs: &str) -> Option<&str> {
    docs.lines().map(str::trim).find(|line| !line.is_empty())
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
/// A component name containing `/` would otherwise address a different path entirely, and the `$ref`
/// would parse and resolve to nothing.
fn escape_pointer(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}
