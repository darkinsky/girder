//! Type reflection for the Remote surface: describing a Rust type as JSON Schema.
//!
//! This is the first step of the `typert` question, not the answer to it. A client needs to know
//! what a Remote method accepts; upstream gets that from the `TypeScript` checker, which sees the
//! whole type graph. A Rust procedural macro sees type *tokens* — `Vec<SessionSummary>` is a name and
//! nothing more — so the extraction has to be built rather than read.
//!
//! # Why hand-written first
//!
//! A derive macro that generates these implementations can be added without changing this trait. If
//! the schema shape is wrong, the macro would generate the wrong shape faster. Building the trait and
//! a handful of implementations by hand first means the shape is decided by evidence — a real schema
//! compared against what a client needs — rather than by what a macro happens to emit.
//!
//! # What a schema has to answer
//!
//! Three questions, and the third is the one usually forgotten:
//!
//! 1. What is this value's shape?
//! 2. Can it be absent? (`Option`)
//! 3. **Can it be added to later without breaking an old client?**
//!
//! Question 3 has no JSON Schema spelling. [`Field::added_in`] answers it instead: a field that
//! arrived in a later revision must be optional from the client's point of view, even when it is
//! required on the server. That distinction cannot live in the schema and is recorded beside it.

use std::collections::BTreeMap;

/// A JSON Schema fragment describing one value.
///
/// A small enum rather than raw `serde_json::Value`: a schema built from a `Value` can be anything,
/// including wrong, and the compiler cannot help. This enum can only represent shapes that exist.
#[derive(Clone, Debug, PartialEq)]
pub enum Schema {
    /// A string.
    String,
    /// A boolean.
    Bool,
    /// An integer that fits in 64 bits.
    Int64,
    /// A floating-point number.
    Float64,
    /// Any JSON value, when the shape is genuinely opaque.
    ///
    /// Used for a passthrough field. A schema that used this everywhere would describe nothing, so it
    /// is spelled out rather than defaulted to.
    Any,
    /// One of a fixed set of strings.
    Enum(Vec<String>),
    /// A list of one element type.
    Array(Box<Schema>),
    /// A map from string keys to one value type.
    Map(Box<Schema>),
    /// One of several shapes, discriminated by a field.
    OneOf(Vec<Variant>),
    /// An object with named fields.
    Object(ObjectSchema),
    /// Another named type, referenced rather than inlined.
    ///
    /// A reference rather than a copy: inlining a type used in five places produces five chances for
    /// the copies to disagree, and a recursive type cannot be inlined at all.
    Ref(String),
    /// A value that may be absent.
    Optional(Box<Schema>),
}

impl Schema {
    /// Renders the schema as JSON Schema.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::String => serde_json::json!({ "type": "string" }),
            Self::Bool => serde_json::json!({ "type": "boolean" }),
            Self::Int64 => serde_json::json!({ "type": "integer", "format": "int64" }),
            Self::Float64 => serde_json::json!({ "type": "number", "format": "double" }),
            // An empty schema accepts anything, which is exactly what "opaque" means.
            Self::Any => serde_json::json!({}),
            Self::Enum(values) => serde_json::json!({ "type": "string", "enum": values }),
            Self::Array(element) => serde_json::json!({
                "type": "array",
                "items": element.to_json(),
            }),
            Self::Map(value) => serde_json::json!({
                "type": "object",
                "additionalProperties": value.to_json(),
            }),
            Self::OneOf(variants) => {
                let mut branches = Vec::new();
                for variant in variants {
                    branches.push(variant.schema.to_json_with_discriminator(&variant.tag));
                }
                serde_json::json!({ "oneOf": branches })
            }
            Self::Object(object) => object.to_json(),
            Self::Ref(name) => serde_json::json!({ "$ref": format!("#/$defs/{name}") }),
            Self::Optional(inner) => {
                // A nullable schema rather than a bare one: `Option<String>` accepts null, and a
                // client that did not know would send "" instead.
                let mut json = inner.to_json();
                if let Some(object) = json.as_object_mut() {
                    object.insert("nullable".to_owned(), serde_json::json!(true));
                    json
                } else {
                    serde_json::json!({ "oneOf": [json, { "type": "null" }] })
                }
            }
        }
    }
}

