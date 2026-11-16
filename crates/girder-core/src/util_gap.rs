//! What a streaming tool call's arguments look like before they are finished.
//!
//! Upstream `packages/util/values` is 959 source lines of two related things. The first,
//! `index.ts`, is a set of value predicates — `isJsonValue`, `deepEqualJson`, `deepFreeze`,
//! `snapshotJsonValue`, `assertNever`, `WeakMapWithValues` — that exist because JavaScript has
//! values JSON cannot carry and because a `for…in` walk can be fooled by a `Proxy` or a forged
//! prototype. Rust has neither problem: a [`Value`] cannot hold `NaN`, `-0`, a cycle, a sparse
//! array, or a foreign prototype, so `isJsonValue` answers `true` by construction and `deepFreeze`
//! has no referent. Those functions are deliberately absent, and [`is_json_value`] exists only to
//! state the conclusion rather than leave a caller hunting for the guard that was skipped.
//!
//! The second, `partial-json.ts` at 666 lines, is the part that does not translate away.
//!
//! # Why a prefix of an argument is not parseable
//!
//! A model streams a tool call's arguments as text, and every consumer of that call — a UI row, a
//! length limit, a path check — wants to read one field *while the text is still arriving*. Parsing
//! the prefix is not an option: `{"path": "src/` is not JSON, and waiting for the closing brace
//! means a progress display shows nothing until the call is over.
//!
//! So [`PartialArguments`] is a **scanner**, not a parser. It walks the text once, records where
//! each top-level field starts and ends, and never decodes a value until somebody asks for it. That
//! separation is what makes [`PartialArguments::has`] answer `true` for a key whose value is still
//! arriving, and what makes [`PartialArguments::string_length`] cheap on a forty-megabyte argument:
//! it counts decoded characters without ever building the string.
//!
//! # Every reader is total
//!
//! An absent field, a field of the wrong type, and a field whose text is malformed all answer
//! `None` rather than raising. A consumer rendering one row per field cannot be asked to defend
//! against a half-arrived argument, and a scanner that threw on `{"path": "src` would make every
//! progress display responsible for the scanner's failures.
//!
//! # Why an answer is remembered
//!
//! A streaming view answers the same questions over and over as text arrives — once per frame. The
//! owner does not want to re-render everything each time; it wants to know *what changed*. So each
//! read is recorded, and [`PartialArguments::refresh`] re-evaluates the recorded reads and reports
//! whether any answer moved. Without that, a display either re-renders unconditionally (wasteful
//! and flickery) or never re-renders at all.
//!
//! A read may be recorded at a **granularity** — [`PartialArguments::string_length`] with a step,
//! [`PartialArguments::text_prefix`] with a length — because a row showing `12.3 KB` does not
//! change when byte 12,301 arrives. Remembering the rounded value rather than the raw one is what
//! keeps a fast stream from reporting a change on every fragment.
//!
//! # Why the length is counted in UTF-16 units
//!
//! Upstream counts JavaScript string `length`, which is UTF-16 code units: `"😀"` is 2. The limits
//! this feeds are model-facing "the argument is too long" messages that must agree with the
//! client's own count, and a client counts UTF-16. [`PartialArguments::string_length`] therefore
//! counts UTF-16 units too, and [`utf16_len`] is public so a caller checking a finished string uses
//! exactly the arithmetic a streaming check uses.
//!
//! # What is deliberately absent
//!
//! - `WeakMapWithValues` — a workaround for a JavaScript `WeakMap` the ecosystem did not expose
//!   iteration for. A Rust map has no such split.
//! - `assertNever` — an exhaustiveness device for a `switch` over a closed union. A Rust `match` on
//!   an enum is checked at compile time, so the function would have no caller.
//! - `deepFreeze` / `snapshotJsonValue` — upstream freezes a value so a later mutation cannot change
//!   what was published. Rust's ownership does this statically, and a [`Value`] that needs to be
//!   shared is wrapped in an `Arc` by the caller, not deep-copied here.
//! - `PartialArguments.fromObject` — a caller holding a parsed value already has a map. What *is*
//!   useful is asking the same questions of a finished call that were asked of a streaming one, so
//!   that a call site does not branch; [`PartialArguments::from_value`] is that, and nothing more.

use std::collections::BTreeMap;

use serde_json::Value;

/// Whether a value survives JSON transport unchanged.
///
/// Upstream's `isJsonValue` walks the value to reject `NaN`, `Infinity`, `-0`, cycles, sparse
/// arrays, and foreign prototypes. A [`Value`] cannot hold any of those, so this answers `true` for
/// every value it is given. It is kept so the question has somewhere to be asked: a caller porting
/// upstream code finds the answer instead of concluding that a guard was forgotten.
///
/// The one hazard Rust *does* have — an integer above 2^53 that a JavaScript client cannot read
/// back exactly — is not a property of a value's shape. It is checked by `json::hazards`, which
/// knows about the transport, not by this predicate.
#[must_use]
pub const fn is_json_value(_value: &Value) -> bool {
    true
}

/// The number of UTF-16 code units in `text`.
///
/// JavaScript's `string.length` counts UTF-16 code units, so a character outside the Basic
/// Multilingual Plane counts as two. Argument length limits are compared against a count the
/// client produces the same way; counting `char`s here would let a string of emoji pass a limit the
/// client then rejects.
///
/// # Examples
///
/// ```
/// use girder_core::util_gap::utf16_len;
///
/// assert_eq!(utf16_len("abc"), 3);
/// assert_eq!(utf16_len("😀"), 2);
/// assert_eq!(utf16_len(""), 0);
/// ```
#[must_use]
pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Structural equality of two JSON values.
///
/// Upstream's `deepEqualJson` exists because `===` compares object identity, so two separately
/// parsed copies of the same document are not equal. [`Value`] implements structural equality, so
/// this is a named alias — kept because a port of a call site that used `deepEqualJson` should not
/// have to prove that `==` is the same thing.
///
/// # Number comparison
///
/// `serde_json` is built here with `arbitrary_precision`, and compares numbers by their textual
/// form: `1` and `1.0` are *not* equal. That is the conservative answer for a value that crossed a
/// wire, where the spelling is what the peer actually sent.
#[must_use]
pub fn deep_equal_json(left: &Value, right: &Value) -> bool {
    left == right
}

