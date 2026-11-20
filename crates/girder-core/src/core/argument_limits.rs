//! Size limits on one tool call's arguments, applied *before* the JSON is parsed.
//!
//! The dispatch path takes the model's raw argument text and hands it straight to `serde_json`
//! (`agent_loop`'s `run_body`), reacting only to what the parser says. A parser is the wrong place
//! to learn that a call is enormous, because the cost is already paid by then: parsing allocates in
//! proportion to the input, so a 100 MB argument string is 100 MB of work before any later check
//! gets a turn. This module is therefore a pure function over the raw text — no tool context, no
//! I/O, no parsing of its own beyond what the size bound has already made affordable — so it can be
//! called at the one point where running it is still cheap.
//!
//! # Why the total is checked first
//!
//! Every other finding is a statement about one field. Once the whole argument string is over the
//! total limit, those statements are noise: a 10 MB `content` field inside a 10 MB call produces
//! "`content` is 10 MB", which invites the model to shrink one field while the call it must
//! actually restructure stays the same size. The total limit is the one that says "this call as a
//! whole cannot be sent", so it is the one reported, and it is reported without parsing anything.
//!
//! # Why the field name is carried
//!
//! "The arguments are too large" is not actionable: a call can have a dozen fields, and the model
//! that produced it has no way to know which one it can shorten. [`Violation::field`] names the
//! offending string, with the path to it when it is nested, so the remedy is visible in the report
//! instead of being something the reader has to search for. The path is dotted for object members
//! and bracketed for array elements (`input.body`, `files[2]`); it is a locator for a person, not
//! a JSON Pointer, and a key that itself contains a dot will read ambiguously — which is acceptable
//! for a message and preferable to quoting rules nobody reads.
//!
//! # The order of the checks, and what each one is for
//!
//! 1. **Total** — on the raw byte count, before parsing. This is the only bound that can be applied
//!    without decoding the text, and it is the bound that makes the later ones affordable.
//! 2. **Key count** — on the top-level object. A call may be built from ten thousand tiny keys,
//!    each far below the per-string limit and each contributing a few bytes: the shape defeats a
//!    per-field rule, so it needs a rule of its own. Keys below the top level are left to the total,
//!    because counting them would mean the same walk the string check already performs.
//! 3. **Longest string** — after the structure is known. When several strings are over the limit,
//!    the largest is reported: it is the one that has to shrink the most, and reporting the first
//!    one in document order would name a field whose repair does not make the call fit.
//!
//! Every limit is inclusive: a value exactly equal to its limit is accepted, because the limit
//! names the largest permitted size. A caller that wrote "one mebibyte" means one mebibyte is fine.
//!
//! # What this cannot check
//!
//! A malformed argument string has no fields to measure, so it yields no violation here; the parse
//! error is the report, and `tool_retry` already turns it into corrective advice. Sizes are UTF-8
//! byte counts, not character counts, because bytes are what the transport and the allocator spend.

use std::fmt::Write as _;

use serde_json::Value;

/// The default cap on one string argument, in bytes.
///
/// One mebibyte covers a source file or a generous prompt fragment, and stops well short of the
/// calls that motivated this module: a model writing a multi-megabyte file in one `write` call
/// should chunk it, and a caller that genuinely needs a larger payload can pass its own [`Limits`].
const DEFAULT_MAX_STRING_BYTES: usize = 1024 * 1024;

/// The default cap on the whole argument string, in bytes.
///
/// Four mebibytes leaves room for several large fields plus the punctuation around them while
/// keeping the parse that follows bounded. It is deliberately larger than the per-string cap: a
/// call with four large arguments is unusual but legitimate, while one argument of that size is not.
const DEFAULT_MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;

/// The default cap on top-level keys.
///
/// Real calls carry a handful of named arguments. A few hundred is already beyond anything a tool
/// schema declares, so the number is set where a call stops looking like a call and starts looking
/// like a container being used to smuggle bulk past a per-field check.
const DEFAULT_MAX_KEYS: usize = 256;

/// The caps one caller applies to one call.
///
/// All three are inclusive: a value exactly equal to its limit is accepted, so a limit of `n` means
/// `n` bytes are allowed. A limit smaller than another is not an error — the total check runs first,
/// so a total below the per-string cap simply makes the total the binding constraint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// The field names are the API: each one is the limit it holds, and the shared `max_` prefix is what
// makes that readable at a call site. Renaming them to satisfy the lint would take the meaning away.
#[allow(clippy::struct_field_names)]
pub struct Limits {
    /// The largest single string argument, in bytes.
    pub max_string_bytes: usize,
    /// The largest complete argument string, in bytes.
    pub max_total_bytes: usize,
    /// The most keys a top-level object may have.
    pub max_keys: usize,
}

/// One reason a call was refused.
///
/// Carries the numbers that produced it rather than a rendered sentence: wording belongs to the
/// caller's presentation layer, and a stored message cannot be compared against a different policy
/// later. Use [`Violation::explain`] for the default wording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The path of the offending value, or empty when the finding is about the call as a whole.
    ///
    /// Object members are joined with dots and array elements are bracketed, so `input.body` and
    /// `files[2]` name the string that is too large. [`Kind::TotalTooLarge`] and
    /// [`Kind::TooManyKeys`] describe the arguments themselves and leave this empty.
    pub field: String,
    /// What was measured: bytes for the size kinds, a count for [`Kind::TooManyKeys`].
    pub actual: usize,
    /// The limit the measurement exceeded.
    pub limit: usize,
    /// Which limit it was.
    pub kind: Kind,
}