/// One branch of a discriminated union.
#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    /// The value of the discriminating field for this branch.
    pub tag: String,
    /// The branch's shape.
    pub schema: Schema,
}

impl Schema {
    /// Renders a variant branch, constraining the discriminator.
    fn to_json_with_discriminator(&self, tag: &str) -> serde_json::Value {
        let mut json = self.to_json();
        if let Some(object) = json.as_object_mut()
            && let Some(properties) = object.get_mut("properties").and_then(|p| p.as_object_mut())
        {
            // The discriminator is fixed per branch, so a client can narrow without checking.
            properties.insert("kind".to_owned(), serde_json::json!({ "const": tag }));
        }
        json
    }
}

/// One field of an object.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    /// Its name on the wire.
    pub name: String,
    /// Its shape.
    pub schema: Schema,
    /// Whether a client must send it.
    ///
    /// Different from the schema being optional: a required field that arrived in a later revision is
    /// still required *on the server* while an old client cannot know to send it. See
    /// [`Field::added_in`].
    pub required: bool,
    /// The revision that introduced this field, when it was not in the first one.
    ///
    /// A field added in revision 2 must be treated as optional by a revision-1 client, and this is
    /// the only place that fact can live: JSON Schema has no notion of revisions.
    pub added_in: Option<u32>,
    /// What the field means, for a generated client's documentation.
    pub docs: Option<String>,
}

impl Field {
    /// Builds a required field.
    #[must_use]
    pub fn required(name: impl Into<String>, schema: Schema) -> Self {
        Self {
            name: name.into(),
            schema,
            required: true,
            added_in: None,
            docs: None,
        }
    }

    /// Builds an optional field.
    #[must_use]
    pub fn optional(name: impl Into<String>, schema: Schema) -> Self {
        Self {
            name: name.into(),
            schema,
            required: false,
            added_in: None,
            docs: None,
        }
    }

    /// Marks when the field was introduced.
    #[must_use]
    pub fn added_in(mut self, revision: u32) -> Self {
        self.added_in = Some(revision);
        self
    }

    /// Adds documentation.
    #[must_use]
    pub fn with_docs(mut self, docs: impl Into<String>) -> Self {
        self.docs = Some(docs.into());
        self
    }

    /// Whether a client at `revision` may omit this field.
    ///
    /// An old client must be able to omit a field it has never heard of, so a field added later is
    /// omittable regardless of `required`.
    #[must_use]
    pub fn omittable_at(&self, revision: u32) -> bool {
        if !self.required {
            return true;
        }
        self.added_in.is_some_and(|added| added > revision)
    }
}

/// An object's shape.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ObjectSchema {
    /// Its name, used for the `$ref` target and the generated type.
    pub name: String,
    /// Its fields, in declaration order.
    pub fields: Vec<Field>,
    /// The revision that introduced the type itself.
    pub added_in: Option<u32>,
    /// What the type means.
    pub docs: Option<String>,
}

impl ObjectSchema {
    /// Builds an object schema.
    #[must_use]
    pub fn new(name: impl Into<String>, fields: Vec<Field>) -> Self {
        Self {
            name: name.into(),
            fields,
            added_in: None,
            docs: None,
        }
    }

    /// The required field names at `revision`.
    ///
    /// A field added after `revision` is excluded even when `required`: an old client cannot know to
    /// send it, and a schema that demanded it would make every old client fail.
    #[must_use]
    pub fn required_at(&self, revision: u32) -> Vec<String> {
        self.fields
            .iter()
            .filter(|field| !field.omittable_at(revision))
            .map(|field| field.name.clone())
            .collect()
    }