/// The answer to one remembered question.
///
/// Upstream compares answers with `Object.is`, which distinguishes `-0` from `0` and treats `NaN`
/// as equal to itself. Every answer here is a boolean, a count, a string, or an ordered key list, so
/// `PartialEq` is that same relation. A parsed value is carried as its rendered text so that two
/// structurally equal values compare equal without a second parse.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Answer {
    /// The field is absent, still open, or of the wrong type.
    Missing,
    /// A boolean answer.
    Bool(bool),
    /// A count, already rounded to the reader's step.
    Stepped(Option<usize>),
    /// A string answer.
    Text(String),
    /// A parsed non-string value, rendered so equality is structural.
    Value(String),
    /// Ordered keys, so a reordering counts as a change.
    Keys(Vec<String>),
}

/// One answered question, kept so a later fragment can be compared against it.
#[derive(Clone, Debug)]
struct Read {
    /// Whether the answer must be re-checked after any content read reports an error.
    completion: bool,
    /// The answer at the last check.
    last: Answer,
}

/// A remembered question, re-askable without capturing a borrow of the view.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Question {
    /// Whether no further fields can arrive.
    Closed,
    /// The discovered keys, in first-appearance order.
    Keys,
    /// Whether a field has appeared.
    Has(String),
    /// Whether a field's closing delimiter has arrived.
    Complete(String),
    /// A string's decoded length, at a step and offset.
    Length {
        /// The field.
        key: String,
        /// Characters per reported change.
        step: usize,
        /// Completed characters included in the displayed total.
        offset: usize,
    },
    /// Whether a string exceeds a limit.
    Exceeds {
        /// The field.
        key: String,
        /// The limit, inclusive.
        limit: usize,
    },
    /// A string's decoded text.
    Text(String),
    /// The first `limit` decoded characters of a string.
    Prefix {
        /// The field.
        key: String,
        /// The maximum decoded length.
        limit: usize,
    },
    /// A completed non-string value.
    Value(String),
}

/// The scanner's position in the object text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    /// Before the opening brace.
    #[default]
    Root,
    /// After `{`: a key or `}` may follow.
    KeyOrEnd,
    /// After `,`: only a key may follow.
    KeyOnly,
    /// Inside a key's quotes.
    Key,
    /// After a key, waiting for `:`.
    Colon,
    /// After `:`, waiting for a value.
    Value,
    /// Inside a string value's quotes.
    String,
    /// Inside an unquoted scalar: a number, `true`, `false`, or `null`.
    Scalar,
    /// Inside a nested object or array, tracking its brackets.
    Nested,
    /// After a value: `,` or `}` may follow.
    CommaOrEnd,
    /// After the closing brace.
    Closed,
    /// The text cannot be an object; no further field will be located.
    Invalid,
}

/// One located top-level field.
///
/// Only ranges are stored. A value is decoded when a reader asks, which is what keeps the scanner
/// linear in the text and independent of how large any one value is.
#[derive(Clone, Debug)]
enum Entry {
    /// A string value; the range excludes its quotes.
    String {
        /// Offset of the first content character, after the opening quote.
        start: usize,
        /// Offset of the closing quote, or `None` while the string is still open.
        end: Option<usize>,
        /// Whether the raw range contains a backslash or a control character.
        needs_decoding: bool,
        /// Offset of an escape sequence that could not be decoded, if one was found.
        invalid_at: Option<usize>,
        /// The length reader's position.
        length: Option<StringRead>,
        /// The text reader's position.
        text: Option<StringRead>,
        /// Text readers keyed by their decoded limit.
        prefixes: BTreeMap<usize, StringRead>,
    },
    /// A non-string value, kept as a raw range and parsed only when asked for.
    Raw {
        /// Offset of the value's first character.
        start: usize,
        /// Offset one past the value's last character, or `None` while still open.
        end: Option<usize>,
        /// The parsed value, once somebody asked for it.
        parsed: Option<Value>,
        /// Whether parsing the range failed.
        invalid: bool,
    },
}

/// What one string read covers and how far it may go.
///
/// A struct rather than five positional parameters: three of them are offsets and limits, and a
/// caller that swapped two would still compile and silently read the wrong range.
#[derive(Clone, Copy, Debug)]
struct StringSpan {
    /// Offset of the field's first content character.
    start: usize,
    /// Offset of the closing quote, or `None` while the string is still open.
    end: Option<usize>,
    /// Whether the raw range contains a backslash or a control character.
    needs_decoding: bool,
    /// Offset of an escape sequence that could not be decoded, if one was found.
    invalid_at: Option<usize>,
    /// The largest number of decoded units to produce.
    limit: usize,
    /// Whether the decoded characters should be kept.
    materialize: bool,
}

/// How far one string reader has progressed.
///
/// Upstream keeps `{ at, length, text }` per reader, and the three fields are needed here for the
/// same reason: a read must resume where it stopped rather than rescan from the start of the field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct StringRead {
    /// Source offset the read has consumed up to.
    at: usize,
    /// Decoded UTF-16 units produced so far.
    length: usize,
    /// Decoded text produced so far, when the reader asked for it.
    text: String,
}

