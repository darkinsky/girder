//! JSON Schema emission for the Remote type catalogue.
//!
//! [`girder_core::typert`] decides what a Remote value *is*; this module spells that decision in
//! JSON Schema (draft 2020-12), which is the format MCP, `OpenAI` function calling and most tool
//! ecosystems consume. The TypeScript emitter in [`girder_core::to_typescript`] stays where it is:
//! a browser client wants a `zod` runtime schema, a model-facing tool wants JSON Schema, and one
//! emitter cannot serve both without serving one of them badly.
//!
//! # The three shapes of "may be absent"
//!
//! The type model records absence in two independent places, and JSON Schema records it in two
//! more. Mapping them wrongly is the usual way a generated schema lies:
//!
//! 1. [`Schema::Optional`] says the *value* may be `null`. Draft 2020-12 has no `nullable`
//!    keyword — that is `OpenAPI` 3.0 — so this module widens the type to `["string", "null"]`, or
//!    wraps in `anyOf` when the inner schema has no single type to widen.
//! 2. [`Field::required`] and [`Field::added_in`] say the *property* may be missing. JSON Schema
//!    says that by leaving the name out of `required`, not by widening the type: an omitted field
//!    and a null field are different values on the wire, and conflating them makes a client send
//!    `null` where the server expected nothing at all.
//!
//! `typert::Schema::to_json` emits `"nullable": true`, which draft 2020-12 does not define: a
//! validator ignores the keyword and then rejects the `null` the server accepts. Emitting type
//! unions instead is the reason a second emitter exists.
//!
//! # Why `Option` also settles presence
//!
//! A field of type [`Schema::Optional`] is left out of `required` even when its [`Field::required`]
//! flag is set. That is not a conflation of "null" with "absent" but the encoding the derive macro
//! already chose: `girder-macros` emits `Field::optional` for `Option<T>` precisely because,
//! as its own comment puts it, "an `Option` that still claimed `required` would make a client send
//! null rather than omitting the field". Honouring that intent here keeps a hand-written
//! `Field::required(name, Schema::Optional(..))` consistent with a derived one; the value still
//! accepts `null`, it is just not something a client is obliged to send.
//!
//! # Where `$defs` lives
//!
//! A named object is a definition, not a copy: inlining it at every use gives a client several
//! chances to disagree about one type, and a recursive type cannot be inlined at all.
//! [`to_json_schema`] renders a fragment and so has nowhere to put definitions — it inlines named
//! objects and passes [`Schema::Ref`] through as `$ref`. The two document-level entry points,
//! [`method_to_json_schema`] and [`catalogue_to_json_schema`], hoist what they reference into a
//! `$defs` block beside the body, so every `$ref` they emit resolves inside the same document.
//!
//! A reference the document cannot resolve — a [`Schema::Ref`] to a type the caller never supplied
//! — is reported in `x-girder-unresolved-refs` rather than dropped. A dangling `$ref` is a schema
//! that silently validates nothing useful, and silence is the expensive failure here. That is why
//! a standalone [`method_to_json_schema`] cannot resolve references at all: a [`Method`] carries
//! type *names*, and the definitions live in the catalogue. Use [`catalogue_to_json_schema`] when
//! the references have to resolve.
//!
//! # Extension keywords
//!
//! The catalogue data has no spelling in JSON Schema, so it hangs off `x-girder-` keywords, which
//! a validator ignores by definition: `x-girder-revision`, `x-girder-methods`, `x-girder-output`
//! and `x-girder-unresolved-refs`. The validation keywords are the schema's own.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use girder_core::{Catalogue, Method, ObjectSchema, Schema};

/// The JSON Schema dialect this module emits.
///
/// Draft 2020-12 rather than an earlier draft because it is the first one where `$defs` is the
/// canonical definitions keyword. A consumer that understands only `definitions` can rename the
/// block without touching a single `$ref`, which is exactly why the draft renamed it.
const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// The revision at which a schema with no revision context is rendered.
///
/// A standalone method describes the newest server, not one client's view of it: no field can be
/// omitted for being newer than this revision, so only genuinely optional fields leave `required`.
/// A catalogue renders at [`Catalogue::revision`] instead, matching what `Catalogue::to_json` does.
const NEWEST_REVISION: u32 = u32::MAX;

/// The name of the annotation listing references the document could not resolve.
const UNRESOLVED: &str = "x-girder-unresolved-refs";

/// Named types available to resolve a `$ref`.
///
/// A fragment emitter borrows this empty table: it has no document to hoist definitions into, so a
/// reference stays a reference and a named object is inlined.
static EMPTY_TYPES: BTreeMap<String, ObjectSchema> = BTreeMap::new();