/// The limit a [`Violation`] is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One string argument is larger than [`Limits::max_string_bytes`].
    StringTooLarge,
    /// The complete argument string is larger than [`Limits::max_total_bytes`].
    TotalTooLarge,
    /// The top-level object has more keys than [`Limits::max_keys`].
    TooManyKeys,
}

/// Decide whether one call's arguments fit within `limits`.
///
/// Takes the argument string exactly as the model produced it, and returns the first violation in
/// the order documented at the module level, or `None` when the call is admissible. The total check
/// runs on the raw byte count, before `serde_json` sees the text, which is what keeps an oversized
/// call from being paid for twice.
///
/// Text that is not a JSON value yields `None`: there are no fields to measure, and the parse
/// failure is reported by the caller that parses. Empty or whitespace-only input likewise yields
/// `None`, since a call with no arguments cannot be too large.
#[must_use]
pub fn check(limits: &Limits, arguments: &str) -> Option<Violation> {
    // Measured on the untouched text: no decoding, no allocation, and so the one check that is safe
    // to run on input of any size. This is also why it comes first -- it is the reason the parse
    // below is bounded work rather than an attack surface.
    if arguments.len() > limits.max_total_bytes {
        return Some(Violation {
            field: String::new(),
            actual: arguments.len(),
            limit: limits.max_total_bytes,
            kind: Kind::TotalTooLarge,
        });
    }

    // Nothing to parse and nothing to measure. `trim` rather than `is_empty` because a body of
    // whitespace is the same call as an empty one, and `serde_json` would reject it as malformed.
    if arguments.trim().is_empty() {
        return None;
    }

    // Parsing is bounded by the check above, and the structure is needed for the two remaining
    // checks: keys live in the object, string sizes live in its leaves.
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return None;
    };

    // Counted on the top level only: that is where a schema declares its arguments, and a bulk
    // container smuggled in one field is caught by the total on the next attempt anyway.
    if let Value::Object(map) = &value
        && map.len() > limits.max_keys
    {
        return Some(Violation {
            field: String::new(),
            actual: map.len(),
            limit: limits.max_keys,
            kind: Kind::TooManyKeys,
        });
    }

    let mut worst: Option<Violation> = None;
    let mut path = String::new();
    walk(limits, &value, &mut path, &mut worst);
    worst
}

/// The default limits.
///
/// A policy rather than three constants at the call site so that a deployment can start from a
/// stated baseline and adjust one number, and so that tests can name the behaviour they exercise
/// without repeating the values.
#[must_use]
pub fn defaults() -> Limits {
    Limits {
        max_string_bytes: DEFAULT_MAX_STRING_BYTES,
        max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        max_keys: DEFAULT_MAX_KEYS,
    }
}

impl Violation {
    /// Render the finding as a sentence for a person or a model.
    ///
    /// The field name and both numbers are included: a reader who is told which value to shorten,
    /// how large it is, and what the cap is has everything needed to fix the call. Byte counts are
    /// given exactly rather than rounded into a unit, because the number is compared against a
    /// configured limit and a rounded figure can read as "at the limit" when it is over it.
    #[must_use]
    pub fn explain(&self) -> String {
        match self.kind {
            Kind::TotalTooLarge => format!(
                "the arguments are {} bytes; the limit is {} bytes",
                self.actual, self.limit
            ),
            Kind::TooManyKeys => format!(
                "the arguments have {} keys; the limit is {}",
                self.actual, self.limit
            ),
            Kind::StringTooLarge if self.field.is_empty() => format!(
                "the argument is a {}-byte string; the limit is {} bytes",
                self.actual, self.limit
            ),
            Kind::StringTooLarge => format!(
                "`{}` is {} bytes; the limit is {} bytes",
                self.field, self.actual, self.limit
            ),
        }
    }
}

/// Walk `value`, recording the largest oversized string and the path that leads to it.
///
/// Recursion is bounded by the parser: `serde_json` refuses to build a value nested more than 128
/// containers deep, and its own refusal is the depth guard here, so no counter is needed. `path` is
/// one buffer that is extended on the way down and truncated on the way back, which keeps the walk
/// free of per-node allocation; it is copied only when a candidate is recorded, and there is at most
/// one recorded candidate.
fn walk(limits: &Limits, value: &Value, path: &mut String, worst: &mut Option<Violation>) {
    match value {
        Value::String(text) => {
            if text.len() > limits.max_string_bytes
                && worst.as_ref().is_none_or(|found| text.len() > found.actual)
            {
                *worst = Some(Violation {
                    field: path.clone(),
                    actual: text.len(),
                    limit: limits.max_string_bytes,
                    kind: Kind::StringTooLarge,
                });
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                let mark = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(key);
                walk(limits, child, path, worst);
                path.truncate(mark);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                let mark = path.len();
                // A `String` write cannot fail; the result is discarded because the buffer is the
                // destination and there is no error channel to report.
                let _ = write!(path, "[{index}]");
                walk(limits, child, path, worst);
                path.truncate(mark);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}