/// Lazily scanned view of one JSON object's top-level fields.
///
/// Built from text that may still be arriving, or from a finished value. Nothing is scanned until a
/// reader asks, and a reader's answer is remembered so [`refresh`](Self::refresh) can report what
/// changed.
///
/// # Examples
///
/// ```
/// use girder_core::util_gap::PartialArguments;
///
/// let mut view = PartialArguments::new();
/// // The append answers `Err` only when the view is sealed; a fresh view always accepts.
/// assert!(view.append(r#"{"path": "src/main.rs", "limit": 40}"#).is_ok());
///
/// assert!(view.has("path"));
/// assert_eq!(view.text("path").as_deref(), Some("src/main.rs"));
/// assert_eq!(view.value("limit"), Some(serde_json::json!(40)));
/// ```
// The flags below are independent facts about the same text — a pending escape, an escaped key, a
// nested value inside a string, a read that failed to parse, a sealed view — and grouping them into
// a sub-struct would name a concept the scanner does not have.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent scanner fact, not a mode"
)]
#[derive(Clone, Debug, Default)]
pub struct PartialArguments {
    /// Streamed fragments, kept so any range can be materialized without a cumulative copy.
    chunks: Vec<String>,
    /// End offset of each fragment in the concatenated text.
    ends: Vec<usize>,
    /// Total length of the concatenated text.
    size: usize,
    /// How much of the text the scanner has consumed.
    consumed: usize,
    /// The scanner's current position.
    mode: Mode,
    /// Whether a backslash in the current string or key is pending.
    escape: bool,
    /// Offset of the current key's first character.
    key_start: usize,
    /// Whether the current key contained a backslash.
    key_escaped: bool,
    /// The current key, once decoded.
    key: String,
    /// Brackets of the nested value currently being skipped, outermost first.
    nested_ends: Vec<char>,
    /// Whether the nested value is inside a string.
    nested_in_string: bool,
    /// Offset where the text stopped being a valid object, if it did.
    invalid_at: Option<usize>,
    /// Whether a *read*, rather than the scanner, discovered an invalid value.
    invalid_value: bool,
    /// Located fields.
    entries: BTreeMap<String, Entry>,
    /// Keys in first-appearance order.
    order: Vec<String>,
    /// Remembered questions and their answers, in insertion order.
    reads: Vec<(Question, Read)>,
    /// Whether this view rejects further appends.
    sealed: bool,
    /// The parsed source, when the view was built from one.
    object: Option<Vec<(String, Value)>>,
}

impl PartialArguments {
    /// A fresh streaming view that accepts fragments.
    ///
    /// Upstream's `new PartialArguments()`: the constructor is unsealed, and
    /// [`append`](Self::append) grows it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A view of a call whose arguments have not arrived.
    ///
    /// Upstream's `PartialArguments.EMPTY`. The view is over a finished, empty object, so it is
    /// sealed and answers "no" to every field question.
    ///
    /// # Examples
    ///
    /// ```
    /// use girder_core::util_gap::PartialArguments;
    ///
    /// let mut view = PartialArguments::empty();
    /// assert!(view.is_empty());
    /// assert!(view.closed());
    /// assert!(!view.has("path"));
    /// ```
    #[must_use]
    pub fn empty() -> Self {
        Self::from_value(&serde_json::json!({}))
    }