/// Builds one document, collecting the definitions it refers to.
struct Emitter<'a> {
    /// The definitions a reference may point at.
    types: &'a BTreeMap<String, ObjectSchema>,
    /// Definitions hoisted out of the body, keyed by type name.
    ///
    /// A map rather than a list: two references to one type must produce one definition, and
    /// sorted keys make two runs over one catalogue produce the same bytes.
    defs: BTreeMap<String, Value>,
    /// Type names whose definition is being built right now.
    ///
    /// A recursive type names itself before its definition exists. Without this stack, hoisting
    /// `Node { next: Node }` would recurse until the process ran out of stack; with it, the inner
    /// reference stays a `$ref` and resolves once the outer definition is inserted.
    open: Vec<String>,
    /// Names referenced but not defined, because the document had no body for them.
    unresolved: BTreeSet<String>,
    /// Whether a named object is hoisted into `$defs` instead of inlined.
    hoist: bool,
    /// The revision at which field requirements are rendered.
    revision: u32,
}

impl<'a> Emitter<'a> {
    /// Builds an emitter for `types` at `revision`.
    fn new(types: &'a BTreeMap<String, ObjectSchema>, revision: u32, hoist: bool) -> Self {
        Self {
            types,
            defs: BTreeMap::new(),
            open: Vec::new(),
            unresolved: BTreeSet::new(),
            hoist,
            revision,
        }
    }

    /// Renders one schema.
    fn emit(&mut self, schema: &Schema) -> Value {
        match schema {
            Schema::String => json!({ "type": "string" }),
            Schema::Bool => json!({ "type": "boolean" }),
            // The format is an annotation, not a constraint: a validator that knows nothing about
            // `int64` still checks `type`, and a client that does gets the precision for free.
            Schema::Int64 => json!({ "type": "integer", "format": "int64" }),
            Schema::Float64 => json!({ "type": "number", "format": "double" }),
            // An empty schema accepts anything, which is exactly what "opaque" means.
            Schema::Any => json!({}),
            Schema::Enum(values) => json!({ "type": "string", "enum": values }),
            Schema::Array(element) => {
                let items = self.emit(element);
                json!({ "type": "array", "items": items })
            }
            Schema::Map(value) => {
                let additional = self.emit(value);
                json!({ "type": "object", "additionalProperties": additional })
            }
            Schema::OneOf(variants) => {
                let branches: Vec<Value> = variants
                    .iter()
                    .map(|variant| {
                        let branch = self.emit(&variant.schema);
                        discriminate(branch, &variant.tag)
                    })
                    .collect();
                json!({ "oneOf": branches })
            }
            Schema::Object(object) => self.emit_object(object),
            Schema::Ref(name) => self.reference(name),
            Schema::Optional(inner) => {
                let inner = self.emit(inner);
                nullable(inner)
            }
        }
    }

    /// Renders one named object, hoisting it into `$defs` when the document has one.
    fn emit_object(&mut self, object: &ObjectSchema) -> Value {
        if !self.hoist || object.name.is_empty() {
            return self.emit_object_inline(object);
        }
        let name = &object.name;
        // A recursive field refers to the type whose definition is still being built; referencing
        // it here rather than defining it again is what keeps the recursion finite.
        if !self.defs.contains_key(name) && !self.open.iter().any(|open| open == name) {
            self.open.push(name.clone());
            let body = self.emit_object_inline(object);
            self.open.pop();
            self.defs.insert(name.clone(), body);
        }
        reference_to(name)
    }

    /// Renders a schema as an inline object, without hoisting it.
    fn emit_object_inline(&mut self, object: &ObjectSchema) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for field in &object.fields {
            let mut schema = self.emit(&field.schema);
            if let Some(docs) = &field.docs {
                annotate(&mut schema, "description", json!(docs));
            }
            // A field the client may omit is absent from `required`; its *type* is untouched, so a
            // nullable field still says so. See the module docs for why both rules apply.
            if !field.omittable_at(self.revision) && !nullable_at_type_level(&field.schema) {
                required.push(Value::String(field.name.clone()));
            }
            properties.insert(field.name.clone(), schema);
        }

        let mut json_value = json!({
            "type": "object",
            "properties": properties,
            "required": required,
        });
        if let Some(docs) = &object.docs {
            annotate(&mut json_value, "description", json!(docs));
        }
        json_value
    }

    /// Renders a reference, defining its target first when the document has a `$defs` block.
    fn reference(&mut self, name: &str) -> Value {
        if self.hoist {
            self.define(name);
        }
        reference_to(name)
    }

    /// Hoists the definition of `name` into `$defs`, or records that there is none to hoist.
    fn define(&mut self, name: &str) {
        if self.defs.contains_key(name) || self.open.iter().any(|open| open == name) {
            return;
        }
        // Copy the table out of `self` first: `emit_object_inline` needs `&mut self`, and the
        // object it renders lives in the map that `self` only borrows.
        let types = self.types;
        let Some(object) = types.get(name) else {
            self.unresolved.insert(name.to_owned());
            return;
        };
        self.open.push(name.to_owned());
        let body = self.emit_object_inline(object);
        self.open.pop();
        self.defs.insert(name.to_owned(), body);
    }
}

