//! `terminal.retain` as a handler a client can actually call.
//!
//! [`girder_core::retain`] already decides *what* to keep: it cuts a transcript to its tail, hands
//! the rest to [`TerminalPersistStore`], and reports what landed on disk. What it does not do — and
//! must not, because it is a library function a host may call directly — is face a network. It takes
//! a typed [`RetainRequest`](girder_core::RetainRequest) and returns a typed
//! [`RetainError`](girder_core::RetainError), neither of which crosses a request/response boundary.
//! This module is that boundary and nothing else: untrusted JSON in, one JSON value out.
//!
//! It exists as its own file because the alternative — a closure inside `terminal_rpc` — would put
//! argument validation and reply shaping next to the registry's method table, where they would be
//! read as registration detail. The three steps a handler performs (parse, run, describe) are stated
//! here so the wiring can stay one call.
//!
//! # Why `lines` may be omitted
//!
//! A client that did not ask for a cut must not receive one. The distinction matters: a transcript
//! silently shortened to its last screenful looks exactly like a terminal that only ever printed
//! that much, and the reader has no way to tell the two apart. So the field is optional, its absence
//! (and its `null`) mean "the whole transcript", and the cut happens only when a count is named.
//! `null` is treated as absence rather than refused because a client that serialises every field of
//! a request sends `null` for what it is not using — the same convention `terminal.create` accepts.
//!
//! # Why the reply is exactly `bytes`/`lines`/`truncated`
//!
//! These are the three facts [`Retained`](girder_core::RetainedOutput) reports, and each answers a
//! question the client cannot answer for itself: how much is on disk, how many lines a reader will
//! see, and — the one that cannot be recovered afterwards — whether the transcript is a *suffix* of
//! what the terminal printed. A reply that dropped `truncated` would let a partial history be read
//! as the whole one, which is the failure this method exists to make visible. Nothing else is added:
//! a client that wants the stored text calls `terminal.read`, and duplicating it here would turn one
//! reply into two sources of the same bytes.
//!
//! # Why errors are strings
//!
//! The handler boundary already carries a message, and a client's only response to any failure here
//! is to show it. [`RetainError`](girder_core::RetainError) is flattened with `to_string`, which
//! keeps the store's own text ("`../x` is not a usable terminal id: …") instead of rewriting it into
//! a generic failure — the caller needs to know that the *id* was wrong, not that something went
//! wrong. Parse failures are formatted the same way, so a caller has one kind of error to render.
//!
//! # Why the description carries an `inputSchema`
//!
//! A method list is what a client reads before it knows which fields to send. `terminal.retain` was
//! previously declared as a gap with no arguments at all, so a client could not even find out that
//! it takes an id and an optional line count. [`description`] publishes those three things — the
//! callable name, what the method does, and the arguments — in the same
//! `{name, description, inputSchema}` shape [`girder_core::to_mcp`] uses, so it can be dropped into
//! an existing listing without a second projection to keep in sync.
//!
//! The schema is deliberately *not* stricter than the parser: it does not set
//! `additionalProperties: false`, because [`parse_args`] ignores fields it does not know. A schema
//! that refused them would make a validating client reject a request this server would happily
//! answer, and it would make adding a field later a breaking change for every client that validates.

// The method is not mounted yet: adding `retain` to `terminal_rpc`'s table is a separate change to
// that module, so until the crate root registers this handler every item here is reachable from the
// module's own tests and from nowhere else. Declared rather than removed so the file keeps compiling
// as one unit, and so wiring it does not require deleting this attribute.
#![allow(dead_code)]

use serde_json::{Value, json};

use girder_core::{RetainRequest, TerminalPersistStore, retain};

/// The namespace `terminal.retain` lives under.
pub const NAMESPACE: &str = "terminal";

/// The method name this module serves.
pub const METHOD: &str = "retain";

/// The arguments of one `terminal.retain` call, after validation.
///
/// Only [`parse_args`] builds one, and it builds one only from arguments that passed every check
/// this module performs. That is why the type does not derive `Deserialize`: deriving it would let a
/// caller skip the checks with `serde_json::from_value` and reach [`execute`] with a blank id or a
/// zero-line cut, and the error would then come from the store instead of naming the field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    /// The terminal whose output is being retained.
    ///
    /// Also the name of the stored transcript: the store validates it as a single path component,
    /// and its message is what a caller sees when the shape is wrong.
    pub id: String,

    /// How many trailing lines to keep, or `None` for the whole transcript.
    ///
    /// `None` is absence, which means "everything" — never "zero". A request that names no cut must
    /// come back complete, so the default cannot be a limit of any size.
    pub lines: Option<usize>,
}

