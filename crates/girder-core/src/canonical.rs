//! Canonical encodings for the strings that decide identity.
//!
//! Three modules in this crate ask the same underlying question — "are these two inputs the same?"
//! — and each answers it with a string that is compared for equality rather than hashed:
//! `response_cache`'s `Fingerprint` (one [`LlmRequest`]), `tool_cache`'s `Key` (one tool call), and
//! `tool_idempotency`'s `fingerprint` (one recorded call). They are *not* the same key: they answer
//! three different questions over three different inputs, they live for three different lifetimes,
//! and merging them would mean one cache could answer for another. What they do share is the
//! encoding discipline that makes a string comparison trustworthy, and that discipline is the only
//! thing this module provides.
//!
//! # The failure this module exists to prevent
//!
//! An identity string that is "usually" right fails silently. A key collision does not produce a
//! visible error; it produces a cache hit — the *other* request's answer, delivered with the shape
//! of a real response. Two properties are therefore non-negotiable, and this module exists to make
//! them reusable instead of re-derived at every call site:
//!
//! 1. **No hashing.** A 64-bit digest halves the stored bytes and, on a birthday-bound schedule,
//!    hands a caller someone else's answer. Every encoding here is injective by construction, so
//!    two different inputs cannot produce the same string at all. A caller that insists on compact
//!    keys may hash the result itself, and takes responsibility for the collision when it does.
//! 2. **No delimiters.** Any separator can occur inside a value, so `["ab", "c"]` and `["a", "bc"]`
//!    would both encode as `ab:c` if a colon separated them. Every variable-length record here is
//!    prefixed with its byte length, which makes the field boundaries unambiguous by construction
//!    rather than by hope. [`Encoder`] and [`key_of`] implement that discipline for flat records.
//!
//! # Why JSON needs its own rule
//!
//! A `serde_json::Value` is a tree, and JSON object members are *unordered*: `{"a":1,"b":2}` and
//! `{"b":2,"a":1}` are the same object by the specification. `serde_json` happens to keep `Map`
//! sorted today, but that is a default of one crate, not a property of the value: enabling
//! `preserve_order` (which any crate in a shared build graph can do) turns the same map into an
//! insertion-ordered `IndexMap`. [`canonical_json`] therefore sorts object keys itself instead of
//! trusting the container, and recurses, because an unsorted nested object is exactly as likely as
//! an unsorted top-level one — and is the case a "sort the outer map" shortcut misses.
//!
//! The trade this module deliberately does *not* make: it never normalizes a *value*. `1` and `1.0`
//! encode differently, `"é"` and `"e\u{301}"` encode differently, and `-0.0` and `0.0` encode
//! differently, because merging two inputs that a caller meant to distinguish is the failure mode
//! above. Only member *order* is normalized, since order carries no information. Two spellings of
//! one object therefore both encode the same; two spellings of one number may miss a cache hit,
//! which is the harmless direction.
//!
//! # What this module does not do
//!
//! It does not read, parse, or validate JSON text. Callers that hold raw argument bytes hand them
//! in unchanged; a caller that wants order-insensitivity must have parsed the value first, and
//! parsing costs the distinction between two textually different spellings. It does not own any
//! cache or any lifetime: it produces a `String` and forgets about it.
//!
//! [`LlmRequest`]: girder_core::LlmRequest

use std::fmt::Write as _;

use serde_json::Value;

/// Encodes `value` as a stable, self-delimiting string.
///
/// Two values produce the same string exactly when they are the same JSON value: object members are
/// sorted by key (recursively, at every depth), array elements keep their order (order is the
/// meaning of an array), and every string and number carries its byte length so that no value can
/// impersonate a boundary.
///
/// The result is not JSON and is not meant to be parsed back. It is meant to be compared.
///
/// # Why not `Value::to_string`
///
/// `Display` for a `Value` writes object members in the map's own order. That is sorted only by
/// `serde_json`'s default backend; a build with `preserve_order` enabled — a feature any crate in
/// the dependency graph may turn on — writes insertion order, and the same object built two ways
/// then compares unequal. This function sorts explicitly, so it is correct under both backends.
///
/// # Panics
///
/// Does not panic. Every record is appended to a `String`, whose `write!` cannot fail.
#[must_use]
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    encode_value(&mut out, value);
    out
}