/// Renders one schema as a JSON Schema fragment.
///
/// The result is a fragment, not a document: a named [`Schema::Object`] is inlined and a
/// [`Schema::Ref`] keeps its `$ref` without a `$defs` block for it to land in. Use
/// [`method_to_json_schema`] or [`catalogue_to_json_schema`] when the output has to stand alone.
#[must_use]
pub fn to_json_schema(schema: &Schema) -> Value {
    Emitter::new(&EMPTY_TYPES, NEWEST_REVISION, false).emit(schema)
}

/// Renders one method's request body as a JSON Schema document.
///
/// The root of the document *is* the body: `type: object`, one property per parameter, and the
/// required parameters in `required`. That shape is what a tool definition wants, so a caller can
/// hand the result to an MCP or `OpenAI` tool declaration without digging the body out of a wrapper.
///
/// The return value is attached as `x-girder-output`, outside the validation keywords: it is not
/// part of the request, and a property named `output` would be a lie about the wire shape.
///
/// Reference resolution is limited by what a [`Method`] knows: it carries type names, not type
/// definitions, so a body that references a named type reports it in `x-girder-unresolved-refs`.
/// [`catalogue_to_json_schema`] is the entry point that can resolve them.
#[must_use]
pub fn method_to_json_schema(method: &Method) -> Value {
    let mut emitter = Emitter::new(&EMPTY_TYPES, NEWEST_REVISION, true);
    let (input, output) = emit_method(&mut emitter, method);
    let Value::Object(mut document) = input else {
        // `emit_method` always builds an object; this branch exists because a schema value is not
        // statically known to be one, and returning the body unchanged is better than panicking.
        return input;
    };
    document.insert("$schema".to_owned(), json!(DIALECT));
    document.insert("x-girder-output".to_owned(), output);
    finish(document, emitter)
}

/// Renders every method in a catalogue as one JSON Schema document.
///
/// The root asserts nothing about a value — a catalogue is not a value — and is therefore a schema
/// that accepts anything, which is what a schema *bundle* is. What it carries is the contract:
/// every named type in `$defs`, and every method's body and return schema under `x-girder-methods`
/// with references that resolve against those definitions.
///
/// Every defined type is hoisted, including one no method mentions. A type is part of the contract
/// whether or not a method body currently exercises it: a client that has to construct one needs
/// its definition, and dropping it would make the schema depend on which methods happen to exist.
#[must_use]
pub fn catalogue_to_json_schema(catalogue: &Catalogue) -> Value {
    let mut emitter = Emitter::new(&catalogue.types, catalogue.revision, true);

    // Seed the definitions before rendering bodies, so a method's `$ref` never races the
    // definition that has to answer it.
    for name in catalogue.types.keys() {
        emitter.define(name);
    }

    let mut methods = Vec::with_capacity(catalogue.methods.len());
    for method in &catalogue.methods {
        let (input, output) = emit_method(&mut emitter, method);
        let mut entry = Map::new();
        entry.insert("name".to_owned(), json!(method.qualified()));
        if let Some(docs) = &method.docs {
            entry.insert("description".to_owned(), json!(docs));
        }
        entry.insert("input".to_owned(), input);
        entry.insert("output".to_owned(), output);
        methods.push(Value::Object(entry));
    }

    let mut document = Map::new();
    document.insert("$schema".to_owned(), json!(DIALECT));
    document.insert("title".to_owned(), json!("Girder remote catalogue"));
    document.insert(
        "description".to_owned(),
        json!("Every method's request body and return schema, with the named types in $defs."),
    );
    document.insert("x-girder-revision".to_owned(), json!(catalogue.revision));
    document.insert("x-girder-methods".to_owned(), Value::Array(methods));
    finish(document, emitter)
}

/// Renders one method's body and return schema into an existing document.
fn emit_method(emitter: &mut Emitter<'_>, method: &Method) -> (Value, Value) {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for parameter in &method.parameters {
        let mut schema = emitter.emit(&parameter.schema);
        if let Some(docs) = &parameter.docs {
            annotate(&mut schema, "description", json!(docs));
        }
        // A `Parameter` has no revision field: the method signature is the current one, and a
        // caller that cannot send a parameter simply does not call this version of the method.
        if parameter.required && !nullable_at_type_level(&parameter.schema) {
            required.push(Value::String(parameter.name.clone()));
        }
        properties.insert(parameter.name.clone(), schema);
    }

    let mut input = json!({
        "type": "object",
        "properties": properties,
        "required": required,
    });
    annotate(&mut input, "title", json!(method.qualified()));
    if let Some(docs) = &method.docs {
        annotate(&mut input, "description", json!(docs));
    }

    let output = emitter.emit(&method.returns);
    (input, output)
}