/// Reads the arguments of a `terminal.retain` call.
///
/// Each field is decoded with `serde`'s own type checking rather than read out of the `Value` by
/// hand: a hand-read `lines` is how `{"lines": -1}` or `{"lines": "2"}` turns into a plausible
/// default and retains something nobody asked for. Decoding happens one field at a time so the
/// failure can name the field — `serde`'s whole-struct message reports the expected type but not
/// which member of the object failed, which leaves the caller guessing between `id` and `lines`.
///
/// # Errors
///
/// Returns a message for the caller when `args` is not a JSON object, when `id` is absent or not a
/// string, when `lines` is present but not a non-negative integer, and when either field is outside
/// what this method accepts: `id` must not be empty, and `lines` must be at least 1.
///
/// The two value checks run here rather than in [`execute`] because both are decidable without
/// touching the store — a blank id can never name a transcript, and a cut of zero lines keeps
/// nothing — so the caller is told which *field* to fix before any output is written. Every other
/// id rule (dots, separators, length) belongs to [`TerminalPersistStore`] and is not repeated here:
/// a second copy of that rule set would drift from the one that actually decides.
pub fn parse_args(args: &Value) -> Result<Args, String> {
    let Some(object) = args.as_object() else {
        return Err(format!(
            "the arguments of `terminal.retain` must be a JSON object, not {}",
            json_kind(args)
        ));
    };

    // Unknown fields are ignored rather than refused: a client built against a later version of this
    // method sends fields this build has never heard of, and failing its request would make the
    // server forward-incompatible for a reason the client cannot see or fix.
    let Some(id) = object.get("id") else {
        return Err("`id` is required: it names the terminal whose output is retained".to_owned());
    };
    let id: String = serde_json::from_value(id.clone())
        .map_err(|error| format!("`id` must be a string: {error}"))?;

    // An absent field and an explicit `null` mean the same thing, because a client that serialises
    // every field of its request sends `null` for the ones it is not using. Neither is a cut.
    let lines = match object.get("lines") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value::<usize>(value.clone()).map_err(|error| {
                format!(
                    "`lines` must be a non-negative whole number of lines, or omitted to keep the \
                     whole transcript: {error}"
                )
            })?,
        ),
    };

    // The store would refuse this too, but with a message about file names — which is not what went
    // wrong, and would send the caller looking at the id's characters instead of at its emptiness.
    if id.is_empty() {
        return Err("`id` must not be empty: it names the transcript to write".to_owned());
    }

    // Reachable only from an explicit `0`. Absence is `None` and keeps everything, so this cannot
    // catch a caller who simply omitted the field.
    if lines == Some(0) {
        return Err(
            "`lines` must be at least 1: a retain of zero lines would keep nothing".to_owned(),
        );
    }

    Ok(Args { id, lines })
}

/// Retains `output` as the transcript of `args.id` and returns the reply for the client.
///
/// `output` is the text the registry is holding for that terminal at the moment of the call; this
/// function never reads the terminal itself, so a caller may pass a transcript it captured earlier.
///
/// # Errors
///
/// Returns the string form of [`RetainError`](girder_core::RetainError): the message for empty
/// output (or a zero-line cut, which leaves nothing to write), and the store's own message when it
/// refuses the id or cannot write the file. The store's text is forwarded verbatim because it is
/// the only description of *why* the write failed — the store knows the path and the errno, and this
/// layer does not.
pub fn execute(store: &TerminalPersistStore, args: &Args, output: &str) -> Result<Value, String> {
    let request = RetainRequest {
        id: args.id.clone(),
        lines: args.lines,
    };

    let retained = retain(store, &request, output).map_err(|error| error.to_string())?;

    // The field names are the ones in `Retained`, not a rewording of them, so a client generated
    // from the type sees the same three words on the wire as in the library.
    Ok(json!({
        "bytes": retained.bytes,
        "lines": retained.lines,
        "truncated": retained.truncated,
    }))
}

/// Describes `terminal.retain` for a method list or a tool listing.
///
/// The name is fully qualified because a listing of methods is flat: a client looks up the string it
/// calls, and it calls `terminal.retain`, not `retain` in some implicit namespace.
///
/// `lines` is absent from `required` on purpose — that absence is how a client learns the field is
/// optional, and optional is what makes "keep everything" expressible. Its `minimum` of 1 matches
/// what [`parse_args`] accepts, so a client that validates before sending is never told a request is
/// fine that the server then refuses.
#[must_use]
pub fn description() -> Value {
    json!({
        "name": format!("{NAMESPACE}.{METHOD}"),
        "description": "Keep a terminal's output on disk so it outlives the process that printed \
                        it. Omit `lines` to keep the whole transcript; name a count to keep that \
                        many trailing lines.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Terminal whose output is retained. Also names the stored \
                                    transcript, so it must be usable as a single file name."
                },
                "lines": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "How many trailing lines to keep. Omit, or send null, to keep \
                                    the whole transcript. When the transcript is cut, `truncated` \
                                    in the reply is true."
                }
            },
            "required": ["id"]
        }
    })
}

/// Names what kind of JSON value `value` is, for an error message about a wrong top-level type.
///
/// `serde`'s own message for this case names the `struct` it was deserialising into, which is an
/// implementation detail a client should not have to read. This function exists so the failure says
/// what arrived ("not an array") rather than what this module expected internally.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}