    /// Renders the schema at revision 0.
    ///
    /// Revision 0 is "everything the server has", which is what a schema dump wants. A client asks
    /// for its own revision through [`ObjectSchema::to_json_at`].
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut properties = serde_json::Map::new();
        for field in &self.fields {
            let mut schema = field.schema.to_json();
            if let Some(docs) = &field.docs
                && let Some(object) = schema.as_object_mut()
            {
                object.insert("description".to_owned(), serde_json::json!(docs));
            }
            properties.insert(field.name.clone(), schema);
        }

        let mut json = serde_json::json!({
            "type": "object",
            "title": self.name,
            "properties": properties,
        });
        if let Some(docs) = &self.docs
            && let Some(object) = json.as_object_mut()
        {
            object.insert("description".to_owned(), serde_json::json!(docs));
        }
        // Required is rendered at revision 0 by default; a caller that needs a specific revision's
        // view calls `to_json_at`.
        if let Some(object) = json.as_object_mut() {
            object.insert(
                "required".to_owned(),
                serde_json::json!(self.required_at(0)),
            );
        }
        json
    }

    /// Renders the schema as a client at `revision` sees it.
    #[must_use]
    pub fn to_json_at(&self, revision: u32) -> serde_json::Value {
        let mut json = self.to_json();
        if let Some(object) = json.as_object_mut() {
            object.insert(
                "required".to_owned(),
                serde_json::json!(self.required_at(revision)),
            );
        }
        json
    }
}

/// One parameter of a Remote method.
#[derive(Clone, Debug, PartialEq)]
pub struct Parameter {
    /// Its name.
    pub name: String,
    /// Its shape.
    pub schema: Schema,
    /// Whether a caller must supply it.
    pub required: bool,
    /// What it means.
    pub docs: Option<String>,
}

impl Parameter {
    /// Builds a required parameter.
    #[must_use]
    pub fn required(name: impl Into<String>, schema: Schema) -> Self {
        Self {
            name: name.into(),
            schema,
            required: true,
            docs: None,
        }
    }

    /// Builds an optional parameter.
    #[must_use]
    pub fn optional(name: impl Into<String>, schema: Schema) -> Self {
        Self {
            name: name.into(),
            schema,
            required: false,
            docs: None,
        }
    }

    /// Adds documentation.
    #[must_use]
    pub fn with_docs(mut self, docs: impl Into<String>) -> Self {
        self.docs = Some(docs.into());
        self
    }
}

/// One Remote method's signature.
#[derive(Clone, Debug, PartialEq)]
pub struct Method {
    /// Its namespace, for grouping in a generated client.
    pub namespace: String,
    /// Its name.
    pub name: String,
    /// Its parameters, in declaration order.
    pub parameters: Vec<Parameter>,
    /// What it returns.
    pub returns: Schema,
    /// What it means.
    pub docs: Option<String>,
}

impl Method {
    /// Builds a method.
    #[must_use]
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            name: name.into(),
            parameters: Vec::new(),
            returns: Schema::Any,
            docs: None,
        }
    }

    /// Adds a parameter.
    #[must_use]
    pub fn with_parameter(mut self, parameter: Parameter) -> Self {
        self.parameters.push(parameter);
        self
    }

    /// Sets the return type.
    #[must_use]
    pub fn returning(mut self, schema: Schema) -> Self {
        self.returns = schema;
        self
    }

    /// Adds documentation.
    #[must_use]
    pub fn with_docs(mut self, docs: impl Into<String>) -> Self {
        self.docs = Some(docs.into());
        self
    }

    /// The fully qualified name a client calls.
    #[must_use]
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.namespace, self.name)
    }

    /// Renders the method as a call descriptor.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut parameters = serde_json::Map::new();
        for parameter in &self.parameters {
            let mut schema = parameter.schema.to_json();
            if let Some(docs) = &parameter.docs
                && let Some(object) = schema.as_object_mut()
            {
                object.insert("description".to_owned(), serde_json::json!(docs));
            }
            parameters.insert(parameter.name.clone(), schema);
        }

        let required: Vec<&str> = self
            .parameters
            .iter()
            .filter(|parameter| parameter.required)
            .map(|parameter| parameter.name.as_str())
            .collect();

        let mut json = serde_json::json!({
            "name": self.qualified(),
            "input": {
                "type": "object",
                "properties": parameters,
                "required": required,
            },
            "output": self.returns.to_json(),
        });
        if let Some(docs) = &self.docs
            && let Some(object) = json.as_object_mut()
        {
            object.insert("description".to_owned(), serde_json::json!(docs));
        }
        json
    }
}