    /// A sealed view over complete argument text.
    ///
    /// Upstream's `PartialArguments.fromText`. Nothing is scanned until a reader asks.
    ///
    /// # Examples
    ///
    /// ```
    /// use girder_core::util_gap::PartialArguments;
    ///
    /// let mut view = PartialArguments::from_text(r#"{"a": 1}"#);
    /// assert!(view.is_sealed());
    /// assert_eq!(view.keys(), vec!["a".to_owned()]);
    /// ```
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        let mut view = Self::default();
        // A fresh, unsealed view has a zero-length text, so this append cannot be refused. The
        // result is still discarded rather than unwrapped: no panic path belongs in a scanner.
        let _ = view.append(text);
        view.sealed = true;
        view
    }

    /// A sealed view over an already parsed payload.
    ///
    /// Upstream's `PartialArguments.fromObject`. A payload that is not an object has no top-level
    /// fields, which is why a non-object becomes an empty view rather than an error: the call had
    /// no fields to read either way, and a caller indexing an array payload by name was already
    /// asking the wrong question.
    ///
    /// # Examples
    ///
    /// ```
    /// use girder_core::util_gap::PartialArguments;
    /// use serde_json::json;
    ///
    /// let mut view = PartialArguments::from_value(&json!({"n": null}));
    /// assert!(view.has("n"));
    /// assert_eq!(view.value("n"), Some(serde_json::Value::Null));
    /// ```
    #[must_use]
    pub fn from_value(value: &Value) -> Self {
        let object = match value {
            Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            _ => Vec::new(),
        };
        Self {
            object: Some(object),
            sealed: true,
            ..Self::default()
        }
    }

    /// Whether this view has no fields.
    pub fn is_empty(&mut self) -> bool {
        self.keys().is_empty()
    }

    /// Whether this view rejects further appends.
    #[must_use]
    pub const fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// Whether indexing or a content read found invalid JSON.
    ///
    /// Unread value contents are not validated, matching upstream: a view is not obliged to parse
    /// forty megabytes of argument text to answer whether its *keys* are well formed. The only
    /// errors reported here are ones the scanner hit, or ones a reader actually parsed into.
    pub fn invalid(&mut self) -> bool {
        self.scan();
        self.mode == Mode::Invalid || self.invalid_value
    }

    /// Retains streamed argument text without scanning or comparing observed answers.
    ///
    /// # Errors
    ///
    /// Returns the reason when the view is sealed: a sealed view is over text that will not grow,
    /// so accepting one more fragment would make its answers depend on the order of reads.
    pub fn append(&mut self, fragment: &str) -> Result<(), &'static str> {
        if self.sealed {
            return Err("cannot append to a sealed view");
        }
        if fragment.is_empty() {
            return Ok(());
        }
        self.size += fragment.len();
        self.ends.push(self.size);
        self.chunks.push(fragment.to_owned());
        Ok(())
    }

    /// Reconciles a streamed prefix with the authoritative complete text.
    ///
    /// When every streamed fragment matched the final text, this seals the view in place and keeps
    /// its caches; otherwise it builds a new sealed view over `text`. Upstream's `settle`, and for
    /// the same reason: a settled view has already answered the questions the display is showing,
    /// and rebuilding it throws those answers away — as does joining the fragments first, which is
    /// the allocation this avoids.
    #[must_use]
    pub fn settle(self, text: &str) -> Self {
        if self.object.is_some() || text.len() != self.size {
            return Self::from_text(text);
        }
        let mut offset = 0;
        for chunk in &self.chunks {
            if !text
                .get(offset..)
                .is_some_and(|rest| rest.starts_with(chunk.as_str()))
            {
                return Self::from_text(text);
            }
            offset += chunk.len();
        }
        let mut view = self;
        view.chunks = if text.is_empty() {
            Vec::new()
        } else {
            vec![text.to_owned()]
        };
        view.ends = if text.is_empty() {
            Vec::new()
        } else {
            vec![text.len()]
        };
        view.sealed = true;
        view
    }

    /// Compares observed answers and advances their publication baseline.
    ///
    /// Returns whether any observed answer changed since its first read or the preceding refresh. A
    /// view nobody has read stays unscanned, so a refresh costs nothing until there is a displayed
    /// answer to refresh.
    ///
    /// # Why completion answers are checked twice
    ///
    /// A content read can discover an undecodable escape, which makes a field that *looked*
    /// complete incomplete. Upstream therefore re-checks the completion reads after the others
    /// rather than in the same pass: a completion answer is derived from state that the other reads
    /// can change, so evaluating them first would compare against a stale baseline.
    pub fn refresh(&mut self) -> bool {
        if self.reads.is_empty() {
            return false;
        }
        self.scan();
        let mut changed = false;
        for index in 0..self.reads.len() {
            if self.reads[index].1.completion {
                continue;
            }
            let now = self.answer(&self.reads[index].0.clone());
            if now != self.reads[index].1.last {
                changed = true;
            }
            self.reads[index].1.last = now;
        }
        for index in 0..self.reads.len() {
            if !self.reads[index].1.completion {
                continue;
            }
            let now = self.answer(&self.reads[index].0.clone());
            if now != self.reads[index].1.last {
                changed = true;
            }
            self.reads[index].1.last = now;
        }
        if self.sealed {
            self.reads.clear();
        }
        changed
    }

    /// Whether no further fields can arrive.
    ///
    /// True once the outer object closed, once indexing failed, or once the view is sealed.
    pub fn closed(&mut self) -> bool {
        self.ask(Question::Closed) == Answer::Bool(true)
    }

    /// The fields discovered so far, in first-appearance order.
    pub fn keys(&mut self) -> Vec<String> {
        match self.ask(Question::Keys) {
            Answer::Keys(keys) => keys,
            _ => Vec::new(),
        }
    }

    /// Whether a top-level field has appeared.
    ///
    /// A field has appeared once its key is followed by `:`, even while its value is still
    /// arriving — which is what lets a row render its label before its content.
    pub fn has(&mut self, key: &str) -> bool {
        self.ask(Question::Has(key.to_owned())) == Answer::Bool(true)
    }

    /// Whether a field's closing delimiter has arrived, without validating its contents.
    ///
    /// A string value is complete once its closing quote arrived, and a non-string value once its
    /// bracket closed. A content read that then found an undecodable escape makes the field
    /// incomplete again, which is why this answer is one of the reads `refresh` re-checks last.
    pub fn complete(&mut self, key: &str) -> bool {
        self.ask(Question::Complete(key.to_owned())) == Answer::Bool(true)
    }

    /// The decoded UTF-16 length of a string field so far.
    ///
    /// `step` and `offset` control change granularity: the remembered answer is
    /// `ceil((length + offset) / step)`, so a display showing kilobytes reports a change once per
    /// kilobyte rather than once per character. A step of zero is raised to one.
    ///
    /// Returns `None` when the field is absent, still arriving, or not a string.
    pub fn string_length(&mut self, key: &str, step: usize, offset: usize) -> Option<usize> {
        let step = step.max(1);
        match self.ask(Question::Length {
            key: key.to_owned(),
            step,
            offset,
        }) {
            Answer::Stepped(value) => value,
            _ => None,
        }
    }

    /// Whether a string field is longer than `max_length` UTF-16 units.
    ///
    /// The read stops one character past the limit, so checking a one-megabyte argument against a
    /// hundred-character limit does not decode a megabyte. A field that is absent or not a string
    /// does not exceed anything.
    pub fn string_exceeds(&mut self, key: &str, max_length: usize) -> bool {
        self.ask(Question::Exceeds {
            key: key.to_owned(),
            limit: max_length,
        }) == Answer::Bool(true)
    }

    /// The decoded text of a string field so far.
    ///
    /// A streaming string answers with the prefix that has arrived, decoded; an escape sequence
    /// split across two fragments is held back until its second byte arrives rather than reported
    /// as an error.
    pub fn text(&mut self, key: &str) -> Option<String> {
        match self.ask(Question::Text(key.to_owned())) {
            Answer::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The first `max_length` decoded UTF-16 characters of a string field.
    ///
    /// A limit of zero is raised to one, matching upstream: a caller that asked for a prefix wanted
    /// something to show, and an empty rendering is not that.
    pub fn text_prefix(&mut self, key: &str, max_length: usize) -> Option<String> {
        match self.ask(Question::Prefix {
            key: key.to_owned(),
            limit: max_length.max(1),
        }) {
            Answer::Text(text) => Some(text),
            _ => None,
        }
    }

    /// A completed non-string argument.
    ///
    /// `None` while the value is still open, when the field is absent, and when the value is a
    /// string — a string has a text reader instead, and answering it here would make every caller
    /// re-check the type.
    pub fn value(&mut self, key: &str) -> Option<Value> {
        match self.ask(Question::Value(key.to_owned())) {
            Answer::Value(rendered) => serde_json::from_str(&rendered).ok(),
            _ => None,
        }
    }
}

impl PartialArguments {
    /// Answers a question and, on a streaming view, remembers it for change detection.
    fn ask(&mut self, question: Question) -> Answer {
        self.scan();
        let answer = self.answer(&question);
        if !self.sealed {
            let completion = matches!(question, Question::Complete(_));
            if self.reads.iter().all(|(seen, _)| *seen != question) {
                self.reads.push((
                    question,
                    Read {
                        // The baseline is the answer just computed, so the next refresh compares
                        // against what this caller actually saw.
                        last: answer.clone(),
                        completion,
                    },
                ));
            }
        }
        answer
    }

    /// Derives one answer without remembering it.
    fn answer(&mut self, question: &Question) -> Answer {
        match question {
            Question::Closed => Answer::Bool(self.closed_now()),
            Question::Keys => Answer::Keys(self.keys_now()),
            Question::Has(key) => Answer::Bool(self.has_now(key)),
            Question::Complete(key) => Answer::Bool(self.complete_now(key)),
            Question::Length { key, step, offset } => Answer::Stepped(
                self.length_now(key, usize::MAX)
                    .map(|length| length.saturating_add(*offset).div_ceil(*step)),
            ),
            Question::Exceeds { key, limit } => {
                Answer::Bool(self.length_now(key, limit.saturating_add(1)).unwrap_or(0) > *limit)
            }
            Question::Text(key) => match self.text_now(key) {
                Some(text) => Answer::Text(text),
                None => Answer::Missing,
            },
            Question::Prefix { key, limit } => match self.text_prefix_now(key, *limit) {
                Some(text) => Answer::Text(text),
                None => Answer::Missing,
            },
            Question::Value(key) => match self.value_now(key) {
                Some(value) => Answer::Value(value.to_string()),
                None => Answer::Missing,
            },
        }
    }

    fn closed_now(&self) -> bool {
        self.sealed || self.mode == Mode::Closed || self.mode == Mode::Invalid
    }

    fn keys_now(&self) -> Vec<String> {
        match &self.object {
            Some(object) => object.iter().map(|(key, _)| key.clone()).collect(),
            None => self.order.clone(),
        }
    }

    fn has_now(&self, key: &str) -> bool {
        match &self.object {
            Some(object) => object.iter().any(|(name, _)| name == key),
            None => self.entries.contains_key(key),
        }
    }

    fn complete_now(&self, key: &str) -> bool {
        if let Some(object) = &self.object {
            return object.iter().any(|(name, _)| name == key);
        }
        match self.entries.get(key) {
            None => false,
            Some(Entry::String {
                end, invalid_at, ..
            }) => end.is_some() && invalid_at.is_none(),
            Some(Entry::Raw { end, invalid, .. }) => end.is_some() && !invalid,
        }
    }

    /// The parsed value of a string field, or `None`.
    fn string_field(&self, key: &str) -> Option<String> {
        let object = self.object.as_ref()?;
        object
            .iter()
            .find(|(name, _)| name == key)
            .and_then(|(_, value)| match value {
                Value::String(text) => Some(text.clone()),
                _ => None,
            })
    }

    /// The range and decoding facts of a string entry, copied so the reader is not borrow-blocked.
    fn string_entry(&self, key: &str) -> Option<(usize, Option<usize>, bool, Option<usize>)> {
        match self.entries.get(key) {
            Some(Entry::String {
                start,
                end,
                needs_decoding,
                invalid_at,
                ..
            }) => Some((*start, *end, *needs_decoding, *invalid_at)),
            _ => None,
        }
    }

    fn length_now(&mut self, key: &str, limit: usize) -> Option<usize> {
        if let Some(text) = self.string_field(key) {
            return Some(utf16_len(&text));
        }
        let (start, end, needs_decoding, invalid_at) = self.string_entry(key)?;
        if let Some(Entry::String {
            text: Some(finished),
            end: Some(end),
            ..
        }) = self.entries.get(key)
        {
            if finished.at == *end && finished.length <= limit {
                return Some(finished.length);
            }
        }
        let cached = match self.entries.get(key) {
            Some(Entry::String { length, .. }) => length.clone(),
            _ => None,
        };
        let mut read = cached.unwrap_or_default();
        self.read_string(
            key,
            StringSpan {
                start,
                end,
                needs_decoding,
                invalid_at,
                limit,
                materialize: false,
            },
            &mut read,
        );
        let length = read.length;
        if let Some(Entry::String { length: slot, .. }) = self.entries.get_mut(key) {
            *slot = Some(read);
        }
        Some(length)
    }

    fn text_now(&mut self, key: &str) -> Option<String> {
        if let Some(text) = self.string_field(key) {
            return Some(text);
        }
        let (start, end, needs_decoding, invalid_at) = self.string_entry(key)?;
        if let (Some(finished), Some(end)) = (
            match self.entries.get(key) {
                Some(Entry::String { text, .. }) => text.clone(),
                _ => None,
            },
            end,
        ) {
            if finished.at == end {
                return Some(finished.text);
            }
        }
        let mut read = match self.entries.get(key) {
            Some(Entry::String { text, .. }) => text.clone().unwrap_or_default(),
            _ => StringRead::default(),
        };
        self.read_string(
            key,
            StringSpan {
                start,
                end,
                needs_decoding,
                invalid_at,
                limit: usize::MAX,
                materialize: true,
            },
            &mut read,
        );
        let text = read.text.clone();
        if let Some(Entry::String { text: slot, .. }) = self.entries.get_mut(key) {
            *slot = Some(read);
        }
        Some(text)
    }

    fn text_prefix_now(&mut self, key: &str, limit: usize) -> Option<String> {
        if let Some(text) = self.string_field(key) {
            return Some(text.chars().take(limit).collect());
        }
        let (start, end, needs_decoding, invalid_at) = self.string_entry(key)?;
        let mut read = match self.entries.get(key) {
            Some(Entry::String { prefixes, .. }) => {
                prefixes.get(&limit).cloned().unwrap_or_default()
            }
            _ => StringRead::default(),
        };
        self.read_string(
            key,
            StringSpan {
                start,
                end,
                needs_decoding,
                invalid_at,
                limit,
                materialize: true,
            },
            &mut read,
        );
        let text = read.text.clone();
        if let Some(Entry::String { prefixes, .. }) = self.entries.get_mut(key) {
            prefixes.insert(limit, read);
        }
        Some(text)
    }

    fn value_now(&mut self, key: &str) -> Option<Value> {
        if let Some(object) = &self.object {
            return object.iter().find(|(name, _)| name == key).and_then(
                |(_, value)| match value {
                    Value::String(_) => None,
                    other => Some(other.clone()),
                },
            );
        }
        let (start, end, parsed, invalid) = match self.entries.get(key) {
            Some(Entry::Raw {
                start,
                end,
                parsed,
                invalid,
            }) => (*start, *end, parsed.clone(), *invalid),
            _ => return None,
        };
        if invalid {
            return None;
        }
        let end = end?;
        if let Some(parsed) = parsed {
            return Some(parsed);
        }
        let raw = self.slice(start, end);
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            if let Some(Entry::Raw { parsed, .. }) = self.entries.get_mut(key) {
                *parsed = Some(value.clone());
            }
            return Some(value);
        }
        // A requested value that does not parse is remembered as invalid, so a second read answers
        // the same `None` instead of reparsing the same bytes on every frame.
        if let Some(Entry::Raw { invalid, .. }) = self.entries.get_mut(key) {
            *invalid = true;
        }
        self.invalid_value = true;
        None
    }

    /// The index of the fragment containing `at`, or the fragment count when `at` is past the end.
    fn chunk_at(&self, at: usize) -> usize {
        self.ends.partition_point(|end| *end <= at)
    }

    /// Materializes one range, never the whole source.
    fn slice(&self, start: usize, end: usize) -> String {
        if start >= end || start >= self.size {
            return String::new();
        }
        let end = end.min(self.size);
        let first = self.chunk_at(start);
        let last = self.chunk_at(end - 1);
        if first >= self.chunks.len() || last >= self.chunks.len() {
            return String::new();
        }
        let base = if first == 0 { 0 } else { self.ends[first - 1] };
        if first == last {
            return self.chunks[first]
                .get(start - base..end - base)
                .unwrap_or_default()
                .to_owned();
        }
        let mut parts = Vec::with_capacity(last - first + 1);
        parts.push(self.chunks[first].get(start - base..).unwrap_or_default());
        for chunk in &self.chunks[first + 1..last] {
            parts.push(chunk.as_str());
        }
        let tail_base = self.ends[last - 1];
        parts.push(self.chunks[last].get(..end - tail_base).unwrap_or_default());
        parts.concat()
    }

    /// Advances one string reader, decoding escapes and counting UTF-16 units.
    ///
    /// `materialize` distinguishes a length read from a text read. Both must decode, because an
    /// escape sequence is one character of the decoded string but several of the source; only the
    /// text read keeps the characters.
    ///
    /// A read stops at `limit`, at the string's closing quote, and at an undecodable escape. It also
    /// stops — without recording an error — when the text ends mid-escape, because a later fragment
    /// may still complete it.
    fn read_string(&mut self, key: &str, span: StringSpan, read: &mut StringRead) -> usize {
        let StringSpan {
            start,
            end,
            needs_decoding,
            invalid_at,
            limit,
            materialize,
        } = span;
        if read.at < start {
            read.at = start;
        }
        let bound = end
            .unwrap_or(self.size)
            .min(invalid_at.unwrap_or(usize::MAX))
            .min(self.invalid_at.unwrap_or(usize::MAX))
            .min(self.size);
        // A string with no escapes is the common case, and an all-ASCII one can be counted by
        // subtraction because there a byte, a `char`, and a UTF-16 unit are all the same thing.
        // A non-ASCII run cannot: `"😀"` is four bytes but two units, so it falls through to the
        // decoding loop below and the counts agree with the client's own `string.length`.
        if !needs_decoding && self.range_is_ascii(read.at, bound) {
            let length = bound
                .saturating_sub(read.at)
                .min(limit.saturating_sub(read.length));
            if length == 0 {
                return read.length;
            }
            if materialize {
                read.text.push_str(&self.slice(read.at, read.at + length));
            }
            read.at += length;
            read.length += length;
            return read.length;
        }
        while read.at < bound && read.length < limit {
            let Some(byte) = self.byte_at(read.at) else {
                break;
            };
            if byte != b'\\' {
                // Decode one character: it may be several bytes and up to two UTF-16 units, and a
                // limit that would split a surrogate pair stops before the character rather than
                // producing half of one.
                let rest = self.slice(read.at, bound.min(read.at + 4));
                let Some(ch) = rest.chars().next() else {
                    break;
                };
                let units = ch.len_utf16();
                if read.length + units > limit {
                    break;
                }
                if materialize {
                    read.text.push(ch);
                }
                read.at += ch.len_utf8();
                read.length += units;
                continue;
            }
            let Some(kind) = self.byte_at(read.at + 1) else {
                // The backslash is the last byte available. Streaming: wait for more. Sealed: the
                // escape can never complete, so the prefix decoded so far is the answer.
                break;
            };
            if kind == b'u' {
                if read.at + 6 > bound {
                    // A `\u` escape needs four hex digits, and fewer have arrived.
                    break;
                }
                let hex = self.slice(read.at + 2, read.at + 6);
                let decoded = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
                let Some(ch) = decoded else {
                    if let Some(Entry::String { invalid_at, .. }) = self.entries.get_mut(key) {
                        *invalid_at = Some(read.at);
                    }
                    self.invalid_value = true;
                    break;
                };
                if read.length + ch.len_utf16() > limit {
                    break;
                }
                if materialize {
                    read.text.push(ch);
                }
                read.length += ch.len_utf16();
                read.at += 6;
                continue;
            }
            let Some(decoded) = simple_escape(kind) else {
                if let Some(Entry::String { invalid_at, .. }) = self.entries.get_mut(key) {
                    *invalid_at = Some(read.at);
                }
                self.invalid_value = true;
                break;
            };
            if read.length >= limit {
                break;
            }
            if materialize {
                read.text.push(decoded);
            }
            read.length += 1;
            read.at += 2;
        }
        read.length
    }

    /// Whether every byte in `[start, end)` is ASCII.
    ///
    /// Checked over the fragment ranges directly rather than by joining them, because this runs on
    /// every length read and the answer for a long argument is usually "yes".
    fn range_is_ascii(&self, start: usize, end: usize) -> bool {
        if start >= end {
            return true;
        }
        let mut at = start;
        while at < end {
            let chunk = self.chunk_at(at);
            let Some(text) = self.chunks.get(chunk) else {
                return false;
            };
            let base = if chunk == 0 { 0 } else { self.ends[chunk - 1] };
            let from = at - base;
            let to = (end - base).min(text.len());
            let Some(piece) = text.as_bytes().get(from..to) else {
                return false;
            };
            if !piece.is_ascii() {
                return false;
            }
            if to == 0 {
                return false;
            }
            at = base + to;
        }
        true
    }

    fn byte_at(&self, at: usize) -> Option<u8> {
        if at >= self.size {
            return None;
        }
        let chunk = self.chunk_at(at);
        let base = if chunk == 0 { 0 } else { self.ends[chunk - 1] };
        self.chunks.get(chunk)?.as_bytes().get(at - base).copied()
    }

    /// Locates new field ranges without decoding or parsing their contents.
    fn scan(&mut self) {
        if self.object.is_some() || self.consumed >= self.size {
            return;
        }
        let mut index = self.consumed;
        while index < self.size && self.invalid_at.is_none() && self.mode != Mode::Invalid {
            // Inside a string, the only byte that can end it is an unescaped quote; skipping to it
            // keeps a forty-megabyte value from being stepped through byte by byte.
            if self.mode == Mode::String || (self.mode == Mode::Nested && self.nested_in_string) {
                // The terminating quote is at `end`; stepping it below advances the scanner, since
                // a string ends there and a nested string's closing quote clears `nested_in_string`.
                if let Some(end) = self.string_boundary(index) {
                    index = end;
                    self.consumed = index;
                } else {
                    // No terminator has arrived yet, so everything after `index` is content that
                    // will still grow. The scan resumes in the same mode on the next fragment.
                    self.consumed = self.size;
                    return;
                }
            }
            let Some(byte) = self.byte_at(index) else {
                break;
            };
            self.step(byte, index);
            index += 1;
            self.consumed = index;
        }
    }

    /// Finds the quote that terminates the string containing `start`.
    ///
    /// Returns the terminator's offset, or `None` when the text available so far contains no
    /// unescaped quote — the string may still be growing, so that is not an error. Only a quote
    /// whose preceding run of backslashes is even terminates a string, which is why the run is
    /// counted rather than the text decoded.
    ///
    /// The same pass notes whether the content contains any backslash or control character, which
    /// is what decides whether the string needs decoding at all. The common case is that it does
    /// not, and a length read over such a string then costs one subtraction.
    fn string_boundary(&mut self, start: usize) -> Option<usize> {
        // A backslash at the end of the previous fragment escapes whatever arrives next, so the
        // first character of this range cannot terminate the string.
        let mut at = start;
        if self.escape && at < self.size {
            at += 1;
            self.escape = false;
        }
        loop {
            let quote = (at..self.size).find(|probe| self.byte_at(*probe) == Some(b'"'));
            let end = quote.unwrap_or(self.size);
            if self.mode == Mode::String {
                let needs = self.slice(at, end).bytes().any(|b| b == b'\\' || b < 0x20);
                if let Some(Entry::String { needs_decoding, .. }) = self.entries.get_mut(&self.key)
                {
                    *needs_decoding |= needs;
                }
            }
            let mut slash_start = end;
            while slash_start > at && self.byte_at(slash_start - 1) == Some(b'\\') {
                slash_start -= 1;
            }
            // An odd run of backslashes escapes the quote; an unterminated run at the end of the
            // available text leaves the escape pending for the next fragment.
            let escaped = (end - slash_start) % 2 == 1;
            match quote {
                None => {
                    self.escape = escaped;
                    return None;
                }
                Some(_) if escaped => at = end + 1,
                Some(_) => return quote,
            }
        }
    }

    fn step(&mut self, byte: u8, at: usize) {
        let ch = byte as char;
        match self.mode {
            Mode::Root => {
                if is_whitespace(ch) {
                    return;
                }
                if ch == '{' {
                    self.mode = Mode::KeyOrEnd;
                } else {
                    self.fail(at);
                }
            }
            Mode::KeyOrEnd => {
                if is_whitespace(ch) {
                    return;
                }
                if ch == '}' {
                    self.mode = Mode::Closed;
                } else if ch == '"' {
                    self.begin_key(at);
                } else {
                    self.fail(at);
                }
            }
            Mode::KeyOnly => {
                if is_whitespace(ch) {
                    return;
                }
                if ch == '"' {
                    self.begin_key(at);
                } else {
                    self.fail(at);
                }
            }
            Mode::Key => self.step_key(ch, at),
            Mode::Colon => {
                if is_whitespace(ch) {
                    return;
                }
                if ch == ':' {
                    self.mode = Mode::Value;
                } else {
                    self.fail(at);
                }
            }
            Mode::Value => self.begin_value(ch, at),
            Mode::String => {
                if let Some(Entry::String { end, .. }) = self.entries.get_mut(&self.key) {
                    *end = Some(at);
                }
                self.mode = Mode::CommaOrEnd;
            }
            Mode::Scalar => self.step_scalar(ch, at),
            Mode::Nested => self.step_nested(ch, at),
            Mode::CommaOrEnd => {
                if is_whitespace(ch) {
                    return;
                }
                if ch == ',' {
                    self.mode = Mode::KeyOnly;
                } else if ch == '}' {
                    self.mode = Mode::Closed;
                } else {
                    self.fail(at);
                }
            }
            Mode::Closed => {
                if !is_whitespace(ch) {
                    self.fail(at);
                }
            }
            // `scan` stops stepping once the view is invalid, so this arm is unreachable from it.
            // It is spelled out rather than replaced by a wildcard so that adding a mode is a
            // compile error instead of a silently ignored state.
            Mode::Invalid => {}
        }
    }

    fn fail(&mut self, at: usize) {
        self.invalid_at = Some(at);
        self.mode = Mode::Invalid;
    }

    fn begin_key(&mut self, at: usize) {
        self.mode = Mode::Key;
        self.key_start = at + 1;
        self.key_escaped = false;
        self.escape = false;
    }

    fn step_key(&mut self, ch: char, at: usize) {
        if ch < ' ' {
            self.fail(at);
            return;
        }
        if self.escape {
            self.escape = false;
            return;
        }
        if ch == '\\' {
            self.escape = true;
            self.key_escaped = true;
            return;
        }
        if ch != '"' {
            return;
        }
        let raw = self.slice(self.key_start, at);
        if self.key_escaped {
            if let Ok(key) = serde_json::from_str::<String>(&format!("\"{raw}\"")) {
                self.key = key;
            } else {
                // An undecodable key makes every later field's name unknowable, so the whole view
                // is refused rather than silently renaming the field.
                self.fail(at);
                return;
            }
        } else {
            self.key = raw;
        }
        self.mode = Mode::Colon;
    }

    fn begin_value(&mut self, ch: char, at: usize) {
        if is_whitespace(ch) {
            return;
        }
        if ch == '"' {
            self.insert(Entry::String {
                start: at + 1,
                end: None,
                needs_decoding: false,
                invalid_at: None,
                length: None,
                text: None,
                prefixes: BTreeMap::new(),
            });
            self.escape = false;
            self.mode = Mode::String;
            return;
        }
        if ch == '}' || ch == ',' || ch == ':' || ch == ']' {
            self.fail(at);
            return;
        }
        self.insert(Entry::Raw {
            start: at,
            end: None,
            parsed: None,
            invalid: false,
        });
        if ch == '{' || ch == '[' {
            self.mode = Mode::Nested;
            self.nested_ends = vec![if ch == '{' { '}' } else { ']' }];
            self.nested_in_string = false;
            self.escape = false;
            return;
        }
        self.mode = Mode::Scalar;
    }

    fn insert(&mut self, entry: Entry) {
        let key = self.key.clone();
        if !self.entries.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.entries.insert(key, entry);
    }

    fn step_scalar(&mut self, ch: char, at: usize) {
        if ch != ',' && ch != '}' && !is_whitespace(ch) {
            return;
        }
        self.close_value(at);
        self.mode = match ch {
            ',' => Mode::KeyOnly,
            '}' => Mode::Closed,
            _ => Mode::CommaOrEnd,
        };
    }

    fn step_nested(&mut self, ch: char, at: usize) {
        if self.nested_in_string {
            self.nested_in_string = false;
            return;
        }
        if ch == '"' {
            self.nested_in_string = true;
            return;
        }
        if ch == '{' || ch == '[' {
            self.nested_ends.push(if ch == '{' { '}' } else { ']' });
            return;
        }
        if ch == '}' || ch == ']' {
            if self.nested_ends.pop() != Some(ch) {
                self.fail(at);
                return;
            }
            if self.nested_ends.is_empty() {
                // A nested value's range includes its closing bracket, unlike a scalar's, which
                // stops at the delimiter that ended it.
                self.close_value(at + 1);
                self.mode = Mode::CommaOrEnd;
            }
        }
    }

    /// Records where the current field's non-string value ended.
    fn close_value(&mut self, end: usize) {
        if let Some(Entry::Raw { end: slot, .. }) = self.entries.get_mut(&self.key) {
            *slot = Some(end);
        }
    }
}

/// The character a two-byte JSON escape names, or `None` when the escape is not valid.
const fn simple_escape(kind: u8) -> Option<char> {
    match kind {
        b'"' => Some('"'),
        b'\\' => Some('\\'),
        b'/' => Some('/'),
        b'b' => Some('\u{8}'),
        b'f' => Some('\u{c}'),
        b'n' => Some('\n'),
        b'r' => Some('\r'),
        b't' => Some('\t'),
        _ => None,
    }
}

/// Whether a byte is JSON's insignificant whitespace.
const fn is_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\n' | '\r' | '\t')
}