/// Completes a document with the definitions and unresolved references it accumulated.
fn finish(mut document: Map<String, Value>, emitter: Emitter<'_>) -> Value {
    if !emitter.defs.is_empty() {
        document.insert(
            "$defs".to_owned(),
            Value::Object(emitter.defs.into_iter().collect()),
        );
    }
    if !emitter.unresolved.is_empty() {
        let names: Vec<String> = emitter.unresolved.into_iter().collect();
        document.insert(UNRESOLVED.to_owned(), json!(names));
    }
    Value::Object(document)
}

/// Renders a `$ref` to a named definition.
fn reference_to(name: &str) -> Value {
    json!({ "$ref": format!("#/$defs/{}", escape_pointer(name)) })
}

/// Escapes a name for use as a JSON Pointer segment.
///
/// A name is not necessarily a bare identifier, and an unescaped `/` in a pointer silently
/// addresses a different location: the `$ref` would parse fine and resolve to nothing.
fn escape_pointer(name: &str) -> String {
    name.replace('~', "~0").replace('/', "~1")
}

/// Adds an annotation keyword to a schema.
fn annotate(schema: &mut Value, keyword: &'static str, value: Value) {
    // A non-object schema never leaves the emitter, but an annotation on one would be lost rather
    // than corrupting the schema, so a failed insert is the safe outcome.
    if let Some(object) = schema.as_object_mut() {
        object.insert(keyword.to_owned(), value);
    }
}

/// Whether the type itself admits `null`.
///
/// Consulted when deciding presence: see the module docs for why an `Option`-typed field is
/// omittable.
fn nullable_at_type_level(schema: &Schema) -> bool {
    matches!(schema, Schema::Optional(_))
}

/// Widens a schema so it also accepts `null`.
fn nullable(mut schema: Value) -> Value {
    // An empty schema already accepts null; widening it would only add noise.
    if schema.as_object().is_some_and(Map::is_empty) {
        return schema;
    }
    // Read the type out before mutating: holding a borrow of the map while replacing its `type`
    // key is the one place this function could not compile.
    let keyword = schema
        .as_object()
        .and_then(|object| object.get("type"))
        .cloned();

    match keyword {
        // Already accepts null.
        Some(Value::String(kind)) if kind == "null" => schema,
        Some(Value::String(kind)) => {
            let widened = json!([kind, "null"]);
            if let Some(object) = schema.as_object_mut() {
                object.insert("type".to_owned(), widened);
            }
            schema
        }
        Some(Value::Array(kinds)) if kinds.iter().any(|kind| kind == "null") => schema,
        Some(Value::Array(mut kinds)) => {
            kinds.push(json!("null"));
            if let Some(object) = schema.as_object_mut() {
                object.insert("type".to_owned(), Value::Array(kinds));
            }
            schema
        }
        // A `$ref`, a `oneOf`, or anything else without a single `type` to widen. `anyOf` is the
        // general form and, unlike `oneOf`, does not make "an object that is also null" ambiguous.
        _ => json!({ "anyOf": [schema, { "type": "null" }] }),
    }
}

/// Constrains a union branch's discriminator to the branch's tag.
fn discriminate(branch: Value, tag: &str) -> Value {
    // Siblings of `$ref` are evaluated in draft 2020-12, but a `$ref` with a constraint beside it
    // is still mishandled by plenty of consumers, and `allOf` has no such history.
    if branch.get("$ref").is_some() {
        return json!({
            "allOf": [
                branch,
                { "properties": { "kind": { "const": tag } }, "required": ["kind"] },
            ],
        });
    }

    let is_object = branch.get("type").is_some_and(|kind| kind == "object")
        || branch.get("properties").is_some();
    if !is_object {
        // A branch that is not an object has nowhere to put a `kind`; it stays as it is rather
        // than gaining a `properties` keyword that would never match.
        return branch;
    }

    let mut branch = branch;
    if let Some(object) = branch.as_object_mut() {
        if !object.contains_key("properties") {
            object.insert("properties".to_owned(), json!({}));
        }
        if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
            // The discriminator is fixed per branch, so a client can narrow without checking.
            properties.insert("kind".to_owned(), json!({ "const": tag }));
        }
        // `const` narrows only if the field is there at all: a branch that made `kind` optional
        // would let a value match it without ever naming the variant.
        if !object.contains_key("required") {
            object.insert("required".to_owned(), json!([]));
        }
        if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut)
            && !required.iter().any(|name| name == "kind")
        {
            required.push(json!("kind"));
        }
    }
    branch
}