/// A set of types and methods, as a client receives them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalogue {
    /// The named types, keyed by name.
    pub types: BTreeMap<String, ObjectSchema>,
    /// The methods.
    pub methods: Vec<Method>,
    /// The wire revision this catalogue describes.
    pub revision: u32,
}

impl Catalogue {
    /// Creates an empty catalogue at `revision`.
    #[must_use]
    pub fn new(revision: u32) -> Self {
        Self {
            types: BTreeMap::new(),
            methods: Vec::new(),
            revision,
        }
    }

    /// Adds a type.
    pub fn define(&mut self, schema: ObjectSchema) {
        self.types.insert(schema.name.clone(), schema);
    }

    /// Adds a method.
    pub fn add(&mut self, method: Method) {
        self.methods.push(method);
    }

    /// Renders the catalogue as a JSON document a client can consume.
    ///
    /// The `$defs` block holds every named type, so a method's `$ref` resolves without the client
    /// having to ask for types separately.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut definitions = serde_json::Map::new();
        for (name, schema) in &self.types {
            definitions.insert(name.clone(), schema.to_json_at(self.revision));
        }

        serde_json::json!({
            "revision": self.revision,
            "$defs": definitions,
            "methods": self.methods.iter().map(Method::to_json).collect::<Vec<_>>(),
        })
    }
}

// ---- TypeScript emission ----------------------------------------------------------------------

/// Renders a catalogue as a TypeScript client module.
///
/// The client is the reason this whole mechanism exists. Upstream generates a `zod` schema rather than
/// only a type: a type is erased at runtime, so a client would compile against it and still send the
/// wrong shape. A schema validates at the boundary where the mistake actually costs something.
///
/// # What is emitted
///
/// - One `zod` schema per named type, in dependency order so a reference resolves.
/// - One map of methods, each with its input schema and return type.
///
/// # Why the discriminator is a literal
///
/// A union branch's `kind` is `z.literal("completed")`, not `z.string()`: the narrowing a client does
/// on `kind` is only sound if the schema refuses a value the branch does not own.
#[must_use]
pub fn to_typescript(catalogue: &Catalogue) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    out.push_str("// Generated by girder typert — do not edit.\n");
    out.push_str("// Run the generator after changing a Remote signature.\n\n");
    out.push_str("import { z } from 'zod'\n\n");

    // Named types first: a method's schema references them, and a forward reference would need a
    // hoisted declaration that zod's chained form does not have.
    for (name, object) in &catalogue.types {
        // `//` rather than `///`: TypeScript has no doc-comment prefix of that shape, and a
        // `///` line is a plain comment that an editor will not attach to the symbol.
        let _ = writeln!(out, "/** {} */", object.docs.as_deref().unwrap_or(name));
        let _ = writeln!(out, "export const {name}Schema = z.object({{");
        for field in &object.fields {
            let mut chain = render_zod(&field.schema);
            if !field.required {
                chain.push_str(".optional()");
            }
            if let Some(docs) = &field.docs {
                // Escape single quotes so a doc comment cannot break the literal.
                let _ = write!(chain, ".describe('{}')", docs.replace('\'', "\\'"));
            }
            let _ = writeln!(out, "  {}: {chain},", quote_property(&field.name));
        }
        let _ = writeln!(out, "}})\n");
        let _ = writeln!(out, "export type {name} = z.infer<typeof {name}Schema>\n");
    }

    out.push_str("/** Every Remote method this build serves. */\n");
    out.push_str("export const methods = {\n");
    for method in &catalogue.methods {
        let _ = writeln!(out, "  '{}': {{", method.qualified());
        out.push_str("    input: z.object({\n");
        for parameter in &method.parameters {
            let mut chain = render_zod(&parameter.schema);
            if !parameter.required {
                chain.push_str(".optional()");
            }
            let _ = writeln!(out, "      {}: {chain},", quote_property(&parameter.name));
        }
        out.push_str("    }),\n");
        let _ = writeln!(out, "    output: {},", render_zod(&method.returns));
        let _ = writeln!(out, "  }},");
    }
    out.push_str("} as const\n\n");

    out.push_str("/** The wire revision this module describes. */\n");
    let _ = writeln!(out, "export const revision = {}", catalogue.revision);
    out
}