/// Appends one value's encoding.
///
/// The tags are one byte, so no encoding is a prefix of a differently tagged one. Numbers and
/// strings both carry their length, so their text is never read past its end; arrays and objects
/// carry their element count first, for the same reason.
fn encode_value(out: &mut String, value: &Value) {
    match value {
        // `z` rather than `n`: `n` starts the number tag, and distinct tags are cheaper to reason
        // about than distinct lengths.
        Value::Null => out.push('z'),
        Value::Bool(true) => out.push('t'),
        Value::Bool(false) => out.push('f'),
        Value::Number(number) => {
            out.push('n');
            // `serde_json`'s own rendering of a number, which is exact for every variant it can
            // hold: integers print as integers and floats print their shortest round-trip form. It
            // is not `f64`-normalized, so `1` and `1.0` stay distinct — see the module docs.
            push_record(out, &number.to_string());
        }
        Value::String(text) => {
            out.push('s');
            push_record(out, text);
        }
        Value::Array(items) => {
            out.push('a');
            push_count(out, items.len());
            for item in items {
                encode_value(out, item);
            }
        }
        Value::Object(map) => {
            out.push('o');
            push_count(out, map.len());
            // Sorted here, not inherited from the map: `Map` is a `BTreeMap` only under the default
            // features, and this encoding must not change meaning when a downstream crate enables
            // `preserve_order`.
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            for (key, item) in entries {
                out.push('k');
                push_record(out, key);
                encode_value(out, item);
            }
        }
    }
}

/// Appends `length` in decimal, followed by `:`.
fn push_count(out: &mut String, length: usize) {
    // Writing into a `String` cannot fail; the result is dropped rather than unwrapped so this
    // module keeps its promise of having no panic path.
    let _ = write!(out, "{length}:");
}

/// Appends one self-delimiting byte string: its byte length, `:`, then its bytes.
///
/// The length is in *bytes*, not `char`s: text arrives from a model and from the filesystem, and a
/// character count would slice a multi-byte code point in half if anything ever decoded it.
fn push_record(out: &mut String, text: &str) {
    let _ = write!(out, "{}:", text.len());
    out.push_str(text);
}

/// A length-prefixed encoder for flat, named records.
///
/// This is the primitive the request fingerprints and tool keys need: a fixed set of named fields,
/// written in a fixed order, that must encode identically for equal inputs and differently for
/// different ones. Names are length-prefixed too, so a field's name and value can never be confused
/// with a neighbouring field, and the number variant is tagged separately from the string variant so
/// a numeric field cannot impersonate a textual one.
///
/// # Shape
///
/// ```text
/// let mut encoder = Encoder::new();
/// encoder.field("tool", "read").number("offset", 0);
/// encoder.finish() == "s4:tool4:readn6:offset0;"
/// ```
///
/// Encoded as text rather than as a doctest because the crate root does not declare this module
/// yet, so a doctest here could not name it. `tests/canonical.rs` pins the same bytes from outside
/// the crate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Encoder {
    out: String,
}

impl Encoder {
    /// Starts an empty encoding.
    #[must_use]
    pub const fn new() -> Self {
        Self { out: String::new() }
    }

    /// Appends one named textual field.
    ///
    /// The name is part of the encoding, not a comment: two records that differ only in which field
    /// holds a value are different records, and dropping the name would merge them.
    pub fn field(&mut self, name: &str, value: &str) -> &mut Self {
        self.out.push('s');
        push_record(&mut self.out, name);
        push_record(&mut self.out, value);
        self
    }

    /// Appends one named numeric field.
    ///
    /// Tagged `n` and terminated by `;`. The digits can contain neither, so a number is never read
    /// as the start of the next field, and [`field`](Self::field) with the same name and the text
    /// `"0"` encodes differently from this method with `0`.
    ///
    /// `u64` rather than a signed or arbitrary-precision type: every count and offset this crate
    /// fingerprints is non-negative and bounded, and a caller with a wider number can render it
    /// itself and pass it to [`field`](Self::field).
    pub fn number(&mut self, name: &str, value: u64) -> &mut Self {
        self.out.push('n');
        push_record(&mut self.out, name);
        let _ = write!(self.out, "{value};");
        self
    }

    /// Returns the encoded record.
    #[must_use]
    pub fn finish(self) -> String {
        self.out
    }
}

/// Encodes a list of strings into one stable key.
///
/// The key is the element count followed by each element length-prefixed:
///
/// ```text
/// key_of(&["a"])          == "1:1:a"
/// key_of(&["a", ""])      == "2:1:a0:"      // the count makes the arity part of the key
/// key_of(&["ab", "c"])    == "2:2:ab1:c"
/// key_of(&["a", "bc"])    == "2:1:a2:bc"    // the lengths keep the two splits apart
/// ```
///
/// # Why a count as well as lengths
///
/// Lengths alone already delimit the elements, but not the *number* of them: without a count a
/// reader would have to consume the string to its end, and `["a"]` followed by no more fields would
/// be indistinguishable from `["a"]` followed by an empty one. The count states the shape up front.
#[must_use]
pub fn key_of(parts: &[&str]) -> String {
    let mut out = String::new();
    push_count(&mut out, parts.len());
    for part in parts {
        push_record(&mut out, part);
    }
    out
}