/// Renders one schema as a `zod` expression.
fn render_zod(schema: &Schema) -> String {
    match schema {
        Schema::String => "z.string()".to_owned(),
        Schema::Bool => "z.boolean()".to_owned(),
        // `int()` rather than `number()`: a fractional value for an integer field is a bug the client
        // can catch before sending.
        Schema::Int64 => "z.number().int()".to_owned(),
        Schema::Float64 => "z.number()".to_owned(),
        Schema::Any => "z.unknown()".to_owned(),
        Schema::Enum(values) => {
            let literals: Vec<String> = values
                .iter()
                .map(|value| format!("z.literal('{}')", value.replace('\'', "\\'")))
                .collect();
            if literals.len() == 1 {
                literals
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| "z.never()".to_owned())
            } else {
                format!("z.union([{}])", literals.join(", "))
            }
        }
        Schema::Array(element) => format!("z.array({})", render_zod(element)),
        Schema::Map(value) => format!("z.record(z.string(), {})", render_zod(value)),
        Schema::OneOf(variants) => {
            let branches: Vec<String> = variants
                .iter()
                .map(|variant| render_zod_branch(&variant.tag, &variant.schema))
                .collect();
            if branches.len() == 1 {
                branches
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| "z.never()".to_owned())
            } else {
                format!("z.union([{}])", branches.join(", "))
            }
        }
        Schema::Object(object) => {
            let fields: Vec<String> = object
                .fields
                .iter()
                .map(|field| {
                    let mut chain = render_zod(&field.schema);
                    if !field.required {
                        chain.push_str(".optional()");
                    }
                    format!("{}: {chain}", quote_property(&field.name))
                })
                .collect();
            format!("z.object({{{}}})", fields.join(", "))
        }
        Schema::Ref(name) => format!("{name}Schema"),
        Schema::Optional(inner) => format!("{}.nullable()", render_zod(inner)),
    }
}

/// Renders a union branch, fixing its discriminator.
///
/// The `kind` is a literal so the narrowing a client performs is sound: a branch that accepted any
/// string would let a value through that no variant owns.
fn render_zod_branch(tag: &str, schema: &Schema) -> String {
    match schema {
        Schema::Object(object) => {
            let mut fields = vec![format!("kind: z.literal('{}')", tag.replace('\'', "\\'"))];
            for field in &object.fields {
                let mut chain = render_zod(&field.schema);
                if !field.required {
                    chain.push_str(".optional()");
                }
                fields.push(format!("{}: {chain}", quote_property(&field.name)));
            }
            format!("z.object({{{}}})", fields.join(", "))
        }
        other => render_zod(other),
    }
}

/// Quotes an object property name when it is not a bare identifier.
fn quote_property(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_');
    if bare {
        name.to_owned()
    } else {
        format!("'{}'", name.replace('\'', "\\'"))
    }
}
